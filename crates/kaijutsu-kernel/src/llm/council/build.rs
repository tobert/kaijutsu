//! Translate kaijutsu [`Message`]s, tools, and effort into council v2
//! messages and tools (gogi's `docs/council-api.md`).
//!
//! - The system prompt is one `system` message, first. A kernel instruction
//!   later in the history is a `system` message at its point, which the
//!   server renders as its own system turn.
//! - Results go back as `tool` messages in the order of the calls they
//!   answer, each naming its call's id, and a failed result is marked with
//!   `is_error`. A history whose results do not answer exactly the
//!   preceding calls is refused.
//! - `Reasoning` blocks go back as `reasoning`; the template decides what to
//!   keep.
//! - The server takes text only. An image becomes a visible marker.

use gogi_proto::{Message as CouncilMessage, Role as CouncilRole, Tool as CouncilTool, ToolCall as CouncilToolCall};
use kaijutsu_council::Json;

use crate::llm::{ContentBlock, LlmError, LlmResult, Message, MessageContent, Role, ToolDefinition};

/// Whether `effort` asks for thinking: `None` when no effort is named,
/// `false` for `none`, and `true` for `xhigh`, `medium`, or `low`. Any
/// other effort is refused.
pub(super) fn effort(effort: Option<&str>) -> LlmResult<Option<bool>> {
    Ok(match effort {
        None => None,
        Some("none") => Some(false),
        Some("xhigh" | "medium" | "low") => Some(true),
        Some(other) => {
            return Err(LlmError::InvalidRequest(format!(
                "the council backend takes effort xhigh, medium, low, or none; got {other:?}"
            )));
        }
    })
}

/// The tool definitions as council v2 tools. An input schema that is not a
/// JSON object is refused.
pub(super) fn tools(tools: &[ToolDefinition]) -> LlmResult<Vec<CouncilTool>> {
    tools
        .iter()
        .map(|t| match &t.input_schema {
            serde_json::Value::Object(schema) => Ok(CouncilTool {
                name: t.name.clone(),
                description: (!t.description.is_empty()).then(|| t.description.clone()),
                input_schema: schema.clone(),
            }),
            _ => Err(LlmError::InvalidRequest(format!("tool {:?} input schema is not a JSON object", t.name))),
        })
        .collect()
}

/// The system prompt and history as council v2 messages
/// (gogi's `docs/council-api.md`, "Contexts"), with snapshot boundaries.
///
/// Calls keep the kernel's ids, and each result names the call it answers.
/// An assistant message leaves out empty text. `snap` marks the system
/// message, the last of each run of tool results, and the message that ends
/// each user turn: the last message before an assistant message, and the
/// last message of all. A later system message is marked as a user message
/// is. A flag depends only on the messages up to the next
/// assistant message, so a later turn never moves an earlier boundary.
pub(super) fn messages(system: Option<&str>, history: &[Message]) -> LlmResult<Vec<CouncilMessage>> {
    let mut out: Vec<CouncilMessage> = walk(system, history)?
        .into_iter()
        .map(|m| -> LlmResult<CouncilMessage> { Ok(match m {
            Walked::System(text) => CouncilMessage::system(text),
            Walked::User(text) => CouncilMessage::user(text),
            Walked::Assistant { content, reasoning, calls } => CouncilMessage {
                content: content.filter(|c| !c.is_empty()).map(Into::into),
                reasoning,
                tool_calls: calls
                    .into_iter()
                    .map(|(id, name, arguments)| match serde_json::to_value(&arguments) {
                        Ok(serde_json::Value::Object(input)) => Ok(CouncilToolCall { id, name, input }),
                        _ => Err(LlmError::InvalidRequest(format!("tool call {id} arguments are not a JSON object"))),
                    })
                    .collect::<LlmResult<_>>()?,
                ..CouncilMessage::assistant("")
            },
            Walked::Tool { id, content, is_error: true } => CouncilMessage::tool_error(id, content),
            Walked::Tool { id, content, .. } => CouncilMessage::tool(id, content),
        }) })
        .collect::<LlmResult<_>>()?;
    for i in 0..out.len() {
        let next = out.get(i + 1).map(|m| m.role);
        out[i].snap = match out[i].role {
            CouncilRole::System if i == 0 => true,
            CouncilRole::Assistant => false,
            CouncilRole::User | CouncilRole::System => matches!(next, None | Some(CouncilRole::Assistant)),
            CouncilRole::Tool => !matches!(next, Some(CouncilRole::Tool)),
        };
    }
    Ok(out)
}

/// One message of the walk the council v2 messages are built from.
enum Walked {
    System(String),
    User(String),
    /// Calls are `(id, name, arguments)`.
    Assistant { content: Option<String>, reasoning: Option<String>, calls: Vec<(String, String, Json)> },
    Tool { id: String, content: String, is_error: bool },
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
    LlmError::InvalidRequest(format!("council: tool results do not pair with their calls at message {index}: {detail}"))
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
                                "council: message {index}: tool call {name:?} ({id}) input is not a JSON object"
                            )));
                        }
                        let arguments: Json = serde_json::from_value(input.clone())
                            .map_err(|e| LlmError::InvalidRequest(format!("council: message {index}: tool call {id}: {e}")))?;
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
    let mut results: Vec<(&str, String, bool)> = Vec::new();
    let mut text = String::new();
    for block in blocks {
        match block {
            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                results.push((tool_use_id.as_str(), content.clone(), *is_error));
            }
            ContentBlock::Text { text: t } => join(&mut text, t),
            ContentBlock::Image { hash, media_type, .. } => {
                join(&mut text, &format!("[image {hash} ({media_type}) omitted: the council backend takes text only]"))
            }
            ContentBlock::ToolUse { .. } | ContentBlock::Reasoning { .. } => {}
        }
    }
    if results.len() != calls.len() {
        return Err(pairing(index, format!("{} call(s) and {} result(s)", calls.len(), results.len())));
    }
    for id in calls.drain(..) {
        let Some(at) = results.iter().position(|(r, _, _)| *r == id) else {
            return Err(pairing(index, format!("no result answers call {id}")));
        };
        let (_, content, is_error) = results.swap_remove(at);
        out.push(Walked::Tool { content, id, is_error });
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
        Message { role: Role::Assistant, content: MessageContent::Blocks(blocks), turn_scoped: false }
    }

    fn wire(system: Option<&str>, history: &[Message]) -> serde_json::Value {
        serde_json::to_value(messages(system, history).unwrap()).unwrap()
    }

    #[test]
    fn the_system_prompt_is_one_message_first_and_an_empty_one_is_omitted() {
        assert_eq!(
            wire(Some("rules"), &[Message::user("hi")]),
            json!([{"role": "system", "content": "rules", "snap": true}, {"role": "user", "content": "hi", "snap": true}])
        );
        assert_eq!(messages(Some(""), &[Message::user("hi")]).unwrap().len(), 1);
        assert_eq!(messages(None, &[Message::user("hi")]).unwrap().len(), 1);
    }

    #[test]
    fn a_stored_unparsed_call_replays_as_an_ordinary_call() {
        let history = vec![
            Message::user("write it"),
            said(vec![call("c", "write", json!({"truncated_arguments": "<parameter=path>"}))]),
            Message::tool_results(vec![result("c", "This call did not run", true)]),
        ];
        let w = wire(None, &history);
        assert_eq!(w[1]["tool_calls"], json!([{"id": "c", "name": "write", "input": {"truncated_arguments": "<parameter=path>"}}]));
        assert_eq!(w[2], json!({"role": "tool", "tool_call_id": "c", "content": "This call did not run", "is_error": true, "snap": true}));
    }

    /// Council v2 messages keep the kernel's call ids, and `snap` falls on
    /// the system message, the end of each run of tool results, and the end
    /// of each user turn, and stays where it was when a turn is appended.
    ///
    /// Falsified by a builder that drops ids, snaps an assistant message, or
    /// snaps each of several tool results.
    #[test]
    fn messages_keep_call_ids_and_snap_the_ends_of_user_turns() {
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
        let first = messages(Some("rules"), &history).unwrap();
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            json!([
                {"role": "system", "content": "rules", "snap": true},
                {"role": "user", "content": "look", "snap": true},
                {"role": "assistant", "reasoning": "two calls",
                 "tool_calls": [{"id": "a", "name": "ls", "input": {"path": "/"}},
                                {"id": "b", "name": "cat", "input": {"path": "/x"}}]},
                {"role": "tool", "tool_call_id": "a", "content": "x"},
                {"role": "tool", "tool_call_id": "b", "content": "no such file", "is_error": true, "snap": true},
                {"role": "user", "content": "and hurry", "snap": true},
            ])
        );
        history.push(said(vec![ContentBlock::Text { text: "done".into() }]));
        history.push(Message::user("thanks"));
        let second = messages(Some("rules"), &history).unwrap();
        assert_eq!(second[..first.len()], first[..], "appending a turn moves no earlier boundary");
        assert_eq!(serde_json::to_value(&second[6]).unwrap(), json!({"role": "assistant", "content": "done"}));
        assert!(second[7].snap);
    }

    /// A kernel instruction is a `system` message at its point in the
    /// history, and its boundary falls where a user message's would. One
    /// that follows unanswered calls is refused.
    #[test]
    fn a_kernel_instruction_is_a_system_message_at_its_point() {
        let history = vec![
            Message::user("list the files"),
            said(vec![ContentBlock::Reasoning { text: "one file".into(), signature: None }, ContentBlock::Text { text: "There is a.txt.".into() }]),
            Message::system("You replied without a tool call."),
        ];
        assert_eq!(
            wire(Some("rules"), &history),
            json!([
                {"role": "system", "content": "rules", "snap": true},
                {"role": "user", "content": "list the files", "snap": true},
                {"role": "assistant", "content": "There is a.txt.", "reasoning": "one file"},
                {"role": "system", "content": "You replied without a tool call.", "snap": true},
            ])
        );
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
        let w = wire(None, &history);
        assert_eq!(w[2], json!({"role": "tool", "tool_call_id": "a", "content": "done", "snap": true}));
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
    fn effort_says_whether_the_turn_asks_for_thinking() {
        assert_eq!(effort(None).unwrap(), None);
        assert_eq!(effort(Some("none")).unwrap(), Some(false));
        for asks in ["low", "medium", "xhigh"] {
            assert_eq!(effort(Some(asks)).unwrap(), Some(true), "{asks}");
        }
        for other in ["high", "max", ""] {
            assert!(matches!(effort(Some(other)), Err(LlmError::InvalidRequest(_))), "{other}");
        }
    }

    #[test]
    fn tools_keep_their_schema_and_a_non_object_schema_is_refused() {
        let defs = vec![
            ToolDefinition { name: "ls".into(), description: "List.".into(), input_schema: json!({"type": "object"}) },
            ToolDefinition { name: "pwd".into(), description: String::new(), input_schema: json!({"type": "object"}) },
        ];
        assert_eq!(
            serde_json::to_value(tools(&defs).unwrap()).unwrap(),
            json!([
                {"name": "ls", "description": "List.", "input_schema": {"type": "object"}},
                {"name": "pwd", "input_schema": {"type": "object"}},
            ])
        );
        let bad = [ToolDefinition { name: "x".into(), description: String::new(), input_schema: json!(true) }];
        assert!(matches!(tools(&bad), Err(LlmError::InvalidRequest(_))));
    }
}
