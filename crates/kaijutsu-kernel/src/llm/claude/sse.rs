//! Server-Sent Events parser for the Anthropic Messages streaming API.
//!
//! Wraps [`eventsource_stream::EventStream`] (over a `reqwest` byte
//! stream) and decodes each event's JSON body into a typed
//! [`ClaudeSseEvent`]. Dispatch is on the SSE `event:` line — Anthropic
//! emits one of `message_start`, `content_block_start`,
//! `content_block_delta`, `content_block_stop`, `message_delta`,
//! `message_stop`, `error`, or `ping`. The data JSON's own `type` field
//! mirrors the event name and is ignored here.
//!
//! Tests feed fixture byte slices through the same parser the wire path
//! uses, so they fail when Anthropic shifts the event shape. Fixtures
//! are inline strings rather than checked-in files — small enough that
//! diff readability beats fixture-file overhead.

use serde::Deserialize;

use super::types::ResponseUsage;
use crate::llm::stream::StreamErrorKind;

/// Typed Anthropic streaming event.
///
/// Variants map 1:1 to the documented event names. The inner payload
/// types decode each event's JSON body via serde; the `type` field on
/// the JSON itself is redundant with the event name and gets dropped.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaudeSseEvent {
    MessageStart(MessageStartPayload),
    ContentBlockStart(ContentBlockStartPayload),
    ContentBlockDelta(ContentBlockDeltaPayload),
    ContentBlockStop(ContentBlockStopPayload),
    MessageDelta(MessageDeltaPayload),
    MessageStop,
    Error(SseError),
    Ping,
    /// An event name this client does not know. The API versioning policy
    /// says new event types may appear, so the stream goes on without it.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessageStartPayload {
    pub message: StartedMessage,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct StartedMessage {
    pub id: String,
    pub model: String,
    pub role: String,
    #[serde(default)]
    pub stop_reason: Option<String>,
    pub usage: ResponseUsage,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ContentBlockStartPayload {
    pub index: usize,
    pub content_block: StartedBlock,
}

/// The opening shape of a streamed content block. tool_use blocks carry
/// `id` and `name` here; the `input` field opens empty and gets
/// populated by `input_json_delta` events. Text and thinking blocks
/// open empty.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StartedBlock {
    Text {
        #[serde(default)]
        text: String,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    /// Thinking the API withheld. `data` is opaque and must go back
    /// unchanged on the next request.
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ContentBlockDeltaPayload {
    pub index: usize,
    pub delta: BlockDelta,
}

/// Delta variants — one per content-block type. `input_json_delta`
/// streams the tool_use input as a JSON-string-fragment chain;
/// concatenation across deltas yields the full input JSON, parsed at
/// `content_block_stop` time.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockDelta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
    },
    /// Citation metadata for a text block. Nothing downstream reads it.
    CitationsDelta {},
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ContentBlockStopPayload {
    pub index: usize,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessageDeltaPayload {
    pub delta: MessageDeltaInner,
    /// Cumulative usage at message close. Each field is present only when
    /// the API sent it: `input_tokens` and the cache counts ride here on some
    /// replies and are absent on others.
    #[serde(default)]
    pub usage: DeltaUsage,
}

/// The `usage` object of a `message_delta`, with absent fields kept absent
/// so a field the API left out does not overwrite `message_start`'s count.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct DeltaUsage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessageDeltaInner {
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SseError {
    pub error: SseErrorBody,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SseErrorBody {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub message: String,
}

/// What an API error type asks of the caller. The wire strings are those of
/// <https://platform.claude.com/docs/en/api/errors>; the same names arrive in
/// an HTTP error body and in a mid-stream `error` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiErrorKind {
    /// 529: the API is overloaded. A repeat may succeed.
    Overloaded,
    /// 429.
    RateLimit,
    /// 500: an internal failure. A repeat may succeed.
    Api,
    /// 504.
    Timeout,
    /// 401.
    Authentication,
    /// 403.
    Permission,
    /// 402: the account cannot pay. A repeat fails until the balance changes.
    Billing,
    /// 400, and the other 4xx statuses without a type of their own.
    InvalidRequest,
    /// 404.
    NotFound,
    /// 413.
    RequestTooLarge,
    /// A type this client does not know.
    Other(String),
}

impl ApiErrorKind {
    pub fn parse(kind: &str) -> Self {
        match kind {
            "overloaded_error" => Self::Overloaded,
            "rate_limit_error" => Self::RateLimit,
            "api_error" => Self::Api,
            "timeout_error" => Self::Timeout,
            "authentication_error" => Self::Authentication,
            "permission_error" => Self::Permission,
            "billing_error" => Self::Billing,
            "invalid_request_error" => Self::InvalidRequest,
            "not_found_error" => Self::NotFound,
            "request_too_large" => Self::RequestTooLarge,
            other => Self::Other(other.to_string()),
        }
    }

    /// The stream error kind this API error type becomes.
    pub fn stream_kind(&self) -> StreamErrorKind {
        match self {
            Self::Overloaded => StreamErrorKind::Overloaded,
            Self::RateLimit => StreamErrorKind::RateLimited,
            Self::Api => StreamErrorKind::ServerError,
            Self::Timeout => StreamErrorKind::Timeout,
            Self::Authentication | Self::Permission => StreamErrorKind::Auth,
            Self::Billing => StreamErrorKind::Billing,
            Self::InvalidRequest | Self::NotFound | Self::RequestTooLarge => StreamErrorKind::InvalidRequest,
            Self::Other(kind) => StreamErrorKind::Unknown(kind.clone()),
        }
    }

    /// The status that tells an endpoint it is busy, for the kinds that mean
    /// "too much load": the limiter cools down on 429 and 503 only, so an
    /// overloaded 529 is reported as 503.
    pub fn busy_status(&self) -> Option<u16> {
        match self {
            Self::Overloaded => Some(503),
            Self::RateLimit => Some(429),
            _ => None,
        }
    }
}

impl SseErrorBody {
    pub fn class(&self) -> ApiErrorKind {
        ApiErrorKind::parse(&self.kind)
    }
}

/// Errors from the SSE layer.
#[derive(Debug, thiserror::Error)]
pub enum SseDecodeError {
    /// A content block type this client cannot represent. The stream fails
    /// rather than dropping content the model produced.
    #[error("unsupported content block type: {0}")]
    UnsupportedBlock(String),
    /// A delta type this client cannot represent.
    #[error("unsupported content block delta type: {0}")]
    UnsupportedDelta(String),
    #[error("invalid JSON in event {event}: {source}")]
    InvalidJson {
        event: String,
        #[source]
        source: serde_json::Error,
    },
}

/// Decode one [`eventsource_stream::Event`] into a typed
/// [`ClaudeSseEvent`].
///
/// An unknown event name decodes to [`ClaudeSseEvent::Unknown`]: the API
/// versioning policy adds event types over time, and none of them carries
/// content a block writer needs. An unknown content block or delta type does
/// fail, because dropping it would drop model output.
pub fn decode_event(event: &eventsource_stream::Event) -> Result<ClaudeSseEvent, SseDecodeError> {
    match event.event.as_str() {
        "message_start" => parse_json(&event.event, &event.data).map(ClaudeSseEvent::MessageStart),
        "content_block_start" => {
            check_tag(&event.data, "/content_block/type", SseDecodeError::UnsupportedBlock, &[
                "text", "thinking", "redacted_thinking", "tool_use",
            ])?;
            parse_json(&event.event, &event.data).map(ClaudeSseEvent::ContentBlockStart)
        }
        "content_block_delta" => {
            check_tag(&event.data, "/delta/type", SseDecodeError::UnsupportedDelta, &[
                "text_delta", "input_json_delta", "thinking_delta", "signature_delta",
                "citations_delta",
            ])?;
            parse_json(&event.event, &event.data).map(ClaudeSseEvent::ContentBlockDelta)
        }
        "content_block_stop" => {
            parse_json(&event.event, &event.data).map(ClaudeSseEvent::ContentBlockStop)
        }
        "message_delta" => parse_json(&event.event, &event.data).map(ClaudeSseEvent::MessageDelta),
        "message_stop" => Ok(ClaudeSseEvent::MessageStop),
        "error" => parse_json(&event.event, &event.data).map(ClaudeSseEvent::Error),
        "ping" => Ok(ClaudeSseEvent::Ping),
        other => Ok(ClaudeSseEvent::Unknown(other.to_string())),
    }
}

/// Fail with `unsupported(tag)` when the JSON at `pointer` names a type
/// outside `known`. Data that does not parse falls through to the typed
/// decode, which reports the JSON error.
fn check_tag(
    data: &str,
    pointer: &str,
    unsupported: fn(String) -> SseDecodeError,
    known: &[&str],
) -> Result<(), SseDecodeError> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
        return Ok(());
    };
    match value.pointer(pointer).and_then(|t| t.as_str()) {
        Some(tag) if !known.contains(&tag) => Err(unsupported(tag.to_string())),
        _ => Ok(()),
    }
}

fn parse_json<T: for<'de> serde::Deserialize<'de>>(
    event: &str,
    data: &str,
) -> Result<T, SseDecodeError> {
    serde_json::from_str(data).map_err(|source| SseDecodeError::InvalidJson {
        event: event.to_string(),
        source,
    })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::stream::RetryDisposition;
    use eventsource_stream::Eventsource;
    use futures::StreamExt;
    use std::convert::Infallible;

    /// Drive a byte payload through eventsource-stream + decode_event,
    /// collecting the typed results. Mirrors the path the live wire
    /// layer uses, so these tests fail if either the SSE framing or the
    /// JSON shape shifts.
    async fn parse_all(payload: &str) -> Vec<Result<ClaudeSseEvent, SseDecodeError>> {
        let bytes = bytes::Bytes::from(payload.to_string());
        let stream =
            futures::stream::iter(vec![Ok::<_, Infallible>(bytes)]).eventsource();
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

    /// Drive a byte payload split across multiple chunks. SSE framing
    /// must tolerate event boundaries falling anywhere in the byte
    /// stream — this is the realistic transport case.
    async fn parse_chunked(chunks: Vec<&str>) -> Vec<Result<ClaudeSseEvent, SseDecodeError>> {
        let items: Vec<Result<bytes::Bytes, Infallible>> =
            chunks.into_iter().map(|c| Ok(bytes::Bytes::from(c.to_string()))).collect();
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

    const SIMPLE_COMPLETION: &str = "\
event: message_start
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-haiku-4-5\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":12,\"output_tokens\":1}}}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\", world\"}}

event: content_block_stop
data: {\"type\":\"content_block_stop\",\"index\":0}

event: message_delta
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":7}}

event: message_stop
data: {\"type\":\"message_stop\"}

";

    #[tokio::test]
    async fn parses_simple_text_completion_sequence() {
        let events: Vec<_> = parse_all(SIMPLE_COMPLETION)
            .await
            .into_iter()
            .map(|r| r.expect("decode must succeed"))
            .collect();

        assert_eq!(events.len(), 7);
        match &events[0] {
            ClaudeSseEvent::MessageStart(p) => {
                assert_eq!(p.message.id, "msg_01");
                assert_eq!(p.message.model, "claude-haiku-4-5");
                assert_eq!(p.message.usage.input_tokens, 12);
            }
            other => panic!("expected MessageStart, got {other:?}"),
        }
        match &events[1] {
            ClaudeSseEvent::ContentBlockStart(p) => {
                assert_eq!(p.index, 0);
                assert!(matches!(p.content_block, StartedBlock::Text { .. }));
            }
            other => panic!("expected ContentBlockStart, got {other:?}"),
        }
        match &events[2] {
            ClaudeSseEvent::ContentBlockDelta(p) => match &p.delta {
                BlockDelta::TextDelta { text } => assert_eq!(text, "Hello"),
                other => panic!("expected TextDelta, got {other:?}"),
            },
            other => panic!("expected ContentBlockDelta, got {other:?}"),
        }
        assert!(matches!(events[6], ClaudeSseEvent::MessageStop));
    }

    const TOOL_USE_SEQUENCE: &str = "\
event: message_start
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_02\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-haiku-4-5\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":50,\"output_tokens\":1}}}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_ABC\",\"name\":\"get_weather\",\"input\":{}}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"location\\\":\\\"\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"Tokyo\\\"}\"}}

event: content_block_stop
data: {\"type\":\"content_block_stop\",\"index\":0}

event: message_delta
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":20}}

event: message_stop
data: {\"type\":\"message_stop\"}

";

    #[tokio::test]
    async fn parses_tool_use_sequence_with_input_json_deltas() {
        let events: Vec<_> = parse_all(TOOL_USE_SEQUENCE)
            .await
            .into_iter()
            .map(|r| r.expect("decode must succeed"))
            .collect();

        // ContentBlockStart for tool_use carries id and name; input is empty.
        match &events[1] {
            ClaudeSseEvent::ContentBlockStart(p) => match &p.content_block {
                StartedBlock::ToolUse { id, name, input } => {
                    assert_eq!(id, "toolu_ABC");
                    assert_eq!(name, "get_weather");
                    assert!(input.as_object().is_some_and(|o| o.is_empty()));
                }
                other => panic!("expected ToolUse, got {other:?}"),
            },
            other => panic!("expected ContentBlockStart, got {other:?}"),
        }
        // Two InputJsonDelta partials whose concatenation is valid JSON.
        let parts: Vec<String> = events[2..=3]
            .iter()
            .map(|ev| match ev {
                ClaudeSseEvent::ContentBlockDelta(p) => match &p.delta {
                    BlockDelta::InputJsonDelta { partial_json } => partial_json.clone(),
                    other => panic!("expected InputJsonDelta, got {other:?}"),
                },
                other => panic!("expected ContentBlockDelta, got {other:?}"),
            })
            .collect();
        let assembled: String = parts.join("");
        let parsed: serde_json::Value = serde_json::from_str(&assembled)
            .expect("concatenated partial_json must parse as valid JSON");
        assert_eq!(parsed["location"], "Tokyo");

        // message_delta carries stop_reason: tool_use
        match &events[5] {
            ClaudeSseEvent::MessageDelta(p) => {
                assert_eq!(p.delta.stop_reason.as_deref(), Some("tool_use"));
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    const THINKING_SEQUENCE: &str = "\
event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me reason about this.\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_xyz\"}}

event: content_block_stop
data: {\"type\":\"content_block_stop\",\"index\":0}

";

    #[tokio::test]
    async fn parses_extended_thinking_with_signature() {
        let events: Vec<_> = parse_all(THINKING_SEQUENCE)
            .await
            .into_iter()
            .map(|r| r.expect("decode must succeed"))
            .collect();
        assert_eq!(events.len(), 4);
        match &events[0] {
            ClaudeSseEvent::ContentBlockStart(p) => {
                assert!(matches!(p.content_block, StartedBlock::Thinking { .. }));
            }
            other => panic!("expected thinking block, got {other:?}"),
        }
        match &events[1] {
            ClaudeSseEvent::ContentBlockDelta(p) => match &p.delta {
                BlockDelta::ThinkingDelta { thinking } => {
                    assert_eq!(thinking, "Let me reason about this.");
                }
                other => panic!("expected ThinkingDelta, got {other:?}"),
            },
            other => panic!("expected delta, got {other:?}"),
        }
        match &events[2] {
            ClaudeSseEvent::ContentBlockDelta(p) => match &p.delta {
                BlockDelta::SignatureDelta { signature } => {
                    assert_eq!(signature, "sig_xyz");
                }
                other => panic!("expected SignatureDelta, got {other:?}"),
            },
            other => panic!("expected delta, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ping_events_decode_to_ping_variant() {
        let payload = "event: ping\ndata: {\"type\":\"ping\"}\n\n";
        let events: Vec<_> = parse_all(payload)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ClaudeSseEvent::Ping));
    }

    #[tokio::test]
    async fn error_events_decode_with_typed_payload() {
        let payload = "\
event: error
data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"servers are busy\"}}

";
        let events: Vec<_> = parse_all(payload)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        match &events[0] {
            ClaudeSseEvent::Error(e) => {
                assert_eq!(e.error.kind, "overloaded_error");
                assert_eq!(e.error.message, "servers are busy");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unknown_event_name_decodes_to_unknown() {
        let payload = "event: future_event\ndata: {\"type\":\"future_event\"}\n\n";
        let events = parse_all(payload).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            Ok(ClaudeSseEvent::Unknown(name)) => assert_eq!(name, "future_event"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unknown_content_block_type_is_a_typed_decode_error() {
        let payload = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"fallback\"}}\n\n";
        match &parse_all(payload).await[0] {
            Err(SseDecodeError::UnsupportedBlock(name)) => assert_eq!(name, "fallback"),
            other => panic!("expected UnsupportedBlock, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unknown_delta_type_is_a_typed_decode_error() {
        let payload = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"compaction_delta\"}}\n\n";
        match &parse_all(payload).await[0] {
            Err(SseDecodeError::UnsupportedDelta(name)) => assert_eq!(name, "compaction_delta"),
            other => panic!("expected UnsupportedDelta, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_redacted_thinking_block_and_a_citations_delta_decode() {
        let payload = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"EmwK\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"citations_delta\",\"citation\":{\"x\":1}}}\n\n";
        let events = parse_all(payload).await;
        match &events[0] {
            Ok(ClaudeSseEvent::ContentBlockStart(p)) => {
                assert_eq!(p.content_block, StartedBlock::RedactedThinking { data: "EmwK".into() });
            }
            other => panic!("expected redacted block, got {other:?}"),
        }
        assert!(matches!(&events[1], Ok(ClaudeSseEvent::ContentBlockDelta(p)) if p.delta == BlockDelta::CitationsDelta {}));
    }

    #[test]
    fn error_types_map_to_distinguishable_kinds() {
        let cases = [
            ("overloaded_error", ApiErrorKind::Overloaded, true, Some(503)),
            ("rate_limit_error", ApiErrorKind::RateLimit, true, Some(429)),
            ("api_error", ApiErrorKind::Api, true, None),
            ("timeout_error", ApiErrorKind::Timeout, true, None),
            ("billing_error", ApiErrorKind::Billing, false, None),
            ("authentication_error", ApiErrorKind::Authentication, false, None),
            ("permission_error", ApiErrorKind::Permission, false, None),
            ("invalid_request_error", ApiErrorKind::InvalidRequest, false, None),
            ("request_too_large", ApiErrorKind::RequestTooLarge, false, None),
            ("not_found_error", ApiErrorKind::NotFound, false, None),
        ];
        for (wire, kind, transient, busy) in cases {
            let parsed = ApiErrorKind::parse(wire);
            assert_eq!(parsed, kind, "{wire}");
            assert_eq!(
                parsed.stream_kind().retry_disposition() == RetryDisposition::Transient,
                transient,
                "{wire}"
            );
            assert_eq!(parsed.busy_status(), busy, "{wire}");
        }
        let other = ApiErrorKind::parse("brand_new_error");
        assert_eq!(other, ApiErrorKind::Other("brand_new_error".into()));
        assert_eq!(other.stream_kind(), StreamErrorKind::Unknown("brand_new_error".into()));
        assert_eq!(other.stream_kind().retry_disposition(), RetryDisposition::Permanent, "an unknown type is not retried");
    }

    #[tokio::test]
    async fn message_delta_usage_keeps_absent_fields_absent() {
        let payload = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":9,\"service_tier\":\"standard\"}}\n\n";
        match &parse_all(payload).await[0] {
            Ok(ClaudeSseEvent::MessageDelta(p)) => {
                assert_eq!(p.usage.output_tokens, Some(9));
                assert_eq!(p.usage.input_tokens, None);
                assert_eq!(p.usage.cache_read_input_tokens, None);
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_json_in_known_event_surfaces_typed_error() {
        let payload = "event: message_stop\ndata: not json at all\n\n";
        // message_stop has no payload, but trying it on an event that does
        // verifies the json-error path; pick content_block_stop with bad JSON.
        let payload = format!(
            "{}event: content_block_stop\ndata: not-json\n\n",
            payload
        );
        let events = parse_all(&payload).await;
        // message_stop has no JSON parse (it's hard-coded Ok); the second
        // event triggers the InvalidJson error.
        let bad = events
            .iter()
            .find(|r| matches!(r, Err(SseDecodeError::InvalidJson { .. })))
            .expect("expected at least one InvalidJson error");
        match bad {
            Err(SseDecodeError::InvalidJson { event, .. }) => {
                assert_eq!(event, "content_block_stop");
            }
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn chunked_transport_reassembles_event_boundaries() {
        // SSE bytes split mid-event must still parse — this is the case
        // that real reqwest streams hit. Split arbitrarily across the
        // "data:" line and the blank-line terminator.
        let chunks = vec![
            "event: content_block_delta\ndata: {\"type\":\"content_b",
            "lock_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"te",
            "xt\":\"split chunks\"}}\n\n",
        ];
        let events: Vec<_> = parse_chunked(chunks)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ClaudeSseEvent::ContentBlockDelta(p) => match &p.delta {
                BlockDelta::TextDelta { text } => assert_eq!(text, "split chunks"),
                other => panic!("expected TextDelta, got {other:?}"),
            },
            other => panic!("expected ContentBlockDelta, got {other:?}"),
        }
    }
}
