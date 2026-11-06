//! DeepSeek SSE → kaijutsu [`StreamEvent`] state machine.
//!
//! OpenAI-style streaming does *not* bracket content blocks the way
//! Anthropic does (`content_block_start` / `_stop`). Instead each chunk's
//! `delta` carries whichever of `reasoning_content` (vLLM: `reasoning`) /
//! `content` / `tool_calls` is active. This state machine reconstructs the bracketed
//! `*Start` / `*Delta` / `*End` lifecycle kaijutsu's block writer expects
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
//! [DONE] after finish_reason      terminal             Done{usage}
//! [DONE] with no finish_reason    terminal             Error(Truncated), tools dropped
//! ```
//!
//! A `finish_reason` that ends the reply in a failure (a capacity cut, a
//! content filter) drops the pending tool calls, and its
//! [`StreamEvent::Error`] waits for `[DONE]`: the usage chunk the server
//! sends between them rides on the error, because the failed call still
//! spent its tokens. After an `Error` the machine emits nothing more.
//!
//! Tool-call arguments stream as JSON-string fragments keyed by
//! `tool_calls[].index`; they accumulate and parse at flush time
//! (`finish_reason`), emitting one atomic [`StreamEvent::ToolUse`] each —
//! mirroring the Claude path's `content_block_stop` behavior.
//!
//! OpenAI-compatible reasoning carries no verification signature, so every
//! [`StreamEvent::ThinkingEnd`] this machine emits carries the sentinel
//! [`REASONING_NONCE`] instead: it marks the reasoning rehydratable, and the
//! next request sends the text back as `reasoning_content` (see
//! [`super::build`]).

use std::collections::BTreeMap;

use crate::llm::stream::{OpenAiCompatUsageExtra, SpentUsage, StreamError, StreamErrorKind, StreamEvent, UsageExtra};

use super::sse::OpenAiSseEvent;
use super::types::{ChunkChoice, ToolCallChunk, Usage};

/// Which content block is currently open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
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
    /// A `finish_reason` or an error chunk ended the stream in an error. The
    /// `[DONE]` that follows completes nothing.
    failed: bool,
    /// The error a failing `finish_reason` ends the reply with, held until
    /// `[DONE]` so the usage reported after it rides along.
    failure: Option<StreamError>,
    /// An `Error` was emitted. The machine emits nothing more.
    ended: bool,
}

/// `finish_reason` a provider sends when it cut the response for lack of
/// capacity. The reply is incomplete and a repeat may succeed.
const INSUFFICIENT_RESOURCE: &str = "insufficient_system_resource";

/// `finish_reason` a provider sends when its content filter ended the reply.
const CONTENT_FILTER: &str = "content_filter";

/// The [`StreamEvent::Error`] for a capacity cut.
pub(crate) fn capacity_cut() -> StreamError {
    StreamError::new(
        StreamErrorKind::CapacityCut,
        "the provider cut the response for lack of capacity",
    )
}

/// The error a reply's `finish_reason` ends it with, or `None` for a reason
/// that completes it. A missing reason is a truncation. `length` completes a
/// reply at the output limit, and a reason this client does not know
/// completes it too, since servers name their own (`eos_token`).
fn finish_failure(finish_reason: Option<&str>) -> Option<StreamError> {
    match finish_reason {
        None => Some(StreamError::new(
            StreamErrorKind::Truncated("no_finish_reason".into()),
            "the reply ended without a finish_reason",
        )),
        Some(INSUFFICIENT_RESOURCE) => Some(capacity_cut()),
        Some(CONTENT_FILTER) => Some(StreamError::new(
            StreamErrorKind::ContentFilter,
            "the provider's content filter ended the response",
        )),
        Some(_) => None,
    }
}

/// Sentinel token marking openai-compatible reasoning as rehydratable. Opaque —
/// the value is never read back (only its presence gates rehydration); the build
/// path re-emits the reasoning *text* as `reasoning_content`.
const REASONING_NONCE: &str = "openai-compat-reasoning";

impl StateMachine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a `finish_reason` or an error chunk ended the reply in an error.
    pub(crate) fn failed(&self) -> bool {
        self.failed
    }

    /// The `ThinkingEnd` signature: the rehydration nonce, so a model's own
    /// reasoning goes back to it on the next request. See
    /// [`kaijutsu_types::BlockSnapshot::signature`].
    fn thinking_signature(&self) -> Option<String> {
        Some(REASONING_NONCE.to_string())
    }

    /// Take one event. After the first `Error` it emits, the machine emits
    /// nothing more: no `ToolUse` and no `Done` follow a failure.
    pub fn step(&mut self, event: OpenAiSseEvent) -> Vec<StreamEvent> {
        if self.ended {
            return Vec::new();
        }
        let mut out = self.dispatch(event);
        if let Some(position) = out.iter().position(|e| matches!(e, StreamEvent::Error(_))) {
            out.truncate(position + 1);
            self.failed = true;
            self.ended = true;
        }
        out
    }

    fn dispatch(&mut self, event: OpenAiSseEvent) -> Vec<StreamEvent> {
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
            OpenAiSseEvent::Error(error) => vec![StreamEvent::Error(self.with_spent(error))],
            OpenAiSseEvent::Done => {
                if let Some(error) = self.take_failure() {
                    return vec![error];
                }
                if self.failed {
                    return Vec::new();
                }
                let mut out = Vec::new();
                self.close_open_block(&mut out);
                // A reply that never said it finished may have been cut
                // anywhere, a tool call's arguments included: its open text
                // is kept, its tool calls are not run.
                if self.stop_reason.is_none()
                    && let Some(error) = finish_failure(None)
                {
                    self.tool_calls.clear();
                    self.failed = true;
                    out.push(StreamEvent::Error(self.with_spent(error)));
                    return out;
                }
                self.flush_tool_calls(&mut out);
                let (input_tokens, output_tokens, extra) = self.usage_counts();
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

    /// The failure a `finish_reason` ended the reply with, carrying the usage
    /// reported so far. Taken at `[DONE]`, or when the source closes without
    /// one.
    pub(crate) fn take_failure(&mut self) -> Option<StreamEvent> {
        if self.ended {
            return None;
        }
        let error = self.failure.take()?;
        self.ended = true;
        Some(StreamEvent::Error(self.with_spent(error)))
    }

    /// `error` with the usage the server reported, when it reported any.
    fn with_spent(&self, error: StreamError) -> StreamError {
        match self.usage_counts() {
            (None, None, None) => error,
            (input_tokens, output_tokens, extra) => error.with_usage(SpentUsage { input_tokens, output_tokens, extra }),
        }
    }

    /// The reported usage as `Done` carries it.
    fn usage_counts(&self) -> (Option<u64>, Option<u64>, Option<UsageExtra>) {
        match &self.usage {
            Some(u) => (
                Some(u.prompt_tokens),
                Some(u.completion_tokens),
                Some(UsageExtra::OpenAiCompat(OpenAiCompatUsageExtra {
                    prompt_cache_hit_tokens: u.cache_hit_tokens(),
                    prompt_cache_miss_tokens: u.cache_miss_tokens(),
                    reasoning_tokens: u.reasoning_tokens(),
                })),
            ),
            None => (None, None, None),
        }
    }

    fn on_choice(&mut self, choice: ChunkChoice, out: &mut Vec<StreamEvent>) {
        let ChunkChoice {
            mut delta,
            finish_reason,
        } = choice;

        // reasoning_content or reasoning → Thinking block.
        if let Some(rc) = delta.take_reasoning().filter(|s| !s.is_empty()) {
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
        // A capacity cut or a content filter ends the reply without its
        // pending tool calls: they are dropped, not run, and the error waits
        // for `[DONE]`.
        if let Some(fr) = finish_reason {
            self.close_open_block(out);
            if let Some(error) = finish_failure(Some(&fr)) {
                self.tool_calls.clear();
                self.failed = true;
                self.failure.get_or_insert(error);
            } else {
                self.flush_tool_calls(out);
            }
            self.stop_reason = Some(fr);
        }
    }

    /// Emit the closing bracket for whatever block is open, then reset to
    /// [`Phase::None`]. Idempotent — a second call with nothing open is a
    /// no-op.
    fn close_open_block(&mut self, out: &mut Vec<StreamEvent>) {
        match self.phase {
            Phase::Thinking => out.push(StreamEvent::ThinkingEnd {
                signature: self.thinking_signature(),
            }),
            Phase::Text => out.push(StreamEvent::TextEnd),
            Phase::None => {}
        }
        self.phase = Phase::None;
    }

    fn accumulate_tool_call(&mut self, tc: ToolCallChunk) {
        let entry = self.tool_calls.entry(tc.index).or_default();
        // An empty id is treated as absent: some OpenAI-compatible servers
        // send `"id": ""` on every fragment, and taking it would either
        // erase a real id from an earlier fragment or collide two parallel
        // calls on "" at flush time.
        if let Some(id) = tc.id.filter(|id| !id.is_empty()) {
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
    /// events (index order). A missing name surfaces a loud
    /// [`StreamEvent::Error`] rather than a bogus call; arguments that do not
    /// parse surface as [`StreamEvent::ToolUseInvalid`], which the runtime
    /// answers with an error tool result.
    fn flush_tool_calls(&mut self, out: &mut Vec<StreamEvent>) {
        let drained = std::mem::take(&mut self.tool_calls);
        for (index, accum) in drained {
            let Some(name) = accum.name else {
                out.push(StreamEvent::Error(StreamError::new(
                    StreamErrorKind::Protocol,
                    format!("tool call at index {index} arrived without a function name"),
                )));
                continue;
            };
            // DeepSeek always sends an id; fall back to an index-derived
            // correlation key for OpenAI-compatible servers that omit it
            // (or send it empty — `accumulate_tool_call` drops those).
            let id = accum.id.unwrap_or_else(|| format!("call_{index}"));
            let input = if accum.arguments.trim().is_empty() {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                match serde_json::from_str(&accum.arguments) {
                    Ok(v) => v,
                    Err(e) => {
                        out.push(StreamEvent::ToolUseInvalid {
                            id,
                            name,
                            arguments: accum.arguments,
                            error: e.to_string(),
                        });
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
// the StreamEvent sequence the kernel block writer will see.
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

    const OPENAI_CACHED: &str = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}

data: {\"choices\":[],\"usage\":{\"prompt_tokens\":2006,\"completion_tokens\":4,\"total_tokens\":2010,\"prompt_tokens_details\":{\"cached_tokens\":1920}}}

data: [DONE]

";

    #[tokio::test]
    async fn openai_cached_tokens_reach_the_done_extra() {
        let events = run(OPENAI_CACHED).await;
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Done {
                stop_reason: Some("stop".into()),
                input_tokens: Some(2006),
                output_tokens: Some(4),
                extra: Some(UsageExtra::OpenAiCompat(OpenAiCompatUsageExtra {
                    prompt_cache_hit_tokens: 1920,
                    prompt_cache_miss_tokens: 86,
                    reasoning_tokens: 0,
                })),
            })
        );
    }

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
                StreamEvent::ThinkingEnd { signature: Some(REASONING_NONCE.to_string()) },
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

    /// vLLM streams reasoning as `delta.reasoning` (tenchi's Qwen).
    const VLLM_REASONING_THEN_TEXT: &str = "\
data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"reasoning\":\"We\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"reasoning\":\" need\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}

data: [DONE]

";

    #[tokio::test]
    async fn vllm_reasoning_field_streams_as_thinking() {
        let events = run(VLLM_REASONING_THEN_TEXT).await;
        assert_eq!(
            &events[..6],
            &[
                StreamEvent::ThinkingStart,
                StreamEvent::ThinkingDelta("We".into()),
                StreamEvent::ThinkingDelta(" need".into()),
                StreamEvent::ThinkingEnd { signature: Some(REASONING_NONCE.to_string()) },
                StreamEvent::TextStart,
                StreamEvent::TextDelta("hi".into()),
            ]
        );
    }

    /// A server that sends the same text under both names shows it once.
    #[tokio::test]
    async fn reasoning_under_both_names_is_not_doubled() {
        let payload = "\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hmm\",\"reasoning\":\"hmm\"},\"finish_reason\":\"stop\"}]}

data: [DONE]

";
        let events = run(payload).await;
        assert_eq!(
            &events[..3],
            &[
                StreamEvent::ThinkingStart,
                StreamEvent::ThinkingDelta("hmm".into()),
                StreamEvent::ThinkingEnd { signature: Some(REASONING_NONCE.to_string()) },
            ]
        );
    }

    /// An empty `reasoning_content` beside a non-empty `reasoning` does not
    /// hide the thinking.
    #[tokio::test]
    async fn an_empty_reasoning_content_does_not_shadow_reasoning() {
        let payload = "\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"\",\"reasoning\":\"We need\"},\"finish_reason\":\"stop\"}]}

data: [DONE]

";
        let events = run(payload).await;
        assert_eq!(
            &events[..2],
            &[StreamEvent::ThinkingStart, StreamEvent::ThinkingDelta("We need".into())]
        );
    }

    /// Every OpenAI-compatible provider's reasoning is rehydratable, not only
    /// DeepSeek's: `ThinkingEnd` carries the sentinel nonce, so the next
    /// request sends the model's own thinking back as `reasoning_content`.
    #[tokio::test]
    async fn generic_provider_stamps_rehydration_nonce_on_thinking_end() {
        let events = run(REASONING_THEN_TEXT).await;
        let thinking_end = events
            .iter()
            .find(|e| matches!(e, StreamEvent::ThinkingEnd { .. }))
            .expect("a ThinkingEnd is emitted");
        assert_eq!(
            thinking_end,
            &StreamEvent::ThinkingEnd {
                signature: Some(REASONING_NONCE.to_string()),
            },
            "a generic provider's reasoning is rehydratable"
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

    /// Arguments that do not parse keep the call: the runtime answers it with
    /// an error tool result and the turn goes on. A `StreamEvent::Error` here
    /// would fail the whole turn and tell the model nothing.
    #[tokio::test]
    async fn malformed_tool_arguments_surface_the_call_and_its_raw_text() {
        let payload = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",\"function\":{\"name\":\"f\",\"arguments\":\"{not json\"}}]},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}

data: [DONE]

";
        let events = run(payload).await;
        assert!(
            !events.iter().any(|e| matches!(e, StreamEvent::Error(_))),
            "a cut-off call is not a stream error: {events:?}"
        );
        let invalid = events
            .iter()
            .find(|e| matches!(e, StreamEvent::ToolUseInvalid { .. }))
            .expect("must surface the call that did not parse");
        match invalid {
            StreamEvent::ToolUseInvalid {
                id,
                name,
                arguments,
                error,
            } => {
                assert_eq!(id, "call_x");
                assert_eq!(name, "f");
                assert_eq!(arguments, "{not json");
                assert!(!error.is_empty(), "the parser's message rides along");
            }
            _ => unreachable!(),
        }
    }

    /// The shape DeepSeek and other OpenAI-compatible servers send when the
    /// output limit lands inside a call's arguments: fragments cut
    /// mid-string, then `finish_reason: "length"`.
    #[tokio::test]
    async fn a_call_cut_at_the_length_limit_surfaces_invalid_then_done() {
        let payload = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_cut\",\"type\":\"function\",\"function\":{\"name\":\"write\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\": \\\"/app/solve.py\\\", \"}}]},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"content\\\": \\\"import re\"}}]},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}

data: [DONE]

";
        let events = run(payload).await;
        assert_eq!(events.len(), 2, "{events:?}");
        match &events[0] {
            StreamEvent::ToolUseInvalid { id, name, arguments, error } => {
                assert_eq!(id, "call_cut");
                assert_eq!(name, "write");
                assert_eq!(arguments, "{\"path\": \"/app/solve.py\", \"content\": \"import re");
                assert!(error.contains("EOF while parsing a string"), "{error}");
            }
            other => panic!("expected ToolUseInvalid, got {other:?}"),
        }
        match &events[1] {
            StreamEvent::Done { stop_reason, .. } => {
                assert_eq!(stop_reason.as_deref(), Some("length"))
            }
            other => panic!("expected Done, got {other:?}"),
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
    async fn empty_tool_call_id_falls_back_to_index_key() {
        // Some OpenAI-compatible servers send `"id": ""` instead of omitting
        // the field. Two parallel calls with empty ids must not collide: an
        // empty id is treated as absent and falls back to the index key.
        let payload = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"\",\"function\":{\"name\":\"a\",\"arguments\":\"{}\"}},{\"index\":1,\"id\":\"\",\"function\":{\"name\":\"b\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}

data: [DONE]

";
        let events = run(payload).await;
        let ids: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["call_0", "call_1"], "empty ids fall back to the index key");
    }

    #[tokio::test]
    async fn empty_id_on_a_later_fragment_does_not_erase_the_real_id() {
        // A real id on the first fragment, then `"id": ""` on the argument
        // continuation. The real id must survive.
        let payload = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_real\",\"function\":{\"name\":\"a\",\"arguments\":\"{\"}}]},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"\",\"function\":{\"arguments\":\"}\"}}]},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}

data: [DONE]

";
        let events = run(payload).await;
        let ids: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["call_real"], "a later empty id must not overwrite the real one");
    }

    /// A `[DONE]` with no `finish_reason` before it ends a reply that did not
    /// say it finished: the open text is closed, and the stream ends in a
    /// `Truncated` error, not a completion.
    ///
    /// Falsified by a `Done` that reports the partial text as a whole reply.
    #[tokio::test]
    async fn done_without_finish_reason_closes_open_text_and_is_truncated() {
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
                StreamEvent::Error(StreamError::new(
                    StreamErrorKind::Truncated("no_finish_reason".into()),
                    "the reply ended without a finish_reason",
                )),
            ]
        );
    }

    /// An `Error` ends the stream: a nameless tool call fails the reply, and
    /// neither the named call after it nor a `Done` follows.
    ///
    /// Falsified by a `ToolUse` or a `Done` after the error.
    #[tokio::test]
    async fn a_nameless_tool_call_ends_the_stream_in_its_error() {
        let payload = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_0\",\"function\":{\"arguments\":\"{}\"}},{\"index\":1,\"id\":\"call_1\",\"function\":{\"name\":\"b\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}

data: [DONE]

";
        let events = run(payload).await;
        match events.as_slice() {
            [StreamEvent::Error(error)] => assert_eq!(error.kind, StreamErrorKind::Protocol, "{error}"),
            other => panic!("expected one Protocol error, got {other:?}"),
        }
    }

    /// An error chunk after a usage report carries that usage: the failed
    /// call spent it.
    ///
    /// Falsified by an in-stream error that drops the usage already received.
    #[tokio::test]
    async fn an_error_chunk_carries_the_usage_received_before_it() {
        let payload = "\
data: {\"choices\":[],\"usage\":{\"prompt_tokens\":21,\"completion_tokens\":4,\"total_tokens\":25}}

data: {\"error\":{\"message\":\"engine overloaded\",\"type\":\"server_error\"}}

";
        let events = run(payload).await;
        match events.as_slice() {
            [StreamEvent::Error(error)] => {
                let spent = error.usage.as_ref().expect("usage rides on the error");
                assert_eq!((spent.input_tokens, spent.output_tokens), (Some(21), Some(4)));
            }
            other => panic!("expected one Error, got {other:?}"),
        }
    }

    /// A tool call whose reply never reached a `finish_reason` may hold
    /// arguments the server cut off; it is dropped, not run. The usage the
    /// server reported rides on the error.
    ///
    /// Falsified by a `ToolUse` emitted at a bare `[DONE]`.
    #[tokio::test]
    async fn done_without_finish_reason_runs_no_tool_call_and_keeps_its_usage() {
        let payload = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}

data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3,\"total_tokens\":15}}

data: [DONE]

";
        let events = run(payload).await;
        match events.as_slice() {
            [StreamEvent::Error(error)] => {
                assert_eq!(error.kind, StreamErrorKind::Truncated("no_finish_reason".into()), "{error}");
                let spent = error.usage.as_ref().expect("usage rides on the error");
                assert_eq!((spent.input_tokens, spent.output_tokens), (Some(12), Some(3)));
            }
            other => panic!("expected one Truncated error, got {other:?}"),
        }
    }

    /// A reply that ends in a failure still spent its tokens, and OpenAI-shaped
    /// servers report them in the usage chunk that follows the
    /// `finish_reason`: the error carries them.
    ///
    /// Falsified by an error emitted at the `finish_reason`, before the usage
    /// chunk arrives.
    #[tokio::test]
    async fn a_failed_finish_carries_the_usage_reported_after_it() {
        for (finish, kind) in [
            ("content_filter", StreamErrorKind::ContentFilter),
            ("insufficient_system_resource", StreamErrorKind::CapacityCut),
        ] {
            let payload = format!("\
data: {{\"choices\":[{{\"delta\":{{\"content\":\"some\"}},\"finish_reason\":null}}]}}

data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"{finish}\"}}]}}

data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":40,\"completion_tokens\":7,\"total_tokens\":47,\"prompt_tokens_details\":{{\"cached_tokens\":32}}}}}}

data: [DONE]

");
            let events = run(&payload).await;
            match events.last() {
                Some(StreamEvent::Error(error)) => {
                    assert_eq!(error.kind, kind, "{finish}: {error}");
                    let spent = error.usage.as_ref().unwrap_or_else(|| panic!("{finish}: usage rides on the error"));
                    assert_eq!((spent.input_tokens, spent.output_tokens), (Some(40), Some(7)), "{finish}");
                    assert_eq!(
                        spent.extra,
                        Some(UsageExtra::OpenAiCompat(OpenAiCompatUsageExtra {
                            prompt_cache_hit_tokens: 32,
                            prompt_cache_miss_tokens: 8,
                            reasoning_tokens: 0,
                        })),
                        "{finish}"
                    );
                }
                other => panic!("{finish}: expected an Error last, got {other:?}"),
            }
        }
    }

    const CUT_WITH_TOOL_CALL: &str = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"FINISH\"}]}

data: [DONE]

";

    #[tokio::test]
    async fn a_capacity_cut_is_a_transient_error_and_runs_no_tool_call() {
        let events = run(&CUT_WITH_TOOL_CALL.replace("FINISH", "insufficient_system_resource")).await;
        assert!(
            !events.iter().any(|e| matches!(e, StreamEvent::ToolUse { .. } | StreamEvent::Done { .. })),
            "a cut stream must not yield a tool call or a completion: {events:?}"
        );
        match events.last() {
            Some(StreamEvent::Error(error)) => {
                assert_eq!(error.kind, StreamErrorKind::CapacityCut, "{error}");
            }
            other => panic!("expected a CapacityCut Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_content_filter_stop_is_a_visible_error_and_runs_no_tool_call() {
        let events = run(&CUT_WITH_TOOL_CALL.replace("FINISH", "content_filter")).await;
        assert!(
            !events.iter().any(|e| matches!(e, StreamEvent::ToolUse { .. } | StreamEvent::Done { .. })),
            "a filtered stream must not yield a tool call or a completion: {events:?}"
        );
        match events.last() {
            Some(StreamEvent::Error(error)) => {
                assert_eq!(error.kind, StreamErrorKind::ContentFilter, "{error}");
            }
            other => panic!("expected an Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_error_chunk_surfaces_the_providers_message() {
        let events = run("data: {\"error\":{\"message\":\"engine overloaded\",\"type\":\"server_error\"}}\n\n").await;
        match events.as_slice() {
            [StreamEvent::Error(error)] => {
                assert_eq!(error.kind, StreamErrorKind::ServerError, "{error}");
                assert!(error.message.contains("engine overloaded"), "{error}");
            }
            other => panic!("expected one Error, got {other:?}"),
        }
    }
}
