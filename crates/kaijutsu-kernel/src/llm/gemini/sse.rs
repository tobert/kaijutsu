//! Server-Sent Events parser for Gemini `streamGenerateContent?alt=sse`.
//!
//! Gemini emits anonymous `data:` events whose JSON payload is a
//! [`GenerateContentResponse`] (or an `{"error": ...}` envelope).

use super::types::{ApiErrorEnvelope, GenerateContentResponse};
use crate::llm::stream::{StreamError, StreamErrorKind};

/// A decoded Gemini SSE event.
#[derive(Debug, Clone, PartialEq)]
pub enum GeminiSseEvent {
    Chunk(GenerateContentResponse),
    Error(StreamError),
}

#[derive(Debug, thiserror::Error)]
pub enum SseDecodeError {
    #[error("invalid JSON in Gemini SSE chunk: {source}; raw={raw:?}")]
    InvalidJson {
        raw: String,
        #[source]
        source: serde_json::Error,
    },
}

/// Map a Google API error status/code into a [`StreamErrorKind`].
pub fn error_kind_for(code: Option<u16>, status: Option<&str>) -> StreamErrorKind {
    match status {
        Some("RESOURCE_EXHAUSTED") => StreamErrorKind::RateLimited,
        Some("UNAVAILABLE") => StreamErrorKind::Overloaded,
        Some("DEADLINE_EXCEEDED") => StreamErrorKind::Timeout,
        Some("UNAUTHENTICATED" | "PERMISSION_DENIED") => StreamErrorKind::Auth,
        Some("INVALID_ARGUMENT" | "NOT_FOUND" | "FAILED_PRECONDITION") => StreamErrorKind::InvalidRequest,
        Some("INTERNAL" | "DATA_LOSS" | "UNKNOWN") => StreamErrorKind::ServerError,
        _ => match code {
            Some(401 | 403) => StreamErrorKind::Auth,
            Some(429) => StreamErrorKind::RateLimited,
            Some(503) => StreamErrorKind::Overloaded,
            Some(504) => StreamErrorKind::Timeout,
            Some(400 | 404 | 422) => StreamErrorKind::InvalidRequest,
            Some(500..=599) => StreamErrorKind::ServerError,
            _ => StreamErrorKind::Unknown(status.unwrap_or("gemini_error").to_string()),
        },
    }
}

/// Decode one [`eventsource_stream::Event`] into a [`GeminiSseEvent`].
pub fn decode_event(event: &eventsource_stream::Event) -> Result<Option<GeminiSseEvent>, SseDecodeError> {
    let data = event.data.trim();
    if data.is_empty() {
        return Ok(None);
    }

    if let Ok(env) = serde_json::from_str::<ApiErrorEnvelope>(data) {
        let kind = error_kind_for(env.error.code, env.error.status.as_deref());
        return Ok(Some(GeminiSseEvent::Error(StreamError::new(kind, env.error.message))));
    }

    let chunk: GenerateContentResponse =
        serde_json::from_str(data).map_err(|source| SseDecodeError::InvalidJson {
            raw: data.to_string(),
            source,
        })?;
    Ok(Some(GeminiSseEvent::Chunk(chunk)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse_event(data: &str) -> eventsource_stream::Event {
        eventsource_stream::Event {
            event: String::new(),
            data: data.to_string(),
            id: String::new(),
            retry: None,
        }
    }

    #[test]
    fn decodes_chunk_with_thought_and_function_call() {
        let raw = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hmm","thought":true,"thoughtSignature":"sigA"},{"functionCall":{"name":"ls","args":{"path":"."}},"thoughtSignature":"sigB"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"thoughtsTokenCount":2}}"#;
        let ev = decode_event(&sse_event(raw)).unwrap().unwrap();
        let GeminiSseEvent::Chunk(c) = ev else { panic!("expected chunk") };
        assert_eq!(c.candidates[0].finish_reason.as_deref(), Some("STOP"));
        let parts = &c.candidates[0].content.as_ref().unwrap().parts;
        assert_eq!(parts[0].thought, Some(true));
        assert_eq!(parts[0].thought_signature.as_deref(), Some("sigA"));
        assert_eq!(parts[1].function_call.as_ref().unwrap().name, "ls");
        assert_eq!(parts[1].thought_signature.as_deref(), Some("sigB"));
    }

    #[test]
    fn decodes_mid_stream_error_envelope() {
        let raw = r#"{"error":{"code":429,"message":"quota exceeded","status":"RESOURCE_EXHAUSTED"}}"#;
        let ev = decode_event(&sse_event(raw)).unwrap().unwrap();
        let GeminiSseEvent::Error(err) = ev else { panic!("expected error") };
        assert_eq!(err.kind, StreamErrorKind::RateLimited);
    }
}
