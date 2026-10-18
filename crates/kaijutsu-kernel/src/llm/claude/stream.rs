//! Anthropic SSE → kaijutsu [`StreamEvent`] state machine.
//!
//! The kernel block writer in `kaijutsu-server` needs bracketed
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
//! content_block_start (redacted_thinking)     ThinkingStart (data kept for ThinkingEnd)
//! content_block_stop                          ThinkingEnd { signature: redacted marker }
//! message_delta                               (capture stop_reason + usage, no emit)
//! message_stop                                (close open blocks), Done or Error
//! error                                       Error(StreamError, kind by the error type)
//! ping, unknown event names                   (no emit)
//! ```
//!
//! `message_stop` ends in `Done` for the stop reasons that complete a reply
//! (`end_turn`, `stop_sequence`, `tool_use`, `max_tokens`, `refusal`; the
//! reason rides on `Done`). It ends in `Error` for a reply that did not
//! complete: `pause_turn` (a server-tool loop this client does not run),
//! `model_context_window_exceeded` (the reply is truncated by the window),
//! a missing reason, and any reason this client does not know; the kind is
//! [`StreamErrorKind::Truncated`] holding the reason.
//!
//! After an `Error` the machine emits nothing more: no `Done` follows a
//! failure.
//!
//! The Messages API returns a redacted thinking block as opaque `data` that
//! must go back unchanged. `StreamEvent::ThinkingEnd` has one string for it,
//! so the block's `data` rides there behind [`REDACTED_THINKING_PREFIX`];
//! [`super::build`] turns it back into a `redacted_thinking` block for the
//! Anthropic dialect only. A real signature is base64 and holds neither `:`
//! nor `-`, so the marker cannot collide with one.
//!
//! A block's opening event can already carry content: text, thinking, or a
//! tool call's whole input. The machine emits it as the first delta, and a
//! tool call whose deltas never come takes the input from the opening event.
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

use crate::llm::stream::{StreamError, StreamErrorKind, StreamEvent};

use super::sse::{BlockDelta, ClaudeSseEvent, StartedBlock};

/// Marks a `ThinkingEnd` signature that carries a `redacted_thinking` block's
/// `data`, not a signature. No other provider's marker starts with it.
pub const REDACTED_THINKING_PREFIX: &str = "anthropic:redacted_thinking:";

/// `Ok` when the `stop_reason` completes a reply, `Err` with the error for a
/// reply that did not complete.
pub(super) fn check_stop_reason(stop_reason: Option<&str>) -> Result<(), StreamError> {
    let truncated = |reason: &str, message: &str| {
        Err(StreamError::new(StreamErrorKind::Truncated(reason.to_string()), message))
    };
    match stop_reason {
        Some("end_turn" | "stop_sequence" | "tool_use" | "max_tokens" | "refusal") => Ok(()),
        Some("pause_turn") => truncated(
            "pause_turn",
            "the reply paused inside a server-tool loop, which this client does not continue",
        ),
        Some("model_context_window_exceeded") => truncated(
            "model_context_window_exceeded",
            "the reply filled the model's context window and is truncated",
        ),
        Some(other) => truncated(other, "unknown stop_reason; the reply may be incomplete"),
        None => truncated("no_stop_reason", "the message ended without a stop_reason"),
    }
}

/// The error for an event that breaks the wire contract.
fn protocol(message: String) -> StreamEvent {
    StreamEvent::Error(StreamError::new(StreamErrorKind::Protocol, message))
}

/// Per-content-block state we need to remember between `start`,
/// `delta`, and `stop` events.
#[derive(Debug, Clone)]
enum BlockState {
    Text,
    /// A `redacted_thinking` block: its opaque `data`, surfaced at stop.
    RedactedThinking {
        data: String,
    },
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
        /// The input the opening event carried, when it was not empty. Used
        /// only when no `input_json_delta` follows.
        start_input: Option<serde_json::Value>,
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
/// Most SSE events produce 0 or 1 kaijutsu events. `message_stop` is the
/// exception: it closes every block still open, in index order, and then
/// emits `Done`.
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
    /// Cache accounting captured at `message_start` (Anthropic reports
    /// the cache split there alongside input tokens). Surfaced on the
    /// terminal `Done` so the telemetry layer can record cache hits
    /// instead of dropping them.
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
    /// An `Error` was emitted. Nothing follows it.
    failed: bool,
}

impl StateMachine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether an `Error` ended the reply.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Process one SSE event, returning the corresponding
    /// [`StreamEvent`]s.
    ///
    /// Returns a `Vec` because `message_stop` can emit the closing events
    /// of blocks left open before its `Done`.
    pub fn step(&mut self, event: ClaudeSseEvent) -> Vec<StreamEvent> {
        if self.failed {
            return Vec::new();
        }
        let mut out = self.dispatch(event);
        if let Some(position) = out.iter().position(|e| matches!(e, StreamEvent::Error(_))) {
            out.truncate(position + 1);
            self.failed = true;
        }
        out
    }

    fn dispatch(&mut self, event: ClaudeSseEvent) -> Vec<StreamEvent> {
        match event {
            ClaudeSseEvent::MessageStart(p) => {
                self.input_tokens = Some(p.message.usage.input_tokens);
                self.cache_read_input_tokens = p.message.usage.cache_read_input_tokens;
                self.cache_creation_input_tokens = p.message.usage.cache_creation_input_tokens;
                vec![]
            }
            ClaudeSseEvent::ContentBlockStart(p) => self.on_block_start(p.index, p.content_block),
            ClaudeSseEvent::ContentBlockDelta(p) => self.on_block_delta(p.index, p.delta),
            ClaudeSseEvent::ContentBlockStop(p) => self.on_block_stop(p.index),
            ClaudeSseEvent::MessageDelta(p) => {
                // A later `message_delta` that restates nothing keeps the reason.
                if let Some(reason) = p.delta.stop_reason {
                    self.stop_reason = Some(reason);
                }
                // `message_delta` usage is cumulative: a field it carries is
                // the final value. A field it leaves out keeps the count from
                // `message_start`.
                if let Some(n) = p.usage.output_tokens {
                    self.output_tokens = Some(n);
                }
                if let Some(n) = p.usage.input_tokens {
                    self.input_tokens = Some(n);
                }
                if let Some(n) = p.usage.cache_read_input_tokens {
                    self.cache_read_input_tokens = n;
                }
                if let Some(n) = p.usage.cache_creation_input_tokens {
                    self.cache_creation_input_tokens = n;
                }
                vec![]
            }
            ClaudeSseEvent::MessageStop => {
                // Close any block the message left open, in index order, so a
                // tool call without its `content_block_stop` still reaches the
                // runtime instead of vanishing with the state machine.
                let mut open: Vec<usize> = self.blocks.keys().copied().collect();
                open.sort_unstable();
                let mut out: Vec<StreamEvent> =
                    open.into_iter().flat_map(|index| self.on_block_stop(index)).collect();
                let extra = (self.cache_read_input_tokens > 0
                    || self.cache_creation_input_tokens > 0)
                    .then_some({
                        crate::llm::stream::UsageExtra::Claude(
                            crate::llm::stream::ClaudeUsageExtra {
                                cache_read_input_tokens: self.cache_read_input_tokens,
                                cache_creation_input_tokens: self.cache_creation_input_tokens,
                            },
                        )
                    });
                match check_stop_reason(self.stop_reason.as_deref()) {
                    Ok(()) => out.push(StreamEvent::Done {
                        stop_reason: self.stop_reason.clone(),
                        input_tokens: self.input_tokens,
                        output_tokens: self.output_tokens,
                        extra,
                    }),
                    Err(error) => out.push(StreamEvent::Error(error)),
                }
                out
            }
            ClaudeSseEvent::Error(e) => vec![StreamEvent::Error(
                StreamError::new(e.error.class().stream_kind(), e.error.message.clone())
                    .with_request_id(e.request_id),
            )],
            ClaudeSseEvent::Ping => vec![],
            ClaudeSseEvent::Unknown(name) => {
                tracing::debug!(event = %name, "ignoring an unknown Messages stream event");
                vec![]
            }
        }
    }

    fn on_block_start(&mut self, index: usize, block: StartedBlock) -> Vec<StreamEvent> {
        if self.blocks.contains_key(&index) {
            return vec![protocol(format!(
                "received content_block_start for index {index}, which is already open"
            ))];
        }
        match block {
            StartedBlock::Text { text } => {
                self.blocks.insert(index, BlockState::Text);
                let mut out = vec![StreamEvent::TextStart];
                if !text.is_empty() {
                    out.push(StreamEvent::TextDelta(text));
                }
                out
            }
            StartedBlock::Thinking { thinking } => {
                self.blocks.insert(
                    index,
                    BlockState::Thinking {
                        signature: String::new(),
                    },
                );
                let mut out = vec![StreamEvent::ThinkingStart];
                if !thinking.is_empty() {
                    out.push(StreamEvent::ThinkingDelta(thinking));
                }
                out
            }
            StartedBlock::RedactedThinking { data } => {
                self.blocks.insert(index, BlockState::RedactedThinking { data });
                vec![StreamEvent::ThinkingStart]
            }
            StartedBlock::ToolUse { id, name, input } => {
                let start_input = match input {
                    serde_json::Value::Null => None,
                    serde_json::Value::Object(ref map) if map.is_empty() => None,
                    other => Some(other),
                };
                self.blocks.insert(
                    index,
                    BlockState::ToolUse {
                        id,
                        name,
                        start_input,
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
            return vec![protocol(format!(
                "received content_block_delta for unknown index {index}",
            ))];
        };
        match (state, delta) {
            (BlockState::Text, BlockDelta::TextDelta { text }) => vec![StreamEvent::TextDelta(text)],
            (BlockState::Text, BlockDelta::CitationsDelta {}) => vec![],
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
            (state, delta) => vec![protocol(format!(
                "content_block_delta type mismatch at index {index}: block={state:?}, delta={delta:?}",
            ))],
        }
    }

    fn on_block_stop(&mut self, index: usize) -> Vec<StreamEvent> {
        let Some(state) = self.blocks.remove(&index) else {
            return vec![protocol(format!(
                "received content_block_stop for unknown index {index}",
            ))];
        };
        match state {
            BlockState::Text => vec![StreamEvent::TextEnd],
            BlockState::RedactedThinking { data } => vec![StreamEvent::ThinkingEnd {
                signature: Some(format!("{REDACTED_THINKING_PREFIX}{data}")),
            }],
            BlockState::Thinking { signature } => {
                let signature = (!signature.is_empty()).then_some(signature);
                vec![StreamEvent::ThinkingEnd { signature }]
            }
            BlockState::ToolUse {
                id,
                name,
                start_input,
                partial_input,
            } => {
                let (raw, parsed) = if partial_input.is_empty() {
                    match start_input {
                        Some(value) => (value.to_string(), Ok(value)),
                        None => (String::new(), Ok(serde_json::Value::Object(Default::default()))),
                    }
                } else {
                    let parsed = serde_json::from_str::<serde_json::Value>(&partial_input);
                    (partial_input, parsed)
                };
                // The call is real and the model waits on it, so a call whose
                // input is not a JSON object keeps its identity: the runtime
                // answers it with an error tool result instead of failing the
                // turn.
                match parsed {
                    Ok(input) if input.is_object() => vec![StreamEvent::ToolUse { id, name, input }],
                    Ok(other) => vec![StreamEvent::ToolUseInvalid {
                        id,
                        name,
                        arguments: raw,
                        error: format!("a tool input must be a JSON object, not {other}"),
                    }],
                    Err(e) => vec![StreamEvent::ToolUseInvalid {
                        id,
                        name,
                        arguments: raw,
                        error: e.to_string(),
                    }],
                }
            }
        }
    }
}

// ============================================================================
// Tests — drive fixtures through SSE parser + state machine and assert
// the StreamEvent sequence the kernel block writer will see.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::claude::sse::decode_event;
    use crate::llm::stream::RetryDisposition;
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
                    extra: None,
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
                ..
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

event: message_delta
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}

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
                    stop_reason: Some("end_turn".into()),
                    input_tokens: None,
                    output_tokens: Some(3),
                    extra: None,
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
data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"servers are busy\"},\"request_id\":\"req_9\"}

";
        let events = run(payload).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::Error(e) => {
                assert_eq!(e.kind, StreamErrorKind::Overloaded);
                assert_eq!(e.message, "servers are busy");
                assert_eq!(e.request_id.as_deref(), Some("req_9"));
                assert_eq!(e.to_string(), "overloaded_error: servers are busy (request_id: req_9)");
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
            StreamEvent::Error(e) => {
                assert_eq!(e.kind, StreamErrorKind::Protocol);
                assert!(e.message.contains("unknown index 99"), "{e}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_tool_input_partials_surface_the_call_and_its_raw_text() {
        // An invalid JSON fragment chain means the call cannot run, most
        // often because the response stopped at the output ceiling part-way
        // through the arguments. The state machine keeps the call's identity
        // and its raw text rather than emitting a bogus ToolUse with garbage
        // input; the runtime answers it with an error tool result.
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
            StreamEvent::ToolUseInvalid {
                id,
                name,
                arguments,
                error,
            } => {
                assert_eq!(id, "toolu_X");
                assert_eq!(name, "f");
                assert_eq!(arguments, "{not valid");
                assert!(!error.is_empty(), "the parser's message rides along");
            }
            other => panic!("expected ToolUseInvalid, got {other:?}"),
        }
    }

    /// The shape Anthropic sends when `max_tokens` lands inside a tool call's
    /// arguments: the block closes on a fragment cut mid-string, then the
    /// message stops with `max_tokens`.
    const TOOL_USE_CUT_AT_MAX_TOKENS: &str = "\
event: message_start
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_cut\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-haiku-4-5\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":30,\"output_tokens\":1}}}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_cut\",\"name\":\"write\",\"input\":{}}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\": \\\"/app/solve.py\\\", \"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"content\\\": \\\"import re\"}}

event: content_block_stop
data: {\"type\":\"content_block_stop\",\"index\":0}

event: message_delta
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":4096}}

event: message_stop
data: {\"type\":\"message_stop\"}

";

    #[tokio::test]
    async fn a_call_cut_at_max_tokens_surfaces_invalid_then_done() {
        let events = run(TOOL_USE_CUT_AT_MAX_TOKENS).await;
        assert_eq!(events.len(), 2, "{events:?}");
        match &events[0] {
            StreamEvent::ToolUseInvalid { id, name, arguments, error } => {
                assert_eq!(id, "toolu_cut");
                assert_eq!(name, "write");
                assert_eq!(arguments, "{\"path\": \"/app/solve.py\", \"content\": \"import re");
                assert!(error.contains("EOF while parsing a string"), "{error}");
            }
            other => panic!("expected ToolUseInvalid, got {other:?}"),
        }
        match &events[1] {
            StreamEvent::Done { stop_reason, .. } => {
                assert_eq!(stop_reason.as_deref(), Some("max_tokens"))
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// A message that stops with a block still open must not lose that block.
    /// A tool call the model made and never saw closed is still its call: it
    /// surfaces before `Done`, as `ToolUseInvalid` when its arguments are cut.
    #[tokio::test]
    async fn message_stop_closes_blocks_left_open() {
        let payload = TOOL_USE_CUT_AT_MAX_TOKENS.replace(
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "",
        );
        assert_ne!(payload, TOOL_USE_CUT_AT_MAX_TOKENS, "the fixture lost its block stop");
        let payload = payload.replacen(
            "event: content_block_start",
            "event: content_block_start\n\
             data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
             event: content_block_start",
            1,
        );
        let events = run(&payload).await;
        assert!(
            matches!(events.first(), Some(StreamEvent::TextStart)),
            "{events:?}"
        );
        let tail: Vec<_> = events[1..].iter().collect();
        assert!(
            matches!(tail.as_slice(), [
                StreamEvent::ToolUseInvalid { id, .. },
                StreamEvent::TextEnd,
                StreamEvent::Done { .. },
            ] if id == "toolu_cut"),
            "open blocks close in index order before Done: {events:?}"
        );
    }

    fn stopped_with(reason: &str) -> String {
        format!(
            "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":{reason}}},\"usage\":{{\"output_tokens\":5}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        )
    }

    /// Every stop reason that completes a reply is a `Done` carrying the
    /// reason; the runtime tells them apart by it.
    #[tokio::test]
    async fn completing_stop_reasons_end_in_done_with_the_reason() {
        for reason in ["end_turn", "stop_sequence", "tool_use", "max_tokens", "refusal"] {
            let events = run(&stopped_with(&format!("\"{reason}\""))).await;
            match events.as_slice() {
                [StreamEvent::Done { stop_reason, .. }] => {
                    assert_eq!(stop_reason.as_deref(), Some(reason))
                }
                other => panic!("{reason}: expected Done, got {other:?}"),
            }
        }
    }

    /// A reply that did not complete is an `Error` that names why, never a
    /// normal end.
    #[tokio::test]
    async fn incomplete_stop_reasons_end_in_a_named_error() {
        let cases = [
            ("\"pause_turn\"", "pause_turn"),
            ("\"model_context_window_exceeded\"", "model_context_window_exceeded"),
            ("\"some_new_reason\"", "some_new_reason"),
            ("null", "no_stop_reason"),
        ];
        for (wire, reason) in cases {
            let events = run(&stopped_with(wire)).await;
            match events.as_slice() {
                [StreamEvent::Error(e)] => {
                    assert_eq!(e.kind, StreamErrorKind::Truncated(reason.into()), "{wire}: {e}");
                    assert_eq!(e.kind.retry_disposition(), RetryDisposition::Permanent);
                }
                other => panic!("{wire}: expected one Error, got {other:?}"),
            }
        }
        let no_delta = run("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n").await;
        assert!(matches!(no_delta.as_slice(), [StreamEvent::Error(e)]
            if e.kind == StreamErrorKind::Truncated("no_stop_reason".into())), "{no_delta:?}");
    }

    #[tokio::test]
    async fn unknown_event_names_are_skipped() {
        let payload = format!(
            "event: future_event\ndata: {{\"type\":\"future_event\"}}\n\n{SIMPLE}"
        );
        let events = run(&payload).await;
        assert!(matches!(events.last(), Some(StreamEvent::Done { .. })), "{events:?}");
    }

    /// A redacted thinking block round-trips as one bracketed thinking run
    /// whose signature carries the opaque data behind the marker.
    #[tokio::test]
    async fn redacted_thinking_surfaces_its_data_behind_the_marker() {
        let payload = "\
event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"EmwKAhgB\"}}

event: content_block_stop
data: {\"type\":\"content_block_stop\",\"index\":0}

";
        let events = run(payload).await;
        assert_eq!(
            events,
            vec![
                StreamEvent::ThinkingStart,
                StreamEvent::ThinkingEnd {
                    signature: Some(format!("{REDACTED_THINKING_PREFIX}EmwKAhgB")),
                },
            ]
        );
    }

    /// `message_delta` usage is cumulative and may restate the input and
    /// cache counts; a count it leaves out keeps `message_start`'s value.
    #[tokio::test]
    async fn message_delta_usage_overrides_what_it_carries_and_keeps_the_rest() {
        let start = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"x\",\"stop_reason\":null,\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":4,\"cache_creation_input_tokens\":2,\"output_tokens\":1}}}\n\n";
        let restated = format!("{start}event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"input_tokens\":100,\"cache_read_input_tokens\":40,\"output_tokens\":7}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n");
        let events = run(&restated).await;
        assert_eq!(
            events,
            vec![StreamEvent::Done {
                stop_reason: Some("end_turn".into()),
                input_tokens: Some(100),
                output_tokens: Some(7),
                extra: Some(crate::llm::stream::UsageExtra::Claude(
                    crate::llm::stream::ClaudeUsageExtra {
                        cache_read_input_tokens: 40,
                        cache_creation_input_tokens: 2,
                    }
                )),
            }]
        );
        let kept = format!("{start}{}", stopped_with("\"end_turn\""));
        match run(&kept).await.as_slice() {
            [StreamEvent::Done { input_tokens, output_tokens, .. }] => {
                assert_eq!(*input_tokens, Some(10));
                assert_eq!(*output_tokens, Some(5));
            }
            other => panic!("{other:?}"),
        }
    }

    fn block_start(index: usize, block: &str) -> String {
        format!(
            "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":{index},\"content_block\":{block}}}\n\n"
        )
    }

    fn block_stop(index: usize) -> String {
        format!("event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{index}}}\n\n")
    }

    const ERROR_EVENT: &str = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"boom\"}}\n\n";
    const MESSAGE_STOP: &str = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    /// A failure ends the reply: a `message_stop` that follows an error event
    /// completes nothing.
    #[tokio::test]
    async fn nothing_follows_an_error_event() {
        let payload = format!("{ERROR_EVENT}{}{}", block_start(0, r#"{"type":"text","text":"late"}"#), stopped_with("\"end_turn\""));
        let events = run(&payload).await;
        assert!(matches!(events.as_slice(), [StreamEvent::Error(e)] if e.kind == StreamErrorKind::ServerError), "{events:?}");
    }

    /// An error the machine raises itself ends the reply too: the
    /// `message_stop` after a protocol error is not a `Done`.
    #[tokio::test]
    async fn nothing_follows_a_protocol_error() {
        let orphan = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":3,\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n";
        let payload = format!("{orphan}{}", stopped_with("\"end_turn\""));
        let events = run(&payload).await;
        assert!(matches!(events.as_slice(), [StreamEvent::Error(_)]), "{events:?}");
    }

    /// A later `message_delta` that restates no reason keeps the earlier one.
    #[tokio::test]
    async fn a_later_message_delta_without_a_stop_reason_keeps_the_earlier_one() {
        let payload = format!(
            "{}{}",
            stopped_with("\"end_turn\"").replace(&format!("{MESSAGE_STOP}"), ""),
            format_args!("event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":null}},\"usage\":{{\"output_tokens\":9}}}}\n\n{MESSAGE_STOP}")
        );
        let events = run(&payload).await;
        assert!(matches!(events.as_slice(), [StreamEvent::Done { stop_reason: Some(r), output_tokens: Some(9), .. }] if r == "end_turn"), "{events:?}");
    }

    #[tokio::test]
    async fn a_block_start_for_an_open_index_is_a_protocol_error() {
        let payload = format!(
            "{}{}",
            block_start(0, r#"{"type":"text","text":""}"#),
            block_start(0, r#"{"type":"text","text":""}"#)
        );
        let events = run(&payload).await;
        match events.as_slice() {
            [StreamEvent::TextStart, StreamEvent::Error(e)] => {
                assert_eq!(e.kind, StreamErrorKind::Protocol);
                assert!(e.message.contains("already open"), "{e}");
            }
            other => panic!("expected TextStart then a protocol Error, got {other:?}"),
        }
    }

    /// A tool input that parses but is not an object cannot be a call's
    /// arguments; the call keeps its identity and the runtime answers it.
    #[tokio::test]
    async fn a_tool_input_that_is_not_an_object_is_invalid() {
        for fragment in ["[1,2]", "\\\"text\\\"", "7"] {
            let payload = format!(
                "{}event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{fragment}\"}}}}\n\n{}",
                block_start(0, r#"{"type":"tool_use","id":"toolu_N","name":"f","input":{}}"#),
                block_stop(0),
            );
            let events = run(&payload).await;
            assert!(matches!(events.as_slice(), [StreamEvent::ToolUseInvalid { id, .. }] if id == "toolu_N"), "{fragment}: {events:?}");
        }
    }

    /// The opening event can carry content; none of it is dropped.
    #[tokio::test]
    async fn a_block_start_with_content_seeds_the_block() {
        let payload = format!(
            "{}{}{}{}{}{}{}",
            block_start(0, r#"{"type":"text","text":"Hel"}"#),
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n",
            block_stop(0),
            block_start(1, r#"{"type":"thinking","thinking":"hm"}"#),
            block_stop(1),
            block_start(2, r#"{"type":"tool_use","id":"toolu_S","name":"f","input":{"path":"/a"}}"#),
            block_stop(2),
        );
        let events = run(&payload).await;
        assert_eq!(
            events,
            vec![
                StreamEvent::TextStart,
                StreamEvent::TextDelta("Hel".into()),
                StreamEvent::TextDelta("lo".into()),
                StreamEvent::TextEnd,
                StreamEvent::ThinkingStart,
                StreamEvent::ThinkingDelta("hm".into()),
                StreamEvent::ThinkingEnd { signature: None },
                StreamEvent::ToolUse {
                    id: "toolu_S".into(),
                    name: "f".into(),
                    input: serde_json::json!({"path": "/a"}),
                },
            ]
        );
    }

    /// The opening input yields to deltas: input_json_delta text is the
    /// call's arguments when it arrives.
    #[tokio::test]
    async fn tool_input_deltas_win_over_the_opening_input() {
        let payload = format!(
            "{}event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{{\\\"b\\\":2}}\"}}}}\n\n{}",
            block_start(0, r#"{"type":"tool_use","id":"toolu_W","name":"f","input":{"a":1}}"#),
            block_stop(0),
        );
        let events = run(&payload).await;
        assert!(matches!(events.as_slice(), [StreamEvent::ToolUse { input, .. }] if *input == serde_json::json!({"b": 2})), "{events:?}");
    }
}
