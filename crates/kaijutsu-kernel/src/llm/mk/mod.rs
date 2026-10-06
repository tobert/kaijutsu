//! The `mk` provider: a context talks to the megakernel service as a model
//! (`docs/mk.md`, "Provider: `BackendKind::Mk`").
//!
//! An adapter over `kaijutsu_mk`; it does no HTTP of its own. Each call
//! renders the request first and checks it against the window, so a prompt
//! that leaves no room for a reply is refused instead of truncated, and a
//! reply that stops at its output limit is never returned as a whole one.
//! `cache_breakpoints`, `thinking_budget`, and `thinking_style` do not apply
//! to this backend and are ignored.

mod build;
mod reply;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kaijutsu_mk::generate::{
    Finish, GenerateEvent, GenerateRequest, GenerateStream, Message as MkMessage, Sample, Source, MAX_TOKENS_LIMIT,
};
use kaijutsu_mk::model::RenderRequest;
use kaijutsu_mk::{MkClient, MkError};
use tokio_util::sync::CancellationToken;

use crate::llm::stream::{BuildOpts, StreamEvent};
use crate::llm::{LlmError, LlmResult, Message};

use reply::{Reply, Timing};

/// The output limit asked for by [`Client::prompt`], which has no tunables:
/// the service's own default.
const PROMPT_MAX_TOKENS: u64 = 4096;

/// A client for one `mk` backend.
#[derive(Clone, Debug)]
pub struct Client {
    name: String,
    mk: MkClient,
    /// Bounds each non-streamed call, and each read of a stream.
    timeout: Duration,
    /// The configured `context_window` per model, where one is set.
    windows: HashMap<String, u64>,
    /// The service's `max_context`, read on first use.
    max_context: Arc<tokio::sync::OnceCell<u64>>,
}

impl Client {
    /// `name` is the backend name, reported as the provider name.
    pub fn new(name: impl Into<String>, base_url: &str, timeout: Duration, windows: HashMap<String, u64>) -> LlmResult<Self> {
        let name = name.into();
        let mk = MkClient::new(base_url, timeout).map_err(|e| to_llm(&name, e))?;
        Ok(Client { name, mk, timeout, windows, max_context: Arc::default() })
    }

    pub fn provider_name(&self) -> &str {
        &self.name
    }

    /// One user message, not streamed, with thinking off; returns the answer
    /// text. A reply that stopped at its output limit is an error.
    pub async fn prompt(&self, model: &str, system: Option<&str>, prompt: &str) -> LlmResult<String> {
        let mut messages = Vec::new();
        if let Some(system) = system.filter(|s| !s.is_empty()) {
            messages.push(MkMessage::system(system));
        }
        messages.push(MkMessage::user(prompt));
        let thinking = Some(false);
        let max_tokens = self.admit_messages(model, &messages, &[], thinking, None, PROMPT_MAX_TOKENS).await?;
        let request = GenerateRequest {
            thinking,
            max_tokens: Some(max_tokens),
            ..GenerateRequest::messages(messages, Vec::new())
        };
        let done = self.mk.generate(&request, self.timeout).await.map_err(|e| to_llm(&self.name, e))?;
        match done.finish {
            Finish::Stop => Ok(done.content),
            Finish::Length => Err(LlmError::ApiError(format!(
                "mk backend '{}': the answer stopped at its {max_tokens}-token output limit",
                self.name
            ))),
            Finish::Cancelled => Err(LlmError::ApiError(format!(
                "mk backend '{}': the megakernel cancelled the generation, and the kernel did not ask it to",
                self.name
            ))),
        }
    }

    /// A streamed turn. The request is rendered and checked against the
    /// window before generation starts.
    pub async fn stream(&self, opts: BuildOpts, history: Vec<Message>) -> LlmResult<Stream> {
        let (thinking, reasoning_effort) = build::effort(opts.effort.as_deref())?;
        let messages = build::messages(opts.system.as_deref(), &history)?;
        let tools = build::tools(&opts.tools)?;
        let max_tokens = self
            .admit_messages(&opts.model, &messages, &tools, thinking, reasoning_effort, opts.max_tokens)
            .await?;
        let sample = (opts.temperature.is_some() || opts.top_p.is_some())
            .then(|| Sample { temperature: opts.temperature, top_p: opts.top_p, ..Sample::default() });
        let has_tools = !tools.is_empty();
        let request = GenerateRequest {
            from: Source::Messages { messages, tools },
            thinking,
            reasoning_effort,
            prefill: None,
            sample,
            max_tokens: Some(max_tokens),
        };
        let started = Instant::now();
        let inner = self.mk.generate_stream(&request, self.timeout).await.map_err(|e| to_llm(&self.name, e))?;
        Ok(Stream {
            name: self.name.clone(),
            inner: Some(inner),
            reply: Some(Reply::new(thinking.unwrap_or(true), has_tools)),
            pending: Default::default(),
            cancel: CancellationToken::new(),
            finished: false,
            started,
            first_token: None,
        })
    }

    /// Renders the request and returns the `max_tokens` it may send.
    async fn admit_messages(
        &self,
        model: &str,
        messages: &[MkMessage],
        tools: &[kaijutsu_mk::generate::Tool],
        thinking: Option<bool>,
        reasoning_effort: Option<kaijutsu_mk::generate::ReasoningEffort>,
        asked: u64,
    ) -> LlmResult<u32> {
        let window = self.window(model).await?;
        let render = RenderRequest {
            messages: messages.to_vec(),
            tools: tools.to_vec(),
            thinking,
            reasoning_effort,
            generation_prompt: None,
        };
        let rendered = self.mk.render(&render, self.timeout).await.map_err(|e| to_llm(&self.name, e))?;
        admit(&self.name, u64::from(rendered.n_tokens), window, asked)
    }

    /// The window for `model`: the service's `max_context`, or the model
    /// row's `context_window` when that is smaller. A row larger than the
    /// service is a configuration error.
    async fn window(&self, model: &str) -> LlmResult<u64> {
        let max_context = *self
            .max_context
            .get_or_try_init(|| async { self.mk.model().await.map(|m| u64::from(m.max_context)) })
            .await
            .map_err(|e| to_llm(&self.name, e))?;
        match self.windows.get(model) {
            Some(&row) if row > max_context => Err(LlmError::InvalidRequest(format!(
                "mk backend '{}': model {model:?} has context_window {row}, but the service holds at most \
                 {max_context} tokens; set the window with `kj backend model set` to {max_context} or less",
                self.name
            ))),
            Some(&row) => Ok(row),
            None => Ok(max_context),
        }
    }
}

/// The `max_tokens` to send: the asked-for amount, lowered to the room the
/// prompt leaves in the window and to the schema's limit. No room is an
/// error.
fn admit(name: &str, prompt: u64, window: u64, asked: u64) -> LlmResult<u32> {
    let room = window.saturating_sub(prompt);
    if room == 0 {
        return Err(LlmError::InvalidRequest(format!(
            "mk backend '{name}': the prompt takes {prompt} of the {window}-token window and leaves no room \
             for a reply; fork with fewer blocks or exclude some (`kj stage exclude`)"
        )));
    }
    if asked == 0 {
        return Err(LlmError::InvalidRequest(format!("mk backend '{name}': max_tokens is 0")));
    }
    let max = asked.min(room).min(u64::from(MAX_TOKENS_LIMIT));
    if max < asked {
        let cause = if max == room { "the room the prompt leaves in the window" } else { "the service's max_tokens limit" };
        tracing::info!(backend = %name, prompt, window, asked, sent = max, "mk: max_tokens lowered to {cause}");
    }
    Ok(max as u32)
}

fn to_llm(name: &str, e: MkError) -> LlmError {
    let detail = format!("mk backend '{name}': {e}");
    match e {
        MkError::Request(_) => LlmError::InvalidRequest(detail),
        MkError::Service { status: 400 | 404 | 413, .. } => LlmError::InvalidRequest(detail),
        MkError::Service { status: 429, .. } => LlmError::RateLimited(detail),
        MkError::Timeout | MkError::Transport(_) => LlmError::NetworkError(detail),
        MkError::Service { .. }
        | MkError::Stream(_)
        | MkError::Truncated(_)
        | MkError::Decode { .. }
        | MkError::Status { .. }
        | MkError::SpecIdMismatch { .. } => LlmError::ApiError(detail),
    }
}

/// A streamed `mk` turn as [`StreamEvent`]s.
///
/// [`Self::cancel`] is observed by the next [`Self::next_event`], which drops
/// the HTTP stream (the service cancels the generation when the connection
/// closes) and yields `Done { stop_reason: None }`, the cancel-confirm the
/// server expects.
pub struct Stream {
    name: String,
    inner: Option<GenerateStream>,
    reply: Option<Reply>,
    pending: std::collections::VecDeque<StreamEvent>,
    cancel: CancellationToken,
    finished: bool,
    started: Instant,
    first_token: Option<Duration>,
}

impl Stream {
    pub async fn next_event(&mut self) -> Option<StreamEvent> {
        loop {
            if let Some(ev) = self.pending.pop_front() {
                return Some(ev);
            }
            if self.finished {
                return None;
            }
            let inner = self.inner.as_mut()?;
            let cancel = self.cancel.clone();
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    self.finished = true;
                    self.inner = None;
                    return Some(StreamEvent::Done { stop_reason: None, input_tokens: None, output_tokens: None, extra: None });
                }
                item = inner.next() => match item {
                    Some(Ok(GenerateEvent::Token(token))) => {
                        self.first_token.get_or_insert_with(|| self.started.elapsed());
                        let reply = self.reply.as_mut().expect("the reply lives until done");
                        self.pending.extend(reply.piece(&token.piece));
                    }
                    Some(Ok(GenerateEvent::Done(done))) => {
                        self.finished = true;
                        self.inner = None;
                        let timing = Timing {
                            first_token_ms: self.first_token.unwrap_or_default().as_millis() as u64,
                            wall_ms: self.started.elapsed().as_millis() as u64,
                        };
                        let id_base = format!("mk-{}", &done.context[..done.context.len().min(16)]);
                        let reply = self.reply.take().expect("done comes once");
                        match reply.done(done, &id_base, timing) {
                            Ok(events) => self.pending.extend(events),
                            Err(e) => return Some(StreamEvent::Error(format!("mk backend '{}': {e}", self.name))),
                        }
                    }
                    Some(Err(e)) => {
                        self.finished = true;
                        self.inner = None;
                        return Some(StreamEvent::Error(format!("mk backend '{}': {e}", self.name)));
                    }
                    None => {
                        self.finished = true;
                        self.inner = None;
                        return None;
                    }
                },
            }
        }
    }

    /// Ends the stream at the next poll. Idempotent.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use kaijutsu_mk::test_server::{reply as http, serve, sse};
    use serde_json::json;

    use super::*;
    use crate::llm::ToolDefinition;

    const MODEL: &str = include_str!("../../../../kaijutsu-mk/tests/fixtures/mk_model.json");
    const ANSWER: &str = include_str!("../../../../kaijutsu-mk/tests/fixtures/mk_stream_answer.sse");
    const TOOL: &str = include_str!("../../../../kaijutsu-mk/tests/fixtures/mk_stream_tool.sse");

    fn rendered(n: u32) -> kaijutsu_mk::test_server::Reply {
        http(200, json!({"text": "", "tokens": [], "n_tokens": n}).to_string())
    }

    fn opts(max_tokens: u64) -> BuildOpts {
        BuildOpts {
            model: "qwen".into(),
            system: Some("rules".into()),
            max_tokens,
            temperature: None,
            top_p: None,
            effort: Some("none".into()),
            thinking_budget: None,
            thinking_style: None,
            tools: Vec::new(),
            cache_breakpoints: Vec::new(),
        }
    }

    fn client(base: &str, windows: HashMap<String, u64>) -> Client {
        Client::new("zorak-mk", base, Duration::from_secs(5), windows).unwrap()
    }

    async fn drain(stream: &mut Stream) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Some(ev) = stream.next_event().await {
            out.push(ev);
        }
        out
    }

    #[test]
    fn admission_lowers_max_tokens_to_the_room_and_refuses_no_room() {
        assert_eq!(admit("b", 1000, 32768, 4096).unwrap(), 4096);
        assert_eq!(admit("b", 30000, 32768, 4096).unwrap(), 2768);
        assert_eq!(admit("b", 0, 65536, 40000).unwrap(), MAX_TOKENS_LIMIT);
        assert!(matches!(admit("b", 32768, 32768, 10), Err(LlmError::InvalidRequest(m)) if m.contains("leaves no room")));
        assert!(matches!(admit("b", 40000, 32768, 10), Err(LlmError::InvalidRequest(_))));
        assert!(matches!(admit("b", 10, 32768, 0), Err(LlmError::InvalidRequest(_))));
    }

    #[tokio::test]
    async fn a_turn_renders_then_streams_and_sends_the_mapped_request() {
        let (base, seen) = serve(vec![http(200, MODEL), rendered(30000), sse(TOOL)]).await;
        let mut o = opts(4096);
        o.temperature = Some(0.0);
        o.tools = vec![ToolDefinition { name: "ls".into(), description: "List.".into(), input_schema: json!({"type": "object"}) }];
        let mut stream = client(&base, HashMap::new()).stream(o, vec![Message::user("list")]).await.unwrap();
        let events = drain(&mut stream).await;
        assert!(matches!(&events[0], StreamEvent::ToolUse { name, .. } if name == "ls"), "{events:?}");
        assert!(matches!(events.last(), Some(StreamEvent::Done { stop_reason: Some(s), .. }) if s == "tool_calls"));

        let seen = seen.lock().unwrap();
        let paths: Vec<&str> = seen.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["/mk/v1/model", "/mk/v1/render", "/mk/v1/generate"]);
        let render: serde_json::Value = serde_json::from_str(&seen[1].body).unwrap();
        let generate: serde_json::Value = serde_json::from_str(&seen[2].body).unwrap();
        assert_eq!(render["messages"], generate["from"]["messages"]);
        assert_eq!(render["thinking"], json!(false));
        assert_eq!(generate["thinking"], json!(false));
        assert_eq!(generate["max_tokens"], json!(2768), "lowered to the room the 30000-token prompt leaves");
        assert_eq!(generate["sample"], json!({"temperature": 0.0}));
        assert_eq!(generate["from"]["messages"][0], json!({"role": "system", "content": "rules"}));
        assert_eq!(generate["from"]["tools"][0]["function"]["name"], json!("ls"));
    }

    #[tokio::test]
    async fn the_model_is_read_once_and_a_row_window_narrows_it() {
        let windows = HashMap::from([("qwen".to_string(), 8192)]);
        let (base, seen) = serve(vec![http(200, MODEL), rendered(8000), sse(ANSWER), rendered(8192)]).await;
        let c = client(&base, windows);
        let mut stream = c.stream(opts(4096), vec![Message::user("hi")]).await.unwrap();
        drain(&mut stream).await;
        let err = c.stream(opts(4096), vec![Message::user("hi")]).await.err().unwrap();
        assert!(matches!(&err, LlmError::InvalidRequest(m) if m.contains("leaves no room")), "{err:?}");
        let seen = seen.lock().unwrap();
        let generate: serde_json::Value = serde_json::from_str(&seen[2].body).unwrap();
        assert_eq!(generate["max_tokens"], json!(192));
        assert_eq!(seen.iter().filter(|c| c.path == "/mk/v1/model").count(), 1);
    }

    #[tokio::test]
    async fn a_row_window_larger_than_the_service_is_refused() {
        let (base, _) = serve(vec![http(200, MODEL)]).await;
        let err = client(&base, HashMap::from([("qwen".to_string(), 65536)]))
            .stream(opts(10), vec![Message::user("hi")])
            .await
            .err()
            .unwrap();
        assert!(matches!(&err, LlmError::InvalidRequest(m) if m.contains("holds at most 32768")), "{err:?}");
    }

    #[tokio::test]
    async fn a_stream_cut_before_done_is_an_error_event() {
        let cut = &ANSWER[..ANSWER.find("event: done").unwrap()];
        let (base, _) = serve(vec![http(200, MODEL), rendered(10), sse(cut)]).await;
        let mut stream = client(&base, HashMap::new()).stream(opts(64), vec![Message::user("hi")]).await.unwrap();
        let events = drain(&mut stream).await;
        assert!(matches!(events.last(), Some(StreamEvent::Error(m)) if m.contains("before its done event")), "{events:?}");
        assert!(!events.iter().any(|e| matches!(e, StreamEvent::Done { .. })));
    }

    #[tokio::test]
    async fn cancel_yields_the_cancel_confirm_done() {
        let mut slow = sse(ANSWER);
        slow.stall = Some(Duration::from_secs(3));
        let (base, _) = serve(vec![http(200, MODEL), rendered(10), slow]).await;
        let mut stream = client(&base, HashMap::new()).stream(opts(64), vec![Message::user("hi")]).await.unwrap();
        stream.cancel();
        let events = drain(&mut stream).await;
        assert!(
            matches!(events.as_slice(), [StreamEvent::Done { stop_reason: None, input_tokens: None, .. }]),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn service_errors_map_to_kernel_errors() {
        let bad = r#"{"error":{"code":400,"message":"noncharacter","type":"bad_request"}}"#;
        let (base, _) = serve(vec![http(200, MODEL), http(400, bad)]).await;
        let err = client(&base, HashMap::new()).stream(opts(64), vec![Message::user("hi")]).await.err().unwrap();
        assert!(matches!(&err, LlmError::InvalidRequest(m) if m.contains("zorak-mk") && m.contains("noncharacter")), "{err:?}");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let err = client(&closed, HashMap::new()).stream(opts(64), vec![Message::user("hi")]).await.err().unwrap();
        assert!(matches!(err, LlmError::NetworkError(_)), "{err:?}");
    }

    #[tokio::test]
    async fn an_unknown_effort_is_refused_before_any_call() {
        let (base, seen) = serve(vec![]).await;
        let mut o = opts(64);
        o.effort = Some("max".into());
        let err = client(&base, HashMap::new()).stream(o, vec![Message::user("hi")]).await.err().unwrap();
        assert!(matches!(err, LlmError::InvalidRequest(_)), "{err:?}");
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn prompt_renders_first_and_a_length_stop_is_an_error() {
        let done = ANSWER.lines().rfind(|l| l.starts_with("data: ")).unwrap()[6..].to_string();
        let mut cut: serde_json::Value = serde_json::from_str(&done).unwrap();
        cut["finish"] = json!("length");
        let (base, seen) = serve(vec![http(200, MODEL), rendered(40), http(200, done), rendered(40), http(200, cut.to_string())]).await;
        let c = client(&base, HashMap::new());
        assert_eq!(c.prompt("qwen", Some("s"), "sky?").await.unwrap(), "The sky is blue on a clear day.");
        let err = c.prompt("qwen", Some("s"), "sky?").await.unwrap_err();
        assert!(matches!(&err, LlmError::ApiError(m) if m.contains("output limit")), "{err:?}");
        let seen = seen.lock().unwrap();
        let paths: Vec<&str> = seen.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["/mk/v1/model", "/mk/v1/render", "/mk/v1/generate", "/mk/v1/render", "/mk/v1/generate"]);
        let body: serde_json::Value = serde_json::from_str(&seen[2].body).unwrap();
        assert_eq!((body["stream"].clone(), body["thinking"].clone(), body["max_tokens"].clone()), (json!(false), json!(false), json!(4096)));
    }

    /// Live, by hand on zorak: a tool call, then its result, through the
    /// provider. Prints the prefix reuse of the second turn.
    /// `MK_URL=http://100.83.138.103:8090 cargo test -p kaijutsu-kernel --lib live_mk -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs a running megakernel at MK_URL"]
    async fn live_mk_tool_loop() {
        use crate::llm::ContentBlock;

        let base = std::env::var("MK_URL").expect("MK_URL names the service");
        let c = Client::new("live-mk", &base, Duration::from_secs(120), HashMap::new()).unwrap();
        let mut o = opts(256);
        o.system = Some("Use the tools to answer. Be brief.".into());
        o.temperature = Some(0.0);
        o.tools = vec![ToolDefinition {
            name: "ls".into(),
            description: "List a directory.".into(),
            input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
        }];
        let mut history = vec![Message::user("What files are in /tmp/demo?")];
        let first = drain(&mut c.stream(o.clone(), history.clone()).await.unwrap()).await;
        let Some(StreamEvent::ToolUse { id, name, input }) = first.iter().find(|e| matches!(e, StreamEvent::ToolUse { .. })).cloned() else {
            panic!("no tool call: {first:?}")
        };
        assert_eq!(name, "ls");
        history.push(Message { role: crate::llm::Role::Assistant, content: crate::llm::MessageContent::Blocks(vec![
            ContentBlock::ToolUse { id: id.clone(), name, input },
        ]) });
        history.push(Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: id,
            content: "notes.txt\nplan.md".into(),
            is_error: false,
        }]));
        let second = drain(&mut c.stream(o, history).await.unwrap()).await;
        let answer: String = second.iter().filter_map(|e| match e {
            StreamEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        }).collect();
        let Some(StreamEvent::Done { stop_reason, extra: Some(crate::llm::UsageExtra::Mk(x)), input_tokens, .. }) = second.last() else {
            panic!("no done: {second:?}")
        };
        assert_eq!(stop_reason.as_deref(), Some("stop"));
        assert!(answer.contains("notes.txt"), "{answer:?}");
        println!("second turn: prompt={input_tokens:?} kept={} fed={} first_token_ms={} wall_ms={} answer={answer:?}",
            x.kept, x.fed, x.first_token_ms, x.wall_ms);
    }

    /// The kernel's `serde_json` keeps object member order (its
    /// `preserve_order` feature), so the template re-renders a call as the
    /// model wrote it and the held prefix can match.
    #[test]
    fn a_replayed_call_keeps_the_models_argument_order() {
        use crate::llm::ContentBlock;

        let written: serde_json::Value = serde_json::from_str(r#"{"path": "/tmp", "all": true}"#).unwrap();
        let history = vec![
            Message::user("x"),
            Message {
                role: crate::llm::Role::Assistant,
                content: crate::llm::MessageContent::Blocks(vec![ContentBlock::ToolUse {
                    id: "a".into(),
                    name: "ls".into(),
                    input: written,
                }]),
            },
            Message::tool_results(vec![ContentBlock::ToolResult { tool_use_id: "a".into(), content: String::new(), is_error: false }]),
        ];
        let messages = build::messages(None, &history).unwrap();
        let sent = serde_json::to_string(&messages[1].tool_calls[0].arguments).unwrap();
        assert_eq!(sent, r#"{"path":"/tmp","all":true}"#);
    }
}
