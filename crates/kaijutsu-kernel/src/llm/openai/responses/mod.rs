//! OpenAI's `/v1/responses` wire under the ChatGPT plan contract.
//!
//! The `chatgpt` backend speaks this wire (`crate::llm::chatgpt`). The
//! request rules are in [`build`], the event mapping in [`stream`]. The
//! caller supplies the bearer for each request, because a plan access token
//! expires within the hour and is refreshed between requests.
//!
//! Errors before the stream opens arrive as `{"error": {code, message}}` or
//! as `{"detail": "..."}`; [`OpenError`] keeps a 401 apart so the caller can
//! refresh its token and send once more. The bearer is removed from every
//! error message, before the stream opens and after, in case a provider
//! echoes it.

pub mod build;
pub mod stream;

use std::collections::VecDeque;

use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::llm::endpoint::Slot;
use crate::llm::stream::{StreamError, StreamErrorKind, StreamEvent};
use crate::llm::LlmError;

use self::stream::{StateMachine, error_kind, with_guidance};

/// Why a request failed before its stream opened.
#[derive(Debug)]
pub enum OpenError {
    /// HTTP 401: the bearer was refused. A refreshed token may succeed.
    Unauthorized(LlmError),
    /// Any other failure.
    Other(LlmError),
}

impl OpenError {
    pub fn into_llm(self) -> LlmError {
        match self {
            Self::Unauthorized(e) | Self::Other(e) => e,
        }
    }
}

/// `message` with every occurrence of `secret` replaced.
pub fn redact(message: &str, secret: &str) -> String {
    if secret.is_empty() { message.to_string() } else { message.replace(secret, "[redacted]") }
}

/// Map a failing response's status and body to an [`OpenError`]. `bearer`
/// is removed from the message.
pub fn open_error(status: reqwest::StatusCode, body: &str, bearer: &str) -> OpenError {
    let body = &redact(body, bearer);
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let error = parsed.as_ref().and_then(|v| v.get("error")).filter(|e| e.is_object());
    let code = error
        .and_then(|e| e.get("code").or_else(|| e.get("type")))
        .and_then(Value::as_str)
        .map(str::to_string);
    let message = error
        .and_then(|e| e.get("message"))
        .or_else(|| parsed.as_ref().and_then(|v| v.get("detail")))
        .and_then(Value::as_str)
        .map_or_else(|| body.to_string(), str::to_string);
    let detail = match &code {
        Some(code) => with_guidance(code, format!("{code}: {message}")),
        None => message,
    };
    let llm = match code.as_deref().map(error_kind) {
        _ if code.as_deref() == Some("context_length_exceeded") => LlmError::ContextFull(detail),
        Some(StreamErrorKind::Billing) => LlmError::InsufficientBalance(detail),
        Some(StreamErrorKind::Auth) => LlmError::AuthError(detail),
        Some(StreamErrorKind::Overloaded | StreamErrorKind::ServerError) => LlmError::ApiError(detail),
        Some(StreamErrorKind::InvalidRequest) => LlmError::InvalidRequest(detail),
        Some(StreamErrorKind::RateLimited) => LlmError::RateLimited(detail),
        _ => match status.as_u16() {
            401 | 403 => LlmError::AuthError(detail),
            402 => LlmError::InsufficientBalance(detail),
            429 => LlmError::RateLimited(detail),
            400..=499 => LlmError::InvalidRequest(detail),
            _ => LlmError::ApiError(format!("HTTP {status}: {detail}")),
        },
    };
    if status == reqwest::StatusCode::UNAUTHORIZED {
        OpenError::Unauthorized(llm)
    } else {
        OpenError::Other(llm)
    }
}

/// A streaming `/v1/responses` reply.
///
/// [`Self::cancel`] is observed by the next [`Self::next_event`] poll, which
/// drops the connection and emits `Done { stop_reason: None }`.
pub struct Stream {
    inner: Option<eventsource_stream::EventStream<BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>>>,
    state: StateMachine,
    pending: VecDeque<StreamEvent>,
    cancel: CancellationToken,
    finished: bool,
    /// Released as soon as the response ends, before its last events are
    /// read: the runtime stops reading at `Done`.
    slot: Option<Slot>,
    /// Removed from every error message.
    bearer: String,
}

impl Stream {
    pub fn from_response(response: reqwest::Response, slot: Option<Slot>, bearer: &str) -> Self {
        Self::from_bytes(response.bytes_stream().boxed(), slot, bearer)
    }

    fn from_bytes(
        bytes: BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
        slot: Option<Slot>,
        bearer: &str,
    ) -> Self {
        use eventsource_stream::Eventsource;
        Self {
            inner: Some(bytes.eventsource()),
            state: StateMachine::new(),
            pending: VecDeque::new(),
            cancel: CancellationToken::new(),
            finished: false,
            slot,
            bearer: bearer.to_string(),
        }
    }

    /// Poll for the next event. Returns `None` once exhausted.
    pub async fn next_event(&mut self) -> Option<StreamEvent> {
        loop {
            if let Some(ev) = self.pending.pop_front() {
                return Some(ev);
            }
            if self.finished {
                self.slot = None;
                return None;
            }
            let Some(inner) = self.inner.as_mut() else {
                let tail = self.state.end_of_stream();
                self.take(tail);
                continue;
            };
            let cancel = self.cancel.clone();
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    self.finished = true;
                    self.inner = None;
                    return Some(StreamEvent::Done { stop_reason: None, input_tokens: None, output_tokens: None, extra: None });
                }
                item = inner.next() => match item {
                    Some(Ok(event)) => {
                        let data = event.data.trim();
                        if data.is_empty() || data == "[DONE]" {
                            continue;
                        }
                        match serde_json::from_str::<Value>(data) {
                            Ok(value) => {
                                let emitted = self.state.step(value);
                                self.take(emitted);
                            }
                            Err(e) => {
                                let shown: String = data.chars().take(200).collect();
                                self.fail(StreamErrorKind::Protocol, format!("SSE decode: {e}: {shown}"))
                            }
                        }
                    }
                    Some(Err(e)) => self.fail(StreamErrorKind::Transport, format!("SSE transport: {e}")),
                    None => self.inner = None,
                }
            }
        }
    }

    /// Queue `emitted`, tell the slot how the answer went, and release it
    /// once the response ends.
    fn take(&mut self, mut emitted: Vec<StreamEvent>) {
        for event in &mut emitted {
            if let StreamEvent::Error(error) = event {
                error.message = redact(&error.message, &self.bearer);
            }
        }
        for event in &emitted {
            match event {
                StreamEvent::Done { .. } => {
                    if let Some(slot) = &self.slot {
                        slot.completed();
                    }
                }
                StreamEvent::Error(error) => {
                    let busy = match error.kind {
                        StreamErrorKind::RateLimited => Some(429),
                        StreamErrorKind::Overloaded => Some(503),
                        _ => None,
                    };
                    if let (Some(slot), Some(status)) = (&self.slot, busy) {
                        slot.answered_with(status, None);
                    }
                }
                _ => {}
            }
        }
        if self.state.ended() {
            self.finished = true;
            self.inner = None;
            self.slot = None;
        }
        self.pending.extend(emitted);
    }

    /// End the stream with an error raised below the state machine.
    fn fail(&mut self, kind: StreamErrorKind, message: String) {
        let error = StreamError::new(kind, redact(&message, &self.bearer));
        let error = match self.state.spent() {
            Some(spent) => error.with_usage(spent),
            None => error,
        };
        self.finished = true;
        self.inner = None;
        self.slot = None;
        self.pending.push_back(StreamEvent::Error(error));
    }

    /// Abort the underlying HTTP stream. Idempotent.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Test constructor: drive a fixed payload through the live pipeline.
    #[cfg(test)]
    pub(crate) fn for_test_bytes(payload: &'static str) -> Self {
        let bytes = futures::stream::iter(std::iter::once(Ok(bytes::Bytes::from(payload)))).boxed();
        Self::from_bytes(bytes, None, "")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn drain(mut stream: Stream) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Some(ev) = stream.next_event().await {
            out.push(ev);
        }
        out
    }

    /// Bytes through the SSE parser and state machine end in one `Done`.
    #[tokio::test]
    async fn a_byte_stream_ends_in_done() {
        let payload = "event: response.output_item.added\n\
data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\"}}\n\n\
event: response.output_text.delta\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n\
event: response.output_item.done\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\"}}\n\n\
event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n";
        let out = drain(Stream::for_test_bytes(payload)).await;
        assert_eq!(out[..3], [StreamEvent::TextStart, StreamEvent::TextDelta("hi".into()), StreamEvent::TextEnd]);
        assert!(matches!(&out[3], StreamEvent::Done { input_tokens: Some(3), .. }), "{out:?}");
        assert_eq!(out.len(), 4);
    }

    /// A stream that closes early ends in exactly one truncation error.
    #[tokio::test]
    async fn an_early_close_is_one_truncation() {
        let payload = "data: {\"type\":\"response.created\"}\n\n";
        let out = drain(Stream::for_test_bytes(payload)).await;
        assert!(matches!(out.as_slice(), [StreamEvent::Error(e)] if matches!(e.kind, StreamErrorKind::Truncated(_))), "{out:?}");
    }

    /// Undecodable data is a protocol error, not a silent skip.
    #[tokio::test]
    async fn undecodable_data_is_a_protocol_error() {
        let out = drain(Stream::for_test_bytes("data: {nope\n\n")).await;
        assert!(matches!(out.as_slice(), [StreamEvent::Error(e)] if e.kind == StreamErrorKind::Protocol), "{out:?}");
    }

    /// Pre-stream errors: a spent plan is a balance error with guidance, a
    /// bare `detail` keeps its text, and only a 401 asks for a refresh.
    #[test]
    fn open_errors_map_codes_and_detail() {
        let spent = open_error(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"limit"}}"#,
            "sk-test-bearer",
        );
        assert!(matches!(&spent, OpenError::Other(LlmError::InsufficientBalance(m)) if m.contains(stream::PLAN_USAGE_URL)), "{spent:?}");
        let detail = open_error(reqwest::StatusCode::UNAUTHORIZED, r#"{"detail":"token expired"}"#, "sk-test-bearer");
        assert!(matches!(&detail, OpenError::Unauthorized(LlmError::AuthError(m)) if m == "token expired"), "{detail:?}");
        let forbidden = open_error(reqwest::StatusCode::FORBIDDEN, r#"{"detail":"no"}"#, "sk-test-bearer");
        assert!(matches!(forbidden, OpenError::Other(LlmError::AuthError(_))));
        let full = open_error(reqwest::StatusCode::BAD_REQUEST, r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#, "sk-test-bearer");
        assert!(matches!(full, OpenError::Other(LlmError::ContextFull(_))), "{full:?}");
        let echoed = open_error(reqwest::StatusCode::FORBIDDEN, r#"{"detail":"bad bearer sk-secret-1"}"#, "sk-secret-1");
        assert!(matches!(&echoed, OpenError::Other(LlmError::AuthError(m)) if m == "bad bearer [redacted]"), "{echoed:?}");
    }
}
