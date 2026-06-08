//! Translate kaijutsu's [`Message`] / [`ContentBlock`] into Anthropic's
//! native [`MessagesRequest`] shape.
//!
//! Per the Phase 0 contract: this is an explicit function, *not* a `From`
//! impl. The translation is lossy in one direction only (kaijutsu →
//! Anthropic — we never come back), and provider-specific knobs
//! (`cache_control`, `thinking`) get applied here from [`BuildOpts`]
//! rather than threaded through a uniform interface.
//!
//! Cache breakpoint policy: all three [`CacheTarget`] variants are honored
//! with their per-breakpoint [`CacheTtl`]. Breakpoints are applied in
//! declaration order, deduped (`Tools` and `System` each land at most
//! once; `MessageIndex` dedupes by index). Anthropic's 4-breakpoint cap
//! is enforced here — drops are logged via `tracing::warn!` with the
//! offending target so populators (rc scripts, drift-router) have a
//! debuggable signal when they over-spec.

use std::collections::HashSet;

use super::types::{
    CacheControl, ImageSource, MessageContent, MessageRole, MessagesRequest, RequestContent,
    RequestMessage, RequestTool, SystemBlock, SystemPrompt, Thinking,
};
use crate::llm::stream::{BuildOpts, CacheTarget, CacheTtl};
use crate::llm::{ContentBlock, Message, MessageContent as KaiContent, Role};

/// Anthropic's hard cap on `cache_control` breakpoints per request.
const MAX_CACHE_BREAKPOINTS: usize = 4;

/// Map a kaijutsu TTL onto the wire-shape `cache_control` annotation.
fn cache_control_for(ttl: CacheTtl) -> CacheControl {
    match ttl {
        CacheTtl::Ephemeral => CacheControl::ephemeral(),
        CacheTtl::Extended => CacheControl::extended(),
    }
}

/// Post-scan plan derived from [`BuildOpts::cache_breakpoints`].
///
/// Encodes which targets get cached and with what TTL, after dedup and
/// 4-cap enforcement. Empty when the carrier was empty.
#[derive(Debug, Default)]
struct CachePlan {
    tools: Option<CacheTtl>,
    system: Option<CacheTtl>,
    /// `(message_index, ttl)` pairs in declaration order, deduped by
    /// index. Out-of-range indices stay in the plan and trigger a
    /// `tracing::warn!` at apply time so the populator hears about it.
    message_indices: Vec<(usize, CacheTtl)>,
}

/// Walk the breakpoint vec once, enforcing the 4-cap and deduping.
/// Drops a `tracing::warn!` for every breakpoint that doesn't make it
/// into the plan; the structured `target` field carries the variant
/// for downstream debugging.
fn plan_cache(breakpoints: &[CacheTarget]) -> CachePlan {
    let mut plan = CachePlan::default();
    let mut budget = MAX_CACHE_BREAKPOINTS;
    let mut seen_indices: HashSet<usize> = HashSet::new();

    for bp in breakpoints {
        if budget == 0 {
            tracing::warn!(
                target = ?bp,
                "cache breakpoint dropped: 4-breakpoint cap reached"
            );
            continue;
        }
        match bp {
            CacheTarget::Tools(ttl) => {
                if plan.tools.is_some() {
                    tracing::warn!(
                        target = ?bp,
                        "cache breakpoint dropped: duplicate Tools breakpoint"
                    );
                    continue;
                }
                plan.tools = Some(*ttl);
                budget -= 1;
            }
            CacheTarget::System(ttl) => {
                if plan.system.is_some() {
                    tracing::warn!(
                        target = ?bp,
                        "cache breakpoint dropped: duplicate System breakpoint"
                    );
                    continue;
                }
                plan.system = Some(*ttl);
                budget -= 1;
            }
            CacheTarget::MessageIndex(i, ttl) => {
                if !seen_indices.insert(*i) {
                    tracing::warn!(
                        target = ?bp,
                        "cache breakpoint dropped: duplicate MessageIndex"
                    );
                    continue;
                }
                plan.message_indices.push((*i, *ttl));
                budget -= 1;
            }
        }
    }

    plan
}

/// Construct an Anthropic [`MessagesRequest`] from kaijutsu inputs.
///
/// `streaming` selects between the SSE form (`stream: Some(true)`) and
/// the non-streaming form (omitted). All other knobs come from `opts`.
///
/// Extended thinking is *not* wired here — Phase 2 lays the type
/// (see [`Thinking`]) but leaves the budget source open until per-context
/// configuration is settled.
pub fn build_request(
    opts: &BuildOpts,
    messages: &[Message],
    streaming: bool,
) -> MessagesRequest {
    let plan = plan_cache(&opts.cache_breakpoints);

    // Tools: a single cache_control on the last tool covers the whole
    // array under Anthropic's prefix semantics.
    let tools = build_tools(&opts.tools, plan.tools);

    // System prompt: block list form when caching, plain string otherwise.
    let system = build_system(opts.system.as_deref(), plan.system);

    let mut request_messages: Vec<RequestMessage> = messages.iter().map(build_message).collect();
    apply_message_breakpoints(&mut request_messages, &plan.message_indices);

    MessagesRequest {
        model: opts.model.clone(),
        max_tokens: opts.max_tokens,
        messages: request_messages,
        system,
        tools,
        tool_choice: None,
        temperature: opts.temperature,
        stream: streaming.then_some(true),
        thinking: None, // Phase 2 leaves config source open
        stop_sequences: vec![],
    }
}

/// Apply `cache_control` to messages at each `(index, ttl)` in the plan.
///
/// `cache_control` rides on a content block, not a message — so when the
/// message content is the bare-string form ([`MessageContent::Text`]),
/// we promote it to a single-element [`MessageContent::Blocks`] first.
/// The annotation lands on the **last cache-eligible block** in the
/// message; under Anthropic's prefix semantics, that caches the entire
/// prefix through the message.
///
/// [`RequestContent::Thinking`] is not cache-eligible (the wire shape
/// has no `cache_control` field). If the last block is a Thinking block,
/// we walk backward to find the nearest cache-eligible block. If no
/// such block exists, we log a warning and skip — the breakpoint is
/// dropped rather than silently no-op.
///
/// Out-of-range indices are logged and skipped.
fn apply_message_breakpoints(
    messages: &mut Vec<RequestMessage>,
    breakpoints: &[(usize, CacheTtl)],
) {
    for &(idx, ttl) in breakpoints {
        let Some(msg) = messages.get_mut(idx) else {
            tracing::warn!(
                index = idx,
                len = messages.len(),
                "MessageIndex cache breakpoint dropped: index out of range"
            );
            continue;
        };

        // Promote Text content to single-block form so cache_control has
        // something to ride on.
        if let MessageContent::Text(text) = &msg.content {
            msg.content = MessageContent::Blocks(vec![RequestContent::Text {
                text: text.clone(),
                cache_control: None,
            }]);
        }

        let MessageContent::Blocks(blocks) = &mut msg.content else {
            unreachable!("content promoted to Blocks above");
        };

        if !apply_cache_to_last_eligible(blocks, ttl) {
            tracing::warn!(
                index = idx,
                "MessageIndex cache breakpoint dropped: no cache-eligible content block \
                 (message contains only Thinking blocks)"
            );
        }
    }
}

/// Set `cache_control` on the last cache-eligible block in `blocks`.
/// Returns `true` if a block accepted the annotation.
fn apply_cache_to_last_eligible(blocks: &mut [RequestContent], ttl: CacheTtl) -> bool {
    let cc = cache_control_for(ttl);
    for block in blocks.iter_mut().rev() {
        match block {
            RequestContent::Text { cache_control, .. }
            | RequestContent::Image { cache_control, .. }
            | RequestContent::ToolUse { cache_control, .. }
            | RequestContent::ToolResult { cache_control, .. } => {
                *cache_control = Some(cc);
                return true;
            }
            RequestContent::Thinking { .. } => continue,
        }
    }
    false
}

/// Builder for extended thinking blocks (typed; not yet wired to a
/// configuration source). Phase 2 exposes the entry point so callers
/// that want thinking can set it after `build_request` returns.
#[allow(dead_code)] // exposed for downstream callers in Phase 2.5+
pub fn with_thinking(mut req: MessagesRequest, budget_tokens: u64) -> MessagesRequest {
    req.thinking = Some(Thinking::Enabled { budget_tokens });
    req
}

fn build_tools(
    tools: &[crate::llm::ToolDefinition],
    cache_ttl: Option<CacheTtl>,
) -> Vec<RequestTool> {
    if tools.is_empty() {
        if cache_ttl.is_some() {
            tracing::warn!(
                "Tools cache breakpoint dropped: tools array is empty"
            );
        }
        return Vec::new();
    }
    let last = tools.len() - 1;
    tools
        .iter()
        .enumerate()
        .map(|(idx, td)| {
            let cache_control = cache_ttl
                .filter(|_| idx == last)
                .map(cache_control_for);
            RequestTool {
                name: td.name.clone(),
                description: td.description.clone(),
                input_schema: td.input_schema.clone(),
                cache_control,
            }
        })
        .collect()
}

fn build_system(
    text: Option<&str>,
    cache_ttl: Option<CacheTtl>,
) -> Option<SystemPrompt> {
    let text = text?;
    if text.is_empty() {
        if cache_ttl.is_some() {
            tracing::warn!(
                "System cache breakpoint dropped: system prompt is empty"
            );
        }
        return None;
    }
    match cache_ttl {
        Some(ttl) => Some(SystemPrompt::Blocks(vec![
            SystemBlock::text(text).with_cache_control(cache_control_for(ttl))
        ])),
        None => Some(SystemPrompt::Text(text.to_string())),
    }
}

fn build_message(msg: &Message) -> RequestMessage {
    let role = match msg.role {
        Role::User => MessageRole::User,
        Role::Assistant => MessageRole::Assistant,
    };
    let content = match &msg.content {
        KaiContent::Text(t) => MessageContent::Text(t.clone()),
        KaiContent::Blocks(blocks) => {
            MessageContent::Blocks(blocks.iter().filter_map(build_block).collect())
        }
    };
    RequestMessage { role, content }
}

/// Translate one kaijutsu [`ContentBlock`] into an Anthropic
/// [`RequestContent`]. Returns `None` for blocks Anthropic can't accept
/// in this role context (e.g. tool_use from a user — not produced by
/// our hydrator, but the type would allow it).
fn build_block(block: &ContentBlock) -> Option<RequestContent> {
    match block {
        ContentBlock::Text { text } => Some(RequestContent::Text {
            text: text.clone(),
            cache_control: None,
        }),
        ContentBlock::ToolUse { id, name, input } => Some(RequestContent::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
            cache_control: None,
        }),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => Some(RequestContent::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.clone(),
            is_error: is_error.then_some(true),
            cache_control: None,
        }),
        ContentBlock::Image {
            hash,
            media_type,
            data_base64,
        } => match data_base64.as_deref() {
            Some(data) if !data.is_empty() => Some(RequestContent::Image {
                source: ImageSource {
                    kind: "base64",
                    media_type: media_type.clone(),
                    data: data.to_string(),
                },
                cache_control: None,
            }),
            // Resolution failed — surface a text marker so the model
            // sees that an image existed at this turn rather than
            // silently dropping the block.
            _ => Some(RequestContent::Text {
                text: format!("[image hash={hash} mime={media_type} unavailable]"),
                cache_control: None,
            }),
        },
        ContentBlock::Reasoning { text, signature } => Some(RequestContent::Thinking {
            thinking: text.clone(),
            signature: signature.clone(),
        }),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolDefinition;

    fn opts(model: &str) -> BuildOpts {
        BuildOpts::new(model)
    }

    #[test]
    fn minimal_user_message_round_trips() {
        let messages = vec![Message::user("hello")];
        let req = build_request(&opts("claude-haiku-4-5"), &messages, false);
        assert_eq!(req.model, "claude-haiku-4-5");
        assert_eq!(req.messages.len(), 1);
        match req.messages[0].role {
            MessageRole::User => {}
            other => panic!("expected user, got {other:?}"),
        }
        match &req.messages[0].content {
            MessageContent::Text(t) => assert_eq!(t, "hello"),
            other => panic!("expected text content, got {other:?}"),
        }
        assert!(req.stream.is_none(), "non-streaming omits stream field");
    }

    #[test]
    fn streaming_flag_emits_stream_true() {
        let messages = vec![Message::user("hi")];
        let req = build_request(&opts("claude-haiku-4-5"), &messages, true);
        assert_eq!(req.stream, Some(true));
    }

    #[test]
    fn tool_use_round_trip_in_assistant_message() {
        let messages = vec![Message::with_tool_uses(
            Some("Let me check".into()),
            vec![ContentBlock::ToolUse {
                id: "toolu_01ABC".into(),
                name: "get_weather".into(),
                input: serde_json::json!({"location": "Tokyo"}),
            }],
        )];
        let req = build_request(&opts("claude-haiku-4-5"), &messages, false);
        let blocks = match &req.messages[0].content {
            MessageContent::Blocks(b) => b,
            other => panic!("expected blocks, got {other:?}"),
        };
        assert_eq!(blocks.len(), 2, "text + tool_use");
        match &blocks[1] {
            RequestContent::ToolUse { id, name, input, .. } => {
                assert_eq!(id, "toolu_01ABC");
                assert_eq!(name, "get_weather");
                assert_eq!(input["location"], "Tokyo");
            }
            other => panic!("expected tool_use second, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_round_trip_with_is_error() {
        let messages = vec![Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "toolu_01ABC".into(),
            content: "boom".into(),
            is_error: true,
        }])];
        let req = build_request(&opts("claude-haiku-4-5"), &messages, false);
        let blocks = match &req.messages[0].content {
            MessageContent::Blocks(b) => b,
            other => panic!("expected blocks, got {other:?}"),
        };
        match &blocks[0] {
            RequestContent::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "toolu_01ABC");
                assert_eq!(content, "boom");
                assert_eq!(*is_error, Some(true), "is_error: true must serialize");
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_success_omits_is_error() {
        // Anthropic treats omitted is_error as false. Round-trip: kaijutsu
        // is_error=false → Anthropic field omitted.
        let messages = vec![Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "toolu_01ABC".into(),
            content: "ok".into(),
            is_error: false,
        }])];
        let req = build_request(&opts("claude-haiku-4-5"), &messages, false);
        let blocks = match &req.messages[0].content {
            MessageContent::Blocks(b) => b,
            other => panic!("expected blocks, got {other:?}"),
        };
        match &blocks[0] {
            RequestContent::ToolResult { is_error, .. } => {
                assert_eq!(*is_error, None, "is_error: false → omit from wire");
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    #[test]
    fn image_with_data_serializes_as_base64_source() {
        let messages = vec![Message {
            role: Role::User,
            content: KaiContent::Blocks(vec![ContentBlock::Image {
                hash: "abc".into(),
                media_type: "image/png".into(),
                data_base64: Some("AAAA".into()),
            }]),
        }];
        let req = build_request(&opts("claude-haiku-4-5"), &messages, false);
        let blocks = match &req.messages[0].content {
            MessageContent::Blocks(b) => b,
            other => panic!("expected blocks, got {other:?}"),
        };
        match &blocks[0] {
            RequestContent::Image { source, .. } => {
                assert_eq!(source.kind, "base64");
                assert_eq!(source.media_type, "image/png");
                assert_eq!(source.data, "AAAA");
            }
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[test]
    fn image_without_data_falls_back_to_text_marker() {
        let messages = vec![Message {
            role: Role::User,
            content: KaiContent::Blocks(vec![ContentBlock::Image {
                hash: "abc".into(),
                media_type: "image/png".into(),
                data_base64: None,
            }]),
        }];
        let req = build_request(&opts("claude-haiku-4-5"), &messages, false);
        let blocks = match &req.messages[0].content {
            MessageContent::Blocks(b) => b,
            other => panic!("expected blocks, got {other:?}"),
        };
        match &blocks[0] {
            RequestContent::Text { text, .. } => {
                assert!(text.contains("abc"), "marker must include hash");
                assert!(text.contains("image/png"));
                assert!(text.contains("unavailable"));
            }
            other => panic!("expected text marker, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_block_round_trips_with_signature() {
        let messages = vec![Message::with_reasoning_text_and_tool_uses(
            vec![("let me think".into(), Some("sig_xyz".into()))],
            Some("answer".into()),
            vec![],
        )];
        let req = build_request(&opts("claude-haiku-4-5"), &messages, false);
        let blocks = match &req.messages[0].content {
            MessageContent::Blocks(b) => b,
            other => panic!("expected blocks, got {other:?}"),
        };
        // Reasoning must come before text per kaijutsu order.
        match &blocks[0] {
            RequestContent::Thinking { thinking, signature } => {
                assert_eq!(thinking, "let me think");
                assert_eq!(signature.as_deref(), Some("sig_xyz"));
            }
            other => panic!("expected thinking first, got {other:?}"),
        }
        match &blocks[1] {
            RequestContent::Text { text, .. } => assert_eq!(text, "answer"),
            other => panic!("expected text second, got {other:?}"),
        }
    }

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: "".into(),
            input_schema: serde_json::json!({}),
        }
    }

    #[test]
    fn cache_breakpoint_tools_applies_to_last_tool_only() {
        let mut o = opts("claude-haiku-4-5");
        o.tools = vec![tool("first"), tool("second"), tool("third")];
        o.cache_breakpoints = vec![CacheTarget::Tools(CacheTtl::Ephemeral)];

        let req = build_request(&o, &[Message::user("hi")], false);
        assert!(req.tools[0].cache_control.is_none());
        assert!(req.tools[1].cache_control.is_none());
        assert_eq!(
            req.tools[2].cache_control,
            Some(CacheControl::ephemeral()),
            "cache_control lands on the last tool — covers the whole array under Anthropic prefix semantics"
        );
    }

    #[test]
    fn cache_breakpoint_tools_with_extended_ttl_serializes_1h() {
        let mut o = opts("claude-haiku-4-5");
        o.tools = vec![tool("only")];
        o.cache_breakpoints = vec![CacheTarget::Tools(CacheTtl::Extended)];

        let req = build_request(&o, &[Message::user("hi")], false);
        assert_eq!(
            req.tools[0].cache_control,
            Some(CacheControl::extended()),
            "Extended TTL must produce the 1h-flavored cache_control"
        );
        let cc = serde_json::to_value(&req.tools[0].cache_control).unwrap();
        assert_eq!(cc["ttl"], "1h");
    }

    #[test]
    fn cache_breakpoint_system_promotes_to_block_list() {
        let mut o = opts("claude-haiku-4-5").with_system("You are helpful.");
        o.cache_breakpoints = vec![CacheTarget::System(CacheTtl::Ephemeral)];

        let req = build_request(&o, &[Message::user("hi")], false);
        match req.system {
            Some(SystemPrompt::Blocks(blocks)) => {
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks[0].cache_control, Some(CacheControl::ephemeral()));
            }
            other => panic!("system must promote to block list when cached, got {other:?}"),
        }
    }

    #[test]
    fn cache_breakpoint_system_with_extended_ttl_serializes_1h() {
        let mut o = opts("claude-haiku-4-5").with_system("hi");
        o.cache_breakpoints = vec![CacheTarget::System(CacheTtl::Extended)];

        let req = build_request(&o, &[Message::user("x")], false);
        match req.system {
            Some(SystemPrompt::Blocks(blocks)) => {
                assert_eq!(blocks[0].cache_control, Some(CacheControl::extended()));
            }
            other => panic!("expected Blocks, got {other:?}"),
        }
    }

    #[test]
    fn no_cache_breakpoints_uses_plain_string_system() {
        let o = opts("claude-haiku-4-5").with_system("You are helpful.");
        let req = build_request(&o, &[Message::user("hi")], false);
        match req.system {
            Some(SystemPrompt::Text(t)) => assert_eq!(t, "You are helpful."),
            other => panic!("system must stay plain string when uncached, got {other:?}"),
        }
    }

    #[test]
    fn empty_system_omits_from_request() {
        let o = opts("claude-haiku-4-5").with_system("");
        let req = build_request(&o, &[Message::user("hi")], false);
        assert!(req.system.is_none(), "empty system must skip-serialize");
    }

    #[test]
    fn no_breakpoints_means_zero_cache_control_on_anything() {
        // Default Phase 2 behavior: carrier is empty, no cache_control
        // anywhere in the request. This is the test that catches a
        // regression where someone defaults cache_breakpoints to non-empty.
        let o = opts("claude-haiku-4-5")
            .with_system("sys")
            .with_tools(vec![ToolDefinition {
                name: "t".into(),
                description: "".into(),
                input_schema: serde_json::json!({}),
            }]);
        let req = build_request(&o, &[Message::user("hi")], false);
        assert!(req.tools[0].cache_control.is_none());
        match req.system {
            Some(SystemPrompt::Text(_)) => {}
            other => panic!("empty breakpoints → string system, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // MessageIndex breakpoint tests
    // ------------------------------------------------------------------

    /// Pull `cache_control` off the last block of the message at `idx`.
    /// Panics on shape mismatch — tests want to be loud when assumptions
    /// don't hold.
    fn last_block_cc(req: &MessagesRequest, idx: usize) -> Option<CacheControl> {
        let msg = req
            .messages
            .get(idx)
            .unwrap_or_else(|| panic!("no message at index {idx}"));
        let blocks = match &msg.content {
            MessageContent::Blocks(b) => b,
            other => panic!("expected Blocks at index {idx}, got {other:?}"),
        };
        let last = blocks.last().expect("message has no blocks");
        match last {
            RequestContent::Text { cache_control, .. }
            | RequestContent::Image { cache_control, .. }
            | RequestContent::ToolUse { cache_control, .. }
            | RequestContent::ToolResult { cache_control, .. } => *cache_control,
            RequestContent::Thinking { .. } => panic!("last block is Thinking — not cache-eligible"),
        }
    }

    #[test]
    fn cache_breakpoint_message_index_applies_cache_control_to_correct_message() {
        let mut o = opts("claude-haiku-4-5");
        o.cache_breakpoints = vec![CacheTarget::MessageIndex(1, CacheTtl::Ephemeral)];

        let messages = vec![
            Message::user("first"),
            Message::user("second"),
            Message::user("third"),
        ];
        let req = build_request(&o, &messages, false);

        // Message 0 and 2 unmodified — Text content stays as bare-string form.
        match &req.messages[0].content {
            MessageContent::Text(t) => assert_eq!(t, "first"),
            other => panic!("message 0 must stay bare-string, got {other:?}"),
        }
        match &req.messages[2].content {
            MessageContent::Text(t) => assert_eq!(t, "third"),
            other => panic!("message 2 must stay bare-string, got {other:?}"),
        }
        // Message 1 promoted with cache_control.
        assert_eq!(last_block_cc(&req, 1), Some(CacheControl::ephemeral()));
    }

    #[test]
    fn cache_breakpoint_message_index_promotes_text_content_to_blocks() {
        let mut o = opts("claude-haiku-4-5");
        o.cache_breakpoints = vec![CacheTarget::MessageIndex(0, CacheTtl::Ephemeral)];

        let req = build_request(&o, &[Message::user("hello")], false);

        // cache_control can't ride on bare-string content, so the target
        // message must have been promoted to a single-element Blocks form.
        match &req.messages[0].content {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                match &blocks[0] {
                    RequestContent::Text { text, cache_control } => {
                        assert_eq!(text, "hello");
                        assert_eq!(*cache_control, Some(CacheControl::ephemeral()));
                    }
                    other => panic!("expected Text block, got {other:?}"),
                }
            }
            other => panic!("Text content must promote to Blocks when breakpointed, got {other:?}"),
        }
    }

    #[test]
    fn cache_breakpoint_message_index_lands_on_last_block_of_multi_block_message() {
        let mut o = opts("claude-haiku-4-5");
        o.cache_breakpoints = vec![CacheTarget::MessageIndex(0, CacheTtl::Ephemeral)];

        // Assistant message with text + tool_use; cache_control must
        // land on the tool_use (the last block) under prefix semantics.
        let messages = vec![Message::with_tool_uses(
            Some("Let me check".into()),
            vec![ContentBlock::ToolUse {
                id: "toolu_01".into(),
                name: "get_weather".into(),
                input: serde_json::json!({"city": "Tokyo"}),
            }],
        )];
        let req = build_request(&o, &messages, false);

        let blocks = match &req.messages[0].content {
            MessageContent::Blocks(b) => b,
            other => panic!("expected Blocks, got {other:?}"),
        };
        assert_eq!(blocks.len(), 2, "text + tool_use");
        match &blocks[0] {
            RequestContent::Text { cache_control, .. } => {
                assert!(cache_control.is_none(), "first block must not be cached");
            }
            other => panic!("expected Text first, got {other:?}"),
        }
        match &blocks[1] {
            RequestContent::ToolUse { cache_control, .. } => {
                assert_eq!(
                    *cache_control,
                    Some(CacheControl::ephemeral()),
                    "cache_control lands on the last block of the target message"
                );
            }
            other => panic!("expected ToolUse last, got {other:?}"),
        }
    }

    #[test]
    fn cache_breakpoint_message_index_skips_trailing_thinking_blocks() {
        // Thinking blocks have no cache_control wire field. The breakpoint
        // must walk past them and land on the previous cache-eligible
        // block (the answer text in this case).
        let mut o = opts("claude-haiku-4-5");
        o.cache_breakpoints = vec![CacheTarget::MessageIndex(0, CacheTtl::Extended)];

        // Use the kaijutsu builder that produces Reasoning → Text order
        // — then manually flip in the assistant message to simulate a
        // message whose last block is Thinking.
        let messages = vec![Message {
            role: Role::Assistant,
            content: KaiContent::Blocks(vec![
                ContentBlock::Text {
                    text: "answer".into(),
                },
                ContentBlock::Reasoning {
                    text: "trailing thought".into(),
                    signature: None,
                },
            ]),
        }];
        let req = build_request(&o, &messages, false);

        let blocks = match &req.messages[0].content {
            MessageContent::Blocks(b) => b,
            other => panic!("expected Blocks, got {other:?}"),
        };
        assert_eq!(blocks.len(), 2);
        match &blocks[0] {
            RequestContent::Text { text, cache_control } => {
                assert_eq!(text, "answer");
                assert_eq!(
                    *cache_control,
                    Some(CacheControl::extended()),
                    "cache_control walks past the trailing Thinking block onto the prior Text"
                );
            }
            other => panic!("expected Text first, got {other:?}"),
        }
        assert!(matches!(&blocks[1], RequestContent::Thinking { .. }));
    }

    #[test]
    fn cache_breakpoint_message_index_drops_when_only_thinking_blocks() {
        // No cache-eligible block exists in the target message — the
        // breakpoint must be dropped (logged via tracing::warn) rather
        // than silently applied to nothing.
        let mut o = opts("claude-haiku-4-5");
        o.cache_breakpoints = vec![CacheTarget::MessageIndex(0, CacheTtl::Ephemeral)];

        let messages = vec![Message {
            role: Role::Assistant,
            content: KaiContent::Blocks(vec![ContentBlock::Reasoning {
                text: "only thinking".into(),
                signature: None,
            }]),
        }];
        let req = build_request(&o, &messages, false);

        let blocks = match &req.messages[0].content {
            MessageContent::Blocks(b) => b,
            other => panic!("expected Blocks, got {other:?}"),
        };
        match &blocks[0] {
            RequestContent::Thinking { .. } => {} // expected, no cache_control field exists
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn cache_breakpoint_message_index_out_of_range_drops_silently() {
        // Out-of-range index emits a warn log and skips. No panic, no
        // accidental cache_control on adjacent messages.
        let mut o = opts("claude-haiku-4-5");
        o.cache_breakpoints = vec![CacheTarget::MessageIndex(7, CacheTtl::Ephemeral)];

        let req = build_request(&o, &[Message::user("only")], false);

        // Single message stays in bare-string form (no breakpoint applied).
        match &req.messages[0].content {
            MessageContent::Text(t) => assert_eq!(t, "only"),
            other => panic!("out-of-range breakpoint must not promote any message, got {other:?}"),
        }
    }

    #[test]
    fn cache_breakpoint_message_index_with_extended_ttl_serializes_1h() {
        let mut o = opts("claude-haiku-4-5");
        o.cache_breakpoints = vec![CacheTarget::MessageIndex(0, CacheTtl::Extended)];

        let req = build_request(&o, &[Message::user("hi")], false);
        assert_eq!(last_block_cc(&req, 0), Some(CacheControl::extended()));
    }

    // ------------------------------------------------------------------
    // Combination + cap + dedupe tests
    // ------------------------------------------------------------------

    #[test]
    fn cache_breakpoints_combined_tools_system_message_with_mixed_ttls() {
        // The fork-time case the doc cares about: extended TTL on the
        // stable bits (tools, system, fork point) so the 1h cache covers
        // multiple follow-up calls on the child.
        let mut o = opts("claude-haiku-4-5")
            .with_system("sys")
            .with_tools(vec![tool("t1"), tool("t2")]);
        o.cache_breakpoints = vec![
            CacheTarget::Tools(CacheTtl::Extended),
            CacheTarget::System(CacheTtl::Ephemeral),
            CacheTarget::MessageIndex(1, CacheTtl::Extended),
        ];

        let req = build_request(
            &o,
            &[
                Message::user("first"),
                Message::user("fork point"),
                Message::user("third"),
            ],
            false,
        );

        // Tools breakpoint on last tool with extended TTL.
        assert_eq!(req.tools[1].cache_control, Some(CacheControl::extended()));
        // System promoted to Blocks with ephemeral TTL.
        match req.system {
            Some(SystemPrompt::Blocks(ref b)) => {
                assert_eq!(b[0].cache_control, Some(CacheControl::ephemeral()));
            }
            ref other => panic!("expected Blocks, got {other:?}"),
        }
        // Message at index 1 has extended TTL on its (sole) block.
        assert_eq!(last_block_cc(&req, 1), Some(CacheControl::extended()));
        // Other messages stay bare-string.
        assert!(matches!(req.messages[0].content, MessageContent::Text(_)));
        assert!(matches!(req.messages[2].content, MessageContent::Text(_)));
    }

    #[test]
    fn cache_breakpoints_exceeding_4_cap_drops_extras_in_declaration_order() {
        // 5 unique breakpoints; the 4-cap means exactly one is dropped.
        // First-come-first-applied: the 5th breakpoint (MessageIndex(3))
        // is the one dropped.
        let mut o = opts("claude-haiku-4-5")
            .with_system("sys")
            .with_tools(vec![tool("t")]);
        o.cache_breakpoints = vec![
            CacheTarget::Tools(CacheTtl::Ephemeral),
            CacheTarget::System(CacheTtl::Ephemeral),
            CacheTarget::MessageIndex(0, CacheTtl::Ephemeral),
            CacheTarget::MessageIndex(1, CacheTtl::Ephemeral),
            CacheTarget::MessageIndex(2, CacheTtl::Ephemeral), // dropped
        ];

        let req = build_request(
            &o,
            &[
                Message::user("a"),
                Message::user("b"),
                Message::user("c"),
            ],
            false,
        );

        assert!(req.tools[0].cache_control.is_some());
        assert!(matches!(req.system, Some(SystemPrompt::Blocks(_))));
        assert!(last_block_cc(&req, 0).is_some());
        assert!(last_block_cc(&req, 1).is_some());
        // Message 2 stays in bare-string form — its breakpoint was over the cap.
        match &req.messages[2].content {
            MessageContent::Text(t) => assert_eq!(t, "c"),
            other => panic!("breakpoint past the 4-cap must not promote, got {other:?}"),
        }
    }

    #[test]
    fn cache_breakpoint_duplicate_message_index_dedupes_first_wins() {
        // Two breakpoints on the same index with different TTLs — the
        // first one applies; the second is dropped without consuming a
        // budget slot (verified by the 4-cap test elsewhere).
        let mut o = opts("claude-haiku-4-5");
        o.cache_breakpoints = vec![
            CacheTarget::MessageIndex(0, CacheTtl::Ephemeral),
            CacheTarget::MessageIndex(0, CacheTtl::Extended),
        ];

        let req = build_request(&o, &[Message::user("hi")], false);
        assert_eq!(
            last_block_cc(&req, 0),
            Some(CacheControl::ephemeral()),
            "first-write-wins: Ephemeral applies, Extended is dropped"
        );
    }

    #[test]
    fn cache_breakpoint_duplicate_tools_drops_second() {
        let mut o = opts("claude-haiku-4-5").with_tools(vec![tool("t")]);
        o.cache_breakpoints = vec![
            CacheTarget::Tools(CacheTtl::Ephemeral),
            CacheTarget::Tools(CacheTtl::Extended), // dropped
        ];
        let req = build_request(&o, &[Message::user("hi")], false);
        assert_eq!(req.tools[0].cache_control, Some(CacheControl::ephemeral()));
    }

    #[test]
    fn cache_breakpoint_tools_dropped_when_tools_empty() {
        // Tools breakpoint with no tools array is meaningless. Drop with
        // a warn log; don't accidentally consume a budget slot.
        let mut o = opts("claude-haiku-4-5");
        o.cache_breakpoints = vec![CacheTarget::Tools(CacheTtl::Ephemeral)];
        let req = build_request(&o, &[Message::user("hi")], false);
        assert!(req.tools.is_empty());
    }
}
