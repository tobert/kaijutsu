//! Translate kaijutsu's [`Message`] / [`ContentBlock`] into Anthropic's
//! native [`MessagesRequest`] shape.
//!
//! Per the Phase 0 contract: this is an explicit function, *not* a `From`
//! impl. The translation is lossy in one direction only (kaijutsu →
//! Anthropic — we never come back), and provider-specific knobs
//! (`cache_control`, `thinking`) get applied here from [`BuildOpts`]
//! rather than threaded through a uniform interface.
//!
//! Cache breakpoint policy: Phase 2 honors only [`CacheTarget::Tools`]
//! and [`CacheTarget::System`]. `MessageIndex` is parsed but currently
//! ignored — Anthropic's 4-breakpoint cap and the cheap wins on
//! tools+system mean we hold off on per-message wiring until policy
//! catches up. Excess breakpoints (over the 4 cap) silently drop;
//! Anthropic would drop them server-side anyway, but the drop happens
//! here for predictable client-side cost accounting.

use super::types::{
    CacheControl, ImageSource, MessageContent, MessageRole, MessagesRequest, RequestContent,
    RequestMessage, RequestTool, SystemBlock, SystemPrompt, Thinking,
};
use crate::llm::stream::{BuildOpts, CacheTarget};
use crate::llm::{ContentBlock, Message, MessageContent as KaiContent, Role};

/// Anthropic's hard cap on `cache_control` breakpoints per request.
const MAX_CACHE_BREAKPOINTS: usize = 4;

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
    let mut budget = MAX_CACHE_BREAKPOINTS;
    let cache_tools = opts.cache_breakpoints.contains(&CacheTarget::Tools);
    let cache_system = opts.cache_breakpoints.contains(&CacheTarget::System);

    // Tools first: a single cache_control on the last tool covers the
    // whole array under Anthropic's prefix semantics.
    let tools = build_tools(&opts.tools, cache_tools, &mut budget);

    // System prompt: block list form when caching, plain string otherwise.
    let system = build_system(opts.system.as_deref(), cache_system, &mut budget);

    let _ = budget; // remaining budget is intentionally dropped for Phase 2.

    let request_messages = messages.iter().map(build_message).collect();

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
    cache: bool,
    budget: &mut usize,
) -> Vec<RequestTool> {
    if tools.is_empty() {
        return Vec::new();
    }
    let last = tools.len() - 1;
    tools
        .iter()
        .enumerate()
        .map(|(idx, td)| {
            let cache_control = if cache && idx == last && *budget > 0 {
                *budget -= 1;
                Some(CacheControl::ephemeral())
            } else {
                None
            };
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
    cache: bool,
    budget: &mut usize,
) -> Option<SystemPrompt> {
    let text = text?;
    if text.is_empty() {
        return None;
    }
    if cache && *budget > 0 {
        *budget -= 1;
        Some(SystemPrompt::Blocks(vec![
            SystemBlock::text(text).with_cache_control(CacheControl::ephemeral())
        ]))
    } else {
        Some(SystemPrompt::Text(text.to_string()))
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
            Some(("let me think".into(), Some("sig_xyz".into()))),
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

    #[test]
    fn cache_breakpoint_tools_applies_to_last_tool_only() {
        let mut o = opts("claude-haiku-4-5");
        o.tools = vec![
            ToolDefinition {
                name: "first".into(),
                description: "".into(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "second".into(),
                description: "".into(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "third".into(),
                description: "".into(),
                input_schema: serde_json::json!({}),
            },
        ];
        o.cache_breakpoints = vec![CacheTarget::Tools];

        let req = build_request(&o, &[Message::user("hi")], false);
        assert!(req.tools[0].cache_control.is_none());
        assert!(req.tools[1].cache_control.is_none());
        assert!(
            req.tools[2].cache_control.is_some(),
            "cache_control lands on the last tool — covers the whole array under Anthropic prefix semantics"
        );
    }

    #[test]
    fn cache_breakpoint_system_promotes_to_block_list() {
        let mut o = opts("claude-haiku-4-5").with_system("You are helpful.");
        o.cache_breakpoints = vec![CacheTarget::System];

        let req = build_request(&o, &[Message::user("hi")], false);
        match req.system {
            Some(SystemPrompt::Blocks(blocks)) => {
                assert_eq!(blocks.len(), 1);
                assert!(blocks[0].cache_control.is_some());
            }
            other => panic!("system must promote to block list when cached, got {other:?}"),
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
}
