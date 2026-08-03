//! Hand-rolled Anthropic Claude provider.
//!
//! Submodules:
//! - [`types`] — Anthropic Messages API native request/response types.
//! - [`build`] — kaijutsu `Message` / `ContentBlock` → Anthropic shapes.
//! - [`sse`] — Server-Sent Events parser over `eventsource-stream`.
//! - [`stream`] — SSE → kaijutsu `StreamEvent` state machine.
//!
//! [`Client`] owns a `reqwest::Client` with the Anthropic auth headers
//! pinned at construction. `stream()` POSTs to `/v1/messages` with
//! `stream: true` and wraps the byte-stream response in [`Stream`].
//! `prompt()` does the non-streaming form, returning concatenated text.

pub mod build;
pub mod models_api;
pub mod sse;
pub mod stream;
pub mod types;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::sync::RwLock as AsyncRwLock;
use tokio_util::sync::CancellationToken;

use crate::llm::stream::{BuildOpts, StreamEvent};
use crate::llm::{LlmError, LlmResult, Message};

use self::models_api::{HttpModelCapabilitySource, ModelCapabilitySource};
use self::sse::{ClaudeSseEvent, decode_event};
use self::stream::StateMachine;
use self::types::{MessagesResponse, ResponseContent};

const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// Anthropic Claude client.
///
/// Owns a configured `reqwest::Client` with the Anthropic auth /
/// version headers baked into `default_headers` — every request the
/// client makes is properly authenticated by construction. The shared
/// `Client` lets reqwest pool TCP connections across multiple stream
/// calls within a session.
///
/// `model_window_cache` + `capability_source` back the live
/// `GET /v1/models/{id}` context-window lookup (see [`Self::context_window`])
/// — the configured `context_window` (backend_models) is checked first by the
/// caller (`LlmRegistry::context_window_for_live`); this is the fallback
/// when that's absent. `capability_source` is the test seam: production
/// code gets `HttpModelCapabilitySource` (a real network call), tests inject
/// a fake via [`Self::with_capability_source`] so `cargo test` never
/// touches the network.
#[derive(Clone, Debug)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    /// Per-model-id cache of resolved live context windows, scoped to this
    /// `Client` instance (one per registered Anthropic provider, i.e. one
    /// per kernel process — restart to pick up a model launched after boot,
    /// same convention as credential reload). Populated lazily on first
    /// live lookup. `Some`/`Ok(None)` results are cached for the process
    /// lifetime — a model's context window does not change under you, so
    /// there is no TTL to get wrong. A lookup that *errors* (network/auth
    /// failure) is deliberately NOT cached, so a transient outage heals
    /// itself on the next call instead of pinning "unknown" until restart.
    model_window_cache: Arc<AsyncRwLock<HashMap<String, Option<u64>>>>,
    capability_source: Arc<dyn ModelCapabilitySource>,
    /// Per-request HTTP timeout (`BackendConfig.request_timeout_secs`,
    /// resolved by `Provider::from_backend`). Applied via
    /// `RequestBuilder::timeout` at each `.send()` call — covers the whole
    /// request/response including a streamed body read, so it's a backstop
    /// for a connection that never responds at all, layered *below*
    /// kaijutsu-server's own total-deadline + idle-timeout
    /// (`llm_stream.rs`), which govern a stream that opened but stalled or
    /// ran long. `None` only in tests that construct a bare `Client::new`
    /// directly instead of going through `Provider::from_backend`.
    request_timeout: Option<std::time::Duration>,
}

impl Client {
    /// Construct a client from an API key.
    ///
    /// Panics only if `reqwest::Client::builder()` fails — which on a
    /// healthy host means a TLS backend init failure (no system roots,
    /// for instance). That's a startup-time misconfiguration; we
    /// surface it via `LlmError::Unavailable` so the caller can fall
    /// back to a different provider gracefully.
    pub fn new(api_key: impl Into<String>) -> Self {
        let api_key = api_key.into();
        let mut headers = reqwest::header::HeaderMap::new();
        // HeaderValue::from_str validates; an API key with non-ASCII
        // would fail. We accept the .expect — a malformed key is
        // operator-side misconfiguration that should crash loudly.
        headers.insert(
            "x-api-key",
            reqwest::header::HeaderValue::from_str(&api_key)
                .expect("Anthropic API key must be ASCII"),
        );
        headers.insert(
            "anthropic-version",
            reqwest::header::HeaderValue::from_static(ANTHROPIC_API_VERSION),
        );
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .expect("reqwest::Client::builder must succeed on healthy host");
        let base_url = ANTHROPIC_DEFAULT_BASE_URL.to_string();
        let capability_source: Arc<dyn ModelCapabilitySource> = Arc::new(
            HttpModelCapabilitySource::new(http.clone(), base_url.clone()),
        );
        Self {
            http,
            base_url,
            model_window_cache: Arc::new(AsyncRwLock::new(HashMap::new())),
            capability_source,
            request_timeout: None,
        }
    }

    /// Set the per-request HTTP timeout (`Provider::from_backend` calls this
    /// with `BackendConfig.request_timeout_secs`, resolved against a default
    /// when unset — see `resolve_request_timeout` in `llm/mod.rs`).
    pub fn with_request_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }

    /// Override the API base URL (for proxies or local mocks).
    ///
    /// Rebuilds `capability_source` to point at the new base URL too, so a
    /// proxy/mock override applies uniformly to `/v1/messages` and
    /// `/v1/models/{id}` alike. Call [`Self::with_capability_source`]
    /// *after* this in a test builder chain if you need to override the
    /// rebuilt source instead.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self.capability_source = Arc::new(HttpModelCapabilitySource::new(
            self.http.clone(),
            self.base_url.clone(),
        ));
        self
    }

    /// Test seam: inject a fake [`ModelCapabilitySource`] so
    /// [`Self::context_window`] never touches the network. Production
    /// callers never need this — `new()`/`with_base_url()` always wire the
    /// real HTTP-backed source.
    #[cfg(any(test, feature = "test-mock"))]
    pub fn with_capability_source(mut self, source: Arc<dyn ModelCapabilitySource>) -> Self {
        self.capability_source = source;
        self
    }

    /// Live context-window lookup for `model`, cached for the life of this
    /// `Client`. Checked by [`crate::llm::LlmRegistry::context_window_for_live`]
    /// only after the config path (the backend's configured `context_window`) came up
    /// empty — config is always the override, this is the fallback.
    ///
    /// A cache hit never touches the network. On a miss, awaits
    /// `capability_source.max_input_tokens(model)` (the real implementation
    /// hits `GET /v1/models/{id}`, reading the wire's `max_input_tokens`
    /// field — there is no `context_window` field on the API; that name is
    /// ours, and `max_tokens` is a different number, the output cap).
    ///
    /// Never fabricates: an `Ok(None)` (model unknown to the API, e.g. a
    /// 404) resolves to `None` and IS cached, same as a genuine window —
    /// repeat lookups for a bad id shouldn't hammer the network either. An
    /// `Err` (network/auth/parse failure) also resolves to `None` but is
    /// NOT cached (see the field doc), and is bubbled at `warn` — `None` is
    /// a legitimate, documented outcome here, so this is an informational
    /// log, not a silently swallowed failure.
    pub async fn context_window(&self, model: &str) -> Option<u64> {
        {
            let cache = self.model_window_cache.read().await;
            if let Some(cached) = cache.get(model) {
                return *cached;
            }
        }
        match self.capability_source.max_input_tokens(model).await {
            Ok(window) => {
                self.model_window_cache
                    .write()
                    .await
                    .insert(model.to_string(), window);
                window
            }
            Err(e) => {
                tracing::warn!(
                    model,
                    error = %e,
                    "live context-window lookup failed; resolving as unknown \
                     (not cached — will retry on the next call)"
                );
                None
            }
        }
    }

    /// Available Claude model IDs surfaced by this provider.
    pub fn available_models(&self) -> Vec<&'static str> {
        vec![
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-sonnet-4-6",
            "claude-haiku-4-5-20251001",
        ]
    }

    /// One-shot prompt with optional system preamble.
    ///
    /// Uses the non-streaming `/v1/messages` form. Concatenates all
    /// `Text` content blocks in the response. Other content blocks
    /// (thinking, tool_use) are dropped with a warning — `prompt()` is
    /// the distillation entry point, not the agentic loop.
    pub async fn prompt(
        &self,
        model: &str,
        system: Option<&str>,
        prompt: &str,
    ) -> LlmResult<String> {
        let messages = vec![Message::user(prompt)];
        let mut opts = BuildOpts::new(model).with_max_tokens(4096);
        if let Some(sys) = system {
            opts = opts.with_system(sys);
        }
        let body = build::build_request(&opts, &messages, false);

        let mut request = self
            .http
            .post(format!("{}/v1/messages", self.base_url))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body);
        if let Some(timeout) = self.request_timeout {
            request = request.timeout(timeout);
        }
        let response = request.send().await.map_err(http_error)?;

        let response = error_for_status(response).await?;

        let parsed: MessagesResponse = response
            .json()
            .await
            .map_err(|e| LlmError::ApiError(format!("response JSON parse: {e}")))?;

        let mut text = String::new();
        for content in &parsed.content {
            match content {
                ResponseContent::Text { text: t } => text.push_str(t),
                ResponseContent::Thinking { .. } => {
                    tracing::debug!(
                        "claude::prompt: dropping thinking block from non-streaming response"
                    );
                }
                ResponseContent::ToolUse { name, .. } => {
                    tracing::warn!(
                        tool = %name,
                        "claude::prompt: dropping unexpected tool_use block (distillation should not invoke tools)"
                    );
                }
            }
        }
        Ok(text)
    }

    /// Start a streaming completion.
    ///
    /// POSTs `MessagesRequest { stream: true, … }` and wraps the
    /// `Content-Type: text/event-stream` response in a [`Stream`].
    /// Errors before the response body opens (auth, rate limit,
    /// malformed request) surface here; mid-stream errors come out as
    /// [`StreamEvent::Error`].
    pub async fn stream(
        &self,
        opts: BuildOpts,
        messages: Vec<Message>,
    ) -> LlmResult<Stream> {
        let mut body = build::build_request(&opts, &messages, true);
        // Provider-tailored knob (the reason this client is bespoke):
        // resolves opts's thinking_style/thinking_budget/effort cascade
        // (model-gated adaptive default when unset) and strips
        // temperature/top_p when thinking ends up on — see
        // `build::apply_thinking`.
        build::apply_thinking(&mut body, &opts)?;

        let mut request = self
            .http
            .post(format!("{}/v1/messages", self.base_url))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);
        if let Some(timeout) = self.request_timeout {
            request = request.timeout(timeout);
        }
        let response = request.send().await.map_err(http_error)?;

        let response = error_for_status(response).await?;

        Ok(Stream::from_response(response))
    }
}

/// Map an Anthropic 4xx/5xx response body into [`LlmError`].
///
/// Anthropic returns JSON `{"type": "error", "error": {"type":
/// "...", "message": "..."}}` for known failure modes. We decode
/// the inner error to map onto kaijutsu-typed variants. Unparseable
/// bodies fall back to raw status text — never silently swallow.
///
/// Free function (not a `Client` method) so [`models_api::HttpModelCapabilitySource`]
/// can reuse the same mapping for `GET /v1/models/{id}` without needing a
/// `Client` reference — it's pure over the response.
pub(crate) async fn error_for_status(response: reqwest::Response) -> LlmResult<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response
        .text()
        .await
        .unwrap_or_else(|e| format!("(failed to read body: {e})"));
    // Parse the error JSON if we can; otherwise pass through.
    let detail = serde_json::from_str::<sse::SseError>(&body)
        .map(|e| format!("{}: {}", e.error.kind, e.error.message))
        .unwrap_or(body);
    let mapped = match status.as_u16() {
        401 | 403 => LlmError::AuthError(detail),
        429 => LlmError::RateLimited(detail),
        400..=499 => LlmError::InvalidRequest(detail),
        500..=599 => LlmError::ApiError(detail),
        _ => LlmError::ApiError(format!("unexpected HTTP {status}: {detail}")),
    };
    Err(mapped)
}

pub(crate) fn http_error(e: reqwest::Error) -> LlmError {
    if e.is_timeout() {
        LlmError::NetworkError(format!("timeout: {e}"))
    } else if e.is_connect() {
        LlmError::NetworkError(format!("connect: {e}"))
    } else {
        LlmError::NetworkError(format!("{e}"))
    }
}

/// Streaming response from Claude.
///
/// Wraps the reqwest byte-stream in an `eventsource-stream` parser,
/// decodes each SSE event into a [`ClaudeSseEvent`], and drives the
/// [`StateMachine`] to produce kaijutsu [`StreamEvent`]s. Multiple
/// kaijutsu events per SSE event are buffered in `pending`.
///
/// Cancellation: [`Self::cancel`] fires a [`CancellationToken`] that
/// the next [`Self::next_event`] poll observes via `tokio::select!`,
/// dropping the inner stream and emitting a `Done { stop_reason: None }`
/// — matching the rig-era contract the server's cancel-confirm path
/// expects.
pub struct Stream {
    inner: Option<eventsource_stream::EventStream<BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>>>,
    state: StateMachine,
    pending: VecDeque<StreamEvent>,
    cancel: CancellationToken,
    finished: bool,
}

impl Stream {
    fn from_response(response: reqwest::Response) -> Self {
        use eventsource_stream::Eventsource;
        let bytes: BoxStream<'static, Result<bytes::Bytes, reqwest::Error>> =
            response.bytes_stream().boxed();
        Self {
            inner: Some(bytes.eventsource()),
            state: StateMachine::new(),
            pending: VecDeque::new(),
            cancel: CancellationToken::new(),
            finished: false,
        }
    }

    /// Test constructor: drive a fixed byte payload through the same
    /// pipeline as the live wire path. Useful for end-to-end tests
    /// that exercise both the SSE parser and the state machine
    /// together with explicit chunk boundaries.
    #[cfg(test)]
    pub(crate) fn for_test_bytes(payload: &'static str) -> Self {
        use eventsource_stream::Eventsource;
        let bytes: BoxStream<'static, Result<bytes::Bytes, reqwest::Error>> = futures::stream::iter(
            std::iter::once(Ok::<_, std::convert::Infallible>(bytes::Bytes::from(payload))),
        )
        // Map Infallible → reqwest::Error via never-occurring conversion.
        .map(|r| r.map_err(|_: std::convert::Infallible| unreachable!()))
        .boxed();
        Self {
            inner: Some(bytes.eventsource()),
            state: StateMachine::new(),
            pending: VecDeque::new(),
            cancel: CancellationToken::new(),
            finished: false,
        }
    }

    /// Poll for the next event. Returns `None` once the stream is
    /// exhausted (after a `Done` or `Error` event, or a clean close
    /// with no `message_stop` — which we surface via the state
    /// machine's already-emitted events).
    pub async fn next_event(&mut self) -> Option<StreamEvent> {
        loop {
            if let Some(ev) = self.pending.pop_front() {
                return Some(ev);
            }
            if self.finished {
                return None;
            }
            let Some(inner) = self.inner.as_mut() else {
                self.finished = true;
                return None;
            };

            // Bias the select toward cancel so a hard-interrupt is
            // observed even when the upstream is mid-burst.
            let cancel = self.cancel.clone();
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    self.finished = true;
                    self.inner = None;
                    return Some(StreamEvent::Done {
                        stop_reason: None,
                        input_tokens: None,
                        output_tokens: None,
                        extra: None,
                    });
                }
                item = inner.next() => {
                    match item {
                        Some(Ok(event)) => match decode_event(&event) {
                            Ok(typed) => {
                                if matches!(&typed, ClaudeSseEvent::MessageStop) {
                                    self.finished = true;
                                }
                                let emitted = self.state.step(typed);
                                for ev in emitted {
                                    self.pending.push_back(ev);
                                }
                            }
                            Err(e) => {
                                self.finished = true;
                                return Some(StreamEvent::Error(format!("SSE decode: {e}")));
                            }
                        },
                        Some(Err(e)) => {
                            self.finished = true;
                            return Some(StreamEvent::Error(format!("SSE transport: {e}")));
                        }
                        None => {
                            // Source stream closed without an explicit
                            // `message_stop` event. Don't synthesize a
                            // Done — that would mask wire-shape bugs.
                            self.finished = true;
                            self.inner = None;
                            return None;
                        }
                    }
                }
            }
        }
    }

    /// Abort the underlying HTTP stream.
    ///
    /// Idempotent. Multiple callers can share a `Stream` reference via
    /// `&Stream`; the [`CancellationToken`] handles concurrent cancel
    /// safely.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

// ============================================================================
// End-to-end tests: drive bytes through Stream::next_event() to verify
// the wire layer integrates correctly with the SSE parser and state
// machine. Network calls live in integration tests gated behind the
// network env var, not here.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_COMPLETION: &str = "\
event: message_start
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-haiku-4-5\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}

event: content_block_stop
data: {\"type\":\"content_block_stop\",\"index\":0}

event: message_delta
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":2}}

event: message_stop
data: {\"type\":\"message_stop\"}

";

    #[tokio::test]
    async fn stream_drains_bytes_through_state_machine_to_done() {
        let mut s = Stream::for_test_bytes(SIMPLE_COMPLETION);
        let mut events = Vec::new();
        while let Some(ev) = s.next_event().await {
            events.push(ev);
        }
        // After Done, further polls must return None.
        assert!(s.next_event().await.is_none());
        assert_eq!(events.len(), 4, "TextStart, TextDelta, TextEnd, Done");
        assert!(matches!(events[0], StreamEvent::TextStart));
        match &events[1] {
            StreamEvent::TextDelta(t) => assert_eq!(t, "Hi"),
            other => panic!("expected TextDelta, got {other:?}"),
        }
        assert!(matches!(events[2], StreamEvent::TextEnd));
        match &events[3] {
            StreamEvent::Done {
                stop_reason,
                input_tokens,
                output_tokens,
                ..
            } => {
                assert_eq!(stop_reason.as_deref(), Some("end_turn"));
                assert_eq!(*input_tokens, Some(10));
                assert_eq!(*output_tokens, Some(2));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_emits_done_with_none_stop_reason() {
        // Hard-interrupt confirmation contract: cancelling the stream
        // yields a Done event whose stop_reason is None — the server
        // distinguishes this from a natural end_turn close.
        let mut s = Stream::for_test_bytes(SIMPLE_COMPLETION);
        s.cancel();
        let ev = s.next_event().await.expect("must emit cancel-Done");
        match ev {
            StreamEvent::Done {
                stop_reason,
                input_tokens,
                output_tokens,
                ..
            } => {
                assert!(stop_reason.is_none(), "cancel Done.stop_reason must be None");
                assert!(input_tokens.is_none());
                assert!(output_tokens.is_none());
            }
            other => panic!("expected Done, got {other:?}"),
        }
        assert!(
            s.next_event().await.is_none(),
            "stream is finished after cancel-Done"
        );
    }

    /// Live API smoke test — exercises the real wire path against
    /// Anthropic. Gated behind `ANTHROPIC_API_KEY` so CI / casual
    /// `cargo test` runs skip it cleanly. Cost: ~30 input tokens, ~20
    /// output tokens against Haiku — negligible.
    ///
    /// Run with `--nocapture` to see what Claude said:
    ///
    /// ```sh
    /// ANTHROPIC_API_KEY=$(< ~/.anthropic-key.txt) \
    ///   cargo test -p kaijutsu-kernel --lib claude_live_smoke \
    ///   -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "requires ANTHROPIC_API_KEY; run with `cargo test --ignored claude_live`"]
    async fn claude_live_smoke_streams_real_response() {
        let api_key = match std::env::var("ANTHROPIC_API_KEY") {
            Ok(k) if !k.is_empty() => k,
            _ => return, // belt-and-suspenders alongside #[ignore]
        };
        let client = Client::new(api_key);
        let opts = BuildOpts::new("claude-haiku-4-5-20251001")
            .with_max_tokens(128)
            .with_system("You are friendly. Respond briefly.");
        let mut stream = client
            .stream(opts, vec![Message::user("hi there")])
            .await
            .expect("stream open must succeed with valid key");
        let mut text = String::new();
        let mut saw_done = false;
        let mut stop_reason = String::new();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        while let Some(ev) = stream.next_event().await {
            match ev {
                StreamEvent::TextStart | StreamEvent::TextEnd => {}
                StreamEvent::TextDelta(t) => text.push_str(&t),
                StreamEvent::Done {
                    stop_reason: sr,
                    input_tokens: it,
                    output_tokens: ot,
                    ..
                } => {
                    saw_done = true;
                    if let Some(s) = sr {
                        stop_reason = s;
                    }
                    if let Some(n) = it {
                        input_tokens = n;
                    }
                    if let Some(n) = ot {
                        output_tokens = n;
                    }
                }
                StreamEvent::Error(e) => panic!("live stream error: {e}"),
                _ => {}
            }
        }
        // Visible only with --nocapture. Captures the actual response so
        // a human running the test can confirm Claude really replied.
        println!("\n--- claude said ---\n{text}\n--- meta ---");
        println!("stop_reason: {stop_reason}");
        println!("tokens: in={input_tokens} out={output_tokens}\n");

        assert!(!text.is_empty(), "live response must include some text");
        assert!(saw_done, "live response must terminate with Done");
        assert_eq!(stop_reason, "end_turn", "expected natural turn end");
    }

    /// Live thinking smoke test — confirms adaptive thinking with
    /// `display: "summarized"` actually yields non-empty ThinkingDelta
    /// text on a model that supports it. Uses a prompt that reliably
    /// triggers thinking under the adaptive policy.
    ///
    /// ```sh
    /// ANTHROPIC_API_KEY=$(< ~/.anthropic-key.txt) \
    ///   cargo test -p kaijutsu-kernel --lib claude_live_thinking \
    ///   -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "requires ANTHROPIC_API_KEY; run with `cargo test --ignored claude_live`"]
    async fn claude_live_thinking_streams_summarized_thinking_text() {
        let api_key = match std::env::var("ANTHROPIC_API_KEY") {
            Ok(k) if !k.is_empty() => k,
            _ => return,
        };
        let client = Client::new(api_key);
        let opts = BuildOpts::new("claude-opus-4-6").with_max_tokens(2048);
        let mut stream = client
            .stream(
                opts,
                vec![Message::user(
                    "A farmer has 17 sheep. All but 9 run away, then a third of the \
                     remainder are sold, and he buys back twice what he sold. How many \
                     sheep now? Reason it through carefully before answering.",
                )],
            )
            .await
            .expect("stream open must succeed with valid key");
        let mut thinking_text = String::new();
        let mut saw_thinking_end_signature = false;
        let mut text = String::new();
        while let Some(ev) = stream.next_event().await {
            match ev {
                StreamEvent::ThinkingDelta(t) => thinking_text.push_str(&t),
                StreamEvent::ThinkingEnd { signature } => {
                    saw_thinking_end_signature |= signature.is_some();
                }
                StreamEvent::TextDelta(t) => text.push_str(&t),
                StreamEvent::Error(e) => panic!("live stream error: {e}"),
                _ => {}
            }
        }
        println!("\n--- thinking ---\n{thinking_text}\n--- answer ---\n{text}\n");
        assert!(!text.is_empty(), "must produce answer text");
        assert!(
            !thinking_text.trim().is_empty(),
            "adaptive + summarized must yield non-empty thinking text \
             (empty means display defaulted to omitted — the bug this fixes)"
        );
        assert!(
            saw_thinking_end_signature,
            "thinking block must carry a signature for multi-turn replay"
        );
    }

    // ── Client::context_window: cache + failure-degradation behavior ─────
    //
    // `LlmRegistry::context_window_for_live`'s own tests (`llm/mod.rs`)
    // cover config-override precedence and the None/unknown-model paths at
    // the registry level. These tests exercise `Client::context_window`
    // directly so a cache hit skipping the seam entirely can be asserted
    // via `FakeModelCapabilitySource::call_count` — the registry's
    // `Arc<dyn ModelCapabilitySource>` type-erasure makes that awkward to
    // reach from the registry-level tests.

    mod context_window_cache_tests {
        use super::*;
        use crate::llm::claude::models_api::test_support::FakeModelCapabilitySource;
        use std::sync::Arc;

        #[tokio::test]
        async fn second_call_for_same_model_hits_cache_not_the_seam() {
            let fake = FakeModelCapabilitySource::always_none()
                .with_response("claude-opus-4-8", Ok(Some(1_000_000)));
            let fake = Arc::new(fake);
            let client = Client::new("fake-key").with_capability_source(fake.clone());

            let first = client.context_window("claude-opus-4-8").await;
            assert_eq!(first, Some(1_000_000));
            assert_eq!(fake.call_count(), 1);

            let second = client.context_window("claude-opus-4-8").await;
            assert_eq!(second, Some(1_000_000), "cached value must be stable");
            assert_eq!(
                fake.call_count(),
                1,
                "a cache hit must not call the capability source a second time"
            );
        }

        #[tokio::test]
        async fn unknown_model_result_is_cached_too() {
            // Ok(None) — "the live API doesn't know this model either" — is
            // a real, cacheable answer, not a failure. Repeat lookups for a
            // bad id shouldn't hammer the network.
            let fake = Arc::new(FakeModelCapabilitySource::always_none());
            let client = Client::new("fake-key").with_capability_source(fake.clone());

            assert_eq!(client.context_window("bogus-model").await, None);
            assert_eq!(fake.call_count(), 1);
            assert_eq!(client.context_window("bogus-model").await, None);
            assert_eq!(fake.call_count(), 1, "Ok(None) must be cached, same as Ok(Some(_))");
        }

        #[tokio::test]
        async fn failed_lookup_degrades_to_none_and_is_not_cached() {
            let fake = Arc::new(FakeModelCapabilitySource::always_none().with_response(
                "claude-opus-4-8",
                Err(LlmError::NetworkError("connect refused".into())),
            ));
            let client = Client::new("fake-key").with_capability_source(fake.clone());

            assert_eq!(
                client.context_window("claude-opus-4-8").await,
                None,
                "a failed live lookup must degrade to None, never panic, never fabricate"
            );
            assert_eq!(fake.call_count(), 1);

            // Retried on the next call (not poisoned by the earlier failure) —
            // simulates the transient outage healing without a restart.
            assert_eq!(
                client.context_window("claude-opus-4-8").await,
                None,
                "still None because the fake is still scripted to fail"
            );
            assert_eq!(
                fake.call_count(),
                2,
                "an Err result must NOT be cached — each call retries the seam"
            );
        }

        #[tokio::test]
        async fn distinct_models_cache_independently() {
            let fake = Arc::new(
                FakeModelCapabilitySource::always_none()
                    .with_response("claude-opus-4-8", Ok(Some(1_000_000)))
                    .with_response("claude-haiku-4-5-20251001", Ok(Some(200_000))),
            );
            let client = Client::new("fake-key").with_capability_source(fake.clone());

            assert_eq!(client.context_window("claude-opus-4-8").await, Some(1_000_000));
            assert_eq!(
                client.context_window("claude-haiku-4-5-20251001").await,
                Some(200_000)
            );
            assert_eq!(fake.call_count(), 2);

            // Both now cached independently.
            assert_eq!(client.context_window("claude-opus-4-8").await, Some(1_000_000));
            assert_eq!(
                client.context_window("claude-haiku-4-5-20251001").await,
                Some(200_000)
            );
            assert_eq!(fake.call_count(), 2, "both hits should come from cache");
        }
    }
}
