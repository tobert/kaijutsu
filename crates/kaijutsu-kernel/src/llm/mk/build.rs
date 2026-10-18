//! Translate kaijutsu [`Message`]s, tools, and tunables into a
//! `/mk/v1/generate` request (`docs/mk.md`, "Request mapping"), or into
//! council v2 messages (`docs/council-api-v2.md`).
//!
//! - The system prompt is one `system` message, first. A kernel instruction
//!   later in the history is a `system` message at its point, which the
//!   service renders as its own system turn.
//! - `/mk/v1` has no tool-call ids. Results go back as `tool` messages in
//!   the order of the calls they answer, matched by the kernel's ids; a
//!   history whose results do not answer exactly the preceding calls is
//!   refused. Council v2 keeps the ids.
//! - `Reasoning` blocks go back as `reasoning_content`; the template decides
//!   what to keep.
//! - The service takes text only. An image becomes a visible marker.

use kaijutsu_mk::Json;
use kaijutsu_mk::council_v2::wire::{Message as V2Message, Role as V2Role, ToolCall as V2ToolCall};
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
    Ok(walk(system, history)?
        .into_iter()
        .map(|m| match m {
            Walked::System(text) => MkMessage::system(text),
            Walked::User(text) => MkMessage::user(text),
            Walked::Assistant { content, reasoning, calls } => MkMessage {
                role: MkRole::Assistant,
                content,
                reasoning_content: reasoning,
                tool_calls: calls.into_iter().map(|(_, name, arguments)| ToolCallIn { name, arguments }).collect(),
            },
            Walked::Tool { content, .. } => MkMessage::tool(content),
        })
        .collect())
}

/// The system prompt and history as council v2 messages
/// (`docs/council-api-v2.md`, "Contexts"), with snapshot boundaries.
///
/// Calls keep the kernel's ids, and each result names the call it answers.
/// An assistant message leaves out empty text. `snap` marks the system
/// message, the last of each run of tool results, and the message that ends
/// each user turn: the last message before an assistant message, and the
/// last message of all. A later system message is marked as a user message
/// is. A flag depends only on the messages up to the next
/// assistant message, so a later turn never moves an earlier boundary.
pub(super) fn messages_v2(system: Option<&str>, history: &[Message]) -> LlmResult<Vec<V2Message>> {
    let mut out: Vec<V2Message> = walk(system, history)?
        .into_iter()
        .map(|m| match m {
            Walked::System(text) => V2Message::system(text),
            Walked::User(text) => V2Message::user(text),
            Walked::Assistant { content, reasoning, calls } => V2Message {
                content: content.filter(|c| !c.is_empty()),
                reasoning,
                tool_calls: calls.into_iter().map(|(id, name, arguments)| V2ToolCall { id, name, arguments }).collect(),
                ..V2Message::assistant("")
            },
            Walked::Tool { id, content } => V2Message::tool(id, content),
        })
        .collect();
    for i in 0..out.len() {
        let next = out.get(i + 1).map(|m| m.role);
        out[i].snap = match out[i].role {
            V2Role::System if i == 0 => true,
            V2Role::Assistant => false,
            V2Role::User | V2Role::System => matches!(next, None | Some(V2Role::Assistant)),
            V2Role::Tool => !matches!(next, Some(V2Role::Tool)),
        };
    }
    Ok(out)
}

/// One message of the walk both shapes are built from.
enum Walked {
    System(String),
    User(String),
    /// Calls are `(id, name, arguments)`.
    Assistant { content: Option<String>, reasoning: Option<String>, calls: Vec<(String, String, Json)> },
    Tool { id: String, content: String },
}

fn walk(system: Option<&str>, history: &[Message]) -> LlmResult<Vec<Walked>> {
    let mut out = Vec::new();
    if let Some(system) = system.filter(|s| !s.is_empty()) {
        out.push(Walked::System(system.to_string()));
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
            Role::System => {
                if !calls.is_empty() {
                    return Err(pairing(index, format!("a system message follows {} unanswered call(s)", calls.len())));
                }
                out.push(Walked::System(text(&message.content)));
            }
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
fn assistant(index: usize, content: &MessageContent) -> LlmResult<(Walked, Vec<String>)> {
    let mut ids = Vec::new();
    let reply = match content {
        MessageContent::Text(text) => Walked::Assistant { content: Some(text.clone()), reasoning: None, calls: Vec::new() },
        MessageContent::Blocks(blocks) => {
            let mut text = String::new();
            let mut reasoning = String::new();
            let mut calls = Vec::new();
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
                        calls.push((id.clone(), name.clone(), arguments));
                        ids.push(id.clone());
                    }
                    ContentBlock::ToolResult { .. } | ContentBlock::Image { .. } => {}
                }
            }
            Walked::Assistant { content: Some(text), reasoning: (!reasoning.is_empty()).then_some(reasoning), calls }
        }
    };
    Ok((reply, ids))
}

/// A user message: first the `tool` messages answering `calls` in call order,
/// then any text as one `user` message.
fn user(index: usize, content: &MessageContent, calls: &mut Vec<String>, out: &mut Vec<Walked>) -> LlmResult<()> {
    let blocks = match content {
        MessageContent::Text(text) => {
            if !calls.is_empty() {
                return Err(pairing(index, format!("a text message follows {} unanswered call(s)", calls.len())));
            }
            out.push(Walked::User(text.clone()));
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
        out.push(Walked::Tool { content: results.swap_remove(at).1, id });
    }
    if !text.is_empty() {
        out.push(Walked::User(text));
    }
    Ok(())
}

/// The text blocks of `content`, one per line.
fn text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Blocks(blocks) => {
            let mut out = String::new();
            for block in blocks {
                if let ContentBlock::Text { text } = block {
                    join(&mut out, text);
                }
            }
            out
        }
    }
}

fn join(into: &mut String, part: &str) {
    if part.is_empty() {
        return;
    }
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

    /// Council v2 messages keep the kernel's call ids, and `snap` falls on
    /// the system message, the end of each run of tool results, and the end
    /// of each user turn, and stays where it was when a turn is appended.
    ///
    /// Falsified by a builder that drops ids, snaps an assistant message, or
    /// snaps each of several tool results.
    #[test]
    fn v2_messages_keep_call_ids_and_snap_the_ends_of_user_turns() {
        let mut history = vec![
            Message::user("look"),
            said(vec![
                ContentBlock::Reasoning { text: "two calls".into(), signature: None },
                call("a", "ls", json!({"path": "/"})),
                call("b", "cat", json!({"path": "/x"})),
            ]),
            Message::tool_results(vec![
                result("b", "no such file", true),
                result("a", "x", false),
                ContentBlock::Text { text: "and hurry".into() },
            ]),
        ];
        let first = messages_v2(Some("rules"), &history).unwrap();
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            json!([
                {"role": "system", "content": "rules", "snap": true},
                {"role": "user", "content": "look", "snap": true},
                {"role": "assistant", "reasoning": "two calls",
                 "tool_calls": [{"id": "a", "name": "ls", "arguments": {"path": "/"}},
                                {"id": "b", "name": "cat", "arguments": {"path": "/x"}}]},
                {"role": "tool", "tool_call_id": "a", "content": "x"},
                {"role": "tool", "tool_call_id": "b", "content": "[tool error]\nno such file", "snap": true},
                {"role": "user", "content": "and hurry", "snap": true},
            ])
        );
        history.push(said(vec![ContentBlock::Text { text: "done".into() }]));
        history.push(Message::user("thanks"));
        let second = messages_v2(Some("rules"), &history).unwrap();
        assert_eq!(second[..first.len()], first[..], "appending a turn moves no earlier boundary");
        assert_eq!(serde_json::to_value(&second[6]).unwrap(), json!({"role": "assistant", "content": "done"}));
        assert!(second[7].snap);
    }

    /// A kernel instruction is a `system` message at its point in the
    /// history, in both shapes, and its boundary falls where a user
    /// message's would.
    #[test]
    fn a_kernel_instruction_is_a_system_message_at_its_point() {
        let history = vec![
            Message::user("list the files"),
            said(vec![ContentBlock::Reasoning { text: "one file".into(), signature: None }, ContentBlock::Text { text: "There is a.txt.".into() }]),
            Message::system("You replied without a tool call."),
        ];
        assert_eq!(
            serde_json::to_value(messages_v2(Some("rules"), &history).unwrap()).unwrap(),
            json!([
                {"role": "system", "content": "rules", "snap": true},
                {"role": "user", "content": "list the files", "snap": true},
                {"role": "assistant", "content": "There is a.txt.", "reasoning": "one file"},
                {"role": "system", "content": "You replied without a tool call.", "snap": true},
            ])
        );
        let v1 = wire(&messages(Some("rules"), &history).unwrap());
        assert_eq!(v1[3], json!({"role": "system", "content": "You replied without a tool call."}));
        let unanswered = [Message::user("x"), said(vec![call("a", "ls", json!({}))]), Message::system("s")];
        assert!(matches!(messages(None, &unanswered), Err(LlmError::InvalidRequest(_))));
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
