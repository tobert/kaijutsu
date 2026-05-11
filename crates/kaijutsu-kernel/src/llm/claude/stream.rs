//! Anthropic SSE → kaijutsu [`StreamEvent`] state machine.
//!
//! The CRDT block writer in `kaijutsu-server` needs bracketed
//! `*Start` / `*Delta` / `*End` events for each text/thinking block,
//! atomic `ToolUse { id, name, input }` events for tool calls (input
//! assembled across `input_json_delta` partials), and a terminal
//! `Done` / `Error`. Anthropic's wire shape is similar but not
//! identical — this module bridges the gap.
//!
//! ```text
//! Anthropic SSE                               kaijutsu StreamEvent
//! ─────────────                               ────────────────────
//! message_start                               (capture usage, no emit)
//! content_block_start (text)                  TextStart
//! content_block_delta (text_delta)            TextDelta
//! content_block_stop                          TextEnd
//! content_block_start (thinking)              ThinkingStart
//! content_block_delta (thinking_delta)        ThinkingDelta
//! content_block_delta (signature_delta)       (buffered, attached to Reasoning later)
//! content_block_stop                          ThinkingEnd
//! content_block_start (tool_use)              (buffer id, name; collect partials)
//! content_block_delta (input_json_delta)      (append partial_json)
//! content_block_stop                          ToolUse { id, name, input }
//! message_delta                               (capture stop_reason + usage, no emit)
//! message_stop                                Done { ... }
//! error                                       Error(String)
//! ping                                        (no emit)
//! ```
//!
//! Signature bytes from `signature_delta` accumulate across deltas
//! (Anthropic emits one in practice, but the `delta` suffix on the
//! event name suggests they reserve the right to split it) and emit
//! on `content_block_stop` as part of [`StreamEvent::ThinkingEnd`].
//! The server-side block writer captures the signature into
//! [`crate::llm::ContentBlock::Reasoning`] on the assistant message,
//! so the next agentic-loop iteration echoes the reasoning chain back
//! with its verifying signature.

use std::collections::HashMap;

use crate::llm::stream::StreamEvent;

use super::sse::{BlockDelta, ClaudeSseEvent, StartedBlock};

/// Per-content-block state we need to remember between `start`,
/// `delta`, and `stop` events.
#[derive(Debug, Clone)]
enum BlockState {
    Text,
    Thinking {
        /// Accumulated `signature_delta` payload. Anthropic emits a
        /// single signature_delta per thinking block in practice, but
        /// the wire-event name reserves room to split it across
        /// multiple deltas, so we concatenate defensively. Surfaced
        /// on `content_block_stop` via [`StreamEvent::ThinkingEnd`].
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
        /// Concatenated `partial_json` fragments from `input_json_delta`
        /// events. Parsed at `content_block_stop` time. Empty after a
        /// bare `content_block_start` for tool_use means "no arguments"
        /// — we synthesize `{}` so the model's tool call still validates.
        partial_input: String,
    },
}

/// State-machine driver. Takes a [`ClaudeSseEvent`], updates internal
/// state, and returns zero or more [`StreamEvent`]s to forward.
///
/// Most SSE events produce 0 or 1 kaijutsu events; the only 2-event
/// case is `content_block_stop` for a tool_use block (emits a single
/// `ToolUse` event from accumulated state) where the next iteration
/// might need to close another open block first — but Anthropic only
/// opens one block at a time per index, so single-event emission is
/// sufficient.
#[derive(Debug, Default)]
pub struct StateMachine {
    blocks: HashMap<usize, BlockState>,
    /// Final stop reason from `message_delta`.
    stop_reason: Option<String>,
    /// Input tokens captured at `message_start`. Anthropic reports them
    /// once at message open; `message_delta` carries only output.
    input_tokens: Option<u64>,
    /// Output tokens — updated by `message_delta` (final value).
    output_tokens: Option<u64>,
}

impl StateMachine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process one SSE event, returning the corresponding
    /// [`StreamEvent`]s.
    ///
    /// Returns a `Vec` rather than an `Option` to leave room for future
    /// multi-emit events (e.g. emitting a `Done` synthesized from
    /// `message_stop` + remembered usage). Phase 2 only emits 0 or 1.
    pub fn step(&mut self, event: ClaudeSseEvent) -> Vec<StreamEvent> {
        match event {
            ClaudeSseEvent::MessageStart(p) => {
                self.input_tokens = Some(p.message.usage.input_tokens);
                vec![]
            }
            ClaudeSseEvent::ContentBlockStart(p) => self.on_block_start(p.index, p.content_block),
            ClaudeSseEvent::ContentBlockDelta(p) => self.on_block_delta(p.index, p.delta),
            ClaudeSseEvent::ContentBlockStop(p) => self.on_block_stop(p.index),
            ClaudeSseEvent::MessageDelta(p) => {
                self.stop_reason = p.delta.stop_reason;
                // message_delta carries only output_tokens (Anthropic
                // omits input_tokens here); overwrite is intentional —
                // it's the final value.
                self.output_tokens = Some(p.usage.output_tokens);
                vec![]
            }
            ClaudeSseEvent::MessageStop => vec![StreamEvent::Done {
                stop_reason: self.stop_reason.clone(),
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
            }],
            ClaudeSseEvent::Error(e) => vec![StreamEvent::Error(format!(
                "{}: {}",
                e.error.kind, e.error.message
            ))],
            ClaudeSseEvent::Ping => vec![],
        }
    }

    fn on_block_start(&mut self, index: usize, block: StartedBlock) -> Vec<StreamEvent> {
        match block {
            StartedBlock::Text { .. } => {
                self.blocks.insert(index, BlockState::Text);
                vec![StreamEvent::TextStart]
            }
            StartedBlock::Thinking { .. } => {
                self.blocks.insert(
                    index,
                    BlockState::Thinking {
                        signature: String::new(),
                    },
                );
                vec![StreamEvent::ThinkingStart]
            }
            StartedBlock::ToolUse { id, name, .. } => {
                self.blocks.insert(
                    index,
                    BlockState::ToolUse {
                        id,
                        name,
                        partial_input: String::new(),
                    },
                );
                vec![]
            }
        }
    }

    fn on_block_delta(&mut self, index: usize, delta: BlockDelta) -> Vec<StreamEvent> {
        let Some(state) = self.blocks.get_mut(&index) else {
            // Anthropic doesn't send deltas for blocks we never opened —
            // surface the inconsistency so a wire-shape shift fails
            // loudly rather than silently dropping content.
            return vec![StreamEvent::Error(format!(
                "received content_block_delta for unknown index {index}",
            ))];
        };
        match (state, delta) {
            (BlockState::Text, BlockDelta::TextDelta { text }) => vec![StreamEvent::TextDelta(text)],
            (BlockState::Thinking { .. }, BlockDelta::ThinkingDelta { thinking }) => {
                vec![StreamEvent::ThinkingDelta(thinking)]
            }
            (BlockState::Thinking { signature }, BlockDelta::SignatureDelta { signature: sig }) => {
                // Accumulate; emit at content_block_stop alongside the
                // ThinkingEnd event so the assistant message can carry
                // it on the next agentic-loop iteration.
                signature.push_str(&sig);
                vec![]
            }
            (
                BlockState::ToolUse { partial_input, .. },
                BlockDelta::InputJsonDelta { partial_json },
            ) => {
                partial_input.push_str(&partial_json);
                vec![]
            }
            (state, delta) => vec![StreamEvent::Error(format!(
                "content_block_delta type mismatch at index {index}: block={state:?}, delta={delta:?}",
            ))],
        }
    }

    fn on_block_stop(&mut self, index: usize) -> Vec<StreamEvent> {
        let Some(state) = self.blocks.remove(&index) else {
            return vec![StreamEvent::Error(format!(
                "received content_block_stop for unknown index {index}",
            ))];
        };
        match state {
            BlockState::Text => vec![StreamEvent::TextEnd],
            BlockState::Thinking { signature } => {
                let signature = (!signature.is_empty()).then_some(signature);
                vec![StreamEvent::ThinkingEnd { signature }]
            }
            BlockState::ToolUse {
                id,
                name,
                partial_input,
            } => {
                let input = if partial_input.is_empty() {
                    serde_json::Value::Object(serde_json::Map::new())
                } else {
                    match serde_json::from_str(&partial_input) {
                        Ok(v) => v,
                        Err(e) => {
                            return vec![StreamEvent::Error(format!(
                                "tool_use input JSON parse failed for {name} ({id}): {e}",
                            ))];
                        }
                    }
                };
                vec![StreamEvent::ToolUse { id, name, input }]
            }
        }
    }
}

// ============================================================================
// Tests — drive fixtures through SSE parser + state machine and assert
// the StreamEvent sequence the CRDT block writer will see.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::claude::sse::decode_event;
    use eventsource_stream::Eventsource;
    use futures::StreamExt;
    use std::convert::Infallible;

    /// Drive SSE bytes → typed events → state machine → flatten.
    async fn run(payload: &str) -> Vec<StreamEvent> {
        let bytes = bytes::Bytes::from(payload.to_string());
        let stream =
            futures::stream::iter(vec![Ok::<_, Infallible>(bytes)]).eventsource();
        let mut sm = StateMachine::new();
        let mut out = Vec::new();
        let mut stream = Box::pin(stream);
        while let Some(item) = stream.next().await {
            let event = decode_event(&item.expect("SSE parse")).expect("decode");
            out.extend(sm.step(event));
        }
        out
    }

    const SIMPLE: &str = "\
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
    async fn simple_completion_emits_bracketed_text_then_done() {
        let events = run(SIMPLE).await;
        assert_eq!(
            events,
            vec![
                StreamEvent::TextStart,
                StreamEvent::TextDelta("Hello".into()),
                StreamEvent::TextDelta(", world".into()),
                StreamEvent::TextEnd,
                StreamEvent::Done {
                    stop_reason: Some("end_turn".into()),
                    input_tokens: Some(12),
                    output_tokens: Some(7),
                },
            ]
        );
    }

    const TOOL_USE: &str = "\
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
    async fn tool_use_emits_single_atomic_event_with_assembled_input() {
        let events = run(TOOL_USE).await;
        // ContentBlockStart for tool_use emits nothing; partials accumulate;
        // ContentBlockStop emits one ToolUse.
        assert_eq!(events.len(), 2);
        match &events[0] {
            StreamEvent::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_ABC");
                assert_eq!(name, "get_weather");
                assert_eq!(input["location"], "Tokyo");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        match &events[1] {
            StreamEvent::Done {
                stop_reason,
                input_tokens,
                output_tokens,
            } => {
                assert_eq!(stop_reason.as_deref(), Some("tool_use"));
                assert_eq!(*input_tokens, Some(50));
                assert_eq!(*output_tokens, Some(20));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_use_with_no_arguments_emits_empty_object() {
        // Some tools take no arguments; Anthropic emits content_block_start
        // for tool_use immediately followed by content_block_stop, no
        // input_json_delta events. The state machine must synthesize {}
        // rather than failing JSON parse on an empty string.
        let payload = "\
event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_NIL\",\"name\":\"ping\",\"input\":{}}}

event: content_block_stop
data: {\"type\":\"content_block_stop\",\"index\":0}

";
        let events = run(payload).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::ToolUse { name, input, .. } => {
                assert_eq!(name, "ping");
                assert!(input.as_object().is_some_and(|o| o.is_empty()));
            }
            other => panic!("expected ToolUse with empty input, got {other:?}"),
        }
    }

    const THINKING: &str = "\
event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"step 1\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_xyz\"}}

event: content_block_stop
data: {\"type\":\"content_block_stop\",\"index\":0}

event: message_stop
data: {\"type\":\"message_stop\"}

";

    #[tokio::test]
    async fn thinking_emits_bracketed_with_signature_attached_to_end() {
        // signature_delta accumulates inside the Thinking block state
        // and surfaces on content_block_stop attached to ThinkingEnd —
        // the server-side block writer uses it to round-trip the
        // reasoning chain back on subsequent tool-use turns.
        let events = run(THINKING).await;
        assert_eq!(
            events,
            vec![
                StreamEvent::ThinkingStart,
                StreamEvent::ThinkingDelta("step 1".into()),
                StreamEvent::ThinkingEnd {
                    signature: Some("sig_xyz".into()),
                },
                StreamEvent::Done {
                    stop_reason: None,
                    input_tokens: None,
                    output_tokens: None,
                },
            ]
        );
    }

    #[tokio::test]
    async fn thinking_without_signature_emits_none() {
        // Older / shorter Claude responses may not include a
        // signature_delta. ThinkingEnd.signature must be None so the
        // server doesn't echo a fabricated value back to Anthropic.
        let payload = "\
event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"brief\"}}

event: content_block_stop
data: {\"type\":\"content_block_stop\",\"index\":0}

";
        let events = run(payload).await;
        let end = events
            .iter()
            .find(|e| matches!(e, StreamEvent::ThinkingEnd { .. }))
            .expect("must emit ThinkingEnd");
        match end {
            StreamEvent::ThinkingEnd { signature } => assert!(signature.is_none()),
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn error_event_emits_typed_kaijutsu_error() {
        let payload = "\
event: error
data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"servers are busy\"}}

";
        let events = run(payload).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Error(s) => {
                assert!(s.contains("overloaded_error"));
                assert!(s.contains("servers are busy"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ping_events_produce_no_kaijutsu_events() {
        let payload = "event: ping\ndata: {\"type\":\"ping\"}\n\n";
        let events = run(payload).await;
        assert!(events.is_empty(), "ping must not emit anything: {events:?}");
    }

    #[tokio::test]
    async fn delta_for_unknown_block_index_surfaces_error() {
        // Wire-shape regression check: deltas arriving without a prior
        // content_block_start at that index must produce an Error event,
        // not silently drop. Catches the case where future Anthropic
        // versions add an event we didn't model.
        let payload = "\
event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":99,\"delta\":{\"type\":\"text_delta\",\"text\":\"orphan\"}}

";
        let events = run(payload).await;
        match &events[0] {
            StreamEvent::Error(s) => assert!(s.contains("unknown index 99")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_tool_input_partials_surface_parse_error() {
        // Anthropic streaming an invalid JSON fragment chain is a bug
        // (or wire-shape shift). The state machine must surface it
        // rather than emitting a bogus ToolUse with garbage input.
        let payload = "\
event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_X\",\"name\":\"f\",\"input\":{}}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{not valid\"}}

event: content_block_stop
data: {\"type\":\"content_block_stop\",\"index\":0}

";
        let events = run(payload).await;
        match &events[0] {
            StreamEvent::Error(s) => {
                assert!(s.contains("tool_use input JSON parse failed"));
                assert!(s.contains("toolu_X"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
