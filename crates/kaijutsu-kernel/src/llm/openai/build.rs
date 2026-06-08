//! Translate kaijutsu [`Message`] / [`ContentBlock`] into the DeepSeek
//! (OpenAI-compatible) [`ChatRequest`] shape.
//!
//! Specializations that distinguish DeepSeek from Claude's `build`:
//!
//! - **`Reasoning` blocks become `reasoning_content`.** DeepSeek V4
//!   thinks by default and *requires* the chain-of-thought to be echoed
//!   back on any assistant turn that performed tool calls (else HTTP 400);
//!   on other turns it is ignored. Since the API checks only presence, we
//!   set `reasoning_content` on every assistant message — the real text
//!   when a `Reasoning` block is present (the in-turn agentic loop carries
//!   it), an empty string otherwise (tool-only turns, and cross-provider
//!   history hydrated without thinking blocks). See `super::types`.
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
///
/// `reasoning_required` is the DeepSeek V4 quirk: when `true`, every
/// assistant message carries `reasoning_content` (real, synthesized, or
/// `""`) so tool-call turns survive the API's presence check. Plain
/// OpenAI-compatible servers pass `false` — the field is then emitted only
/// when genuine reasoning exists, and omitted otherwise.
pub fn build_request(
    opts: &BuildOpts,
    messages: &[Message],
    streaming: bool,
    reasoning_required: bool,
) -> ChatRequest {
    let mut wire: Vec<RequestMessage> = Vec::with_capacity(messages.len() + 1);

    if let Some(system) = &opts.system {
        wire.push(RequestMessage::system(system.clone()));
    }

    for msg in messages {
        match msg.role {
            Role::User => translate_user(msg, &mut wire),
            Role::Assistant => translate_assistant(msg, &mut wire, reasoning_required),
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
        reasoning_content: None,
        tool_calls: Vec::new(),
        tool_call_id: None,
    }
}

/// Assistant turn → one assistant message. Text blocks concatenate into
/// `content`; ToolUse blocks become `tool_calls`; a Reasoning block becomes
/// `reasoning_content`. When `reasoning_required`, every assistant message
/// carries `reasoning_content` (empty string when there is no Reasoning
/// block) so tool-call turns meet DeepSeek's V4 round-trip requirement;
/// otherwise the field is emitted only when genuine reasoning exists. An
/// assistant message with only tool calls has `content: None`.
fn translate_assistant(msg: &Message, out: &mut Vec<RequestMessage>, reasoning_required: bool) {
    match &msg.content {
        MessageContent::Text(t) => {
            out.push(RequestMessage::assistant(
                Some(WireContent::Text(t.clone())),
                reasoning_required.then(String::new),
                Vec::new(),
            ));
        }
        MessageContent::Blocks(blocks) => {
            let mut text = String::new();
            let mut reasoning = String::new();
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
                    // Echoed back as reasoning_content (V4 requires it on
                    // tool-call turns). Concatenate if several are present.
                    ContentBlock::Reasoning { text: r, .. } => {
                        if !reasoning.is_empty() {
                            reasoning.push('\n');
                        }
                        reasoning.push_str(r);
                    }
                    // Tool results never appear in an assistant turn.
                    ContentBlock::ToolResult { .. } => {}
                    ContentBlock::Image { .. } => {}
                }
            }
            // Real reasoning rides verbatim, always. Beyond that the
            // behavior splits on `reasoning_required`:
            //   - DeepSeek (true): a tool-call turn that lost its
            //     chain-of-thought gets a synthesized marker (V4 requires the
            //     field on tool-call turns); a plain turn gets "" (the API
            //     ignores it, so "" is the cheapest way to keep the field
            //     present and uniform).
            //   - Generic OpenAI-compatible (false): omit the field entirely
            //     when there's no genuine reasoning — no synth, no echo noise.
            let reasoning_content = if !reasoning.is_empty() {
                Some(reasoning)
            } else if reasoning_required {
                if !tool_calls.is_empty() {
                    Some(synthesized_reasoning(&tool_calls))
                } else {
                    Some(String::new())
                }
            } else {
                None
            };
            let content = (!text.is_empty()).then_some(WireContent::Text(text));
            out.push(RequestMessage::assistant(
                content,
                reasoning_content,
                tool_calls,
            ));
        }
    }
}

/// Fallback `reasoning_content` for a tool-call turn whose real
/// chain-of-thought is unavailable (history forked from another provider, or
/// reasoning never captured). DeepSeek V4 only checks that the field is
/// present, so we emit a non-empty, bracketed marker that names the tool(s)
/// called. Per DeepSeek's own guidance: the `[…]` convention reads as a
/// meta-note rather than a thought, the "synthesized" tag stops the model
/// mistaking it for genuine prior reasoning, and the tool name gives one
/// factual clue about *what* was happening without inventing *why* — so the
/// model leans on the tool result instead of a ghost thought. An empty string
/// also passes the 400 check but wastes the slot the model will read.
fn synthesized_reasoning(tool_calls: &[RequestToolCall]) -> String {
    let names: Vec<&str> = tool_calls
        .iter()
        .map(|t| t.function.name.as_str())
        .collect();
    format!(
        "[synthesized: prior reasoning unavailable; called {}]",
        names.join(", ")
    )
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
        let req = build_request(&o, &[Message::user("hi")], false, true);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["messages"][0]["role"], "system");
        assert_eq!(v["messages"][0]["content"], "be terse");
        assert_eq!(v["messages"][1]["role"], "user");
        assert_eq!(v["messages"][1]["content"], "hi");
    }

    #[test]
    fn streaming_sets_stream_and_include_usage() {
        let req = build_request(&opts(), &[Message::user("hi")], true, true);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["stream"], true);
        assert_eq!(v["stream_options"]["include_usage"], true);
    }

    #[test]
    fn non_streaming_omits_stream_fields() {
        let req = build_request(&opts(), &[Message::user("hi")], false, true);
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
        let req = build_request(&o, &[Message::user("hi")], false, true);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["function"]["name"], "read_file");
        assert_eq!(v["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn reasoning_block_round_trips_as_reasoning_content() {
        // V4 contract: the chain-of-thought is echoed back, separate from
        // content — required on tool-call turns, ignored otherwise.
        let msg = Message::with_reasoning_text_and_tool_uses(
            vec![("internal thoughts".into(), Some("sig".into()))],
            Some("the answer".into()),
            vec![],
        );
        let req = build_request(&opts(), &[msg], false, true);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["messages"][0]["content"], "the answer");
        assert_eq!(
            v["messages"][0]["reasoning_content"], "internal thoughts",
            "reasoning must echo back as reasoning_content, not in content"
        );
    }

    #[test]
    fn tool_call_turn_without_reasoning_gets_synthesized_marker() {
        // The synthesized fallback: a tool-call assistant turn that carries
        // no Reasoning block (cross-provider history, or a tool-only turn)
        // still emits a non-empty reasoning_content so DeepSeek V4 won't 400 —
        // an honest, bracketed marker naming the tool, not fake reasoning.
        let msg = Message::with_tool_uses(
            None,
            vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "ls".into(),
                input: serde_json::json!({}),
            }],
        );
        let req = build_request(&opts(), &[msg], false, true);
        let v = serde_json::to_value(&req).unwrap();
        let rc = v["messages"][0]["reasoning_content"].as_str().unwrap();
        assert!(!rc.is_empty(), "tool-call turn must carry non-empty reasoning_content");
        assert!(rc.contains("synthesized"), "marker must flag itself as not genuine: {rc}");
        assert!(rc.contains("ls"), "marker should name the tool called: {rc}");
        assert_eq!(v["messages"][0]["tool_calls"][0]["function"]["name"], "ls");
    }

    #[test]
    fn synthesized_marker_lists_multiple_tools() {
        let msg = Message::with_tool_uses(
            None,
            vec![
                ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({}),
                },
                ContentBlock::ToolUse {
                    id: "c2".into(),
                    name: "grep".into(),
                    input: serde_json::json!({}),
                },
            ],
        );
        let req = build_request(&opts(), &[msg], false, true);
        let v = serde_json::to_value(&req).unwrap();
        let rc = v["messages"][0]["reasoning_content"].as_str().unwrap();
        assert!(rc.contains("read_file") && rc.contains("grep"), "both tools named: {rc}");
    }

    #[test]
    fn non_assistant_messages_omit_reasoning_content() {
        // Only assistant turns carry the field; system/user/tool skip it.
        let o = opts().with_system("be terse");
        let req = build_request(&o, &[Message::user("hi")], false, true);
        let v = serde_json::to_value(&req).unwrap();
        assert!(v["messages"][0].get("reasoning_content").is_none()); // system
        assert!(v["messages"][1].get("reasoning_content").is_none()); // user
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
        let req = build_request(&opts(), &[msg], false, true);
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
        let req = build_request(&opts(), &[msg], false, true);
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
        let req = build_request(&opts(), &[msg], false, true);
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
        let req = build_request(&opts(), &[msg], false, true);
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
        let req = build_request(&opts(), &[msg], false, true);
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
        let req = build_request(&o, &[Message::user("hi")], false, true);
        assert!(req.max_tokens > 0, "zero must become a sane default");
    }

    // ── Generic OpenAI-compatible mode (reasoning_required = false) ──────
    // These assert the inverse of the DeepSeek contract above: no echoed
    // empty string, no synthesized marker — the field is present only when
    // genuine reasoning exists. Lemonade/llama.cpp ignore the field on
    // input, so emitting it would be noise the model might read.

    #[test]
    fn generic_text_assistant_omits_reasoning_content() {
        let msg = Message::assistant("plain answer");
        let req = build_request(&opts(), &[msg], false, false);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["messages"][0]["content"], "plain answer");
        assert!(
            v["messages"][0].get("reasoning_content").is_none(),
            "generic mode must omit reasoning_content on a plain assistant turn"
        );
    }

    #[test]
    fn generic_tool_call_turn_omits_synthesized_reasoning() {
        // A tool-call turn with no Reasoning block: DeepSeek would synthesize
        // a marker; generic mode emits nothing.
        let msg = Message::with_tool_uses(
            None,
            vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "ls".into(),
                input: serde_json::json!({}),
            }],
        );
        let req = build_request(&opts(), &[msg], false, false);
        let v = serde_json::to_value(&req).unwrap();
        assert!(
            v["messages"][0].get("reasoning_content").is_none(),
            "generic mode must not synthesize reasoning_content on tool-call turns"
        );
        assert_eq!(v["messages"][0]["tool_calls"][0]["function"]["name"], "ls");
    }

    #[test]
    fn generic_mode_still_echoes_real_reasoning() {
        // Genuine reasoning is preserved regardless of the flag — a local
        // reasoning model (Gemma-4) emits it and the round-trip keeps it.
        let msg = Message::with_reasoning_text_and_tool_uses(
            vec![("real thoughts".into(), None)],
            Some("the answer".into()),
            vec![],
        );
        let req = build_request(&opts(), &[msg], false, false);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["messages"][0]["content"], "the answer");
        assert_eq!(
            v["messages"][0]["reasoning_content"], "real thoughts",
            "genuine reasoning must survive even in generic mode"
        );
    }
}
