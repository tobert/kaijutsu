//! Translate kaijutsu [`Message`] / [`ContentBlock`] into Gemini's
//! [`GenerateContentRequest`].

use std::collections::HashMap;

use serde_json::json;

use super::models::resolve_thinking;
use super::types::{
    Content, FunctionCall, FunctionDeclaration, FunctionResponse, GenerateContentRequest,
    GenerationConfig, InlineData, Part, Tool,
};
use crate::llm::stream::BuildOpts;
use crate::llm::{ContentBlock, LlmResult, Message, MessageContent, Role};

/// Prefix on `BlockSnapshot.signature` / `ContentBlock::*.signature` for
/// opaque `thoughtSignature` tokens produced by Gemini. Prevents cross-provider
/// signature confusion (e.g. Anthropic's `claude/build.rs` drops any
/// signature containing `:`).
pub const GEMINI_SIG_PREFIX: &str = "gemini:";

fn own_signature(signature: Option<&str>) -> Option<String> {
    signature
        .and_then(|s| s.strip_prefix(GEMINI_SIG_PREFIX))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Build a native Gemini `GenerateContentRequest` from `opts` and `messages`.
pub fn build_request(opts: &BuildOpts, messages: &[Message]) -> LlmResult<GenerateContentRequest> {
    if !opts.cache_breakpoints.is_empty() {
        tracing::debug!(
            count = opts.cache_breakpoints.len(),
            "gemini backend ignores explicit cache_breakpoints"
        );
    }

    let system_instruction = opts
        .system
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(Content::system_instruction);

    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut contents: Vec<Content> = Vec::with_capacity(messages.len());

    for msg in messages {
        let (role, parts) = match msg.role {
            Role::User | Role::System => ("user", translate_user_or_system(&msg.content, &tool_names)),
            Role::Assistant => ("model", translate_assistant(&msg.content, &mut tool_names)),
        };
        if parts.is_empty() {
            continue;
        }
        // Coalesce adjacent same-role turns (e.g., mid-conversation System + User)
        // so `contents` alternates cleanly between `user` and `model`.
        if let Some(last) = contents.last_mut()
            && last.role.as_deref() == Some(role)
        {
            last.parts.extend(parts);
        } else {
            contents.push(Content { role: Some(role.to_string()), parts });
        }
    }

    let tools = (!opts.tools.is_empty()).then(|| {
        vec![Tool {
            function_declarations: opts
                .tools
                .iter()
                .map(|t| FunctionDeclaration {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters_json_schema: Some(t.input_schema.clone()),
                })
                .collect(),
        }]
    });

    let thinking_config = resolve_thinking(opts)?;
    let generation_config = Some(GenerationConfig {
        max_output_tokens: Some(opts.max_tokens),
        temperature: opts.temperature,
        top_p: opts.top_p,
        thinking_config,
    });

    Ok(GenerateContentRequest {
        system_instruction,
        contents,
        tools,
        generation_config,
    })
}

fn translate_user_or_system(
    content: &MessageContent,
    tool_names: &HashMap<String, String>,
) -> Vec<Part> {
    match content {
        MessageContent::Text(text) => {
            if text.is_empty() { Vec::new() } else { vec![Part::text(text)] }
        }
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| build_user_block(b, tool_names))
            .collect(),
    }
}

fn build_user_block(
    block: &ContentBlock,
    tool_names: &HashMap<String, String>,
) -> Option<Part> {
    match block {
        ContentBlock::Text { text } if text.is_empty() => None,
        ContentBlock::Text { text } => Some(Part::text(text)),
        ContentBlock::ToolResult { tool_use_id, content, is_error } => {
            let name = match tool_names.get(tool_use_id) {
                Some(n) => n.clone(),
                None => {
                    tracing::warn!(
                        tool_use_id = %tool_use_id,
                        "ToolResult has no matching ToolUse in conversation; using fallback tool name"
                    );
                    "unknown_tool".to_string()
                }
            };
            let response = if *is_error {
                json!({ "error": content })
            } else {
                json!({ "result": content })
            };
            Some(Part {
                function_response: Some(FunctionResponse {
                    id: Some(tool_use_id.clone()),
                    name,
                    response,
                }),
                ..Default::default()
            })
        }
        // Gemini's API reads audio and video inline too; this builder sends
        // images only until a Gemini omni model needs the rest.
        ContentBlock::Media { hash, media_type, data_base64 } => {
            match crate::llm::image_or_marker(hash, media_type, data_base64.as_deref()) {
                Ok(data) => Some(Part {
                    inline_data: Some(InlineData {
                        mime_type: media_type.clone(),
                        data: data.to_string(),
                    }),
                    ..Default::default()
                }),
                Err(marker) => Some(Part::text(marker)),
            }
        }
        ContentBlock::ToolUse { .. } | ContentBlock::Reasoning { .. } => None,
    }
}

fn translate_assistant(
    content: &MessageContent,
    tool_names: &mut HashMap<String, String>,
) -> Vec<Part> {
    match content {
        MessageContent::Text(text) => {
            if text.is_empty() { Vec::new() } else { vec![Part::text(text)] }
        }
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| build_assistant_block(b, tool_names))
            .collect(),
    }
}

fn build_assistant_block(
    block: &ContentBlock,
    tool_names: &mut HashMap<String, String>,
) -> Option<Part> {
    match block {
        ContentBlock::Reasoning { text, signature } => {
            let sig = own_signature(signature.as_deref())?;
            Some(Part::thought(text, Some(sig)))
        }
        ContentBlock::Text { text } if text.is_empty() => None,
        ContentBlock::Text { text } => Some(Part::text(text)),
        ContentBlock::ToolUse { id, name, input, signature } => {
            tool_names.insert(id.clone(), name.clone());
            Some(Part {
                function_call: Some(FunctionCall {
                    id: Some(id.clone()),
                    name: name.clone(),
                    args: input.clone(),
                }),
                thought_signature: own_signature(signature.as_deref()),
                ..Default::default()
            })
        }
        ContentBlock::ToolResult { .. } | ContentBlock::Media { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolDefinition;

    #[test]
    fn correlates_tool_result_name_and_preserves_thought_signature() {
        let mut opts = BuildOpts::new("gemini-3-pro", 4096);
        opts.system = Some("You are helpful.".into());
        opts.tools = vec![ToolDefinition {
            name: "read_file".into(),
            description: "Read a file".into(),
            input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }];

        let messages = vec![
            Message::user("Read Cargo.toml"),
            Message::with_reasoning_text_and_tool_uses(
                vec![("I should read it.".into(), Some("gemini:sig-think-1".into()))],
                None,
                vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    input: json!({"path": "Cargo.toml"}),
                    signature: Some("gemini:sig-call-1".into()),
                }],
            ),
            Message::tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "[package]".into(),
                is_error: false,
            }]),
        ];

        let req = build_request(&opts, &messages).unwrap();
        assert_eq!(
            req.system_instruction.as_ref().unwrap().parts[0].text.as_deref(),
            Some("You are helpful.")
        );
        assert_eq!(req.contents.len(), 3);
        let model_parts = &req.contents[1].parts;
        assert_eq!(model_parts.len(), 2);
        assert_eq!(model_parts[0].thought, Some(true));
        assert_eq!(model_parts[0].thought_signature.as_deref(), Some("sig-think-1"));
        assert_eq!(model_parts[1].function_call.as_ref().unwrap().name, "read_file");
        assert_eq!(model_parts[1].thought_signature.as_deref(), Some("sig-call-1"));

        let resp = req.contents[2].parts[0].function_response.as_ref().unwrap();
        assert_eq!(resp.name, "read_file");
        assert_eq!(resp.id.as_deref(), Some("call_1"));
        assert_eq!(resp.response, json!({"result": "[package]"}));
    }

    #[test]
    fn drops_foreign_signatures_from_reasoning_and_tool_use() {
        let opts = BuildOpts::new("gemini-3-flash", 2048);
        let messages = vec![Message::with_reasoning_text_and_tool_uses(
            vec![("claude thought".into(), Some("anthropic-base64-sig".into()))],
            Some("calling tool".into()),
            vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "ls".into(),
                input: json!({}),
                signature: Some("foreign-sig".into()),
            }],
        )];

        let req = build_request(&opts, &messages).unwrap();
        let parts = &req.contents[0].parts;
        // Foreign reasoning dropped; text + functionCall kept without foreign signature.
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].text.as_deref(), Some("calling tool"));
        assert!(parts[1].function_call.is_some());
        assert_eq!(parts[1].thought_signature, None);
    }

    #[test]
    fn folds_mid_conversation_system_into_user_turn() {
        let opts = BuildOpts::new("gemini-2.5-flash", 1024);
        let messages = vec![
            Message::system("kernel note"),
            Message::user("hello"),
        ];
        let req = build_request(&opts, &messages).unwrap();
        assert_eq!(req.contents.len(), 1);
        assert_eq!(req.contents[0].role.as_deref(), Some("user"));
        assert_eq!(req.contents[0].parts.len(), 2);
    }
}
