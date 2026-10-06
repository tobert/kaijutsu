//! Translate kaijutsu [`Message`]s, tools, and tunables into a
//! `/mk/v1/generate` request (`docs/mk.md`, "Request mapping").
//!
//! - The system prompt is one `system` message, first.
//! - The service has no tool-call ids. Results go back as `tool` messages in
//!   the order of the calls they answer, matched by the kernel's ids; a
//!   history whose results do not answer exactly the preceding calls is
//!   refused.
//! - `Reasoning` blocks go back as `reasoning_content`; the template decides
//!   what to keep.
//! - The service takes text only. An image becomes a visible marker.

use kaijutsu_mk::Json;
use kaijutsu_mk::generate::{Function, Message as MkMessage, ReasoningEffort, Role as MkRole, Tool, ToolCallIn};

use crate::llm::{ContentBlock, LlmError, LlmResult, Message, MessageContent, Role, ToolDefinition};

/// `effort` as the service's `thinking` and `reasoning_effort`. Absent
/// sends neither, so the template's default applies (thinking on, `xhigh`).
pub(super) fn effort(effort: Option<&str>) -> LlmResult<(Option<bool>, Option<ReasoningEffort>)> {
    Ok(match effort {
        None => (None, None),
        Some("none") => (Some(false), None),
        Some("xhigh") => (Some(true), Some(ReasoningEffort::Xhigh)),
        Some("medium") => (Some(true), Some(ReasoningEffort::Medium)),
        Some("low") => (Some(true), Some(ReasoningEffort::Low)),
        Some(other) => {
            return Err(LlmError::InvalidRequest(format!(
                "the mk backend takes effort xhigh, medium, low, or none; got {other:?}"
            )));
        }
    })
}

/// The tool definitions as the service's function tools.
pub(super) fn tools(tools: &[ToolDefinition]) -> LlmResult<Vec<Tool>> {
    tools
        .iter()
        .map(|t| {
            let parameters: Json = serde_json::from_value(t.input_schema.clone()).map_err(|e| {
                LlmError::InvalidRequest(format!("tool {:?} input schema is not JSON the mk client keeps: {e}", t.name))
            })?;
            Ok(Tool::Function {
                function: Function {
                    name: t.name.clone(),
                    description: (!t.description.is_empty()).then(|| t.description.clone()),
                    parameters: Some(parameters),
                },
            })
        })
        .collect()
}

/// The system prompt and history as the service's messages.
pub(super) fn messages(system: Option<&str>, history: &[Message]) -> LlmResult<Vec<MkMessage>> {
    let mut out = Vec::new();
    if let Some(system) = system.filter(|s| !s.is_empty()) {
        out.push(MkMessage::system(system));
    }
    // The ids of the calls the last assistant message made, still unanswered.
    let mut calls: Vec<String> = Vec::new();
    for (index, message) in history.iter().enumerate() {
        match message.role {
            Role::Assistant => {
                if !calls.is_empty() {
                    return Err(pairing(index, format!("an assistant message follows {} unanswered call(s)", calls.len())));
                }
                let (reply, made) = assistant(index, &message.content)?;
                calls = made;
                out.push(reply);
            }
            Role::User => user(index, &message.content, &mut calls, &mut out)?,
        }
    }
    if !calls.is_empty() {
        return Err(pairing(history.len(), format!("the history ends with {} unanswered call(s)", calls.len())));
    }
    Ok(out)
}

fn pairing(index: usize, detail: String) -> LlmError {
    LlmError::InvalidRequest(format!("mk: tool results do not pair with their calls at message {index}: {detail}"))
}

/// An assistant message, and the ids of the calls it made.
fn assistant(index: usize, content: &MessageContent) -> LlmResult<(MkMessage, Vec<String>)> {
    let mut reply = MkMessage { role: MkRole::Assistant, content: None, reasoning_content: None, tool_calls: Vec::new() };
    let mut ids = Vec::new();
    match content {
        MessageContent::Text(text) => reply.content = Some(text.clone()),
        MessageContent::Blocks(blocks) => {
            let mut text = String::new();
            let mut reasoning = String::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text: t } => join(&mut text, t),
                    ContentBlock::Reasoning { text: r, .. } => join(&mut reasoning, r),
                    ContentBlock::ToolUse { id, name, input } => {
                        if !input.is_object() {
                            return Err(LlmError::InvalidRequest(format!(
                                "mk: message {index}: tool call {name:?} ({id}) input is not a JSON object"
                            )));
                        }
                        let arguments: Json = serde_json::from_value(input.clone())
                            .map_err(|e| LlmError::InvalidRequest(format!("mk: message {index}: tool call {id}: {e}")))?;
                        reply.tool_calls.push(ToolCallIn { name: name.clone(), arguments });
                        ids.push(id.clone());
                    }
                    ContentBlock::ToolResult { .. } | ContentBlock::Image { .. } => {}
                }
            }
            reply.content = Some(text);
            reply.reasoning_content = (!reasoning.is_empty()).then_some(reasoning);
        }
    }
    Ok((reply, ids))
}

/// A user message: first the `tool` messages answering `calls` in call order,
/// then any text as one `user` message.
fn user(index: usize, content: &MessageContent, calls: &mut Vec<String>, out: &mut Vec<MkMessage>) -> LlmResult<()> {
    let blocks = match content {
        MessageContent::Text(text) => {
            if !calls.is_empty() {
                return Err(pairing(index, format!("a text message follows {} unanswered call(s)", calls.len())));
            }
            out.push(MkMessage::user(text.clone()));
            return Ok(());
        }
        MessageContent::Blocks(blocks) => blocks,
    };
    let mut results: Vec<(&str, String)> = Vec::new();
    let mut text = String::new();
    for block in blocks {
        match block {
            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                let body = if *is_error { format!("[tool error]\n{content}") } else { content.clone() };
                results.push((tool_use_id.as_str(), body));
            }
            ContentBlock::Text { text: t } => join(&mut text, t),
            ContentBlock::Image { hash, media_type, .. } => {
                join(&mut text, &format!("[image {hash} ({media_type}) omitted: the mk backend takes text only]"))
            }
            ContentBlock::ToolUse { .. } | ContentBlock::Reasoning { .. } => {}
        }
    }
    if results.len() != calls.len() {
        return Err(pairing(index, format!("{} call(s) and {} result(s)", calls.len(), results.len())));
    }
    for id in calls.drain(..) {
        let Some(at) = results.iter().position(|(r, _)| *r == id) else {
            return Err(pairing(index, format!("no result answers call {id}")));
        };
        out.push(MkMessage::tool(results.swap_remove(at).1));
    }
    if !text.is_empty() {
        out.push(MkMessage::user(text));
    }
    Ok(())
}

fn join(into: &mut String, part: &str) {
    if !into.is_empty() {
        into.push('\n');
    }
    into.push_str(part);
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn call(id: &str, name: &str, input: serde_json::Value) -> ContentBlock {
        ContentBlock::ToolUse { id: id.into(), name: name.into(), input }
    }

    fn result(id: &str, content: &str, is_error: bool) -> ContentBlock {
        ContentBlock::ToolResult { tool_use_id: id.into(), content: content.into(), is_error }
    }

    fn said(blocks: Vec<ContentBlock>) -> Message {
        Message { role: Role::Assistant, content: MessageContent::Blocks(blocks) }
    }

    fn wire(messages: &[MkMessage]) -> serde_json::Value {
        serde_json::to_value(messages).unwrap()
    }

    #[test]
    fn the_system_prompt_is_one_message_first_and_an_empty_one_is_omitted() {
        let m = messages(Some("rules"), &[Message::user("hi")]).unwrap();
        assert_eq!(wire(&m), json!([{"role": "system", "content": "rules"}, {"role": "user", "content": "hi"}]));
        assert_eq!(messages(Some(""), &[Message::user("hi")]).unwrap().len(), 1);
        assert_eq!(messages(None, &[Message::user("hi")]).unwrap().len(), 1);
    }

    #[test]
    fn results_go_back_in_call_order_with_errors_marked() {
        let history = vec![
            Message::user("look"),
            said(vec![
                ContentBlock::Reasoning { text: "two calls".into(), signature: None },
                ContentBlock::Text { text: "checking".into() },
                call("a", "ls", json!({"path": "/"})),
                call("b", "cat", json!({"path": "/x"})),
            ]),
            Message::tool_results(vec![result("b", "no such file", true), result("a", "x\ny", false)]),
        ];
        let m = messages(None, &history).unwrap();
        assert_eq!(
            wire(&m),
            json!([
                {"role": "user", "content": "look"},
                {"role": "assistant", "content": "checking", "reasoning_content": "two calls",
                 "tool_calls": [{"name": "ls", "arguments": {"path": "/"}}, {"name": "cat", "arguments": {"path": "/x"}}]},
                {"role": "tool", "content": "x\ny"},
                {"role": "tool", "content": "[tool error]\nno such file"},
            ])
        );
    }

    #[test]
    fn a_stored_unparsed_call_replays_as_an_ordinary_call() {
        let history = vec![
            Message::user("write it"),
            said(vec![call("c", "write", json!({"truncated_arguments": "<parameter=path>"}))]),
            Message::tool_results(vec![result("c", "This call did not run", true)]),
        ];
        let m = messages(None, &history).unwrap();
        assert_eq!(wire(&m)[1]["tool_calls"], json!([{"name": "write", "arguments": {"truncated_arguments": "<parameter=path>"}}]));
        assert_eq!(wire(&m)[2], json!({"role": "tool", "content": "[tool error]\nThis call did not run"}));
    }

    #[test]
    fn results_that_do_not_answer_the_calls_are_refused() {
        let ask = || said(vec![call("a", "ls", json!({})), call("b", "ls", json!({}))]);
        for history in [
            vec![Message::user("x"), ask(), Message::tool_results(vec![result("a", "", false)])],
            vec![Message::user("x"), ask(), Message::tool_results(vec![result("a", "", false), result("z", "", false)])],
            vec![Message::user("x"), ask(), Message::user("no results")],
            vec![Message::user("x"), ask()],
            vec![Message::user("x"), ask(), ask()],
        ] {
            assert!(matches!(messages(None, &history), Err(LlmError::InvalidRequest(_))), "{history:?}");
        }
    }

    #[test]
    fn text_beside_results_follows_them_and_an_image_is_a_marker() {
        let history = vec![
            Message::user("x"),
            said(vec![call("a", "shot", json!({}))]),
            Message::tool_results(vec![
                ContentBlock::Image { hash: "h1".into(), media_type: "image/png".into(), data_base64: None },
                result("a", "done", false),
                ContentBlock::Text { text: "and look".into() },
            ]),
        ];
        let w = wire(&messages(None, &history).unwrap());
        assert_eq!(w[2], json!({"role": "tool", "content": "done"}));
        assert_eq!(w[3]["role"], json!("user"));
        let text = w[3]["content"].as_str().unwrap();
        assert!(text.contains("[image h1 (image/png) omitted") && text.ends_with("\nand look"), "{text}");
    }

    #[test]
    fn a_call_whose_input_is_not_an_object_is_refused() {
        let history = vec![
            Message::user("x"),
            said(vec![call("a", "ls", json!("not an object"))]),
            Message::tool_results(vec![result("a", "", false)]),
        ];
        assert!(matches!(messages(None, &history), Err(LlmError::InvalidRequest(_))));
    }

    #[test]
    fn effort_maps_to_thinking_and_reasoning_effort() {
        assert_eq!(effort(None).unwrap(), (None, None));
        assert_eq!(effort(Some("none")).unwrap(), (Some(false), None));
        assert_eq!(effort(Some("low")).unwrap(), (Some(true), Some(ReasoningEffort::Low)));
        assert_eq!(effort(Some("medium")).unwrap(), (Some(true), Some(ReasoningEffort::Medium)));
        assert_eq!(effort(Some("xhigh")).unwrap(), (Some(true), Some(ReasoningEffort::Xhigh)));
        for other in ["high", "max", ""] {
            assert!(matches!(effort(Some(other)), Err(LlmError::InvalidRequest(_))), "{other}");
        }
    }

    #[test]
    fn tools_become_function_tools_with_their_schema() {
        let defs = vec![
            ToolDefinition { name: "ls".into(), description: "List.".into(), input_schema: json!({"type": "object"}) },
            ToolDefinition { name: "pwd".into(), description: String::new(), input_schema: json!({"type": "object"}) },
        ];
        assert_eq!(
            serde_json::to_value(tools(&defs).unwrap()).unwrap(),
            json!([
                {"type": "function", "function": {"name": "ls", "description": "List.", "parameters": {"type": "object"}}},
                {"type": "function", "function": {"name": "pwd", "parameters": {"type": "object"}}},
            ])
        );
    }
}
