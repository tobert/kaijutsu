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
#[cfg(test)]
mod live;
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

use crate::llm::endpoint::{Endpoint, Slot};
use crate::llm::stream::{BuildOpts, StreamError, StreamErrorKind, StreamEvent};
use crate::llm::{DEFAULT_REQUEST_TIMEOUT_SECS, LlmError, LlmResult, Message, http_user_agent};

use self::models_api::{HttpModelCapabilitySource, ModelCapabilitySource};
use self::sse::{ClaudeSseEvent, decode_event};
use self::stream::StateMachine;
use self::types::{MessagesResponse, ResponseContent};

const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const DEEPSEEK_MESSAGES_BASE_URL: &str = "https://api.deepseek.com/anthropic";

/// Which server speaks the Messages wire. The wire shape is the same; the
/// dialects differ in what the server accepts, so each difference is one
/// method here and one branch where it applies.
///
/// - `Anthropic`: `api.anthropic.com` and compatible gateways.
/// - `DeepSeek`: `api.deepseek.com/anthropic`. It has no `GET /v1/models`
///   (404), takes `thinking` as `enabled` or `disabled` only, ignores
///   `cache_control`, and takes sampling parameters beside thinking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Anthropic,
    DeepSeek,
}

impl Dialect {
    /// The provider label on request dumps.
    fn provider_label(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::DeepSeek => "deepseek-messages",
        }
    }

    /// Whether `GET /v1/models/{id}` answers with `max_input_tokens`.
    fn has_models_api(self) -> bool {
        self == Self::Anthropic
    }
}

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
    /// The endpoint each `/v1/messages` request takes a slot from. `None`
    /// only for a client built directly rather than by
    /// `Provider::from_backend`.
    endpoint: Option<Arc<Endpoint>>,
    dialect: Dialect,
}

impl Client {
    /// Construct a client from an API key.
    ///
    /// A key that is not a valid header value (a control character, such as
    /// the newline inside a two-line key file) is an `AuthError` that does
    /// not repeat the key. Panics if `reqwest::Client::builder()` fails,
    /// which on a healthy host means a TLS backend init failure (no system
    /// roots, for instance).
    pub fn new(api_key: impl Into<String>) -> LlmResult<Self> {
        let api_key = api_key.into();
        let mut headers = reqwest::header::HeaderMap::new();
        let mut key = reqwest::header::HeaderValue::from_str(&api_key).map_err(|_| {
            LlmError::AuthError(
                "the Anthropic API key is not a valid HTTP header value (it holds a control \
                 character, such as a newline)"
                    .to_string(),
            )
        })?;
        key.set_sensitive(true);
        headers.insert("x-api-key", key);
        headers.insert(
            "anthropic-version",
            reqwest::header::HeaderValue::from_static(ANTHROPIC_API_VERSION),
        );
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .user_agent(http_user_agent())
            .build()
            .expect("reqwest::Client::builder must succeed on healthy host");
        let base_url = ANTHROPIC_DEFAULT_BASE_URL.to_string();
        let capability_source: Arc<dyn ModelCapabilitySource> = Arc::new(
            HttpModelCapabilitySource::new(http.clone(), base_url.clone()),
        );
        Ok(Self {
            http,
            base_url,
            model_window_cache: Arc::new(AsyncRwLock::new(HashMap::new())),
            capability_source,
            request_timeout: None,
            endpoint: None,
            dialect: Dialect::Anthropic,
        })
    }

    /// A client for DeepSeek's Messages endpoint,
    /// `https://api.deepseek.com/anthropic`. Auth is the same `x-api-key`
    /// header; `anthropic-version` is sent and ignored.
    pub fn deepseek(api_key: impl Into<String>) -> LlmResult<Self> {
        let mut client = Self::new(api_key)?.with_base_url(DEEPSEEK_MESSAGES_BASE_URL);
        client.dialect = Dialect::DeepSeek;
        Ok(client)
    }

    /// The dialect this client speaks.
    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// Take a slot from `endpoint` before each `/v1/messages` request.
    pub fn with_endpoint(mut self, endpoint: Arc<Endpoint>) -> Self {
        self.endpoint = Some(endpoint);
        self
    }

    /// The endpoint this client takes slots from.
    pub fn endpoint(&self) -> Option<&Arc<Endpoint>> {
        self.endpoint.as_ref()
    }

    /// How long a request waits for a slot: its own request timeout.
    fn slot_wait(&self) -> std::time::Duration {
        self.request_timeout
            .unwrap_or(std::time::Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS))
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
    /// A cache hit never touches the network. On a miss, takes a slot at
    /// the client's endpoint, as every outbound request does, and awaits
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
        if !self.dialect.has_models_api() {
            return None;
        }
        {
            let cache = self.model_window_cache.read().await;
            if let Some(cached) = cache.get(model) {
                return *cached;
            }
        }
        let slot = match crate::llm::endpoint::take_for_request(self.endpoint.as_ref(), self.slot_wait()).await {
            Ok(slot) => slot,
            Err(e) => {
                tracing::warn!(model, error = %e, "live context-window lookup got no slot; resolving as unknown (not cached)");
                return None;
            }
        };
        let looked_up = self.capability_source.max_input_tokens(model).await;
        drop(slot);
        match looked_up {
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

    /// Model IDs surfaced by this provider.
    pub fn available_models(&self) -> Vec<&'static str> {
        match self.dialect {
            Dialect::Anthropic => vec![
                "claude-opus-5-5",
                "claude-sonnet-5-5",
                "claude-haiku-5-5",
                "claude-opus-4-8",
                "claude-opus-4-7",
                "claude-sonnet-4-6",
                "claude-haiku-4-5-20251001",
            ],
            Dialect::DeepSeek => vec!["deepseek-flash", "deepseek-v4-pro"],
        }
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
        let mut body = build::build_request(&opts, &messages, false, self.dialect)?;
        if self.dialect == Dialect::DeepSeek {
            // The server thinks by default, and thinking tokens count against
            // `max_tokens`; a distillation wants the answer text.
            opts.thinking_style = Some("off".into());
            build::apply_thinking(&mut body, &opts, self.dialect)?;
        }

        let slot = crate::llm::endpoint::take_for_request(self.endpoint.as_ref(), self.slot_wait()).await?;
        let mut request = self
            .http
            .post(format!("{}/v1/messages", self.base_url))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body);
        if let Some(timeout) = self.request_timeout {
            request = request.timeout(timeout);
        }
        crate::llm::request_dump::record(
            None, self.dialect.provider_label(), model, &self.base_url, system, &messages, &body,
        );
        let response = request.send().await.map_err(http_error)?;
        observe(slot.as_ref(), &response, false);

        let response = error_for_status(response).await?;

        let parsed: MessagesResponse = response
            .json()
            .await
            .map_err(|e| LlmError::ApiError(format!("response JSON parse: {e}")))?;

        // A reply that stopped before it finished is an error, not partial text.
        stream::check_stop_reason(parsed.stop_reason.as_deref())?;

        let mut text = String::new();
        for content in &parsed.content {
            match content {
                ResponseContent::Text { text: t } => text.push_str(t),
                ResponseContent::Thinking { .. } | ResponseContent::RedactedThinking { .. } => {
                    tracing::debug!(
                        "claude::prompt: dropping thinking block from non-streaming response"
                    );
                }
                ResponseContent::Other => {
                    tracing::debug!("claude::prompt: dropping a non-text block from the response");
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
        let mut body = build::build_request(&opts, &messages, true, self.dialect)?;
        // Provider-tailored knob (the reason this client is bespoke):
        // resolves opts's thinking_style/thinking_budget/effort cascade
        // (model-gated adaptive default when unset) and strips
        // temperature/top_p when thinking ends up on — see
        // `build::apply_thinking`.
        build::apply_thinking(&mut body, &opts, self.dialect)?;

        let slot = crate::llm::endpoint::take_for_request(self.endpoint.as_ref(), self.slot_wait()).await?;
        let mut request = self
            .http
            .post(format!("{}/v1/messages", self.base_url))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);
        if let Some(timeout) = self.request_timeout {
            request = request.timeout(timeout);
        }
        crate::llm::request_dump::record(
            opts.trace.as_deref(), self.dialect.provider_label(), &opts.model, &self.base_url,
            opts.system.as_deref(), &messages, &body,
        );
        let response = request.send().await.map_err(http_error)?;
        observe(slot.as_ref(), &response, true);

        let response = error_for_status(response).await?;

        Ok(Stream::from_response(response, slot))
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
        .map(|e| match e.request_id {
            Some(id) => format!("{}: {} (request_id: {id})", e.error.kind, e.error.message),
            None => format!("{}: {}", e.error.kind, e.error.message),
        })
        .unwrap_or(body);
    Err(map_status(status.as_u16(), detail))
}

/// Map a failing HTTP status onto [`LlmError`]. 529 is Anthropic's
/// `overloaded_error` and 504 its `timeout_error`; both are transient server
/// failures. A 400 that says the prompt is too long is a full context, which
/// no retry fixes.
fn map_status(status: u16, detail: String) -> LlmError {
    match status {
        401 | 403 => LlmError::AuthError(detail),
        402 => LlmError::InsufficientBalance(detail),
        429 => LlmError::RateLimited(detail),
        400 if detail.contains("prompt is too long") => LlmError::ContextFull(detail),
        400..=499 => LlmError::InvalidRequest(detail),
        500..=599 => LlmError::ApiError(detail),
        _ => LlmError::ApiError(format!("unexpected HTTP {status}: {detail}")),
    }
}

/// Tells `slot`'s endpoint how it answered `response`. A 529 is told as a
/// 503 so the limiter cools down on it. `opening` is a stream open: a 2xx
/// there waits for the stream's end event before it counts as an answer.
fn observe(slot: Option<&Slot>, response: &reqwest::Response, opening: bool) {
    let status = response.status();
    if status.as_u16() == 529 {
        if let Some(slot) = slot {
            slot.answered_with(503, crate::llm::endpoint::retry_after(response.headers()));
        }
    } else if opening {
        crate::llm::endpoint::observe_open(slot, response);
    } else {
        crate::llm::endpoint::observe(slot, response);
    }
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
    /// Released once the stream finishes. `message_stop` tells it the reply
    /// completed.
    slot: Option<Slot>,
}

impl Stream {
    fn from_response(response: reqwest::Response, slot: Option<Slot>) -> Self {
        use eventsource_stream::Eventsource;
        let bytes: BoxStream<'static, Result<bytes::Bytes, reqwest::Error>> =
            response.bytes_stream().boxed();
        Self {
            inner: Some(bytes.eventsource()),
            state: StateMachine::new(),
            pending: VecDeque::new(),
            cancel: CancellationToken::new(),
            finished: false,
            slot,
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
            slot: None,
        }
    }

    /// Poll for the next event. Returns `None` once the stream is
    /// exhausted (after a `Done` or `Error` event, or a clean close
    /// with no `message_stop`).
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
                                let ends = matches!(&typed, ClaudeSseEvent::MessageStop);
                                let busy = match &typed {
                                    ClaudeSseEvent::Error(e) => e.error.class().busy_status(),
                                    _ => None,
                                };
                                let emitted = self.state.step(typed);
                                // Any error ends the stream, and no completed
                                // answer follows it. An error that means "too
                                // much load" cools the endpoint as the same
                                // answer on the HTTP status would.
                                if self.state.failed() {
                                    self.finished = true;
                                    if let (Some(slot), Some(status)) = (&self.slot, busy) {
                                        slot.answered_with(status, None);
                                    }
                                } else if ends {
                                    self.finished = true;
                                    if let Some(slot) = &self.slot {
                                        slot.completed();
                                    }
                                }
                                for ev in emitted {
                                    self.pending.push_back(ev);
                                }
                            }
                            Err(e) => {
                                self.finished = true;
                                return Some(StreamEvent::Error(StreamError::new(
                                    StreamErrorKind::Protocol,
                                    format!("SSE decode: {e}"),
                                )));
                            }
                        },
                        Some(Err(e)) => {
                            self.finished = true;
                            return Some(StreamEvent::Error(StreamError::new(
                                StreamErrorKind::Transport,
                                format!("SSE transport: {e}"),
                            )));
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

    /// The UA rides the real wire path: reqwest applies client defaults at
    /// `execute()`, so a built request isn't proof — a captured request is.
    /// A loopback listener reads the raw request head the server receives,
    /// then answers a minimal `/v1/messages` completion so `prompt()`
    /// completes. Mirrors `openai::tests::outbound_requests_carry_the_kaijutsu_user_agent`.
    #[tokio::test]
    async fn outbound_requests_carry_the_kaijutsu_user_agent() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind");
        let port = listener.local_addr().expect("local addr").port();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            use tokio::io::AsyncReadExt;
            let mut head = Vec::new();
            loop {
                let mut buf = [0u8; 512];
                let n = sock.read(&mut buf).await.expect("read request");
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&buf[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let body = r#"{"id":"msg_01","model":"claude-haiku-4-5","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            use tokio::io::AsyncWriteExt;
            let _ = sock.write_all(resp.as_bytes()).await;
            String::from_utf8_lossy(&head).into_owned()
        });

        let text = Client::new("fake-key").unwrap()
            .with_base_url(format!("http://127.0.0.1:{port}"))
            .prompt("claude-haiku-4-5", None, "hi")
            .await
            .expect("prompt round-trip against loopback");
        assert_eq!(text, "ok", "response must parse through the real path");

        let head = server.await.expect("server task");
        let ua = head
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("user-agent:"))
            .unwrap_or_else(|| panic!("request head carried no User-Agent:\n{head}"));
        let (name, value) = ua.split_once(':').expect("matched on the colon");
        assert!(name.eq_ignore_ascii_case("user-agent"), "header name: {name}");
        assert_eq!(
            value.trim(),
            http_user_agent(),
            "UA must be exactly the kaijutsu identity, not reqwest/none"
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
            let client = Client::new("fake-key").unwrap().with_capability_source(fake.clone());

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
            let client = Client::new("fake-key").unwrap().with_capability_source(fake.clone());

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
            let client = Client::new("fake-key").unwrap().with_capability_source(fake.clone());

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
            let client = Client::new("fake-key").unwrap().with_capability_source(fake.clone());

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

    /// A 503 cools the endpoint for the first backoff step, and a later
    /// request waits it out.
    #[tokio::test]
    async fn a_503_cools_the_endpoint() {
        use kaijutsu_mk::test_server::{reply, serve};
        let (base, _) = serve(vec![reply(503, r#"{"type": "error", "error": {"type": "overloaded_error", "message": "busy"}}"#)]).await;
        let endpoint = crate::llm::endpoint::Endpoints::default().for_url(&base).unwrap();
        let client = Client::new("k").unwrap().with_base_url(&base).with_endpoint(endpoint.clone());
        let err = client.prompt("m", None, "hi").await.unwrap_err();
        assert!(matches!(err, LlmError::ApiError(_)), "{err:?}");
        let (left, status) = endpoint.status().cooldown.expect("cooling down");
        assert_eq!(status, 503);
        assert!(left > std::time::Duration::from_secs(4), "{left:?}");
        assert_eq!(endpoint.status().in_flight, 0);
    }

    /// The Anthropic stream path dumps the body the server received.
    #[tokio::test]
    async fn a_stream_dumps_the_body_the_server_received() {
        use crate::llm::request_dump::{RequestTrace, wait_for_files};
        use kaijutsu_mk::test_server::{reply, serve};
        use kaijutsu_types::{ContextId, TurnId};
        let (base, seen) = serve(vec![reply(500, r#"{"type": "error", "error": {"type": "api_error", "message": "x"}}"#)]).await;
        let tmp = tempfile::tempdir().unwrap();
        let trace = RequestTrace::new(ContextId::new(), TurnId::new(), 1, Some(tmp.path().to_path_buf()));
        let opts = BuildOpts::new("claude-sonnet-4-6").with_system("sys").with_trace(trace);
        let _ = Client::new("k").unwrap().with_base_url(&base).stream(opts, vec![Message::user("let's do a thing")]).await;
        let files = wait_for_files(tmp.path(), 1).await;
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
        let wire: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[0].body).unwrap();
        assert_eq!(v["body"], wire);
        assert_eq!(v["header"]["provider"], "anthropic");
    }

    /// A 429 cools the endpoint for its `Retry-After` and fails as
    /// `RateLimited`.
    #[tokio::test]
    async fn a_429_cools_the_endpoint_for_its_retry_after() {
        use kaijutsu_mk::test_server::{reply, serve};
        let mut busy = reply(429, r#"{"type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}}"#);
        busy.headers.push(("retry-after", "3".into()));
        let (base, _) = serve(vec![busy]).await;
        let endpoint = crate::llm::endpoint::Endpoints::default().for_url(&base).unwrap();
        let client = Client::new("k").unwrap().with_base_url(&base).with_endpoint(endpoint.clone());
        let err = client.prompt("m", None, "hi").await.unwrap_err();
        assert!(matches!(err, LlmError::RateLimited(_)), "{err:?}");
        let (left, status) = endpoint.status().cooldown.expect("cooling down");
        assert_eq!(status, 429);
        assert!(left > std::time::Duration::from_secs(2) && left <= std::time::Duration::from_secs(3), "{left:?}");
        assert_eq!(endpoint.status().in_flight, 0);
    }

    /// A stream that reaches `message_stop` resets the doubling; opening
    /// it does not.
    ///
    /// Falsified by a stream whose end leaves a busy answer counted.
    #[tokio::test]
    async fn a_stream_that_ends_whole_resets_the_doubling() {
        use kaijutsu_mk::test_server::{serve, sse};
        let (base, _) = serve(vec![sse(SIMPLE_COMPLETION)]).await;
        let endpoint = crate::llm::endpoint::Endpoints::default().for_url(&base).unwrap();
        let client = Client::new("k").unwrap().with_base_url(&base).with_endpoint(endpoint.clone());
        let mut stream = client.stream(BuildOpts::new("m"), vec![Message::user("hi")]).await.unwrap();
        let far = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        endpoint.acquire(far).await.unwrap().answered_with(503, Some(std::time::Duration::ZERO));
        assert_eq!(endpoint.busy_in_a_row(), 1);
        while stream.next_event().await.is_some() {}
        assert_eq!(endpoint.busy_in_a_row(), 0);
    }

    /// The live context-window lookup, `GET /v1/models/{id}`, takes a slot
    /// at the client's endpoint.
    ///
    /// Falsified by a lookup that goes out beside a full endpoint.
    #[tokio::test]
    async fn the_context_window_lookup_takes_a_slot() {
        #[derive(Debug)]
        struct InFlight(Arc<Endpoint>, std::sync::Mutex<Option<u32>>);
        #[async_trait::async_trait]
        impl ModelCapabilitySource for InFlight {
            async fn max_input_tokens(&self, _model: &str) -> LlmResult<Option<u64>> {
                *self.1.lock().unwrap() = Some(self.0.status().in_flight);
                Ok(Some(200_000))
            }
        }
        let endpoint = crate::llm::endpoint::Endpoints::default().get("hosted backend claude");
        let probe = Arc::new(InFlight(endpoint.clone(), Default::default()));
        let client = Client::new("k").unwrap().with_endpoint(endpoint.clone()).with_capability_source(probe.clone());
        assert_eq!(client.context_window("claude-opus-4-8").await, Some(200_000));
        assert_eq!(*probe.1.lock().unwrap(), Some(1), "the lookup held a slot");
        assert_eq!(endpoint.status().in_flight, 0);
    }

    /// A stream that fails does not count as a completed answer: a
    /// `message_stop` that ends in an error leaves the doubling as it was.
    ///
    /// Falsified by a failed stream that still calls `completed`.
    #[tokio::test]
    async fn a_stream_that_ends_in_an_error_keeps_the_doubling() {
        use kaijutsu_mk::test_server::{serve, sse};
        let paused = SIMPLE_COMPLETION.replace("end_turn", "pause_turn");
        let (base, _) = serve(vec![sse(&paused)]).await;
        let endpoint = crate::llm::endpoint::Endpoints::default().for_url(&base).unwrap();
        let client = Client::new("k").unwrap().with_base_url(&base).with_endpoint(endpoint.clone());
        let mut stream = client.stream(BuildOpts::new("m"), vec![Message::user("hi")]).await.unwrap();
        let far = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        endpoint.acquire(far).await.unwrap().answered_with(503, Some(std::time::Duration::ZERO));
        assert_eq!(endpoint.busy_in_a_row(), 1);
        let mut events = Vec::new();
        while let Some(ev) = stream.next_event().await {
            events.push(ev);
        }
        assert!(matches!(events.last(), Some(StreamEvent::Error(e)) if matches!(e.kind, StreamErrorKind::Truncated(_))), "{events:?}");
        assert!(!events.iter().any(|e| matches!(e, StreamEvent::Done { .. })), "{events:?}");
        assert_eq!(endpoint.busy_in_a_row(), 1, "a failed stream is not a completed answer");
    }

    /// `prompt()` returns text only for a reply that finished.
    #[tokio::test]
    async fn a_prompt_that_stopped_before_it_finished_is_an_error() {
        use kaijutsu_mk::test_server::{reply, serve};
        let body = |stop: &str| format!(
            r#"{{"id":"m","model":"claude-haiku-5-5","role":"assistant","content":[{{"type":"text","text":"partial"}}],"stop_reason":{stop},"stop_sequence":null,"usage":{{"input_tokens":1,"output_tokens":1}}}}"#
        );
        for stop in ["\"pause_turn\"", "\"model_context_window_exceeded\"", "null"] {
            let (base, _) = serve(vec![reply(200, &body(stop))]).await;
            let err = Client::new("k").unwrap().with_base_url(&base).prompt("m", None, "hi").await.unwrap_err();
            assert!(matches!(err, LlmError::CompletionError(_)), "{stop}: {err:?}");
        }
        let (base, _) = serve(vec![reply(200, &body("\"end_turn\""))]).await;
        assert_eq!(Client::new("k").unwrap().with_base_url(&base).prompt("m", None, "hi").await.unwrap(), "partial");
    }

    /// A model that thinks on every request rejects `thinking: disabled`, so
    /// the request fails before it is sent.
    #[tokio::test]
    async fn thinking_off_for_an_always_on_model_never_reaches_the_wire() {
        use kaijutsu_mk::test_server::serve;
        let (base, seen) = serve(vec![]).await;
        let mut opts = BuildOpts::new("claude-sonnet-5-5");
        opts.thinking_style = Some("off".into());
        let err = Client::new("k").unwrap().with_base_url(&base).stream(opts, vec![Message::user("hi")]).await.err().unwrap();
        assert!(matches!(&err, LlmError::InvalidRequest(m) if m.contains("claude-sonnet-5-5")), "{err:?}");
        assert!(seen.lock().unwrap().is_empty(), "nothing was sent");
    }

    /// A 529 is Anthropic's `overloaded_error`: transient, and the endpoint
    /// cools as it does for a 503.
    ///
    /// Falsified by a 529 the limiter never hears about.
    #[tokio::test]
    async fn a_529_cools_the_endpoint_as_a_503() {
        use kaijutsu_mk::test_server::{reply, serve};
        let (base, _) = serve(vec![reply(529, r#"{"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}, "request_id": "req_1"}"#)]).await;
        let endpoint = crate::llm::endpoint::Endpoints::default().for_url(&base).unwrap();
        let client = Client::new("k").unwrap().with_base_url(&base).with_endpoint(endpoint.clone());
        let err = client.prompt("m", None, "hi").await.unwrap_err();
        assert!(matches!(&err, LlmError::ApiError(m) if m.contains("overloaded_error") && m.contains("req_1")), "{err:?}");
        let (_, status) = endpoint.status().cooldown.expect("cooling down");
        assert_eq!(status, 503);
    }

    #[test]
    fn http_statuses_map_to_distinguishable_errors() {
        assert!(matches!(map_status(402, "x".into()), LlmError::InsufficientBalance(_)));
        assert!(matches!(map_status(401, "x".into()), LlmError::AuthError(_)));
        assert!(matches!(map_status(403, "x".into()), LlmError::AuthError(_)));
        assert!(matches!(map_status(413, "x".into()), LlmError::InvalidRequest(_)));
        assert!(matches!(map_status(429, "x".into()), LlmError::RateLimited(_)));
        assert!(matches!(map_status(500, "x".into()), LlmError::ApiError(_)));
        assert!(matches!(map_status(504, "x".into()), LlmError::ApiError(_)));
        assert!(matches!(map_status(529, "x".into()), LlmError::ApiError(_)));
        assert!(matches!(
            map_status(400, "invalid_request_error: prompt is too long: 250000 tokens > 200000 maximum".into()),
            LlmError::ContextFull(_)
        ));
        assert!(matches!(map_status(400, "other".into()), LlmError::InvalidRequest(_)));
    }

    /// A mid-stream `overloaded_error` ends the stream with an error that
    /// names its type, and cools the endpoint.
    ///
    /// Falsified by an error event that never reaches the limiter, or one
    /// that leaves the stream open.
    #[tokio::test]
    async fn a_mid_stream_overloaded_error_cools_the_endpoint_and_ends_the_stream() {
        use kaijutsu_mk::test_server::{serve, sse};
        let cut = &SIMPLE_COMPLETION[..SIMPLE_COMPLETION.find("event: content_block_stop").unwrap()];
        let failed = format!(
            "{cut}event: error\ndata: {}\n\n{}",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        let (base, _) = serve(vec![sse(&failed)]).await;
        let endpoint = crate::llm::endpoint::Endpoints::default().for_url(&base).unwrap();
        let client = Client::new("k").unwrap().with_base_url(&base).with_endpoint(endpoint.clone());
        let mut stream = client.stream(BuildOpts::new("m"), vec![Message::user("hi")]).await.unwrap();
        let mut events = Vec::new();
        while let Some(ev) = stream.next_event().await {
            events.push(ev);
        }
        assert!(matches!(events.last(), Some(StreamEvent::Error(e)) if e.kind == StreamErrorKind::Overloaded), "{events:?}");
        assert!(!events.iter().any(|e| matches!(e, StreamEvent::TextEnd)), "the stream stopped at the error: {events:?}");
        let (_, status) = endpoint.status().cooldown.expect("cooling down");
        assert_eq!(status, 503);
    }

    #[tokio::test]
    async fn the_deepseek_preset_points_at_the_messages_endpoint() {
        let client = Client::deepseek("k").unwrap();
        assert_eq!(client.base_url, "https://api.deepseek.com/anthropic");
        assert_eq!(client.dialect(), Dialect::DeepSeek);
        assert_eq!(client.available_models(), vec!["deepseek-flash", "deepseek-v4-pro"]);
        assert_eq!(Client::new("k").unwrap().dialect(), Dialect::Anthropic);
    }

    /// DeepSeek's endpoint answers `GET /v1/models/{id}` with 404, so the
    /// lookup is not made.
    ///
    /// Falsified by a deepseek client that asks its capability source.
    #[tokio::test]
    async fn a_deepseek_client_makes_no_context_window_lookup() {
        use crate::llm::claude::models_api::test_support::FakeModelCapabilitySource;
        let fake = Arc::new(FakeModelCapabilitySource::always_none().with_response("deepseek-flash", Ok(Some(5))));
        let client = Client::deepseek("k").unwrap().with_capability_source(fake.clone());
        assert_eq!(client.context_window("deepseek-flash").await, None);
        assert_eq!(fake.call_count(), 0);
    }

    /// `prompt()` against DeepSeek turns thinking off, so `max_tokens` is
    /// not spent before the answer.
    #[tokio::test]
    async fn a_deepseek_prompt_sends_thinking_disabled() {
        use kaijutsu_mk::test_server::{reply, serve};
        let ok = r#"{"id":"m","model":"deepseek-flash","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}"#;
        let (base, seen) = serve(vec![reply(200, ok)]).await;
        let text = Client::deepseek("k").unwrap().with_base_url(&base).prompt("deepseek-flash", None, "hi").await.unwrap();
        assert_eq!(text, "ok");
        let wire: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[0].body).unwrap();
        assert_eq!(wire["thinking"], serde_json::json!({"type": "disabled"}));
    }
}
