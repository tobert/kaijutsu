//! DeepSeek SSE → kaijutsu [`StreamEvent`] state machine.
//!
//! OpenAI-style streaming does *not* bracket content blocks the way
//! Anthropic does (`content_block_start` / `_stop`). Instead each chunk's
//! `delta` carries whichever of `reasoning_content` / `content` /
//! `tool_calls` is active. This state machine reconstructs the bracketed
//! `*Start` / `*Delta` / `*End` lifecycle kaijutsu's CRDT writer expects
//! by tracking *which* block is currently open and closing it when the
//! active field changes:
//!
//! ```text
//! DeepSeek delta field            transition           emitted
//! ────────────────────            ──────────           ───────
//! reasoning_content (first)       None → Thinking      ThinkingStart, ThinkingDelta
//! reasoning_content (more)        Thinking             ThinkingDelta
//! content (after reasoning)       Thinking → Text      ThinkingEnd{None}, TextStart, TextDelta
//! content (more)                  Text                 TextDelta
//! tool_calls                      * → None             (close open block); accumulate
//! finish_reason present           close + flush        TextEnd/ThinkingEnd, ToolUse…
//! [DONE]                          terminal             Done{usage}
//! ```
//!
//! Tool-call arguments stream as JSON-string fragments keyed by
//! `tool_calls[].index`; they accumulate and parse at flush time
//! (`finish_reason`), emitting one atomic [`StreamEvent::ToolUse`] each —
//! mirroring the Claude path's `content_block_stop` behavior.
//!
//! DeepSeek `reasoning_content` never carries a verification signature
//! (and must never be echoed back — see [`super::build`]), so every
//! [`StreamEvent::ThinkingEnd`] this machine emits has `signature: None`.

use std::collections::BTreeMap;

use crate::llm::stream::{OpenAiCompatUsageExtra, StreamEvent, UsageExtra};

use super::sse::OpenAiSseEvent;
use super::types::{ChunkChoice, ToolCallChunk, Usage};

/// Which content block is currently open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    None,
    Thinking,
    Text,
}

/// Accumulated state for one streamed tool call, keyed by its `index`.
#[derive(Debug, Default, Clone)]
struct ToolAccum {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// State-machine driver. Takes a [`DeepSeekSseEvent`], updates internal
/// state, and returns zero or more [`StreamEvent`]s to forward.
#[derive(Debug, Default)]
pub struct StateMachine {
    phase: Phase,
    /// Tool calls accumulated by `index`. `BTreeMap` so flush order is
    /// the index order DeepSeek assigned.
    tool_calls: BTreeMap<usize, ToolAccum>,
    stop_reason: Option<String>,
    usage: Option<Usage>,
}

impl Default for Phase {
    fn default() -> Self {
        Phase::None
    }
}

impl StateMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn step(&mut self, event: OpenAiSseEvent) -> Vec<StreamEvent> {
        match event {
            OpenAiSseEvent::Chunk(chunk) => {
                let mut out = Vec::new();
                if chunk.usage.is_some() {
                    // The trailing usage-only chunk (or a usage field on a
                    // content chunk). Last writer wins — it's the final tally.
                    self.usage = chunk.usage;
                }
                for choice in chunk.choices {
                    self.on_choice(choice, &mut out);
                }
                out
            }
            OpenAiSseEvent::Done => {
                let mut out = Vec::new();
                // Defensive: if the stream ended without a finish_reason
                // chunk, close any block still open and flush pending tool
                // calls so we never drop content.
                self.close_open_block(&mut out);
                self.flush_tool_calls(&mut out);

                let (input_tokens, output_tokens, extra) = match &self.usage {
                    Some(u) => (
                        Some(u.prompt_tokens),
                        Some(u.completion_tokens),
                        Some(UsageExtra::OpenAiCompat(OpenAiCompatUsageExtra {
                            prompt_cache_hit_tokens: u.prompt_cache_hit_tokens,
                            prompt_cache_miss_tokens: u.prompt_cache_miss_tokens,
                            reasoning_tokens: u.reasoning_tokens(),
                        })),
                    ),
                    None => (None, None, None),
                };
                out.push(StreamEvent::Done {
                    stop_reason: self.stop_reason.clone(),
                    input_tokens,
                    output_tokens,
                    extra,
                });
                out
            }
        }
    }

    fn on_choice(&mut self, choice: ChunkChoice, out: &mut Vec<StreamEvent>) {
        let ChunkChoice {
            delta,
            finish_reason,
        } = choice;

        // reasoning_content → Thinking block.
        if let Some(rc) = delta.reasoning_content.filter(|s| !s.is_empty()) {
            if self.phase != Phase::Thinking {
                self.close_open_block(out);
                out.push(StreamEvent::ThinkingStart);
                self.phase = Phase::Thinking;
            }
            out.push(StreamEvent::ThinkingDelta(rc));
        }

        // content → Text block.
        if let Some(ct) = delta.content.filter(|s| !s.is_empty()) {
            if self.phase != Phase::Text {
                self.close_open_block(out);
                out.push(StreamEvent::TextStart);
                self.phase = Phase::Text;
            }
            out.push(StreamEvent::TextDelta(ct));
        }

        // tool_calls → accumulate (no immediate emit). Opening a tool call
        // closes any open text/thinking block.
        if let Some(tcs) = delta.tool_calls {
            if self.phase != Phase::None {
                self.close_open_block(out);
            }
            for tc in tcs {
                self.accumulate_tool_call(tc);
            }
        }

        // finish_reason → close the open block and flush accumulated tools.
        if let Some(fr) = finish_reason {
            self.close_open_block(out);
            self.flush_tool_calls(out);
            self.stop_reason = Some(fr);
        }
    }

    /// Emit the closing bracket for whatever block is open, then reset to
    /// [`Phase::None`]. Idempotent — a second call with nothing open is a
    /// no-op.
    fn close_open_block(&mut self, out: &mut Vec<StreamEvent>) {
        match self.phase {
            Phase::Thinking => out.push(StreamEvent::ThinkingEnd { signature: None }),
            Phase::Text => out.push(StreamEvent::TextEnd),
            Phase::None => {}
        }
        self.phase = Phase::None;
    }

    fn accumulate_tool_call(&mut self, tc: ToolCallChunk) {
        let entry = self.tool_calls.entry(tc.index).or_default();
        if let Some(id) = tc.id {
            entry.id = Some(id);
        }
        if let Some(func) = tc.function {
            if let Some(name) = func.name {
                entry.name = Some(name);
            }
            if let Some(args) = func.arguments {
                entry.arguments.push_str(&args);
            }
        }
    }

    /// Drain accumulated tool calls into atomic [`StreamEvent::ToolUse`]
    /// events (index order). A missing name or unparseable arguments
    /// surfaces a loud [`StreamEvent::Error`] rather than a bogus call.
    fn flush_tool_calls(&mut self, out: &mut Vec<StreamEvent>) {
        let drained = std::mem::take(&mut self.tool_calls);
        for (index, accum) in drained {
            let Some(name) = accum.name else {
                out.push(StreamEvent::Error(format!(
                    "tool call at index {index} arrived without a function name",
                )));
                continue;
            };
            // DeepSeek always sends an id; fall back to an index-derived
            // correlation key for OpenAI-compatible servers that omit it.
            let id = accum.id.unwrap_or_else(|| format!("call_{index}"));
            let input = if accum.arguments.trim().is_empty() {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                match serde_json::from_str(&accum.arguments) {
                    Ok(v) => v,
                    Err(e) => {
                        out.push(StreamEvent::Error(format!(
                            "tool_call input JSON parse failed for {name} ({id}): {e}",
                        )));
                        continue;
                    }
                }
            };
            out.push(StreamEvent::ToolUse { id, name, input });
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
    use crate::llm::openai::sse::decode_event;
    use eventsource_stream::Eventsource;
    use futures::StreamExt;
    use std::convert::Infallible;

    async fn run(payload: &str) -> Vec<StreamEvent> {
        let bytes = bytes::Bytes::from(payload.to_string());
        let stream = futures::stream::iter(vec![Ok::<_, Infallible>(bytes)]).eventsource();
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
data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"content\":\", world\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}

data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"total_tokens\":13,\"prompt_cache_hit_tokens\":8,\"prompt_cache_miss_tokens\":2}}

data: [DONE]

";

    #[tokio::test]
    async fn simple_completion_brackets_text_then_done_with_usage() {
        let events = run(SIMPLE).await;
        assert_eq!(
            events,
            vec![
                StreamEvent::TextStart,
                StreamEvent::TextDelta("Hello".into()),
                StreamEvent::TextDelta(", world".into()),
                StreamEvent::TextEnd,
                StreamEvent::Done {
                    stop_reason: Some("stop".into()),
                    input_tokens: Some(10),
                    output_tokens: Some(3),
                    extra: Some(UsageExtra::OpenAiCompat(OpenAiCompatUsageExtra {
                        prompt_cache_hit_tokens: 8,
                        prompt_cache_miss_tokens: 2,
                        reasoning_tokens: 0,
                    })),
                },
            ]
        );
    }

    const REASONING_THEN_TEXT: &str = "\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"let me\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"reasoning_content\":\" think\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}

data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":9,\"total_tokens\":14,\"completion_tokens_details\":{\"reasoning_tokens\":6}}}

data: [DONE]

";

    #[tokio::test]
    async fn reasoning_then_text_synthesizes_brackets_and_transition() {
        let events = run(REASONING_THEN_TEXT).await;
        assert_eq!(
            events,
            vec![
                StreamEvent::ThinkingStart,
                StreamEvent::ThinkingDelta("let me".into()),
                StreamEvent::ThinkingDelta(" think".into()),
                // transition reasoning → content closes thinking, opens text
                StreamEvent::ThinkingEnd { signature: None },
                StreamEvent::TextStart,
                StreamEvent::TextDelta("answer".into()),
                StreamEvent::TextEnd,
                StreamEvent::Done {
                    stop_reason: Some("stop".into()),
                    input_tokens: Some(5),
                    output_tokens: Some(9),
                    extra: Some(UsageExtra::OpenAiCompat(OpenAiCompatUsageExtra {
                        prompt_cache_hit_tokens: 0,
                        prompt_cache_miss_tokens: 0,
                        reasoning_tokens: 6,
                    })),
                },
            ]
        );
    }

    const TOOL_CALL: &str = "\
data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"location\\\":\\\"\"}}]},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"Tokyo\\\"}\"}}]},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}

data: [DONE]

";

    #[tokio::test]
    async fn tool_call_accumulates_by_index_and_emits_atomic_tool_use() {
        let events = run(TOOL_CALL).await;
        // No text was streamed; expect a single ToolUse then Done.
        let tool_uses: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolUse { .. }))
            .collect();
        assert_eq!(tool_uses.len(), 1);
        match tool_uses[0] {
            StreamEvent::ToolUse { id, name, input } => {
                assert_eq!(id, "call_abc");
                assert_eq!(name, "get_weather");
                assert_eq!(input["location"], "Tokyo");
            }
            _ => unreachable!(),
        }
        // Done carries tool_calls stop reason.
        match events.last().unwrap() {
            StreamEvent::Done { stop_reason, .. } => {
                assert_eq!(stop_reason.as_deref(), Some("tool_calls"));
            }
            other => panic!("expected Done last, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_call_with_empty_arguments_yields_empty_object() {
        let payload = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_nil\",\"function\":{\"name\":\"ping\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}

data: [DONE]

";
        let events = run(payload).await;
        let tu = events
            .iter()
            .find(|e| matches!(e, StreamEvent::ToolUse { .. }))
            .expect("must emit ToolUse");
        match tu {
            StreamEvent::ToolUse { name, input, .. } => {
                assert_eq!(name, "ping");
                assert!(input.as_object().is_some_and(|o| o.is_empty()));
            }
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn malformed_tool_arguments_surface_error() {
        let payload = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",\"function\":{\"name\":\"f\",\"arguments\":\"{not json\"}}]},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}

data: [DONE]

";
        let events = run(payload).await;
        let err = events
            .iter()
            .find(|e| matches!(e, StreamEvent::Error(_)))
            .expect("must surface parse error");
        match err {
            StreamEvent::Error(s) => {
                assert!(s.contains("tool_call input JSON parse failed"));
                assert!(s.contains("call_x"));
            }
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn two_parallel_tool_calls_flush_in_index_order() {
        let payload = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_0\",\"function\":{\"name\":\"a\",\"arguments\":\"{}\"}},{\"index\":1,\"id\":\"call_1\",\"function\":{\"name\":\"b\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}

data: [DONE]

";
        let events = run(payload).await;
        let names: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUse { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["a", "b"], "flush in index order");
    }

    #[tokio::test]
    async fn done_without_finish_reason_still_closes_open_text() {
        // Stream truncated: content delta then [DONE] with no finish_reason
        // chunk. The defensive close must still bracket the text.
        let payload = "\
data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}

data: [DONE]

";
        let events = run(payload).await;
        assert_eq!(
            events,
            vec![
                StreamEvent::TextStart,
                StreamEvent::TextDelta("partial".into()),
                StreamEvent::TextEnd,
                StreamEvent::Done {
                    stop_reason: None,
                    input_tokens: None,
                    output_tokens: None,
                    extra: None,
                },
            ]
        );
    }
}
