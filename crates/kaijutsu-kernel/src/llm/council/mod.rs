//! The `council` provider: a context talks to a council v2 server as a
//! model (`BackendKind::Council`).
//!
//! An adapter over `kaijutsu_council`; it does no HTTP of its own. Each streamed
//! turn is a council v2 generation that sends the whole conversation
//! ([`generation`]), on a served model that lists `generate`. A turn on any other
//! model is refused before anything is sent. A prompt that leaves no room
//! for a reply is the server's `context_full`, and a reply that stops at its
//! output limit is never returned as a whole one. `cache_breakpoints`,
//! `thinking_budget`, and `thinking_style` do not apply to this backend and
//! are ignored.

mod build;
mod reply;
mod generation;

use std::sync::Arc;
use std::time::{Duration, Instant};

use gogi_proto::identity::ServerIdentity;
use kaijutsu_council::{CouncilClient, CouncilError, GenerationEvent, GenerationStream};
use tokio_util::sync::CancellationToken;

use crate::llm::endpoint::{Endpoint, Slot, SlotWaits};
use crate::llm::stream::{BuildOpts, StreamError, StreamErrorKind, StreamEvent};
use crate::llm::{LlmError, LlmResult, Message};

use reply::{Reply, Timing};

/// The deadline of one prompt or turn, which bounds every slot wait in it,
/// and the waits so far.
struct Turn {
    deadline: tokio::time::Instant,
    waits: SlotWaits,
}

/// A client for one `council` backend.
#[derive(Clone, Debug)]
pub struct Client {
    name: String,
    /// The server address, as the request dump names it.
    base_url: String,
    client: CouncilClient,
    /// Bounds each non-streamed call, and each read of a stream.
    timeout: Duration,
    /// The endpoint each turn takes one slot from, held from the identity
    /// read through the reply. `None` only for a client built directly
    /// rather than by `Provider::from_backend`.
    endpoint: Option<Arc<Endpoint>>,
    /// The server's council v2 identity and when it was read. A turn reads
    /// it when none is cached, when the cached one is older than
    /// `identity_max_age`, and after a generation refused for its `thinking`.
    identity: Arc<parking_lot::Mutex<IdentityCell>>,
    identity_max_age: Duration,
}

/// One read of the council v2 identity, which concurrent turns share.
type IdentityCell = Arc<tokio::sync::OnceCell<(ServerIdentity, tokio::time::Instant)>>;

/// How long a council v2 identity serves turns before the next turn reads
/// it again. A model that gains `thinking` in a redeploy thinks within
/// this time; a model that loses it is read again after the first 400.
const IDENTITY_MAX_AGE: Duration = Duration::from_secs(600);

impl Client {
    /// `name` is the backend name, reported as the provider name.
    pub fn new(name: impl Into<String>, base_url: &str, timeout: Duration) -> LlmResult<Self> {
        let name = name.into();
        let client = CouncilClient::new(base_url, timeout).map_err(|e| to_llm(&name, e))?;
        Ok(Client {
            name,
            base_url: base_url.to_string(),
            client,
            timeout,
            endpoint: None,
            identity: Arc::default(),
            identity_max_age: IDENTITY_MAX_AGE,
        })
    }

    /// Take a slot from `endpoint` for each call to the server.
    pub fn with_endpoint(mut self, endpoint: Arc<Endpoint>) -> Self {
        self.endpoint = Some(endpoint);
        self
    }

    /// The same client, reading the council v2 identity again once the
    /// cached one is older than `age`.
    #[cfg(test)]
    fn with_identity_max_age(mut self, age: Duration) -> Self {
        self.identity_max_age = age;
        self
    }

    /// The endpoint this client takes slots from.
    pub fn endpoint(&self) -> Option<&Arc<Endpoint>> {
        self.endpoint.as_ref()
    }

    /// One call to the server through this client's endpoint
    /// ([`crate::llm::endpoint::council_call`]): a slot first, and the same call
    /// again after the cooldown when the server answers 429, all within
    /// `turn`'s deadline. `call` is given each attempt's timeout, the time
    /// left before the deadline and no more than the client's timeout, and
    /// each attempt ends at the deadline. Returns the slot, which a stream
    /// holds until it ends. With no endpoint, the call goes straight out.
    async fn call<T, F, Fut>(&self, turn: &Turn, opens_stream: bool, mut call: F) -> LlmResult<(Option<Slot>, T)>
    where
        F: FnMut(Duration) -> Fut,
        Fut: std::future::Future<Output = Result<T, CouncilError>>,
    {
        let mut attempt = || {
            let left = turn.deadline.saturating_duration_since(tokio::time::Instant::now()).min(self.timeout);
            let sent = call(left);
            async move { tokio::time::timeout_at(turn.deadline, sent).await.unwrap_or(Err(CouncilError::Timeout)) }
        };
        let Some(endpoint) = &self.endpoint else {
            return attempt().await.map(|v| (None, v)).map_err(|e| to_llm(&self.name, e));
        };
        let answered = if opens_stream {
            crate::llm::endpoint::council_open(endpoint, turn.deadline, &turn.waits, attempt).await
        } else {
            crate::llm::endpoint::council_call(endpoint, turn.deadline, &turn.waits, attempt).await
        };
        tracing::Span::current().record("llm.slot_wait_ms", turn.waits.total().as_millis() as u64);
        match answered {
            Ok((slot, result)) => result.map(|v| (Some(slot), v)).map_err(|e| to_llm(&self.name, e)),
            Err(no_slot) => Err(LlmError::RateLimited(format!("council backend '{}': {no_slot}", self.name))),
        }
    }

    /// The deadline and slot waits of one prompt or turn.
    fn turn(&self) -> Turn {
        Turn { deadline: tokio::time::Instant::now() + self.timeout, waits: SlotWaits::default() }
    }

    pub fn provider_name(&self) -> &str {
        &self.name
    }

    /// The request options a one-shot prompt runs with: thinking off, since a
    /// one-shot wants the answer text and thinking tokens count against
    /// `max_tokens`.
    pub fn one_shot_opts(&self, opts: BuildOpts) -> BuildOpts {
        opts.with_effort("none")
    }

    /// A streamed turn: a council v2 generation after the whole
    /// conversation, on the served model the turn's model names. A model
    /// the server does not serve, or serves without `generate`, is refused
    /// before any generation.
    pub async fn stream(&self, opts: BuildOpts, history: Vec<Message>) -> LlmResult<Stream> {
        build::effort(opts.effort.as_deref())?;
        let turn = self.turn();
        let served = self.served_model(&turn, &opts.model).await?;
        self.stream_generation(opts, history, served, turn).await
    }
}

fn to_llm(name: &str, e: CouncilError) -> LlmError {
    let detail = format!("council backend '{name}': {e}");
    match e {
        CouncilError::Request(_) => LlmError::InvalidRequest(detail),
        CouncilError::Status { status: 400 | 404 | 413, .. } => LlmError::InvalidRequest(detail),
        CouncilError::Status { status: 429, .. } => LlmError::RateLimited(detail),
        CouncilError::Status { status: 402, .. } => LlmError::InsufficientBalance(detail),
        CouncilError::Timeout | CouncilError::Transport(_) => LlmError::NetworkError(detail),
        CouncilError::ContextFull { .. } => LlmError::ContextFull(format!(
            "{detail}; exclude blocks with `kj stage exclude`, then fork"
        )),
        CouncilError::Stream { .. }
        | CouncilError::Truncated(_)
        | CouncilError::Decode { .. }
        | CouncilError::Status { .. }
        | CouncilError::SpecIdMismatch { .. } => LlmError::ApiError(detail),
    }
}

/// The kind of a stream failure that came from the council server. An
/// error event or status maps by its HTTP status; the rest by how the call
/// failed.
fn error_kind(e: &CouncilError) -> StreamErrorKind {
    use StreamErrorKind::*;
    match e {
        CouncilError::Timeout => Timeout,
        CouncilError::Transport(_) | CouncilError::Truncated(_) => Transport,
        CouncilError::Decode { .. } | CouncilError::SpecIdMismatch { .. } => Protocol,
        CouncilError::Request(_) | CouncilError::ContextFull { .. } => InvalidRequest,
        CouncilError::Status { status, .. } => status_kind(*status),
        // A council error event names a type, which `status` maps to the status it stands for.
        CouncilError::Stream { .. } => e.status().map_or(ServerError, status_kind),
    }
}

fn status_kind(status: u16) -> StreamErrorKind {
    use StreamErrorKind::*;
    match status {
        429 => RateLimited,
        503 => Overloaded,
        401 | 403 => Auth,
        402 => Billing,
        408 | 504 => Timeout,
        400..=499 => InvalidRequest,
        500..=599 => ServerError,
        other => Unknown(other.to_string()),
    }
}

/// A streamed `council` turn as [`StreamEvent`]s.
///
/// [`Self::cancel`] is observed by the next [`Self::next_event`], which drops
/// the HTTP stream (the server cancels the generation when the connection
/// closes) and yields `Done { stop_reason: None }`, the cancel-confirm the
/// server expects.
pub struct Stream {
    name: String,
    inner: Option<GenerationStream>,
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
    /// The server identity the reply reports.
    reply_identity: Option<generation::ReplyIdentity>,
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
                    Some(Ok(GenerationEvent::Token(token))) => {
                        self.first_token.get_or_insert_with(|| self.started.elapsed());
                        let reply = self.reply.as_mut().expect("the reply lives until done");
                        self.pending.extend(reply.phased(token.phase, &token.piece));
                    }
                    Some(Ok(GenerationEvent::Done(done))) => {
                        self.finished = true;
                        self.inner = None;
                        if let Some(slot) = &self.slot {
                            slot.completed();
                        }
                        let timing = Timing {
                            first_token_ms: self.first_token.unwrap_or_default().as_millis() as u64,
                            wall_ms: self.started.elapsed().as_millis() as u64,
                        };
                        let identity = self.reply_identity.take().expect("a stream names its server");
                        let reply = self.reply.take().expect("done comes once");
                        match reply.done(*done, &identity.model, &identity.weight_hash, timing) {
                            Ok(events) => self.pending.extend(events),
                            Err(e) => return Some(StreamEvent::Error(StreamError::new(
                                StreamErrorKind::Protocol,
                                format!("council backend '{}': {e}", self.name),
                            ))),
                        }
                    }
                    Some(Err(e)) => {
                        self.finished = true;
                        self.inner = None;
                        if let Some(slot) = &self.slot {
                            slot.failed_council(&e);
                        }
                        return Some(StreamEvent::Error(StreamError::new(
                            error_kind(&e),
                            format!("council backend '{}': {e}", self.name),
                        )));
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
    use kaijutsu_council::test_server::{reply as http, serve, sse};
    use serde_json::json;

    use super::*;
    use crate::llm::ToolDefinition;

    /// A 402 is a refusal that repeating cannot fix.
    #[test]
    fn a_402_is_an_insufficient_balance_error() {
        let e = CouncilError::Status { status: 402, error: None, retry_after: None, body: "pay".into(), request_id: None };
        let err = to_llm("b", e);
        assert!(matches!(&err, LlmError::InsufficientBalance(m) if m.contains("council backend 'b'")), "{err:?}");
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
            trace: None,
        }
    }

    async fn drain(stream: &mut Stream) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Some(ev) = stream.next_event().await {
            out.push(ev);
        }
        out
    }

    use crate::llm::{ContentBlock, MessageContent, Role};

    fn serving(capabilities: serde_json::Value) -> kaijutsu_council::test_server::Reply {
        http(
            200,
            json!({"models": [{"model": "gemma-4", "aliases": ["qwen"], "status": "ready", "weight_hash": "w4",
                               "tokenizer_hash": "t", "template": "x", "engine": "e",
                               "limits": {"context_tokens": 32768, "decision_context_tokens": 3072, "snapshot_bytes": 1,
                                          "contexts_per_decision": 8, "default_timeout_ms": 2000},
                               "capabilities": capabilities}],
                   "limits": {"case_bytes": 1000, "choice_options": 26},
                   "capabilities": []})
            .to_string(),
        )
    }

    fn no_council() -> kaijutsu_council::test_server::Reply {
        http(404, "no such route")
    }

    fn client(base: &str) -> Client {
        Client::new("nagaya", base, Duration::from_secs(5)).unwrap()
    }

    fn snap_id(n: u64) -> String {
        format!("blake3:{n:064x}")
    }

    /// A streamed generation: `content` as one answer token, then `done`
    /// with `head` held, or `null` for 0.
    fn generation(content: &str, calls: serde_json::Value, head: u64) -> kaijutsu_council::test_server::Reply {
        sse(generation_events(content, calls, head))
    }

    /// The events of [`generation`].
    fn generation_events(content: &str, calls: serde_json::Value, head: u64) -> String {
        let mut out = String::new();
        if !content.is_empty() {
            let t = json!({"id": 1, "piece": content, "phase": "content", "p": 0.9, "h": 0.1, "n": 1, "tps": 10.0});
            out.push_str(&format!("event: token\ndata: {t}\n\n"));
        }
        let head = if head == 0 { json!(null) } else { json!(snap_id(head)) };
        let finish = if calls.as_array().is_some_and(|c| !c.is_empty()) { "tool_use" } else { "end_turn" };
        let done = json!({"model": "gemma-4", "content": content, "reasoning": "", "tool_calls": calls, "finish": finish,
                          "usage": {"prompt": 47, "kept": 40, "fed": 7, "completion": 3, "reasoning": 0}, "head": head, "seed": 11,
                          "snapshots": [{"id": snap_id(1), "message": 1, "tokens": 15}]});
        out.push_str(&format!("event: done\ndata: {done}\n\n"));
        out
    }

    fn said(text: &str) -> Message {
        Message { role: Role::Assistant, content: MessageContent::Blocks(vec![ContentBlock::Text { text: text.into() }]), turn_scoped: false }
    }

    fn bodies(seen: &std::sync::Mutex<Vec<kaijutsu_council::test_server::Captured>>, path_prefix: &str) -> Vec<serde_json::Value> {
        seen.lock().unwrap().iter().filter(|c| c.path.starts_with(path_prefix)).map(|c| serde_json::from_str(&c.body).unwrap()).collect()
    }

    /// With a dump directory on the turn's trace, a turn writes the
    /// generation it put on the wire: the file's `body` equals what the
    /// server received, and the header names the backend and the endpoint.
    ///
    /// Falsified by a turn that logs a summary and dumps no body.
    #[tokio::test]
    async fn a_turn_dumps_the_generation_the_server_received() {
        use crate::llm::request_dump::{RequestTrace, wait_for_files};
        use kaijutsu_types::{ContextId, TurnId};
        let (base, seen) = serve(vec![serving(json!(["generate"])), generation("hi", json!([]), 2)]).await;
        let tmp = tempfile::tempdir().unwrap();
        let mut o = opts(64);
        o.trace = Some(Arc::new(RequestTrace::new(ContextId::new(), TurnId::new(), 3, Some(tmp.path().to_path_buf()))));
        drain(&mut client(&base).stream(o, vec![Message::user("u1")]).await.unwrap()).await;
        let files = wait_for_files(tmp.path(), 1).await;
        assert_eq!(files.len(), 1);
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
        assert_eq!(v["body"], bodies(&seen, "/council/v2/generations")[0]);
        assert_eq!(v["header"]["provider"], "nagaya");
        assert_eq!(v["header"]["step"], 3);
        assert!(v["header"]["endpoint_host"].as_str().is_some_and(|h| h.contains("127.0.0.1")), "{}", v["header"]);
    }

    /// A served model without `generate` cannot take a turn: the turn is
    /// refused, naming the backend, the model, and what the server serves,
    /// and no generation is sent.
    ///
    /// Falsified by a turn that falls back to another route, or sends a
    /// generation the model cannot run.
    #[tokio::test]
    async fn a_served_model_without_generate_is_refused() {
        let (base, seen) = serve(vec![serving(json!(["reads", "pins"]))]).await;
        let err = client(&base).stream(opts(64), vec![Message::user("hi")]).await.err().unwrap();
        assert!(
            matches!(&err, LlmError::InvalidRequest(m)
                if m.contains("council backend 'nagaya'") && m.contains("gemma-4") && m.contains("generate")),
            "{err:?}"
        );
        let paths: Vec<String> = seen.lock().unwrap().iter().map(|c| c.path.clone()).collect();
        assert_eq!(paths, ["/council/v2/identity"], "only the identity");
    }

    /// A server without council v2 answers the identity with 404, and a
    /// turn on it is refused, saying so, and sends nothing else.
    ///
    /// Falsified by a turn that tries another route.
    #[tokio::test]
    async fn a_server_without_council_v2_is_refused() {
        let (base, seen) = serve(vec![no_council()]).await;
        let err = client(&base).stream(opts(64), vec![Message::user("hi")]).await.err().unwrap();
        assert!(
            matches!(&err, LlmError::InvalidRequest(m)
                if m.contains("council backend 'nagaya'") && m.contains("404") && m.contains("no council v2 identity")),
            "{err:?}"
        );
        assert_eq!(seen.lock().unwrap().len(), 1, "only the identity");
    }

    /// On a council v2 server that lists `generate`, every turn is one
    /// generation that sends the whole conversation, with `snap` on the
    /// system message, each user turn, and each run of tool results, and
    /// nothing else: no context `PUT`, no context id. The next turn sends
    /// the reply as the server wrote it, its call under the server's id,
    /// and the tool result naming that id. A head the server did not hold
    /// changes nothing.
    ///
    /// Falsified by a provider that keeps a context record on the server,
    /// sends only the new messages, mints its own call ids, or snaps an
    /// assistant message.
    #[tokio::test]
    async fn a_turn_sends_the_whole_conversation_and_no_put() {
        let (base, seen) = serve(vec![
            serving(json!(["generate", "pins"])),
            generation("", json!([{"id": "c0", "name": "ls", "input": {"path": "/tmp"}}]), 0),
            generation("Two files.", json!([]), 3),
        ])
        .await;
        let c = client(&base);
        let mut o = opts(64);
        o.tools = vec![ToolDefinition { name: "ls".into(), description: "List.".into(), input_schema: json!({"type": "object"}) }];
        let mut history = vec![Message::user("list /tmp")];
        let opened = c.stream(o.clone(), history.clone()).await;
        let paths = || -> Vec<String> { seen.lock().unwrap().iter().map(|c| format!("{} {}", c.method, c.path)).collect() };
        assert_eq!(paths(), ["GET /council/v2/identity", "POST /council/v2/generations"], "one generation and no PUT");
        let first = drain(&mut opened.unwrap()).await;
        let Some(StreamEvent::ToolUse { id, name, input }) = first.first().cloned() else { panic!("{first:?}") };
        assert_eq!((id.as_str(), name.as_str(), input), ("c0", "ls", json!({"path": "/tmp"})));
        let Some(StreamEvent::Done { stop_reason, input_tokens, extra: Some(crate::llm::UsageExtra::Council(x)), .. }) = first.last() else {
            panic!("{first:?}")
        };
        assert_eq!((stop_reason.as_deref(), *input_tokens, x.kept, x.seed, x.model.as_str()), (Some("tool_use"), Some(47), 40, Some(11), "gemma-4"));

        history.push(Message { role: Role::Assistant, content: MessageContent::Blocks(vec![ContentBlock::ToolUse { id, name, input: json!({"path": "/tmp"}) }]), turn_scoped: false });
        history.push(Message::tool_results(vec![ContentBlock::ToolResult { tool_use_id: "c0".into(), content: "a\nb".into(), is_error: false }]));
        let second = drain(&mut c.stream(o, history).await.unwrap()).await;
        assert!(second.iter().any(|e| matches!(e, StreamEvent::TextDelta(t) if t == "Two files.")), "{second:?}");

        assert_eq!(paths(), ["GET /council/v2/identity", "POST /council/v2/generations", "POST /council/v2/generations"]);
        let gens = bodies(&seen, "/council/v2/generations");
        let tools = json!([{"name": "ls", "description": "List.", "input_schema": {"type": "object"}}]);
        assert_eq!(
            gens[0],
            json!({"model": "gemma-4", "context": {"thinking": false, "tools": tools,
                               "messages": [{"role": "system", "content": "rules", "snap": true},
                                            {"role": "user", "content": "list /tmp", "snap": true}]},
                   "max_tokens": 64, "stream": true})
        );
        assert_eq!(
            gens[1]["context"]["messages"],
            json!([{"role": "system", "content": "rules", "snap": true},
                   {"role": "user", "content": "list /tmp", "snap": true},
                   {"role": "assistant", "tool_calls": [{"id": "c0", "name": "ls", "input": {"path": "/tmp"}}]},
                   {"role": "tool", "tool_call_id": "c0", "content": "a\nb", "snap": true}])
        );
        assert_eq!(gens[1]["context"]["tools"], tools);
    }

    /// A conversation that changed under the kernel, such as one with an
    /// excluded turn, is sent as it now stands: the server starts from the
    /// prefix it shares with what it holds.
    ///
    /// Falsified by a provider that sends history the kernel no longer has.
    #[tokio::test]
    async fn a_shorter_conversation_is_sent_as_it_stands() {
        let (base, seen) = serve(vec![serving(json!(["generate"])), generation("r2", json!([]), 4), generation("r3", json!([]), 5)]).await;
        let c = client(&base);
        drain(&mut c.stream(opts(64), vec![Message::user("u1"), said("r1"), Message::user("u2")]).await.unwrap()).await;
        drain(&mut c.stream(opts(64), vec![Message::user("u1"), said("r1"), Message::user("u3")]).await.unwrap()).await;
        let gens = bodies(&seen, "/council/v2/generations");
        let contents: Vec<Vec<serde_json::Value>> = gens
            .iter()
            .map(|g| g["context"]["messages"].as_array().unwrap().iter().map(|m| m["content"].clone()).collect())
            .collect();
        assert_eq!(contents, [vec![json!("rules"), json!("u1"), json!("r1"), json!("u2")], vec![json!("rules"), json!("u1"), json!("r1"), json!("u3")]]);
    }

    /// A generation whose prompt leaves no room is `context_full`, which the
    /// kernel reads as a full context.
    ///
    /// Falsified by a 413 mapped to an invalid request.
    #[tokio::test]
    async fn a_context_full_generation_is_a_full_context() {
        let full = http(413, json!({"error": {"type": "context_full", "message": "the prompt takes 32768 of 32768 tokens"}}).to_string());
        let (base, _) = serve(vec![serving(json!(["generate"])), full]).await;
        let err = client(&base).stream(opts(64), vec![Message::user("hi")]).await.err().unwrap();
        assert!(matches!(&err, LlmError::ContextFull(m) if m.contains("32768") && m.contains("fork")), "{err:?}");
        assert_eq!(err.category_and_code(), (kaijutsu_types::ErrorCategory::Context, Some("full")));
    }

    /// A turn's model names a served model by its `model` or an alias, and
    /// the generation names it by its `model`. A model the server does not
    /// serve is refused before any generation, naming the ones it serves.
    ///
    /// Falsified by a generation that sends the alias, or one sent for a
    /// model the server lacks.
    #[tokio::test]
    async fn a_turn_names_its_served_model_and_one_the_server_lacks_is_refused() {
        let (base, seen) = serve(vec![serving(json!(["generate"])), generation("hi", json!([]), 2)]).await;
        let c = client(&base);
        drain(&mut c.stream(opts(64), vec![Message::user("u1")]).await.unwrap()).await;
        assert_eq!(bodies(&seen, "/council/v2/generations")[0]["model"], "gemma-4", "the alias qwen routes to gemma-4");
        let mut other = opts(64);
        other.model = "llama".into();
        let err = c.stream(other, vec![Message::user("u1")]).await.err().unwrap();
        assert!(matches!(&err, LlmError::InvalidRequest(m) if m.contains("\"llama\"") && m.contains("it serves gemma-4")), "{err:?}");
        assert_eq!(seen.lock().unwrap().len(), 2, "the identity and one generation");
    }

    /// A turn sends `thinking: true` only to a model that declares
    /// `thinking` and when the turn wants thinking: with no effort, or an
    /// effort other than `none`. A model without `thinking` gets `false`
    /// when the turn names no effort. A turn that asks such a model for an
    /// effort is refused before any generation.
    ///
    /// Falsified by a turn that sends `true` to a model without `thinking`,
    /// sends `false` to one with it when no effort is named, or drops an
    /// asked effort without a word.
    #[tokio::test]
    async fn a_turn_thinks_only_on_a_model_that_declares_thinking() {
        let sent = |capabilities: serde_json::Value, effort: Option<&str>| {
            let effort = effort.map(str::to_string);
            async move {
                let (base, seen) = serve(vec![serving(capabilities), generation("hi", json!([]), 2)]).await;
                let mut o = opts(64);
                o.effort = effort;
                let opened = match client(&base).stream(o, vec![Message::user("u1")]).await {
                    Ok(mut s) => {
                        drain(&mut s).await;
                        Ok(())
                    }
                    Err(e) => Err(e),
                };
                (opened, bodies(&seen, "/council/v2/generations").first().map(|g| g["context"]["thinking"].clone()))
            }
        };
        let (ok, thinking) = sent(json!(["generate", "thinking"]), None).await;
        assert!(ok.is_ok(), "{ok:?}");
        assert_eq!(thinking, Some(json!(true)), "a thinking model thinks when the turn names no effort");
        let (_, thinking) = sent(json!(["generate", "thinking"]), Some("low")).await;
        assert_eq!(thinking, Some(json!(true)));
        let (_, thinking) = sent(json!(["generate", "thinking"]), Some("none")).await;
        assert_eq!(thinking, Some(json!(false)), "effort none turns thinking off");
        let (ok, thinking) = sent(json!(["generate"]), None).await;
        assert!(ok.is_ok(), "{ok:?}");
        assert_eq!(thinking, Some(json!(false)), "a model without thinking gets false");
        let (_, thinking) = sent(json!(["generate"]), Some("none")).await;
        assert_eq!(thinking, Some(json!(false)));
        let (err, thinking) = sent(json!(["generate"]), Some("medium")).await;
        assert!(matches!(&err, Err(LlmError::InvalidRequest(m)) if m.contains("thinking") && m.contains("medium")), "{err:?}");
        assert_eq!(thinking, None, "nothing is sent");
    }

    /// The `thinking` sent by each generation, in order.
    fn sent_thinking(seen: &std::sync::Mutex<Vec<kaijutsu_council::test_server::Captured>>) -> Vec<serde_json::Value> {
        bodies(seen, "/council/v2/generations").iter().map(|g| g["context"]["thinking"].clone()).collect()
    }

    /// A generation refused with a 400 naming `thinking` drops the cached
    /// identity, and the next turn reads it again. Here the model lost
    /// `thinking` in a redeploy between two turns.
    ///
    /// Falsified by a cached identity that outlives the 400, so the next
    /// turn sends the refused `thinking: true` without asking.
    #[tokio::test]
    async fn a_generation_refused_for_thinking_reads_the_identity_again() {
        let refused = http(
            400,
            json!({"error": {"type": "invalid_request", "param": "thinking",
                             "message": "nothing ran: gemma-4 does not declare thinking; send thinking: false"}})
            .to_string(),
        );
        let (base, seen) = serve(vec![
            serving(json!(["generate", "thinking"])),
            generation("a", json!([]), 2),
            refused,
            serving(json!(["generate"])),
            generation("b", json!([]), 3),
        ])
        .await;
        let c = client(&base);
        let mut o = opts(64);
        o.effort = None;
        drain(&mut c.stream(o.clone(), vec![Message::user("u1")]).await.unwrap()).await;
        let err = c.stream(o.clone(), vec![Message::user("u2")]).await.err().unwrap();
        assert!(matches!(&err, LlmError::InvalidRequest(_) | LlmError::ApiError(_)), "{err:?}");
        drain(&mut c.stream(o, vec![Message::user("u3")]).await.unwrap()).await;
        let paths: Vec<String> = seen.lock().unwrap().iter().map(|c| c.path.clone()).collect();
        assert_eq!(
            paths,
            ["/council/v2/identity", "/council/v2/generations", "/council/v2/generations", "/council/v2/identity", "/council/v2/generations"]
        );
        assert_eq!(sent_thinking(&seen), [json!(true), json!(true), json!(false)]);
    }

    /// An identity older than the client's limit is read again on the next
    /// turn, so a model that gains `thinking` in a redeploy thinks from then
    /// on. No 400 tells a client about a gain, since `false` is always
    /// allowed.
    ///
    /// Falsified by an identity read once for the client's life.
    #[tokio::test]
    async fn an_old_identity_is_read_again_and_a_gained_thinking_is_used() {
        let (base, seen) = serve(vec![
            serving(json!(["generate"])),
            generation("a", json!([]), 2),
            serving(json!(["generate", "thinking"])),
            generation("b", json!([]), 3),
        ])
        .await;
        let c = client(&base).with_identity_max_age(Duration::ZERO);
        let mut o = opts(64);
        o.effort = None;
        drain(&mut c.stream(o.clone(), vec![Message::user("u1")]).await.unwrap()).await;
        drain(&mut c.stream(o, vec![Message::user("u2")]).await.unwrap()).await;
        assert_eq!(sent_thinking(&seen), [json!(false), json!(true)]);
        assert_eq!(seen.lock().unwrap().iter().filter(|c| c.path == "/council/v2/identity").count(), 2);
    }

    /// A conversation that ends with an assistant message has nothing to
    /// reply to and is refused before any generation.
    #[tokio::test]
    async fn a_conversation_that_ends_with_the_assistant_is_refused() {
        let (base, seen) = serve(vec![serving(json!(["generate"]))]).await;
        let err = client(&base).stream(opts(64), vec![Message::user("u1"), said("r1")]).await.err().unwrap();
        assert!(matches!(&err, LlmError::InvalidRequest(m) if m.contains("assistant")), "{err:?}");
        assert_eq!(seen.lock().unwrap().len(), 1, "only the identity");
    }

    #[test]
    fn an_mk_error_maps_to_its_stream_kind() {
        let status = |status: u16| CouncilError::Status { status, error: None, retry_after: None, body: String::new(), request_id: None };
        let council = |r#type| CouncilError::Stream { error: gogi_proto::ErrorBody::new(r#type, "m").error, request_id: None };
        let cases = [
            (status(503), StreamErrorKind::Overloaded),
            (status(429), StreamErrorKind::RateLimited),
            (status(500), StreamErrorKind::ServerError),
            (status(504), StreamErrorKind::Timeout),
            (status(400), StreamErrorKind::InvalidRequest),
            (CouncilError::Timeout, StreamErrorKind::Timeout),
            (CouncilError::Transport("reset".into()), StreamErrorKind::Transport),
            (CouncilError::Truncated("x".into()), StreamErrorKind::Transport),
            (CouncilError::Decode { what: "w", message: "m".into(), body: String::new() }, StreamErrorKind::Protocol),
            (council(gogi_proto::ErrorType::ModelUnavailable), StreamErrorKind::Overloaded),
            (council(gogi_proto::ErrorType::Busy), StreamErrorKind::RateLimited),
            (council(gogi_proto::ErrorType::Timeout), StreamErrorKind::Timeout),
            (council(gogi_proto::ErrorType::InvalidRequest), StreamErrorKind::ServerError),
        ];
        for (error, kind) in cases {
            assert_eq!(error_kind(&error), kind, "{error}");
        }
    }

    #[tokio::test]
    async fn a_stream_cut_before_done_is_an_error_event() {
        let events = generation_events("Hello.", json!([]), 2);
        let cut = &events[..events.find("event: done").unwrap()];
        let (base, _) = serve(vec![serving(json!(["generate"])), sse(cut)]).await;
        let mut stream = client(&base).stream(opts(64), vec![Message::user("hi")]).await.unwrap();
        let events = drain(&mut stream).await;
        assert!(matches!(events.last(), Some(StreamEvent::Error(e))
            if e.kind == StreamErrorKind::Transport && e.message.contains("before its done event")), "{events:?}");
        assert!(!events.iter().any(|e| matches!(e, StreamEvent::Done { .. })));
    }

    #[tokio::test]
    async fn cancel_yields_the_cancel_confirm_done() {
        let mut slow = generation("Hello.", json!([]), 2);
        slow.stall = Some(Duration::from_secs(3));
        let (base, _) = serve(vec![serving(json!(["generate"])), slow]).await;
        let mut stream = client(&base).stream(opts(64), vec![Message::user("hi")]).await.unwrap();
        stream.cancel();
        let events = drain(&mut stream).await;
        assert!(
            matches!(events.as_slice(), [StreamEvent::Done { stop_reason: None, input_tokens: None, .. }]),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn service_errors_map_to_kernel_errors() {
        let bad = r#"{"error":{"type":"invalid_request","message":"noncharacter","param":"context.messages[1].content"}}"#;
        let (base, _) = serve(vec![serving(json!(["generate"])), http(400, bad)]).await;
        let err = client(&base).stream(opts(64), vec![Message::user("hi")]).await.err().unwrap();
        assert!(matches!(&err, LlmError::InvalidRequest(m) if m.contains("council backend 'nagaya'") && m.contains("noncharacter")), "{err:?}");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let err = client(&closed).stream(opts(64), vec![Message::user("hi")]).await.err().unwrap();
        assert!(matches!(err, LlmError::NetworkError(_)), "{err:?}");
    }

    #[tokio::test]
    async fn an_unknown_effort_is_refused_before_any_call() {
        let (base, seen) = serve(vec![]).await;
        let mut o = opts(64);
        o.effort = Some("max".into());
        let err = client(&base).stream(o, vec![Message::user("hi")]).await.err().unwrap();
        assert!(matches!(err, LlmError::InvalidRequest(_)), "{err:?}");
        assert!(seen.lock().unwrap().is_empty());
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
                turn_scoped: false,
            },
            Message::tool_results(vec![ContentBlock::ToolResult { tool_use_id: "a".into(), content: String::new(), is_error: false }]),
        ];
        let messages = build::messages(None, &history).unwrap();
        let sent = serde_json::to_string(&messages[1].tool_calls[0].input).unwrap();
        assert_eq!(sent, r#"{"path":"/tmp","all":true}"#);
    }

    /// A client whose calls take slots at a fresh endpoint for `base`.
    fn limited(base: &str, timeout: Duration) -> (Client, Arc<Endpoint>) {
        let endpoint = crate::llm::endpoint::Endpoints::default().for_url(base).unwrap();
        let c = Client::new("nagaya", base, timeout).unwrap().with_endpoint(endpoint.clone());
        (c, endpoint)
    }

    fn busy() -> kaijutsu_council::test_server::Reply {
        let mut r = http(429, json!({"error": {"type": "busy", "message": "16 requests in progress"}}).to_string());
        r.headers.push(("retry-after", "1".into()));
        r
    }

    /// A streamed turn holds its slot until the stream ends.
    ///
    /// Falsified by a stream that drops its slot when it opens, or keeps it
    /// after its last event.
    #[tokio::test]
    async fn a_turn_holds_its_slot_until_its_stream_ends() {
        let (base, _) = serve(vec![serving(json!(["generate"])), generation("Hello.", json!([]), 2)]).await;
        let (c, endpoint) = limited(&base, Duration::from_secs(5));
        let mut stream = c.stream(opts(64), vec![Message::user("hi")]).await.unwrap();
        assert_eq!(endpoint.status().in_flight, 1);
        drain(&mut stream).await;
        assert_eq!(endpoint.status().in_flight, 0);
    }

    /// The server refuses with 429 before any work starts
    /// (`docs/retries-and-ratelimits.md`), so the turn sends the same call again after
    /// `Retry-After`.
    ///
    /// Falsified by a 429 that fails the turn, or a resend that ignores the
    /// cooldown.
    #[tokio::test]
    async fn a_busy_answer_is_sent_again_after_its_retry_after() {
        let (base, seen) = serve(vec![serving(json!(["generate"])), busy(), generation("Hello.", json!([]), 2)]).await;
        let (c, endpoint) = limited(&base, Duration::from_secs(5));
        let started = std::time::Instant::now();
        let mut stream = c.stream(opts(64), vec![Message::user("hi")]).await.unwrap();
        assert!(started.elapsed() >= Duration::from_secs(1), "resent after {:?}", started.elapsed());
        let events = drain(&mut stream).await;
        assert!(matches!(events.last(), Some(StreamEvent::Done { .. })), "{events:?}");
        let paths: Vec<String> = seen.lock().unwrap().iter().map(|c| c.path.clone()).collect();
        assert_eq!(paths, ["/council/v2/identity", "/council/v2/generations", "/council/v2/generations"]);
        assert_eq!(endpoint.status().cooldown, None);
    }

    /// A 503 cools the endpoint and fails the turn; it is not sent again.
    #[tokio::test]
    async fn a_503_cools_the_endpoint_and_is_not_sent_again() {
        let (base, seen) = serve(vec![serving(json!(["generate"])), http(503, "{}")]).await;
        let (c, endpoint) = limited(&base, Duration::from_secs(5));
        let err = c.stream(opts(64), vec![Message::user("hi")]).await.err().unwrap();
        assert!(matches!(err, LlmError::ApiError(_)), "{err:?}");
        assert_eq!(seen.lock().unwrap().len(), 2);
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
                assert!(why.starts_with("council backend 'nagaya': no slot at http://127.0.0.1:"), "{why}");
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
        let mut slow = generation("Hello.", json!([]), 2);
        slow.delay = Duration::from_secs(4);
        let (base, _) = serve(vec![serving(json!(["generate"])), busy(), slow]).await;
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
        let events = generation_events("Hello.", json!([]), 2);
        let cut = &events[..events.find("event: done").unwrap()];
        let failed = format!("{cut}event: error\ndata: {}\n\n", r#"{"error":{"type":"unavailable","message":"spin"}}"#);
        let (base, _) = serve(vec![serving(json!(["generate"])), sse(failed)]).await;
        let (c, endpoint) = limited(&base, Duration::from_secs(5));
        let mut stream = c.stream(opts(64), vec![Message::user("hi")]).await.unwrap();
        let events = drain(&mut stream).await;
        assert!(matches!(events.last(), Some(StreamEvent::Error(e))
            if e.kind == StreamErrorKind::Overloaded && e.message.contains("spin")), "{events:?}");
        let (_, status) = endpoint.status().cooldown.expect("cooling down");
        assert_eq!(status, 503);
    }

    /// A stream that reaches its done event resets the doubling.
    ///
    /// Falsified by a stream whose end leaves a busy answer counted.
    #[tokio::test]
    async fn a_stream_that_ends_whole_resets_the_doubling() {
        let (base, _) = serve(vec![serving(json!(["generate"])), generation("Hello.", json!([]), 2)]).await;
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
