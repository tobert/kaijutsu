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
#[cfg(test)]
mod probes;
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

use crate::llm::endpoint::{Endpoint, Slot, SlotWaits};
use crate::llm::stream::{BuildOpts, StreamEvent};
use crate::llm::{LlmError, LlmResult, Message};

use reply::{Reply, Timing};

/// The deadline of one prompt or turn, which bounds every slot wait in it,
/// and the waits so far.
struct Turn {
    deadline: tokio::time::Instant,
    waits: SlotWaits,
}

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
    /// The endpoint each prompt or turn takes one slot from, held from the
    /// render through the reply. `None` only for a client built directly
    /// rather than by `Provider::from_backend`.
    endpoint: Option<Arc<Endpoint>>,
}

impl Client {
    /// `name` is the backend name, reported as the provider name.
    pub fn new(name: impl Into<String>, base_url: &str, timeout: Duration, windows: HashMap<String, u64>) -> LlmResult<Self> {
        let name = name.into();
        let mk = MkClient::new(base_url, timeout).map_err(|e| to_llm(&name, e))?;
        Ok(Client { name, mk, timeout, windows, max_context: Arc::default(), endpoint: None })
    }

    /// Take a slot from `endpoint` for each call to the service.
    pub fn with_endpoint(mut self, endpoint: Arc<Endpoint>) -> Self {
        self.endpoint = Some(endpoint);
        self
    }

    /// The endpoint this client takes slots from.
    pub fn endpoint(&self) -> Option<&Arc<Endpoint>> {
        self.endpoint.as_ref()
    }

    /// One call to the service through this client's endpoint
    /// ([`crate::llm::endpoint::mk_call`]): a slot first, and the same call
    /// again after the cooldown when the service answers 429, all within
    /// `turn`'s deadline. `call` is given each attempt's timeout, the time
    /// left before the deadline and no more than the client's timeout, and
    /// each attempt ends at the deadline. Returns the slot, which a stream
    /// holds until it ends. With no endpoint, the call goes straight out.
    async fn call<T, F, Fut>(&self, turn: &Turn, opens_stream: bool, mut call: F) -> LlmResult<(Option<Slot>, T)>
    where
        F: FnMut(Duration) -> Fut,
        Fut: std::future::Future<Output = Result<T, MkError>>,
    {
        let mut attempt = || {
            let left = turn.deadline.saturating_duration_since(tokio::time::Instant::now()).min(self.timeout);
            let sent = call(left);
            async move { tokio::time::timeout_at(turn.deadline, sent).await.unwrap_or(Err(MkError::Timeout)) }
        };
        let Some(endpoint) = &self.endpoint else {
            return attempt().await.map(|v| (None, v)).map_err(|e| to_llm(&self.name, e));
        };
        let answered = if opens_stream {
            crate::llm::endpoint::mk_open(endpoint, turn.deadline, &turn.waits, attempt).await
        } else {
            crate::llm::endpoint::mk_call(endpoint, turn.deadline, &turn.waits, attempt).await
        };
        tracing::Span::current().record("llm.slot_wait_ms", turn.waits.total().as_millis() as u64);
        match answered {
            Ok((slot, result)) => result.map(|v| (Some(slot), v)).map_err(|e| to_llm(&self.name, e)),
            Err(no_slot) => Err(LlmError::RateLimited(format!("mk backend '{}': {no_slot}", self.name))),
        }
    }

    /// The deadline and slot waits of one prompt or turn.
    fn turn(&self) -> Turn {
        Turn { deadline: tokio::time::Instant::now() + self.timeout, waits: SlotWaits::default() }
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
        let turn = self.turn();
        let max_tokens = self.admit_messages(&turn, model, &messages, &[], thinking, None, PROMPT_MAX_TOKENS).await?;
        let request = GenerateRequest {
            thinking,
            max_tokens: Some(max_tokens),
            ..GenerateRequest::messages(messages, Vec::new())
        };
        let (_slot, done) = self.call(&turn, false, |left| self.mk.generate(&request, left)).await?;
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
        let turn = self.turn();
        let max_tokens = self
            .admit_messages(&turn, &opts.model, &messages, &tools, thinking, reasoning_effort, opts.max_tokens)
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
        // The stream opens within the turn's deadline, and then waits up to
        // the client's timeout for each read.
        let (slot, inner) = self.call(&turn, true, |_| self.mk.generate_stream(&request, self.timeout)).await?;
        Ok(Stream {
            name: self.name.clone(),
            inner: Some(inner),
            reply: Some(Reply::new(thinking.unwrap_or(true), has_tools)),
            pending: Default::default(),
            cancel: CancellationToken::new(),
            finished: false,
            started,
            first_token: None,
            slot,
        })
    }

    /// Renders the request and returns the `max_tokens` it may send.
    #[allow(clippy::too_many_arguments)]
    async fn admit_messages(
        &self,
        turn: &Turn,
        model: &str,
        messages: &[MkMessage],
        tools: &[kaijutsu_mk::generate::Tool],
        thinking: Option<bool>,
        reasoning_effort: Option<kaijutsu_mk::generate::ReasoningEffort>,
        asked: u64,
    ) -> LlmResult<u32> {
        let window = self.window(turn, model).await?;
        let render = RenderRequest {
            messages: messages.to_vec(),
            tools: tools.to_vec(),
            thinking,
            reasoning_effort,
            generation_prompt: None,
        };
        let (_slot, rendered) = self.call(turn, false, |left| self.mk.render(&render, left)).await?;
        admit(&self.name, u64::from(rendered.n_tokens), window, asked)
    }

    /// The window for `model`: the service's `max_context`, or the model
    /// row's `context_window` when that is smaller. A row larger than the
    /// service is a configuration error.
    async fn window(&self, turn: &Turn, model: &str) -> LlmResult<u64> {
        let max_context = *self
            .max_context
            .get_or_try_init(|| async {
                let (_slot, model) = self.call(turn, false, |_| self.mk.model()).await?;
                Ok::<_, LlmError>(u64::from(model.max_context))
            })
            .await?;
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
    /// Released once the stream finishes. Told how the stream ended: its
    /// done event is a completed answer, and an error event with 429 or 503
    /// is a busy one.
    slot: Option<Slot>,
}

impl Stream {
    pub async fn next_event(&mut self) -> Option<StreamEvent> {
        loop {
            if self.finished {
                self.slot = None;
            }
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
                        if let Some(slot) = &self.slot {
                            slot.completed();
                        }
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
                        if let Some(slot) = &self.slot {
                            slot.failed_mk(&e);
                        }
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

    /// A client whose calls take slots at a fresh endpoint for `base`.
    fn limited(base: &str, timeout: Duration) -> (Client, Arc<Endpoint>) {
        let endpoint = crate::llm::endpoint::Endpoints::default().for_url(base).unwrap();
        let c = Client::new("zorak-mk", base, timeout, HashMap::new()).unwrap().with_endpoint(endpoint.clone());
        (c, endpoint)
    }

    fn busy() -> kaijutsu_mk::test_server::Reply {
        let mut r = http(429, json!({"error": {"code": 429, "message": "16 requests in progress", "type": "busy"}}).to_string());
        r.headers.push(("retry-after", "1".into()));
        r
    }

    /// A streamed turn holds its slot until the stream ends.
    ///
    /// Falsified by a stream that drops its slot when it opens, or keeps it
    /// after its last event.
    #[tokio::test]
    async fn a_turn_holds_its_slot_until_its_stream_ends() {
        let (base, _) = serve(vec![http(200, MODEL), rendered(30), sse(ANSWER)]).await;
        let (c, endpoint) = limited(&base, Duration::from_secs(5));
        let mut stream = c.stream(opts(64), vec![Message::user("hi")]).await.unwrap();
        assert_eq!(endpoint.status().in_flight, 1);
        drain(&mut stream).await;
        assert_eq!(endpoint.status().in_flight, 0);
    }

    /// The service refuses a 429 before any work starts
    /// (`docs/mk-admission.md`), so the turn sends the same call again after
    /// `Retry-After`.
    ///
    /// Falsified by a 429 that fails the turn, or a resend that ignores the
    /// cooldown.
    #[tokio::test]
    async fn a_busy_answer_is_sent_again_after_its_retry_after() {
        let (base, seen) = serve(vec![http(200, MODEL), busy(), rendered(30), sse(ANSWER)]).await;
        let (c, endpoint) = limited(&base, Duration::from_secs(5));
        let started = std::time::Instant::now();
        let mut stream = c.stream(opts(64), vec![Message::user("hi")]).await.unwrap();
        assert!(started.elapsed() >= Duration::from_secs(1), "resent after {:?}", started.elapsed());
        let events = drain(&mut stream).await;
        assert!(matches!(events.last(), Some(StreamEvent::Done { .. })), "{events:?}");
        let paths: Vec<String> = seen.lock().unwrap().iter().map(|c| c.path.clone()).collect();
        assert_eq!(paths, ["/mk/v1/model", "/mk/v1/render", "/mk/v1/render", "/mk/v1/generate"]);
        assert_eq!(endpoint.status().cooldown, None);
    }

    /// A 503 cools the endpoint and fails the turn; it is not sent again.
    #[tokio::test]
    async fn a_503_cools_the_endpoint_and_is_not_sent_again() {
        let (base, seen) = serve(vec![http(200, MODEL), rendered(30), http(503, "{}")]).await;
        let (c, endpoint) = limited(&base, Duration::from_secs(5));
        let err = c.stream(opts(64), vec![Message::user("hi")]).await.err().unwrap();
        assert!(matches!(err, LlmError::ApiError(_)), "{err:?}");
        assert_eq!(seen.lock().unwrap().len(), 3);
        let (left, status) = endpoint.status().cooldown.expect("cooling down");
        assert_eq!(status, 503);
        assert!(left > Duration::from_secs(4), "{left:?}");
    }

    /// A turn that cannot get a slot within its timeout fails `RateLimited`,
    /// naming the wait, and sends nothing.
    #[tokio::test]
    async fn a_turn_with_no_slot_in_its_timeout_is_rate_limited() {
        let (base, seen) = serve(vec![]).await;
        let (c, endpoint) = limited(&base, Duration::from_millis(300));
        endpoint.acquire(tokio::time::Instant::now()).await.unwrap().answered_with(429, Some(Duration::from_secs(5)));
        let err = c.stream(opts(64), vec![Message::user("hi")]).await.err().unwrap();
        match err {
            LlmError::RateLimited(why) => {
                assert!(why.starts_with("mk backend 'zorak-mk': no slot at http://127.0.0.1:"), "{why}");
                assert!(why.contains("cooling down"), "{why}");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
        assert!(seen.lock().unwrap().is_empty());
    }

    /// A call sent again after a 429 gets only the time left before the
    /// turn's deadline, not the whole timeout again.
    ///
    /// Falsified by a resend that waits its full timeout: the turn would
    /// fail about 3 s in, past its 2 s deadline.
    #[tokio::test]
    async fn a_resent_call_gets_only_the_time_left_before_the_deadline() {
        let mut slow = rendered(30);
        slow.delay = Duration::from_secs(4);
        let (base, _) = serve(vec![http(200, MODEL), busy(), slow]).await;
        let (c, _) = limited(&base, Duration::from_secs(2));
        let started = std::time::Instant::now();
        let err = c.stream(opts(64), vec![Message::user("hi")]).await.err().unwrap();
        let took = started.elapsed();
        assert!(took < Duration::from_millis(2500), "the turn ended after {took:?}: {err:?}");
    }

    /// A stream that fails with a 503 error event cools the endpoint.
    ///
    /// Falsified by a mid-stream 503 that never reaches the limiter.
    #[tokio::test]
    async fn a_mid_stream_503_cools_the_endpoint() {
        let cut = &ANSWER[..ANSWER.find("event: done").unwrap()];
        let failed = format!(
            "{cut}event: error\ndata: {}\n\n",
            r#"{"error":{"code":503,"message":"spin","type":"pass_timeout_error"}}"#
        );
        let (base, _) = serve(vec![http(200, MODEL), rendered(10), sse(failed)]).await;
        let (c, endpoint) = limited(&base, Duration::from_secs(5));
        let mut stream = c.stream(opts(64), vec![Message::user("hi")]).await.unwrap();
        let events = drain(&mut stream).await;
        assert!(matches!(events.last(), Some(StreamEvent::Error(m)) if m.contains("pass_timeout_error")), "{events:?}");
        let (_, status) = endpoint.status().cooldown.expect("cooling down");
        assert_eq!(status, 503);
    }

    /// A stream that reaches its done event resets the doubling.
    ///
    /// Falsified by a stream whose end leaves a busy answer counted.
    #[tokio::test]
    async fn a_stream_that_ends_whole_resets_the_doubling() {
        let (base, _) = serve(vec![http(200, MODEL), rendered(30), sse(ANSWER)]).await;
        let (c, endpoint) = limited(&base, Duration::from_secs(5));
        let mut stream = c.stream(opts(64), vec![Message::user("hi")]).await.unwrap();
        let far = tokio::time::Instant::now() + Duration::from_secs(5);
        endpoint.acquire(far).await.unwrap().answered_with(503, Some(Duration::ZERO));
        assert_eq!(endpoint.busy_in_a_row(), 1);
        let events = drain(&mut stream).await;
        assert!(matches!(events.last(), Some(StreamEvent::Done { .. })), "{events:?}");
        assert_eq!(endpoint.busy_in_a_row(), 0);
    }
}
