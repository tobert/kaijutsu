//! Server-Sent Events parser for the DeepSeek (OpenAI-compatible)
//! streaming chat API.
//!
//! Unlike Anthropic — which names each SSE event (`message_start`,
//! `content_block_delta`, …) — OpenAI-style streams emit anonymous
//! `data:` lines whose payload is a `chat.completion.chunk` JSON object,
//! terminated by a single `data: [DONE]` sentinel. So dispatch is on the
//! *data* (the `[DONE]` literal vs JSON), not on an `event:` name.
//!
//! ```text
//! DeepSeek SSE                          decoded
//! ────────────                          ───────
//! data: {"choices":[{"delta":…}]}       OpenAiSseEvent::Chunk(ChatChunk)
//! data: {"choices":[],"usage":{…}}      OpenAiSseEvent::Chunk (usage-only)
//! data: {"error":{"message":…}}         OpenAiSseEvent::Error
//! data: [DONE]                          OpenAiSseEvent::Done
//! ```
//!
//! Tests feed fixture byte slices through the same parser the wire path
//! uses (including chunk boundaries falling mid-event), so they fail when
//! DeepSeek shifts the chunk shape.

use super::types::ChatChunk;
use crate::llm::stream::{StreamError, StreamErrorKind};

/// The SSE `data: [DONE]` terminator literal.
const DONE_SENTINEL: &str = "[DONE]";

/// A decoded DeepSeek streaming event.
#[derive(Debug, Clone, PartialEq)]
pub enum OpenAiSseEvent {
    /// A `chat.completion.chunk` payload (content / reasoning / tool-call
    /// deltas, or the trailing usage-only chunk).
    Chunk(ChatChunk),
    /// The `[DONE]` sentinel — stream is complete.
    Done,
    /// A `data:` payload carrying an `error` key: the provider failed the
    /// request after the stream opened. Its kind follows the payload's
    /// `type`, or its `code` when there is no type.
    Error(StreamError),
}

/// Errors from the SSE layer.
#[derive(Debug, thiserror::Error)]
pub enum SseDecodeError {
    #[error("invalid JSON in chunk: {source}; raw={raw:?}")]
    InvalidJson {
        raw: String,
        #[source]
        source: serde_json::Error,
    },
}

/// Decode one [`eventsource_stream::Event`] into a typed
/// [`OpenAiSseEvent`].
///
/// The `[DONE]` sentinel decodes to [`OpenAiSseEvent::Done`]; anything
/// else is parsed as a [`ChatChunk`], unless it carries an `error` key, which
/// decodes to [`OpenAiSseEvent::Error`]. A JSON parse failure surfaces as
/// [`SseDecodeError::InvalidJson`] rather than silently dropping — per
/// the crash-over-corrupt principle.
pub fn decode_event(event: &eventsource_stream::Event) -> Result<OpenAiSseEvent, SseDecodeError> {
    let data = event.data.trim();
    if data == DONE_SENTINEL {
        return Ok(OpenAiSseEvent::Done);
    }
    let invalid = |source| SseDecodeError::InvalidJson {
        raw: data.to_string(),
        source,
    };
    let value = serde_json::from_str::<serde_json::Value>(data).map_err(invalid)?;
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        return Ok(OpenAiSseEvent::Error(stream_error(error)));
    }
    serde_json::from_value::<ChatChunk>(value)
        .map(OpenAiSseEvent::Chunk)
        .map_err(invalid)
}

/// The [`StreamError`] for a payload's `error` value: the message of the
/// OpenAI object shape, the string itself for a bare string, the raw JSON
/// otherwise.
fn stream_error(error: &serde_json::Value) -> StreamError {
    if let Some(text) = error.as_str() {
        return StreamError::new(StreamErrorKind::Unknown("error".into()), text);
    }
    let name = |field| error.get(field).and_then(|v| v.as_str());
    let kind = match name("type").or_else(|| name("code")) {
        Some("server_error" | "internal_error") => StreamErrorKind::ServerError,
        Some("rate_limit_error" | "rate_limit_exceeded") => StreamErrorKind::RateLimited,
        Some("overloaded_error" | "engine_overloaded_error") => StreamErrorKind::Overloaded,
        Some("timeout" | "timeout_error") => StreamErrorKind::Timeout,
        Some("invalid_request_error") => StreamErrorKind::InvalidRequest,
        Some("authentication_error" | "invalid_api_key" | "permission_error") => StreamErrorKind::Auth,
        Some("insufficient_quota" | "billing_error") => StreamErrorKind::Billing,
        Some(other) => StreamErrorKind::Unknown(other.to_string()),
        None => StreamErrorKind::Unknown("error".into()),
    };
    let message = name("message").map_or_else(|| error.to_string(), str::to_string);
    StreamError::new(kind, message)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use eventsource_stream::Eventsource;
    use futures::StreamExt;
    use std::convert::Infallible;

    async fn parse_all(payload: &str) -> Vec<Result<OpenAiSseEvent, SseDecodeError>> {
        let bytes = bytes::Bytes::from(payload.to_string());
        let stream = futures::stream::iter(vec![Ok::<_, Infallible>(bytes)]).eventsource();
        let mut out = Vec::new();
        let mut stream = Box::pin(stream);
        while let Some(item) = stream.next().await {
            match item {
                Ok(event) => out.push(decode_event(&event)),
                Err(e) => panic!("SSE parser layer error: {e}"),
            }
        }
        out
    }

    async fn parse_chunked(chunks: Vec<&str>) -> Vec<Result<OpenAiSseEvent, SseDecodeError>> {
        let items: Vec<Result<bytes::Bytes, Infallible>> = chunks
            .into_iter()
            .map(|c| Ok(bytes::Bytes::from(c.to_string())))
            .collect();
        let stream = futures::stream::iter(items).eventsource();
        let mut out = Vec::new();
        let mut stream = Box::pin(stream);
        while let Some(item) = stream.next().await {
            match item {
                Ok(event) => out.push(decode_event(&event)),
                Err(e) => panic!("SSE parser layer error: {e}"),
            }
        }
        out
    }

    // A realistic short completion: role chunk, two content deltas, a
    // finish_reason chunk, the trailing usage chunk, then [DONE].
    const SIMPLE: &str = "\
data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"content\":\", world\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}

data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"total_tokens\":13,\"prompt_cache_hit_tokens\":0,\"prompt_cache_miss_tokens\":10}}

data: [DONE]

";

    #[tokio::test]
    async fn parses_simple_completion_then_done() {
        let events: Vec<_> = parse_all(SIMPLE)
            .await
            .into_iter()
            .map(|r| r.expect("decode must succeed"))
            .collect();
        assert_eq!(events.len(), 6);
        // First chunk: role assignment, empty content.
        match &events[0] {
            OpenAiSseEvent::Chunk(c) => {
                assert_eq!(c.choices[0].delta.role.as_deref(), Some("assistant"));
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
        // Second: content delta "Hello".
        match &events[1] {
            OpenAiSseEvent::Chunk(c) => {
                assert_eq!(c.choices[0].delta.content.as_deref(), Some("Hello"));
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
        // finish_reason chunk.
        match &events[3] {
            OpenAiSseEvent::Chunk(c) => {
                assert_eq!(c.choices[0].finish_reason.as_deref(), Some("stop"));
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
        // usage-only chunk: empty choices, populated usage.
        match &events[4] {
            OpenAiSseEvent::Chunk(c) => {
                assert!(c.choices.is_empty());
                assert_eq!(c.usage.as_ref().unwrap().prompt_tokens, 10);
            }
            other => panic!("expected usage Chunk, got {other:?}"),
        }
        assert_eq!(events[5], OpenAiSseEvent::Done);
    }

    #[tokio::test]
    async fn done_sentinel_decodes_to_done() {
        let events: Vec<_> = parse_all("data: [DONE]\n\n")
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(events, vec![OpenAiSseEvent::Done]);
    }

    #[tokio::test]
    async fn reasoning_content_chunk_decodes() {
        let payload =
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hmm\"},\"finish_reason\":null}]}\n\n";
        let events: Vec<_> = parse_all(payload)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        match &events[0] {
            OpenAiSseEvent::Chunk(c) => {
                assert_eq!(
                    c.choices[0].delta.reasoning_content.as_deref(),
                    Some("hmm")
                );
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_json_surfaces_typed_error() {
        let payload = "data: {not valid json\n\n";
        let events = parse_all(payload).await;
        match &events[0] {
            Err(SseDecodeError::InvalidJson { raw, .. }) => {
                assert!(raw.contains("not valid"));
            }
            other => panic!("expected InvalidJson, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chunked_transport_reassembles_event_boundaries() {
        // A single data: event split across three byte chunks — the
        // realistic reqwest streaming case.
        let chunks = vec![
            "data: {\"choices\":[{\"delta\":{\"con",
            "tent\":\"split\"},\"finish_rea",
            "son\":null}]}\n\n",
        ];
        let events: Vec<_> = parse_chunked(chunks)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(events.len(), 1);
        match &events[0] {
            OpenAiSseEvent::Chunk(c) => {
                assert_eq!(c.choices[0].delta.content.as_deref(), Some("split"));
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_error_object_decodes_to_an_error_event_typed_by_its_type() {
        let events = parse_all("data: {\"error\":{\"message\":\"engine overloaded\",\"type\":\"server_error\"}}\n\n").await;
        match &events[0] {
            Ok(OpenAiSseEvent::Error(error)) => {
                assert_eq!(error.kind, StreamErrorKind::ServerError);
                assert_eq!(error.message, "engine overloaded");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_error_type_maps_to_its_kind() {
        let cases = [
            ("rate_limit_exceeded", StreamErrorKind::RateLimited),
            ("overloaded_error", StreamErrorKind::Overloaded),
            ("invalid_request_error", StreamErrorKind::InvalidRequest),
            ("invalid_api_key", StreamErrorKind::Auth),
            ("insufficient_quota", StreamErrorKind::Billing),
            ("brand_new_type", StreamErrorKind::Unknown("brand_new_type".into())),
        ];
        for (wire, kind) in cases {
            let payload = format!("data: {{\"error\":{{\"message\":\"m\",\"type\":\"{wire}\"}}}}\n\n");
            match &parse_all(&payload).await[0] {
                Ok(OpenAiSseEvent::Error(error)) => assert_eq!(error.kind, kind, "{wire}"),
                other => panic!("{wire}: expected Error, got {other:?}"),
            }
        }
        // No type: the code names the kind.
        match &parse_all("data: {\"error\":{\"message\":\"m\",\"code\":\"rate_limit_exceeded\"}}\n\n").await[0] {
            Ok(OpenAiSseEvent::Error(error)) => assert_eq!(error.kind, StreamErrorKind::RateLimited),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_error_string_decodes_to_an_unknown_error_event() {
        let events = parse_all("data: {\"error\":\"upstream closed\"}\n\n").await;
        match &events[0] {
            Ok(OpenAiSseEvent::Error(error)) => {
                assert_eq!(error.kind, StreamErrorKind::Unknown("error".into()));
                assert_eq!(error.message, "upstream closed");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_null_error_key_is_an_ordinary_chunk() {
        let events = parse_all("data: {\"error\":null,\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n").await;
        assert!(matches!(&events[0], Ok(OpenAiSseEvent::Chunk(_))), "{events:?}");
    }
}
