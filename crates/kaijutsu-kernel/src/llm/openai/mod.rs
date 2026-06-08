//! Generic OpenAI-compatible chat-completions provider.
//!
//! Speaks the OpenAI `/chat/completions` dialect, which a wide field of
//! servers implement: a local lemonade / llama.cpp server, Ollama, OpenAI
//! itself, and DeepSeek. The DeepSeek-specific preset (required key,
//! `api.deepseek.com`, the V4 reasoning-echo quirk) lives in
//! [`crate::llm::deepseek`] as a thin wrapper over this [`Client`].
//!
//! Submodules:
//! - [`types`] — chat-completions native request/response/chunk types.
//! - [`build`] — kaijutsu `Message` / `ContentBlock` → wire shapes.
//! - [`sse`] — Server-Sent Events parser (`data:` chunks + `[DONE]`).
//! - [`stream`] — SSE → kaijutsu `StreamEvent` state machine.
//!
//! [`Client`] owns a `reqwest::Client` and attaches `Authorization: Bearer`
//! per request *when a key is configured* — local servers need none.
//! `stream()` POSTs to `/chat/completions` with `stream: true`; `prompt()`
//! does the non-streaming form.

pub mod build;
pub mod sse;
pub mod stream;
pub mod types;

use std::collections::VecDeque;

use futures::StreamExt;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::llm::stream::{BuildOpts, StreamEvent};
use crate::llm::{LlmError, LlmResult, Message};

use self::sse::{OpenAiSseEvent, decode_event};
use self::stream::StateMachine;
use self::types::{ApiError, ChatResponse};

/// Fallback endpoint when no `base_url` is configured — OpenAI's own API.
/// Local providers (lemonade, Ollama) always set `base_url` in config.
const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Generic OpenAI-compatible chat-completions client.
///
/// One `reqwest::Client` pools connections across calls; the optional
/// bearer key is attached per request (so a keyless local server works).
/// `provider_name` is the stable identifier reported to telemetry / theming
/// (e.g. "deepseek", "lemonade", "ollama"); `reasoning_required` toggles the
/// DeepSeek V4 round-trip quirk in [`build`] (echo `reasoning_content` on
/// every assistant turn) — `false` for plain OpenAI-compatible servers.
#[derive(Clone, Debug)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    provider_name: String,
    reasoning_required: bool,
}

impl Client {
    /// Construct a keyless client identified by `provider_name`, pointed at
    /// the OpenAI default base URL. Use the builders to add a key, override
    /// the base URL, or enable the reasoning-echo quirk.
    pub fn new(provider_name: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .build()
            .expect("reqwest::Client::builder must succeed on healthy host");
        Self {
            http,
            base_url: OPENAI_DEFAULT_BASE_URL.to_string(),
            api_key: None,
            provider_name: provider_name.into(),
            reasoning_required: false,
        }
    }

    /// Attach a bearer API key. Validity is checked at send time (an
    /// invalid key surfaces as an auth error, not a panic).
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Override the API base URL (for the DeepSeek endpoint, proxies, or
    /// local OpenAI-compatible servers).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Enable the DeepSeek V4 quirk: echo `reasoning_content` on every
    /// assistant turn (required on tool-call turns or the API 400s). Plain
    /// OpenAI-compatible servers leave this off.
    pub fn with_reasoning_required(mut self, required: bool) -> Self {
        self.reasoning_required = required;
        self
    }

    /// Stable provider identifier reported to telemetry / theming.
    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    /// Models surfaced by default. Config-driven for generic OpenAI-compatible
    /// servers (the registry sources models from `default_model` + aliases),
    /// so the base client advertises none; presets like DeepSeek override.
    pub fn available_models(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// Attach the bearer header to a request builder when a key is set.
    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => rb.bearer_auth(key),
            None => rb,
        }
    }

    /// One-shot prompt with optional system preamble (non-streaming).
    /// Concatenates response text; drops any `reasoning_content` (the
    /// distillation path doesn't surface the chain-of-thought).
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
        let body = build::build_request(&opts, &messages, false, self.reasoning_required);

        let response = self
            .auth(self.http.post(format!("{}/chat/completions", self.base_url)))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(http_error)?;

        let response = self.error_for_status(response).await?;

        let parsed: ChatResponse = response
            .json()
            .await
            .map_err(|e| LlmError::ApiError(format!("response JSON parse: {e}")))?;

        let text = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();
        Ok(text)
    }

    /// Start a streaming completion. POSTs `stream: true` and wraps the
    /// `text/event-stream` response in a [`Stream`].
    pub async fn stream(&self, opts: BuildOpts, messages: Vec<Message>) -> LlmResult<Stream> {
        let body = build::build_request(&opts, &messages, true, self.reasoning_required);

        let response = self
            .auth(self.http.post(format!("{}/chat/completions", self.base_url)))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(http_error)?;

        let response = self.error_for_status(response).await?;

        Ok(Stream::from_response(response, self.reasoning_required))
    }

    /// Map an OpenAI-compatible 4xx/5xx response body into [`LlmError`].
    ///
    /// These servers return OpenAI-shaped `{"error": {"message": …, "type":
    /// …}}`. We decode the inner error to map onto kaijutsu-typed
    /// variants; unparseable bodies pass through raw — never swallowed.
    async fn error_for_status(
        &self,
        response: reqwest::Response,
    ) -> LlmResult<reqwest::Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response
            .text()
            .await
            .unwrap_or_else(|e| format!("(failed to read body: {e})"));
        let detail = serde_json::from_str::<ApiError>(&body)
            .map(|e| match e.error.kind {
                Some(k) => format!("{k}: {}", e.error.message),
                None => e.error.message,
            })
            .unwrap_or(body);
        let mapped = match status.as_u16() {
            401 | 403 => LlmError::AuthError(detail),
            402 => LlmError::ApiError(format!("insufficient balance: {detail}")),
            429 => LlmError::RateLimited(detail),
            400..=499 => LlmError::InvalidRequest(detail),
            500..=599 => LlmError::ApiError(detail),
            _ => LlmError::ApiError(format!("unexpected HTTP {status}: {detail}")),
        };
        Err(mapped)
    }
}

fn http_error(e: reqwest::Error) -> LlmError {
    if e.is_timeout() {
        LlmError::NetworkError(format!("timeout: {e}"))
    } else if e.is_connect() {
        LlmError::NetworkError(format!("connect: {e}"))
    } else {
        LlmError::NetworkError(format!("{e}"))
    }
}

/// Streaming response from an OpenAI-compatible server.
///
/// Wraps the reqwest byte-stream in an `eventsource-stream` parser,
/// decodes each `data:` payload into a [`OpenAiSseEvent`], and drives
/// the [`StateMachine`] to produce kaijutsu [`StreamEvent`]s. Multiple
/// kaijutsu events per chunk are buffered in `pending`.
///
/// Cancellation mirrors the Claude path: [`Self::cancel`] fires a
/// [`CancellationToken`] observed by the next [`Self::next_event`] poll,
/// dropping the inner stream and emitting `Done { stop_reason: None }` —
/// the cancel-confirm contract the server expects.
pub struct Stream {
    inner: Option<
        eventsource_stream::EventStream<BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>>,
    >,
    state: StateMachine,
    pending: VecDeque<StreamEvent>,
    cancel: CancellationToken,
    finished: bool,
}

impl Stream {
    fn from_response(response: reqwest::Response, reasoning_required: bool) -> Self {
        use eventsource_stream::Eventsource;
        let bytes: BoxStream<'static, Result<bytes::Bytes, reqwest::Error>> =
            response.bytes_stream().boxed();
        Self {
            inner: Some(bytes.eventsource()),
            state: StateMachine::new(reasoning_required),
            pending: VecDeque::new(),
            cancel: CancellationToken::new(),
            finished: false,
        }
    }

    /// Test constructor: drive a fixed byte payload through the same
    /// pipeline as the live wire path.
    #[cfg(test)]
    pub(crate) fn for_test_bytes(payload: &'static str) -> Self {
        use eventsource_stream::Eventsource;
        let bytes: BoxStream<'static, Result<bytes::Bytes, reqwest::Error>> =
            futures::stream::iter(std::iter::once(Ok::<_, std::convert::Infallible>(
                bytes::Bytes::from(payload),
            )))
            .map(|r| r.map_err(|_: std::convert::Infallible| unreachable!()))
            .boxed();
        Self {
            inner: Some(bytes.eventsource()),
            state: StateMachine::new(false),
            pending: VecDeque::new(),
            cancel: CancellationToken::new(),
            finished: false,
        }
    }

    /// Poll for the next event. Returns `None` once exhausted.
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
                                if matches!(&typed, OpenAiSseEvent::Done) {
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
                            // Source closed without an explicit [DONE].
                            // Don't synthesize Done — that would mask a
                            // wire-shape bug.
                            self.finished = true;
                            self.inner = None;
                            return None;
                        }
                    }
                }
            }
        }
    }

    /// Abort the underlying HTTP stream. Idempotent.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

// ============================================================================
// End-to-end tests: drive bytes through Stream::next_event().
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::stream::{OpenAiCompatUsageExtra, UsageExtra};

    const SIMPLE: &str = "\
data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}

data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":1,\"total_tokens\":8,\"prompt_cache_hit_tokens\":0,\"prompt_cache_miss_tokens\":7}}

data: [DONE]

";

    #[tokio::test]
    async fn stream_drains_bytes_through_state_machine_to_done() {
        let mut s = Stream::for_test_bytes(SIMPLE);
        let mut events = Vec::new();
        while let Some(ev) = s.next_event().await {
            events.push(ev);
        }
        assert!(s.next_event().await.is_none(), "None after exhaustion");
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
                extra,
            } => {
                assert_eq!(stop_reason.as_deref(), Some("stop"));
                assert_eq!(*input_tokens, Some(7));
                assert_eq!(*output_tokens, Some(1));
                assert_eq!(
                    *extra,
                    Some(UsageExtra::OpenAiCompat(OpenAiCompatUsageExtra {
                        prompt_cache_hit_tokens: 0,
                        prompt_cache_miss_tokens: 7,
                        reasoning_tokens: 0,
                    }))
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_emits_done_with_none_stop_reason() {
        let mut s = Stream::for_test_bytes(SIMPLE);
        s.cancel();
        let ev = s.next_event().await.expect("must emit cancel-Done");
        match ev {
            StreamEvent::Done {
                stop_reason,
                extra,
                ..
            } => {
                assert!(stop_reason.is_none(), "cancel Done.stop_reason must be None");
                assert!(extra.is_none());
            }
            other => panic!("expected Done, got {other:?}"),
        }
        assert!(s.next_event().await.is_none());
    }

    /// Live smoke test against a local OpenAI-compatible server (lemonade /
    /// llama.cpp / Ollama). Gated behind `OPENAI_COMPAT_BASE_URL` so CI and
    /// casual `cargo test` skip it; reads no API key (local servers need
    /// none). The model defaults to a small lemonade Gemma but is
    /// overridable.
    ///
    /// ```sh
    /// OPENAI_COMPAT_BASE_URL=http://localhost:13305/v1 \
    ///   cargo test -p kaijutsu-kernel --lib openai_live \
    ///   -- --ignored --nocapture
    /// ```
    ///
    /// Gemma-4 puts its chain-of-thought in `reasoning_content` and the
    /// answer in `content`, so the state machine surfaces a Thinking block
    /// then a Text block — give it `max_tokens` headroom or the thinking
    /// eats the budget before the answer lands.
    #[tokio::test]
    #[ignore = "requires a local OpenAI-compatible server; run with `cargo test --ignored openai_live`"]
    async fn openai_live_smoke_streams_real_response() {
        let base_url = match std::env::var("OPENAI_COMPAT_BASE_URL") {
            Ok(u) if !u.is_empty() => u,
            _ => return,
        };
        let model = std::env::var("OPENAI_COMPAT_MODEL")
            .unwrap_or_else(|_| "Gemma-4-E4B-it-GGUF".to_string());
        let client = Client::new("lemonade").with_base_url(base_url);
        let opts = BuildOpts::new(model)
            .with_max_tokens(512)
            .with_system("You are friendly. Respond briefly.");
        let mut stream = client
            .stream(opts, vec![Message::user("hi there")])
            .await
            .expect("stream open must succeed against a healthy local server");
        let mut text = String::new();
        let mut thinking = String::new();
        let mut saw_done = false;
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        while let Some(ev) = stream.next_event().await {
            match ev {
                StreamEvent::TextDelta(t) => text.push_str(&t),
                StreamEvent::ThinkingDelta(t) => thinking.push_str(&t),
                StreamEvent::Done {
                    input_tokens: it,
                    output_tokens: ot,
                    ..
                } => {
                    saw_done = true;
                    input_tokens = it.unwrap_or(0);
                    output_tokens = ot.unwrap_or(0);
                }
                StreamEvent::Error(e) => panic!("live stream error: {e}"),
                _ => {}
            }
        }
        println!("\n--- server said ---\n{text}\n--- thinking ---\n{thinking}\n--- meta ---");
        println!("tokens: in={input_tokens} out={output_tokens}\n");
        assert!(!text.is_empty(), "live response must include some answer text");
        assert!(saw_done, "live response must terminate with Done");
        assert!(input_tokens > 0, "usage must be captured from trailing chunk");
    }
}
