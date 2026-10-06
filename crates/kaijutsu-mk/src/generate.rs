//! `POST /mk/v1/generate`: request and reply types, the SSE decoder, and the
//! calls (`docs/mk.md`).
//!
//! The service renders `messages` with the model's own chat template; the
//! client never formats a prompt. A streamed reply is `token` events, then one
//! `done`. Anything else that ends a stream is an error, never a short reply.

use std::collections::VecDeque;
use std::time::Duration;

use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::client::{
    decode, excerpt, map_reqwest, require_object, to_body, Family, MkClient, MkError, ServiceErrorBody,
};
use crate::json::Json;
use crate::model::Identity;

/// The largest `max_tokens` the schema allows.
pub const MAX_TOKENS_LIMIT: u32 = 32768;

/// A generation request. The calls set `stream` themselves.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GenerateRequest {
    pub from: Source,
    /// Absent: the service's default, thinking on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    /// Only with [`Source::Messages`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Text the reply starts with. Refused with [`Source::Prompt`] and
    /// [`Source::Tokens`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefill: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample: Option<Sample>,
    /// 1 to [`MAX_TOKENS_LIMIT`]; absent is the service's default of 4096.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

impl GenerateRequest {
    /// A request that renders `messages` with `tools`, every other field at
    /// the service's default.
    pub fn messages(messages: Vec<Message>, tools: Vec<Tool>) -> Self {
        GenerateRequest {
            from: Source::Messages { messages, tools },
            thinking: None,
            reasoning_effort: None,
            prefill: None,
            sample: None,
            max_tokens: None,
        }
    }

    /// Checks the rules the schema states that a client can check, so a
    /// request the service would refuse is never sent.
    pub fn validate(&self) -> Result<(), MkError> {
        let refuse = |m: String| Err(MkError::Request(m));
        match &self.from {
            Source::Messages { messages, tools } => {
                if messages.is_empty() {
                    return refuse("from.messages is empty".into());
                }
                validate_tools(tools)?;
                for (i, m) in messages.iter().enumerate() {
                    for call in &m.tool_calls {
                        if call.name.is_empty() {
                            return refuse(format!("message {i} has a tool call with an empty name"));
                        }
                        if !matches!(call.arguments, Json::Object(_)) {
                            return refuse(format!("message {i}: tool call {:?} arguments are not an object", call.name));
                        }
                    }
                }
            }
            Source::Context { context } => {
                if context.len() != 64 || !context.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
                    return refuse(format!("from.context {context:?} is not a context id"));
                }
            }
            Source::Prompt { prompt } if prompt.is_empty() => return refuse("from.prompt is empty".into()),
            Source::Tokens { tokens } if tokens.is_empty() => return refuse("from.tokens is empty".into()),
            Source::Prompt { .. } | Source::Tokens { .. } => {}
        }
        let renders = matches!(self.from, Source::Messages { .. });
        if self.reasoning_effort.is_some() && !renders {
            return refuse("reasoning_effort applies only to from.messages".into());
        }
        match &self.prefill {
            Some(p) if p.is_empty() => return refuse("prefill is empty".into()),
            Some(_) if matches!(self.from, Source::Prompt { .. } | Source::Tokens { .. }) => {
                return refuse("prefill is refused with from.prompt and from.tokens".into());
            }
            _ => {}
        }
        if let Some(n) = self.max_tokens
            && !(1..=MAX_TOKENS_LIMIT).contains(&n)
        {
            return refuse(format!("max_tokens {n} is outside 1 to {MAX_TOKENS_LIMIT}"));
        }
        if let Some(s) = &self.sample {
            s.validate()?;
        }
        Ok(())
    }
}

/// Refuses a tool the schema refuses: an empty name, or parameters that are
/// not an object.
pub(crate) fn validate_tools(tools: &[Tool]) -> Result<(), MkError> {
    for Tool::Function { function } in tools {
        if function.name.is_empty() {
            return Err(MkError::Request("a tool has an empty name".into()));
        }
        if function.parameters.as_ref().is_some_and(|p| !matches!(p, Json::Object(_))) {
            return Err(MkError::Request(format!("tool {:?} parameters are not an object", function.name)));
        }
    }
    Ok(())
}

/// Where generation starts: exactly one of these.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Source {
    /// Rendered by the service's chat template.
    Messages {
        messages: Vec<Message>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tools: Vec<Tool>,
    },
    /// A held context, by id, extended by this reply.
    Context { context: String },
    /// Raw text; special-token text keeps its meaning.
    Prompt { prompt: String },
    /// Raw token ids.
    Tokens { tokens: Vec<u32> },
}

/// One chat message.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Message {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallIn>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::text(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::text(Role::User, content)
    }

    pub fn tool(content: impl Into<String>) -> Self {
        Self::text(Role::Tool, content)
    }

    fn text(role: Role, content: impl Into<String>) -> Self {
        Message { role, content: Some(content.into()), reasoning_content: None, tool_calls: Vec::new() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A tool call sent back in an assistant message. The service has no call
/// ids; a `tool` message answers the calls in order.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolCallIn {
    pub name: String,
    /// A JSON object. Member order is kept, so the template renders the call
    /// as the model wrote it.
    pub arguments: Json,
}

/// A tool the model may call.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Tool {
    Function { function: Function },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Function {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A JSON Schema for the arguments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Json>,
}

/// The chat template's reasoning effort. Absent is the template's `xhigh`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Xhigh,
    Medium,
    Low,
}

/// Sampling settings. `temperature: 0` is greedy and reproducible.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Sample {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// 0 is off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_p: Option<f64>,
    /// Absent draws a fresh seed; [`Done::seed`] reports the one used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

impl Sample {
    fn validate(&self) -> Result<(), MkError> {
        let refuse = |m: &str| Err(MkError::Request(m.into()));
        if self.temperature.is_some_and(|t| t.is_nan() || t < 0.0) {
            return refuse("sample.temperature must be at least 0");
        }
        if self.top_p.is_some_and(|p| !(p > 0.0 && p <= 1.0)) {
            return refuse("sample.top_p must be above 0 and at most 1");
        }
        if self.min_p.is_some_and(|p| !(0.0..1.0).contains(&p)) {
            return refuse("sample.min_p must be at least 0 and below 1");
        }
        Ok(())
    }
}

/// Which part of the reply a token belongs to. Pieces include the
/// template's own markers, such as `</think>` in `Think` and `<tool_call>` in
/// `Tool`; [`Done`] carries the text without them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Think,
    Answer,
    Tool,
}

/// One generated token.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct TokenEvent {
    pub id: u32,
    /// The text this token completes. A UTF-8 character split across tokens
    /// arrives with its last byte.
    pub piece: String,
    pub phase: Phase,
    /// The model's own probability for this token.
    pub p: f64,
    /// Entropy in nats of that distribution.
    pub h: f64,
    /// Tokens generated so far, this one included.
    pub n: u32,
    /// Decode tokens per second so far.
    pub tps: f64,
}

/// The finished reply.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Done {
    /// The held prompt plus reply; a later request may extend it.
    pub context: String,
    pub reasoning_content: String,
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish: Finish,
    pub seed: u64,
    pub usage: Usage,
    pub ms: Ms,
    pub model: Identity,
}

impl Done {
    fn check(&self) -> Result<(), MkError> {
        for call in &self.tool_calls {
            if let ToolCall::Parsed { arguments, .. } = call {
                require_object(arguments, "done tool call")?;
            }
        }
        Ok(())
    }
}

/// A tool call the model made. One that did not parse is kept as written,
/// never repaired and never dropped.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolCall {
    /// The service names every call it parsed; a parsed call without a name
    /// is refused.
    Parsed { name: String, arguments: Json },
    Unparsed { name: Option<String>, raw: String, error: String },
}

impl<'de> Deserialize<'de> for ToolCall {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        use serde::de::Error;

        #[derive(Deserialize)]
        struct Raw {
            /// Required, and may be null.
            #[serde(deserialize_with = "Option::deserialize")]
            name: Option<String>,
            arguments: Option<Json>,
            raw: Option<String>,
            error: Option<String>,
        }
        let r = Raw::deserialize(de)?;
        match (r.arguments, r.raw, r.error) {
            (Some(arguments), None, None) => match r.name {
                Some(name) => Ok(ToolCall::Parsed { name, arguments }),
                None => Err(D::Error::custom("a parsed tool call has no name")),
            },
            (None, Some(raw), Some(error)) => Ok(ToolCall::Unparsed { name: r.name, raw, error }),
            _ => Err(D::Error::custom("a tool call has either `arguments` or both `raw` and `error`")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Finish {
    Stop,
    Length,
    Cancelled,
}

/// Token counts. `kept` came from a held prefix; `fed` was prefilled now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct Usage {
    pub prompt: u32,
    pub kept: u32,
    pub fed: u32,
    pub completion: u32,
}

/// Milliseconds spent in prefill and decode.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
pub struct Ms {
    pub prefill: f64,
    pub decode: f64,
}

/// One event of a streamed reply.
#[derive(Clone, Debug, PartialEq)]
pub enum GenerateEvent {
    Token(TokenEvent),
    /// Always the last event.
    Done(Done),
}

/// The body the calls send: the request plus `stream`.
#[derive(Serialize)]
pub(crate) struct Wire<'a> {
    #[serde(flatten)]
    pub(crate) request: &'a GenerateRequest,
    pub(crate) stream: bool,
}

impl MkClient {
    /// `POST /mk/v1/generate` without streaming. `timeout` bounds the whole
    /// call, prefill and decode included.
    pub async fn generate(&self, request: &GenerateRequest, timeout: Duration) -> Result<Done, MkError> {
        request.validate()?;
        let body = to_body(&Wire { request, stream: false })?;
        let done: Done = self
            .json(Family::Mk, Method::POST, "/mk/v1/generate", Some(body), &[], timeout, 200, "done")
            .await?;
        done.check()?;
        Ok(done)
    }

    /// `POST /mk/v1/generate` as a stream. `idle` bounds the wait for the
    /// response and then each read; nothing bounds the whole reply. Dropping
    /// the stream closes the connection, which cancels the generation.
    pub async fn generate_stream(&self, request: &GenerateRequest, idle: Duration) -> Result<GenerateStream, MkError> {
        request.validate()?;
        let body = to_body(&Wire { request, stream: true })?;
        let send = self.send(Family::Mk, Method::POST, "/mk/v1/generate", Some(body), &[], None);
        let resp = tokio::time::timeout(idle, send).await.map_err(|_| MkError::Timeout)??;
        if resp.status() != reqwest::StatusCode::OK {
            return Err(MkError::Decode {
                what: "status",
                message: format!("expected 200, got {}", resp.status()),
                body: String::new(),
            });
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !content_type.starts_with("text/event-stream") {
            return Err(MkError::Decode {
                what: "content type",
                message: format!("expected text/event-stream, got {content_type:?}"),
                body: String::new(),
            });
        }
        Ok(GenerateStream { resp, idle, decoder: SseDecoder::default(), frames: VecDeque::new(), ended: false })
    }
}

/// A streamed reply. [`next`](Self::next) yields tokens, then `Done`, then
/// `None`. An error is yielded once, and then the stream is over.
pub struct GenerateStream {
    resp: reqwest::Response,
    idle: Duration,
    decoder: SseDecoder,
    frames: VecDeque<Frame>,
    ended: bool,
}

impl GenerateStream {
    pub async fn next(&mut self) -> Option<Result<GenerateEvent, MkError>> {
        loop {
            if self.ended {
                return None;
            }
            if let Some(frame) = self.frames.pop_front() {
                let event = decode_frame(&frame);
                self.ended = !matches!(event, Ok(GenerateEvent::Token(_)));
                return Some(event);
            }
            let chunk = match tokio::time::timeout(self.idle, self.resp.chunk()).await {
                Err(_) => Err(MkError::Timeout),
                Ok(Err(e)) => Err(map_reqwest(e)),
                Ok(Ok(None)) => Err(MkError::Truncated(
                    self.decoder.finish().err().unwrap_or_else(|| "the connection closed".into()),
                )),
                Ok(Ok(Some(bytes))) => self.decoder.push(&bytes).map_err(|message| MkError::Decode {
                    what: "event stream",
                    message,
                    body: excerpt(&bytes),
                }),
            };
            match chunk {
                Ok(frames) => self.frames.extend(frames),
                Err(e) => {
                    self.ended = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

fn decode_frame(frame: &Frame) -> Result<GenerateEvent, MkError> {
    let data = frame.data.as_bytes();
    match frame.event.as_str() {
        "token" => decode(data, "token event").map(GenerateEvent::Token),
        "done" => {
            let done: Done = decode(data, "done event")?;
            done.check()?;
            Ok(GenerateEvent::Done(done))
        }
        "error" => Err(MkError::Stream(decode::<ServiceErrorBody>(data, "error event")?.error)),
        other => Err(MkError::Decode {
            what: "event stream",
            message: format!("unknown event {other:?}"),
            body: excerpt(data),
        }),
    }
}

/// One server-sent event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) event: String,
    pub(crate) data: String,
}

/// Server-sent events from any chunking of the byte stream: LF, CRLF, or lone
/// CR line ends, `:` comment lines, and multi-line `data` joined with
/// newlines.
#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    /// Bytes of the line not yet ended.
    line: Vec<u8>,
    /// The last byte was CR: an LF next ends nothing new.
    after_cr: bool,
    event: Option<String>,
    data: Vec<String>,
    /// A field line arrived since the last blank line.
    started: bool,
}

impl SseDecoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, String> {
        let mut frames = Vec::new();
        for &b in bytes {
            let after_cr = std::mem::replace(&mut self.after_cr, b == b'\r');
            match b {
                b'\n' if after_cr => {}
                b'\n' | b'\r' => {
                    let line = std::mem::take(&mut self.line);
                    if let Some(frame) = self.line_ended(&line)? {
                        frames.push(frame);
                    }
                }
                _ => self.line.push(b),
            }
        }
        Ok(frames)
    }

    /// Fails when the bytes so far end inside a line or an event.
    pub(crate) fn finish(&self) -> Result<(), String> {
        if !self.line.is_empty() {
            return Err(format!("it ended inside a line: {:?}", String::from_utf8_lossy(&self.line)));
        }
        if self.started {
            return Err("it ended inside an event".into());
        }
        Ok(())
    }

    fn line_ended(&mut self, line: &[u8]) -> Result<Option<Frame>, String> {
        if line.is_empty() {
            let event = self.event.take();
            let data = std::mem::take(&mut self.data);
            let started = std::mem::replace(&mut self.started, false);
            if !started || data.is_empty() {
                return Ok(None);
            }
            return Ok(Some(Frame { event: event.unwrap_or_else(|| "message".into()), data: data.join("\n") }));
        }
        if line[0] == b':' {
            return Ok(None);
        }
        let line = std::str::from_utf8(line).map_err(|e| format!("a line is not UTF-8: {e}"))?;
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        self.started = true;
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::test_server::{reply, serve, sse, Reply};

    const ANSWER: &str = include_str!("../tests/fixtures/mk_stream_answer.sse");
    const THINK: &str = include_str!("../tests/fixtures/mk_stream_think.sse");
    const TOOL: &str = include_str!("../tests/fixtures/mk_stream_tool.sse");

    fn decode_all(text: &str, chunk: usize) -> Result<Vec<Frame>, String> {
        let mut d = SseDecoder::default();
        let mut frames = Vec::new();
        for piece in text.as_bytes().chunks(chunk) {
            frames.extend(d.push(piece)?);
        }
        d.finish()?;
        Ok(frames)
    }

    fn events(text: &str) -> Vec<Result<GenerateEvent, MkError>> {
        decode_all(text, 7).unwrap().iter().map(decode_frame).collect()
    }

    fn done_of(text: &str) -> Done {
        match events(text).pop().unwrap().unwrap() {
            GenerateEvent::Done(d) => d,
            other => panic!("last event is not done: {other:?}"),
        }
    }

    fn request() -> GenerateRequest {
        GenerateRequest::messages(vec![Message::user("hi")], vec![])
    }

    fn client(base: &str) -> MkClient {
        MkClient::new(base, Duration::from_secs(5)).unwrap()
    }

    #[test]
    fn any_chunking_and_line_end_gives_the_same_frames() {
        let lf = "event: token\ndata: {\"a\":1}\n\n: a comment\nevent: done\ndata: x\ndata: y\n\n";
        let want = vec![
            Frame { event: "token".into(), data: "{\"a\":1}".into() },
            Frame { event: "done".into(), data: "x\ny".into() },
        ];
        for text in [lf.to_string(), lf.replace('\n', "\r\n"), lf.replace('\n', "\r")] {
            for chunk in [1, 2, 3, 5, 64] {
                assert_eq!(decode_all(&text, chunk).unwrap(), want, "{text:?} in chunks of {chunk}");
            }
        }
    }

    #[test]
    fn a_stream_cut_inside_a_line_or_an_event_is_an_error() {
        assert!(decode_all("event: token\ndata: {\"a\"", 4).unwrap_err().contains("inside a line"));
        assert!(decode_all("event: token\ndata: {}\n", 4).unwrap_err().contains("inside an event"));
        assert_eq!(decode_all("", 4).unwrap(), vec![]);
    }

    #[test]
    fn a_recorded_answer_decodes_to_tokens_then_done() {
        let evs = events(ANSWER);
        let tokens: Vec<_> = evs[..evs.len() - 1]
            .iter()
            .map(|e| match e {
                Ok(GenerateEvent::Token(t)) => t.clone(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert!(tokens.iter().all(|t| t.phase == Phase::Answer));
        let done = done_of(ANSWER);
        assert_eq!(tokens.iter().map(|t| t.piece.as_str()).collect::<String>(), done.content);
        assert_eq!((done.finish, done.seed, done.usage.completion as usize), (Finish::Stop, 7, tokens.len()));
        assert!(done.tool_calls.is_empty());
    }

    #[test]
    fn a_recorded_thinking_reply_splits_think_from_answer() {
        let done = done_of(THINK);
        assert_eq!(done.content, "42");
        assert!(done.reasoning_content.contains("17 + 25"));
        let phases: Vec<Phase> = events(THINK)
            .into_iter()
            .filter_map(|e| match e.unwrap() {
                GenerateEvent::Token(t) => Some(t.phase),
                GenerateEvent::Done(_) => None,
            })
            .collect();
        assert_eq!(phases.len() as u32, done.usage.completion);
        let first_answer = phases.iter().position(|p| *p == Phase::Answer).unwrap();
        assert!(first_answer > 0, "no think tokens before the answer");
        assert!(phases[..first_answer].iter().all(|p| *p == Phase::Think));
        assert!(phases[first_answer..].iter().all(|p| *p == Phase::Answer));
    }

    #[test]
    fn a_recorded_tool_call_keeps_its_argument_order() {
        let done = done_of(TOOL);
        let [ToolCall::Parsed { name, arguments }] = done.tool_calls.as_slice() else { panic!("{:?}", done.tool_calls) };
        assert_eq!(name, "ls");
        assert_eq!(serde_json::to_string(arguments).unwrap(), r#"{"path":"/tmp/demo"}"#);
        assert_eq!(done.finish, Finish::Stop);
    }

    #[test]
    fn an_unparsed_call_is_kept_as_written() {
        let mut v: serde_json::Value = serde_json::from_str(TOOL.lines().rfind(|l| l.starts_with("data: ")).unwrap()[6..].trim()).unwrap();
        v["tool_calls"] = json!([{"name": null, "raw": "<tool_call>\n<function=", "error": "no function name"}]);
        let done: Done = decode(v.to_string().as_bytes(), "done").unwrap();
        assert_eq!(
            done.tool_calls,
            vec![ToolCall::Unparsed { name: None, raw: "<tool_call>\n<function=".into(), error: "no function name".into() }]
        );
        for bad in [
            json!({"raw": "x", "error": "e"}),
            json!({"name": null, "arguments": {}}),
            json!({"name": "ls", "arguments": {}, "raw": "x", "error": "e"}),
            json!({"name": "ls", "raw": "x"}),
        ] {
            v["tool_calls"] = json!([bad]);
            assert!(matches!(decode::<Done>(v.to_string().as_bytes(), "done"), Err(MkError::Decode { .. })), "{bad}");
        }
        v["tool_calls"] = json!([{"name": "ls", "arguments": "not an object"}]);
        let done: Done = decode(v.to_string().as_bytes(), "done").unwrap();
        assert!(matches!(done.check(), Err(MkError::Decode { .. })));
    }

    #[test]
    fn length_and_cancelled_finishes_decode() {
        let line = THINK.lines().rfind(|l| l.starts_with("data: ")).unwrap()[6..].to_string();
        for (finish, want) in [("length", Finish::Length), ("cancelled", Finish::Cancelled)] {
            let mut v: serde_json::Value = serde_json::from_str(&line).unwrap();
            v["finish"] = json!(finish);
            assert_eq!(decode::<Done>(v.to_string().as_bytes(), "done").unwrap().finish, want);
        }
    }

    #[test]
    fn an_unknown_field_is_ignored_and_a_missing_one_is_an_error() {
        let line = ANSWER.lines().rfind(|l| l.starts_with("data: ")).unwrap()[6..].to_string();
        let mut v: serde_json::Value = serde_json::from_str(&line).unwrap();
        v["new_field"] = json!(1);
        v["usage"]["new_count"] = json!(2);
        decode::<Done>(v.to_string().as_bytes(), "done").unwrap();
        v.as_object_mut().unwrap().remove("seed");
        assert!(matches!(decode::<Done>(v.to_string().as_bytes(), "done"), Err(MkError::Decode { .. })));
    }

    #[test]
    fn an_error_event_and_an_unknown_event_are_errors() {
        let err = decode_frame(&Frame {
            event: "error".into(),
            data: r#"{"error":{"code":503,"message":"spin","type":"pass_timeout_error"}}"#.into(),
        })
        .unwrap_err();
        let MkError::Stream(e) = &err else { panic!("{err:?}") };
        assert_eq!((e.code, e.r#type.as_str()), (503, "pass_timeout_error"));
        assert_eq!(err.status(), Some(503));
        let err = decode_frame(&Frame { event: "pass".into(), data: "{}".into() }).unwrap_err();
        assert!(matches!(err, MkError::Decode { what: "event stream", .. }), "{err:?}");
    }

    #[test]
    fn requests_the_service_would_refuse_are_not_built() {
        let bad = |f: &dyn Fn(&mut GenerateRequest)| {
            let mut r = request();
            f(&mut r);
            assert!(matches!(r.validate(), Err(MkError::Request(_))), "{r:?}");
        };
        bad(&|r| r.from = Source::Messages { messages: vec![], tools: vec![] });
        bad(&|r| r.from = Source::Prompt { prompt: String::new() });
        bad(&|r| r.from = Source::Tokens { tokens: vec![] });
        bad(&|r| r.from = Source::Context { context: "abc".into() });
        bad(&|r| {
            r.from = Source::Prompt { prompt: "x".into() };
            r.reasoning_effort = Some(ReasoningEffort::Low);
        });
        bad(&|r| {
            r.from = Source::Tokens { tokens: vec![1] };
            r.prefill = Some("x".into());
        });
        bad(&|r| r.prefill = Some(String::new()));
        bad(&|r| r.max_tokens = Some(0));
        bad(&|r| r.max_tokens = Some(MAX_TOKENS_LIMIT + 1));
        bad(&|r| r.sample = Some(Sample { top_p: Some(0.0), ..Sample::default() }));
        bad(&|r| r.sample = Some(Sample { temperature: Some(-0.1), ..Sample::default() }));
        bad(&|r| r.sample = Some(Sample { min_p: Some(1.0), ..Sample::default() }));
        bad(&|r| {
            let mut m = Message::user("x");
            m.role = Role::Assistant;
            m.tool_calls = vec![ToolCallIn { name: "ls".into(), arguments: Json::from("x") }];
            r.from = Source::Messages { messages: vec![m], tools: vec![] };
        });
        let tool = |name: &str, parameters: Option<Json>| Tool::Function {
            function: Function { name: name.into(), description: None, parameters },
        };
        bad(&|r| r.from = Source::Messages { messages: vec![Message::user("x")], tools: vec![tool("", None)] });
        bad(&|r| {
            r.from = Source::Messages { messages: vec![Message::user("x")], tools: vec![tool("ls", Some(Json::from("x")))] }
        });
        request().validate().unwrap();
    }

    #[tokio::test]
    async fn a_stream_over_http_yields_every_token_then_done_then_none() {
        let (base, seen) = serve(vec![sse(TOOL)]).await;
        let mut stream = client(&base).generate_stream(&request(), Duration::from_secs(5)).await.unwrap();
        let mut tokens = 0;
        let done = loop {
            match stream.next().await.unwrap().unwrap() {
                GenerateEvent::Token(_) => tokens += 1,
                GenerateEvent::Done(d) => break d,
            }
        };
        assert_eq!(tokens, done.usage.completion);
        assert!(stream.next().await.is_none());
        let seen = seen.lock().unwrap();
        assert_eq!((seen[0].method.as_str(), seen[0].path.as_str()), ("POST", "/mk/v1/generate"));
        let body: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(body, json!({"from": {"messages": [{"role": "user", "content": "hi"}]}, "stream": true}));
    }

    #[tokio::test]
    async fn a_stream_cut_before_done_is_truncated_not_a_reply() {
        let cut = &TOOL[..TOOL.find("event: done").unwrap()];
        let (base, _) = serve(vec![sse(cut), sse(&TOOL[..TOOL.len() - 40])]).await;
        let c = client(&base);
        for _ in 0..2 {
            let mut stream = c.generate_stream(&request(), Duration::from_secs(5)).await.unwrap();
            let last = loop {
                match stream.next().await.unwrap() {
                    Ok(GenerateEvent::Token(_)) => continue,
                    other => break other,
                }
            };
            assert!(matches!(last, Err(MkError::Truncated(_))), "{last:?}");
            assert!(stream.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn an_error_event_ends_the_stream() {
        let body = format!(
            "{}event: error\ndata: {}\n\n",
            &TOOL[..TOOL.find("event: done").unwrap()],
            r#"{"error":{"code":503,"message":"spin","type":"pass_timeout_error"}}"#
        );
        let (base, _) = serve(vec![sse(body)]).await;
        let mut stream = client(&base).generate_stream(&request(), Duration::from_secs(5)).await.unwrap();
        let last = loop {
            match stream.next().await.unwrap() {
                Ok(GenerateEvent::Token(_)) => continue,
                other => break other,
            }
        };
        assert!(matches!(last, Err(MkError::Stream(_))), "{last:?}");
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn a_stalled_stream_is_a_timeout() {
        let mut slow = sse(ANSWER);
        slow.stall = Some(Duration::from_millis(800));
        let mut late = sse(ANSWER);
        late.delay = Duration::from_millis(800);
        let (base, _) = serve(vec![slow, late]).await;
        let c = client(&base);
        let mut stream = c.generate_stream(&request(), Duration::from_millis(150)).await.unwrap();
        let last = loop {
            match stream.next().await.unwrap() {
                Ok(GenerateEvent::Token(_)) => continue,
                other => break other,
            }
        };
        assert!(matches!(last, Err(MkError::Timeout)), "{last:?}");
        let err = c.generate_stream(&request(), Duration::from_millis(150)).await.err().unwrap();
        assert!(matches!(err, MkError::Timeout), "{err:?}");
    }

    #[tokio::test]
    async fn an_error_status_carries_the_service_error() {
        let body = r#"{"error":{"code":507,"message":"pins full","type":"insufficient_storage_error"}}"#;
        let (base, _) = serve(vec![reply(507, body), reply(502, "<html>bad gateway</html>")]).await;
        let c = client(&base);
        let err = c.generate_stream(&request(), Duration::from_secs(5)).await.err().unwrap();
        let MkError::Service { status: 507, error: Some(e), .. } = &err else { panic!("{err:?}") };
        assert_eq!(e.r#type, "insufficient_storage_error");
        let err = c.generate(&request(), Duration::from_secs(5)).await.unwrap_err();
        let MkError::Service { status: 502, error: None, body, .. } = &err else { panic!("{err:?}") };
        assert!(body.contains("bad gateway"));
    }

    #[tokio::test]
    async fn a_reply_that_is_not_a_200_stream_is_refused() {
        let done = ANSWER.lines().rfind(|l| l.starts_with("data: ")).unwrap()[6..].to_string();
        let accepted = Reply { status: 202, ..sse(ANSWER) };
        let (base, _) = serve(vec![reply(200, done), accepted]).await;
        let c = client(&base);
        let err = c.generate_stream(&request(), Duration::from_secs(5)).await.err().unwrap();
        assert!(matches!(err, MkError::Decode { what: "content type", .. }), "{err:?}");
        let err = c.generate_stream(&request(), Duration::from_secs(5)).await.err().unwrap();
        assert!(matches!(err, MkError::Decode { what: "status", .. }), "{err:?}");
    }

    /// Live, by hand on zorak:
    /// `MK_URL=http://100.83.138.103:8090 cargo test -p kaijutsu-mk -- --ignored live_`
    #[tokio::test]
    #[ignore = "needs a running megakernel at MK_URL"]
    async fn live_greedy_replay_gives_the_same_tokens() {
        let base = std::env::var("MK_URL").expect("MK_URL names the service");
        let c = MkClient::new(base, Duration::from_secs(10)).unwrap();
        let mut req = GenerateRequest::messages(vec![Message::user("Name three primary colors, comma separated.")], vec![]);
        req.thinking = Some(false);
        req.max_tokens = Some(24);
        req.sample = Some(Sample { temperature: Some(0.0), seed: Some(11), ..Sample::default() });
        let mut runs = Vec::new();
        for _ in 0..2 {
            let mut stream = c.generate_stream(&req, Duration::from_secs(60)).await.unwrap();
            let mut ids = Vec::new();
            let done = loop {
                match stream.next().await.unwrap().unwrap() {
                    GenerateEvent::Token(t) => ids.push(t.id),
                    GenerateEvent::Done(d) => break d,
                }
            };
            assert_eq!(ids.len() as u32, done.usage.completion);
            runs.push((ids, done.content, done.seed));
        }
        assert_eq!(runs[0], runs[1]);
        assert_eq!(c.model().await.unwrap().max_context, 32768);
    }

    #[tokio::test]
    async fn generate_without_streaming_returns_done() {
        let done = TOOL.lines().rfind(|l| l.starts_with("data: ")).unwrap()[6..].to_string();
        let (base, seen) = serve(vec![reply(200, done)]).await;
        let d = client(&base).generate(&request(), Duration::from_secs(5)).await.unwrap();
        assert_eq!(d.tool_calls.len(), 1);
        let body: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[0].body).unwrap();
        assert_eq!(body["stream"], json!(false));
    }
}
