//! Translate kaijutsu [`Message`] / [`ContentBlock`] into the DeepSeek
//! (OpenAI-compatible) [`ChatRequest`] shape.
//!
//! Specializations that distinguish DeepSeek from Claude's `build`:
//!
//! - **`Reasoning` blocks are dropped.** DeepSeek returns HTTP 400 if
//!   `reasoning_content` appears in an input message, so a model's prior
//!   chain-of-thought is never echoed back. (kaijutsu's hydrator already
//!   omits thinking blocks from history, so this only matters for the
//!   in-turn agentic-loop assistant messages the driver synthesizes.)
//! - **Tool results become their own `tool`-role messages**, one per
//!   `tool_use_id` — OpenAI's shape — rather than blocks nested inside a
//!   user turn.
//! - **`cache_breakpoints` are ignored.** DeepSeek caches the prompt
//!   prefix automatically; there is no `cache_control` knob.
//! - The system prompt is the first message with role `system`, not a
//!   top-level field.

use crate::llm::stream::BuildOpts;
use crate::llm::{ContentBlock, Message, MessageContent, Role};

use super::types::{
    ChatRequest, ContentPart, ImageUrl, MessageContent as WireContent, RequestFunctionCall,
    RequestMessage, RequestTool, RequestToolCall, StreamOptions, ToolFunction,
};

/// Build a `/chat/completions` request body.
///
/// `streaming` toggles `stream: true` + `stream_options.include_usage`
/// (so the trailing usage chunk carries token accounting).
pub fn build_request(opts: &BuildOpts, messages: &[Message], streaming: bool) -> ChatRequest {
    let mut wire: Vec<RequestMessage> = Vec::with_capacity(messages.len() + 1);

    if let Some(system) = &opts.system {
        wire.push(RequestMessage::system(system.clone()));
    }

    for msg in messages {
        match msg.role {
            Role::User => translate_user(msg, &mut wire),
            Role::Assistant => translate_assistant(msg, &mut wire),
        }
    }

    let tools = opts
        .tools
        .iter()
        .map(|t| RequestTool {
            kind: "function",
            function: ToolFunction {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.input_schema.clone(),
            },
        })
        .collect();

    ChatRequest {
        model: opts.model.clone(),
        messages: wire,
        max_tokens: ChatRequest::clamp_max_tokens(opts.max_tokens),
        stream: streaming.then_some(true),
        stream_options: streaming.then_some(StreamOptions { include_usage: true }),
        tools,
        tool_choice: None, // server-side default `auto`
        temperature: opts.temperature,
    }
}

/// User turn → one user message (text and/or images) plus a separate
/// `tool` message for each tool result. Tool results are emitted first
/// because they answer the preceding assistant turn; any free text/image
/// content follows as a user message.
fn translate_user(msg: &Message, out: &mut Vec<RequestMessage>) {
    match &msg.content {
        MessageContent::Text(t) => out.push(RequestMessage::user_text(t.clone())),
        MessageContent::Blocks(blocks) => {
            let mut parts: Vec<ContentPart> = Vec::new();
            for block in blocks {
                match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        // OpenAI tool messages have no is_error field; keep
                        // the signal visible by prefixing rather than
                        // silently flattening it away.
                        let body = if *is_error {
                            format!("[tool error]\n{content}")
                        } else {
                            content.clone()
                        };
                        out.push(RequestMessage::tool_result(tool_use_id.clone(), body));
                    }
                    ContentBlock::Text { text } => parts.push(ContentPart::Text {
                        text: text.clone(),
                    }),
                    ContentBlock::Image {
                        hash,
                        media_type,
                        data_base64,
                    } => match data_base64 {
                        Some(data) => parts.push(ContentPart::ImageUrl {
                            image_url: ImageUrl {
                                url: format!("data:{media_type};base64,{data}"),
                            },
                        }),
                        // Unresolved CAS hash — surface a marker instead of
                        // dropping the turn's reference silently.
                        None => parts.push(ContentPart::Text {
                            text: format!("[image {hash} unavailable]"),
                        }),
                    },
                    // A user turn carrying ToolUse / Reasoning is nonsensical
                    // (those are assistant-side); ignore defensively.
                    ContentBlock::ToolUse { .. } | ContentBlock::Reasoning { .. } => {}
                }
            }
            if !parts.is_empty() {
                out.push(user_message_from_parts(parts));
            }
        }
    }
}

/// Collapse content parts into the simplest wire form: a single text
/// part becomes a plain string; anything with an image stays a parts
/// list (OpenAI's multimodal shape).
fn user_message_from_parts(parts: Vec<ContentPart>) -> RequestMessage {
    let only_text = parts.len() == 1 && matches!(parts[0], ContentPart::Text { .. });
    let content = if only_text {
        match parts.into_iter().next() {
            Some(ContentPart::Text { text }) => WireContent::Text(text),
            _ => unreachable!("guarded by only_text"),
        }
    } else {
        WireContent::Parts(parts)
    };
    RequestMessage {
        role: super::types::MessageRole::User,
        content: Some(content),
        tool_calls: Vec::new(),
        tool_call_id: None,
    }
}

/// Assistant turn → one assistant message. Text blocks concatenate into
/// `content`; ToolUse blocks become `tool_calls`; Reasoning blocks are
/// dropped (the no-echo rule). An assistant message with only tool calls
/// has `content: None`.
fn translate_assistant(msg: &Message, out: &mut Vec<RequestMessage>) {
    match &msg.content {
        MessageContent::Text(t) => {
            out.push(RequestMessage::assistant(
                Some(WireContent::Text(t.clone())),
                Vec::new(),
            ));
        }
        MessageContent::Blocks(blocks) => {
            let mut text = String::new();
            let mut tool_calls: Vec<RequestToolCall> = Vec::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text: t } => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(t);
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        tool_calls.push(RequestToolCall {
                            id: id.clone(),
                            kind: "function",
                            function: RequestFunctionCall {
                                name: name.clone(),
                                // OpenAI expects arguments as a JSON-encoded
                                // string, not a nested object.
                                arguments: input.to_string(),
                            },
                        });
                    }
                    // Dropped: DeepSeek 400s on echoed reasoning_content.
                    ContentBlock::Reasoning { .. } => {}
                    // Tool results never appear in an assistant turn.
                    ContentBlock::ToolResult { .. } => {}
                    ContentBlock::Image { .. } => {}
                }
            }
            let content = (!text.is_empty()).then_some(WireContent::Text(text));
            out.push(RequestMessage::assistant(content, tool_calls));
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolDefinition;

    fn opts() -> BuildOpts {
        BuildOpts::new("deepseek-v4-flash")
    }

    #[test]
    fn system_prompt_becomes_first_system_message() {
        let o = opts().with_system("be terse");
        let req = build_request(&o, &[Message::user("hi")], false);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["messages"][0]["role"], "system");
        assert_eq!(v["messages"][0]["content"], "be terse");
        assert_eq!(v["messages"][1]["role"], "user");
        assert_eq!(v["messages"][1]["content"], "hi");
    }

    #[test]
    fn streaming_sets_stream_and_include_usage() {
        let req = build_request(&opts(), &[Message::user("hi")], true);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["stream"], true);
        assert_eq!(v["stream_options"]["include_usage"], true);
    }

    #[test]
    fn non_streaming_omits_stream_fields() {
        let req = build_request(&opts(), &[Message::user("hi")], false);
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("stream").is_none());
        assert!(v.get("stream_options").is_none());
    }

    #[test]
    fn tools_translate_to_function_wrappers() {
        let o = opts().with_tools(vec![ToolDefinition {
            name: "read_file".into(),
            description: "read a file".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }]);
        let req = build_request(&o, &[Message::user("hi")], false);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["function"]["name"], "read_file");
        assert_eq!(v["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn reasoning_block_is_dropped_from_assistant_message() {
        // The no-echo rule: a prior reasoning chain must not reach the wire.
        let msg = Message::with_reasoning_text_and_tool_uses(
            Some(("internal thoughts".into(), Some("sig".into()))),
            Some("the answer".into()),
            vec![],
        );
        let req = build_request(&opts(), &[msg], false);
        let v = serde_json::to_value(&req).unwrap();
        let content = v["messages"][0]["content"].as_str().unwrap();
        assert_eq!(content, "the answer");
        assert!(
            !content.contains("internal thoughts"),
            "reasoning must never appear on the wire"
        );
        let serialized = serde_json::to_string(&req).unwrap();
        assert!(
            !serialized.contains("reasoning_content"),
            "no reasoning_content field anywhere: {serialized}"
        );
    }

    #[test]
    fn assistant_tool_use_becomes_tool_calls_with_string_arguments() {
        let msg = Message::with_tool_uses(
            Some("calling it".into()),
            vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "get_weather".into(),
                input: serde_json::json!({"location": "Tokyo"}),
            }],
        );
        let req = build_request(&opts(), &[msg], false);
        let v = serde_json::to_value(&req).unwrap();
        let m = &v["messages"][0];
        assert_eq!(m["role"], "assistant");
        assert_eq!(m["content"], "calling it");
        assert_eq!(m["tool_calls"][0]["id"], "call_1");
        assert_eq!(m["tool_calls"][0]["function"]["name"], "get_weather");
        // arguments is a JSON-encoded string
        let args = m["tool_calls"][0]["function"]["arguments"].as_str().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(args).unwrap();
        assert_eq!(parsed["location"], "Tokyo");
    }

    #[test]
    fn assistant_with_only_tool_calls_omits_content() {
        let msg = Message::with_tool_uses(
            None,
            vec![ContentBlock::ToolUse {
                id: "call_x".into(),
                name: "ping".into(),
                input: serde_json::json!({}),
            }],
        );
        let req = build_request(&opts(), &[msg], false);
        let v = serde_json::to_value(&req).unwrap();
        assert!(
            v["messages"][0].get("content").is_none(),
            "content omitted when assistant only calls tools"
        );
        assert_eq!(v["messages"][0]["tool_calls"][0]["function"]["name"], "ping");
    }

    #[test]
    fn tool_results_become_separate_tool_role_messages() {
        let msg = Message::tool_results(vec![
            ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "ok".into(),
                is_error: false,
            },
            ContentBlock::ToolResult {
                tool_use_id: "call_2".into(),
                content: "boom".into(),
                is_error: true,
            },
        ]);
        let req = build_request(&opts(), &[msg], false);
        let v = serde_json::to_value(&req).unwrap();
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2, "two tool results → two tool messages");
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["tool_call_id"], "call_1");
        assert_eq!(msgs[0]["content"], "ok");
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "call_2");
        // error signal preserved, not silently flattened
        assert!(msgs[1]["content"].as_str().unwrap().contains("[tool error]"));
        assert!(msgs[1]["content"].as_str().unwrap().contains("boom"));
    }

    #[test]
    fn resolved_image_becomes_data_url_part() {
        let msg = Message::tool_results(vec![ContentBlock::Image {
            hash: "abc123".into(),
            media_type: "image/png".into(),
            data_base64: Some("QUJD".into()),
        }]);
        // (tool_results just wraps blocks in a User message — fine for this test)
        let req = build_request(&opts(), &[msg], false);
        let v = serde_json::to_value(&req).unwrap();
        let parts = v["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "image_url");
        assert_eq!(parts[0]["image_url"]["url"], "data:image/png;base64,QUJD");
    }

    #[test]
    fn unresolved_image_surfaces_marker_not_silent_drop() {
        let msg = Message::tool_results(vec![ContentBlock::Image {
            hash: "deadbeef".into(),
            media_type: "image/png".into(),
            data_base64: None,
        }]);
        let req = build_request(&opts(), &[msg], false);
        let v = serde_json::to_value(&req).unwrap();
        // single text part collapses to a plain string
        let content = v["messages"][0]["content"].as_str().unwrap();
        assert!(content.contains("deadbeef"));
        assert!(content.contains("unavailable"));
    }

    #[test]
    fn max_tokens_clamps_zero_to_default() {
        let mut o = opts();
        o.max_tokens = 0;
        let req = build_request(&o, &[Message::user("hi")], false);
        assert!(req.max_tokens > 0, "zero must become a sane default");
    }
}
