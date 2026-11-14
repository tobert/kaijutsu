//! Native Google Gemini API provider (`generateContent` / `streamGenerateContent`).
//!
//! Submodules:
//! - [`types`] — Gemini v1beta REST request/response types.
//! - [`models`] — per-model thinking controls (`thinkingBudget` / `thinkingLevel`).
//! - [`build`] — kaijutsu `Message` / `ContentBlock` → Gemini shapes.
//! - [`sse`] — Server-Sent Events parser (`data:` chunks).
//! - [`stream`] — SSE → kaijutsu `StreamEvent` state machine.

pub mod build;
pub mod models;
pub mod sse;
pub mod stream;
pub mod types;

use std::collections::VecDeque;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::llm::endpoint::{Endpoint, Slot};
use crate::llm::stream::{BuildOpts, StreamError, StreamErrorKind, StreamEvent};
use crate::llm::{DEFAULT_REQUEST_TIMEOUT_SECS, LlmError, LlmResult, Message, http_user_agent};

use self::sse::decode_event;
use self::stream::StateMachine;
use self::types::ApiErrorEnvelope;

pub const GEMINI_DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Native Google Gemini API client.
#[derive(Clone, Debug)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    provider_name: String,
    request_timeout: Option<std::time::Duration>,
    endpoint: Option<Arc<Endpoint>>,
}

impl Client {
    /// Construct a Gemini client from an API key (`x-goog-api-key` header).
    pub fn new(provider_name: impl Into<String>, api_key: impl Into<String>) -> LlmResult<Self> {
        let api_key = api_key.into();
        let mut headers = reqwest::header::HeaderMap::new();
        let mut key = reqwest::header::HeaderValue::from_str(&api_key).map_err(|_| {
            LlmError::AuthError(
                "the Gemini API key is not a valid HTTP header value (it holds a control \
                 character, such as a newline)"
                    .to_string(),
            )
        })?;
        key.set_sensitive(true);
        headers.insert("x-goog-api-key", key);
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .user_agent(http_user_agent())
            .build()
            .expect("reqwest::Client::builder must succeed on healthy host");
        Ok(Self {
            http,
            base_url: GEMINI_DEFAULT_BASE_URL.to_string(),
            provider_name: provider_name.into(),
            request_timeout: None,
            endpoint: None,
        })
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    pub fn with_request_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }

    pub fn with_endpoint(mut self, endpoint: Arc<Endpoint>) -> Self {
        self.endpoint = Some(endpoint);
        self
    }

    pub fn endpoint(&self) -> Option<&Arc<Endpoint>> {
        self.endpoint.as_ref()
    }

    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    pub fn available_models(&self) -> Vec<&'static str> {
        vec![
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "gemini-2.5-flash-lite",
        ]
    }

    pub fn one_shot_opts(&self, opts: BuildOpts) -> BuildOpts {
        models::one_shot_opts(opts)
    }

    fn slot_wait(&self) -> std::time::Duration {
        self.request_timeout
            .unwrap_or(std::time::Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS))
    }

    /// Start a streaming completion via `:streamGenerateContent?alt=sse`.
    pub async fn stream(&self, opts: BuildOpts, messages: Vec<Message>) -> LlmResult<Stream> {
        let body = build::build_request(&opts, &messages)?;
        let slot = crate::llm::endpoint::take_for_request(self.endpoint.as_ref(), self.slot_wait()).await?;

        let model_path = opts.model.strip_prefix("models/").unwrap_or(&opts.model);
        let url = format!("{}/models/{model_path}:streamGenerateContent?alt=sse", self.base_url);

        let mut request = self
            .http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);
        if let Some(timeout) = self.request_timeout {
            request = request.timeout(timeout);
        }
        crate::llm::request_dump::record(
            opts.trace.as_deref(),
            &self.provider_name,
            &opts.model,
            &self.base_url,
            opts.system.as_deref(),
            &messages,
            &body,
        );

        let response = request.send().await.map_err(http_error)?;
        crate::llm::endpoint::observe_open(slot.as_ref(), &response);

        let response = error_for_status(response).await?;
        Ok(Stream::from_response(response, slot))
    }
}

async fn error_for_status(response: reqwest::Response) -> LlmResult<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response
        .text()
        .await
        .unwrap_or_else(|e| format!("(failed to read body: {e})"));
    let detail = serde_json::from_str::<ApiErrorEnvelope>(&body)
        .map(|e| match e.error.status {
            Some(s) => format!("{s}: {}", e.error.message),
            None => e.error.message,
        })
        .unwrap_or(body);
    Err(map_status(status, detail))
}

fn map_status(status: reqwest::StatusCode, detail: String) -> LlmError {
    match status.as_u16() {
        401 | 403 => LlmError::AuthError(detail),
        429 => LlmError::RateLimited(detail),
        400..=499 => LlmError::InvalidRequest(detail),
        500..=599 => LlmError::ApiError(detail),
        _ => LlmError::ApiError(format!("HTTP {}: {detail}", status.as_u16())),
    }
}

fn busy_status(kind: &StreamErrorKind) -> Option<u16> {
    match kind {
        StreamErrorKind::RateLimited => Some(429),
        StreamErrorKind::Overloaded => Some(503),
        _ => None,
    }
}

fn http_error(err: reqwest::Error) -> LlmError {
    if err.is_timeout() {
        LlmError::NetworkError(format!("timeout: {err}"))
    } else if err.is_connect() {
        LlmError::NetworkError(format!("connect: {err}"))
    } else {
        LlmError::NetworkError(err.to_string())
    }
}

/// Streaming Gemini response, emitting kaijutsu [`StreamEvent`]s.
pub struct Stream {
    inner: Option<
        eventsource_stream::EventStream<BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>>,
    >,
    state: StateMachine,
    pending: VecDeque<StreamEvent>,
    cancel: CancellationToken,
    finished: bool,
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
                            Ok(Some(typed)) => {
                                let events = self.state.step(typed);
                                self.take(events);
                            }
                            Ok(None) => {}
                            Err(e) => {
                                self.finished = true;
                                return Some(self.failure(StreamErrorKind::Protocol, format!("SSE decode: {e}")));
                            }
                        },
                        Some(Err(e)) => {
                            self.finished = true;
                            return Some(self.failure(StreamErrorKind::Transport, format!("SSE transport: {e}")));
                        }
                        None => {
                            self.inner = None;
                            let tail = self.state.finish();
                            self.take(tail);
                            self.finished = true;
                        }
                    }
                }
            }
        }
    }

    fn take(&mut self, events: Vec<StreamEvent>) {
        for ev in events {
            match &ev {
                StreamEvent::Done { .. } => {
                    if let Some(slot) = &self.slot {
                        slot.completed();
                    }
                    self.finished = true;
                }
                StreamEvent::Error(e) => {
                    if let (Some(slot), Some(status)) = (&self.slot, busy_status(&e.kind)) {
                        slot.answered_with(status, None);
                    }
                    self.finished = true;
                }
                _ => {}
            }
            self.pending.push_back(ev);
        }
    }

    fn failure(&mut self, kind: StreamErrorKind, message: String) -> StreamEvent {
        let err = StreamError::new(kind, message);
        if let (Some(slot), Some(status)) = (&self.slot, busy_status(&err.kind)) {
            slot.answered_with(status, None);
        }
        StreamEvent::Error(err)
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Provider, http_user_agent};
    use kaijutsu_council::test_server::{serve, sse};

    #[tokio::test]
    async fn streams_from_loopback_and_sends_key_and_ua() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"hello from gemini\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":12,\"candidatesTokenCount\":3}}\n\n",
        );
        let (base, seen) = serve(vec![sse(body)]).await;
        let client = Client::new("gemini", "test-key-123").unwrap().with_base_url(&base);
        let mut stream = client
            .stream(BuildOpts::new("gemini-2.5-flash", 1024), vec![Message::user("hi")])
            .await
            .unwrap();

        let mut events = Vec::new();
        while let Some(ev) = stream.next_event().await {
            events.push(ev);
        }
        assert!(matches!(events.as_slice(), [
            StreamEvent::TextStart,
            StreamEvent::TextDelta(t),
            StreamEvent::TextEnd,
            StreamEvent::Done { stop_reason: Some(r), .. },
        ] if t == "hello from gemini" && r == "end_turn"), "{events:?}");

        let req = &seen.lock().unwrap()[0];
        assert_eq!(req.header("x-goog-api-key"), Some("test-key-123"));
        assert_eq!(req.header("user-agent"), Some(http_user_agent()));
        assert!(
            req.path
                .ends_with("/models/gemini-2.5-flash:streamGenerateContent?alt=sse"),
            "unexpected path: {}",
            req.path
        );
    }

    #[tokio::test]
    async fn provider_gemini_one_shot_round_trips() {
        let body = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"OK\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":1}}\n\n";
        let (base, _) = serve(vec![sse(body)]).await;
        let provider = Provider::Gemini(Client::new("gemini", "k").unwrap().with_base_url(&base));
        let reply = provider
            .prompt_with_system(BuildOpts::new("gemini-2.5-flash", 256), "ping")
            .await
            .unwrap();
        assert_eq!(reply.text, "OK");
    }

    #[tokio::test]
    #[ignore = "requires GEMINI_API_KEY; run with `cargo test --ignored gemini_live`"]
    async fn gemini_live_smoke_streams_real_response() {
        let key = std::env::var("GEMINI_API_KEY").expect("GEMINI_API_KEY");
        let client = Client::new("gemini", key).unwrap();
        let mut stream = client
            .stream(
                BuildOpts::new("gemini-2.5-flash", 256),
                vec![Message::user("Say hi in three words.")],
            )
            .await
            .unwrap();
        let mut got_done = false;
        while let Some(ev) = stream.next_event().await {
            if matches!(ev, StreamEvent::Done { .. }) {
                got_done = true;
            }
        }
        assert!(got_done);
    }
}
