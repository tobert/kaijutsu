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
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ContentBlockStopPayload {
    pub index: usize,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessageDeltaPayload {
    pub delta: MessageDeltaInner,
    /// Output-side usage at message close. Anthropic omits
    /// `input_tokens` here (already reported in `message_start`); the
    /// `ResponseUsage` default fills 0.
    #[serde(default)]
    pub usage: ResponseUsage,
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
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SseErrorBody {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

/// Errors from the SSE layer.
#[derive(Debug, thiserror::Error)]
pub enum SseDecodeError {
    #[error("unknown event type: {0}")]
    UnknownEvent(String),
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
/// Unknown event names produce [`SseDecodeError::UnknownEvent`] rather
/// than silently dropping — surfacing the mismatch is the point per the
/// crash-over-corrupt principle.
pub fn decode_event(event: &eventsource_stream::Event) -> Result<ClaudeSseEvent, SseDecodeError> {
    match event.event.as_str() {
        "message_start" => parse_json(&event.event, &event.data).map(ClaudeSseEvent::MessageStart),
        "content_block_start" => {
            parse_json(&event.event, &event.data).map(ClaudeSseEvent::ContentBlockStart)
        }
        "content_block_delta" => {
            parse_json(&event.event, &event.data).map(ClaudeSseEvent::ContentBlockDelta)
        }
        "content_block_stop" => {
            parse_json(&event.event, &event.data).map(ClaudeSseEvent::ContentBlockStop)
        }
        "message_delta" => parse_json(&event.event, &event.data).map(ClaudeSseEvent::MessageDelta),
        "message_stop" => Ok(ClaudeSseEvent::MessageStop),
        "error" => parse_json(&event.event, &event.data).map(ClaudeSseEvent::Error),
        "ping" => Ok(ClaudeSseEvent::Ping),
        other => Err(SseDecodeError::UnknownEvent(other.to_string())),
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
    async fn unknown_event_name_surfaces_decode_error() {
        // Anthropic shipping a new event we haven't taught the decoder
        // about must fail loudly, not silently drop.
        let payload = "event: future_event\ndata: {\"type\":\"future_event\"}\n\n";
        let events = parse_all(payload).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            Err(SseDecodeError::UnknownEvent(name)) => assert_eq!(name, "future_event"),
            other => panic!("expected UnknownEvent error, got {other:?}"),
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
