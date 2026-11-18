//! Translate kaijutsu [`Message`] / [`ContentBlock`] into a `/v1/responses`
//! request body under the ChatGPT plan contract.
//!
//! The plan route (OpenAI's "Sign in with ChatGPT" plan usage) accepts a
//! narrower request than the API-key route:
//!
//! - `store: false` and `stream: true` on every request.
//! - An input item with role `system` is refused, so the system prompt is a
//!   `developer` message at the start of `input`, and a later system message
//!   is a `developer` message where it stands. Not `instructions`: from
//!   GPT-6.2 on, top-level instructions are placed after the input, outside
//!   the cached prefix, and billed uncached on every request.
//! - No `max_output_tokens`, `temperature`, `top_p`, `previous_response_id`,
//!   `metadata`, `truncation`, `user`, or `prompt_cache_retention`. The whole
//!   history goes in `input`; only the WebSocket transport ([`super::ws`])
//!   replaces it with `previous_response_id` and the new items.
//!
//! The binding's `max_tokens`, `temperature`, `top_p` and `thinking_budget`
//! therefore have no sink here; an explicitly set sampling knob or budget is
//! warned once per backend, model and value.
//!
//! Cache affinity: the plan route keeps prompt caches per machine and
//! routes by the `session-id` header. A request inside a turn names its
//! context id as [`cache_session`], sent as that header and as
//! `prompt_cache_key`, so a context's requests reach a machine holding its
//! prefix. Without it the requests scatter and rarely hit.
//!
//! Reasoning continues across requests through `encrypted_content`: the
//! request asks for `reasoning.encrypted_content`, the stream puts it in the
//! [`crate::llm::StreamEvent::ThinkingEnd`] signature behind
//! [`REASONING_SIGNATURE_PREFIX`], and this builder sends it back as a
//! `reasoning` item within the current turn: from the last user message
//! that is not only tool results. Earlier reasoning is left out, since the
//! model continues its reasoning only across a turn's tool calls, and each
//! item is kilobytes of input. A reasoning block without that prefix came
//! from another provider and is left out.

use serde_json::{Value, json};

use crate::llm::openai::build::should_warn_effort_dropped;
use crate::llm::stream::BuildOpts;
use crate::llm::{ContentBlock, Message, MessageContent, Role};

/// The prefix on a [`crate::llm::StreamEvent::ThinkingEnd`] signature that
/// holds a Responses reasoning item's `encrypted_content`.
pub const REASONING_SIGNATURE_PREFIX: &str = "openai-responses:";

/// The cache session a request belongs to: its context id, when it runs
/// inside a turn.
pub fn cache_session(opts: &BuildOpts) -> Option<String> {
    opts.trace.as_ref().map(|t| t.context.to_string())
}

/// Build a plan-contract `/v1/responses` body. `backend_label` names the
/// backend in the warnings for knobs this route drops.
pub fn build_request(opts: &BuildOpts, messages: &[Message], backend_label: &str) -> Value {
    let turn_start = messages.iter().rposition(opens_a_turn).unwrap_or(0);
    let mut input = Vec::with_capacity(messages.len() + 1);
    if let Some(system) = &opts.system {
        input.push(text_message("developer", "input_text", system));
    }
    for (i, msg) in messages.iter().enumerate() {
        match msg.role {
            Role::User => translate_user(msg, &mut input),
            Role::Assistant => translate_assistant(msg, &mut input, i > turn_start),
            Role::System => input.push(text_message("developer", "input_text", &plain_text(&msg.content))),
        }
    }

    warn_dropped(opts, backend_label);

    let mut body = json!({
        "model": opts.model,
        "input": input,
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "reasoning": { "summary": "auto" },
    });
    if let Some(session) = cache_session(opts) {
        body["prompt_cache_key"] = Value::String(session);
    }
    if let Some(effort) = &opts.effort {
        body["reasoning"]["effort"] = Value::String(effort.clone());
    }
    if !opts.tools.is_empty() {
        let tools: Vec<Value> = opts
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                    "strict": false,
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = Value::String("auto".into());
        body["parallel_tool_calls"] = Value::Bool(true);
    }
    body
}

/// Whether `msg` starts a turn: a user message with more than tool results.
fn opens_a_turn(msg: &Message) -> bool {
    msg.role == Role::User
        && match &msg.content {
            MessageContent::Text(_) => true,
            MessageContent::Blocks(blocks) => blocks.iter().any(|b| !matches!(b, ContentBlock::ToolResult { .. })),
        }
}

/// Warn once for each knob the binding set that this route cannot send.
fn warn_dropped(opts: &BuildOpts, backend_label: &str) {
    let knobs = [
        ("temperature", opts.temperature.map(|v| v.to_string())),
        ("top_p", opts.top_p.map(|v| v.to_string())),
        ("thinking_budget", opts.thinking_budget.map(|v| v.to_string())),
    ];
    for (knob, value) in knobs {
        if let Some(value) = value
            && should_warn_effort_dropped(backend_label, &opts.model, knob, &value)
        {
            tracing::warn!(
                backend = backend_label,
                model = %opts.model,
                knob,
                value = %value,
                "the ChatGPT plan route takes no {knob}; dropped"
            );
        }
    }
}

/// A `message` input item with one content part of `part_type`.
fn text_message(role: &str, part_type: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "role": role,
        "content": [{ "type": part_type, "text": text }],
    })
}

/// The text of a message, with blocks other than text left out.
fn plain_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// User turn → a `function_call_output` item per tool result, then one user
/// message with the turn's text and images.
fn translate_user(msg: &Message, out: &mut Vec<Value>) {
    let blocks = match &msg.content {
        MessageContent::Text(text) => {
            out.push(text_message("user", "input_text", text));
            return;
        }
        MessageContent::Blocks(blocks) => blocks,
    };
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                // A function call output has no error flag, so the error is
                // stated in the text.
                let output = if *is_error { format!("[tool error]\n{content}") } else { content.clone() };
                out.push(json!({ "type": "function_call_output", "call_id": tool_use_id, "output": output }));
            }
            ContentBlock::Text { text } => parts.push(json!({ "type": "input_text", "text": text })),
            ContentBlock::Media { hash, media_type, data_base64 } => {
                match crate::llm::image_or_marker(hash, media_type, data_base64.as_deref()) {
                    Ok(data) => parts.push(json!({
                        "type": "input_image",
                        "image_url": format!("data:{media_type};base64,{data}"),
                    })),
                    Err(marker) => parts.push(json!({ "type": "input_text", "text": marker })),
                }
            }
            ContentBlock::ToolUse { .. } | ContentBlock::Reasoning { .. } => {}
        }
    }
    if !parts.is_empty() {
        out.push(json!({ "type": "message", "role": "user", "content": parts }));
    }
}

/// Assistant turn → reasoning, message and `function_call` items in block
/// order. Adjacent text blocks share one message item. Reasoning goes only
/// when `keep_reasoning`.
fn translate_assistant(msg: &Message, out: &mut Vec<Value>, keep_reasoning: bool) {
    let blocks = match &msg.content {
        MessageContent::Text(text) => {
            out.push(text_message("assistant", "output_text", text));
            return;
        }
        MessageContent::Blocks(blocks) => blocks,
    };
    let mut text: Vec<Value> = Vec::new();
    let flush = |text: &mut Vec<Value>, out: &mut Vec<Value>| {
        if !text.is_empty() {
            out.push(json!({ "type": "message", "role": "assistant", "content": std::mem::take(text) }));
        }
    };
    for block in blocks {
        match block {
            ContentBlock::Text { text: t } => text.push(json!({ "type": "output_text", "text": t })),
            ContentBlock::Reasoning { text: summary, signature } => {
                let Some(encrypted) = signature.as_deref().and_then(|s| s.strip_prefix(REASONING_SIGNATURE_PREFIX))
                else {
                    continue;
                };
                if !keep_reasoning {
                    continue;
                }
                flush(&mut text, out);
                let summary: Vec<Value> = if summary.is_empty() {
                    Vec::new()
                } else {
                    vec![json!({ "type": "summary_text", "text": summary })]
                };
                out.push(json!({ "type": "reasoning", "summary": summary, "encrypted_content": encrypted }));
            }
            ContentBlock::ToolUse { id, name, input, .. } => {
                flush(&mut text, out);
                out.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": input.to_string(),
                }));
            }
            ContentBlock::ToolResult { .. } | ContentBlock::Media { .. } => {}
        }
    }
    flush(&mut text, out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolDefinition;

    fn body(opts: BuildOpts, messages: &[Message]) -> Value {
        build_request(&opts, messages, "test")
    }

    /// The plan contract's fixed fields ride on every request, and the system
    /// prompt is the first input item, a `developer` message: never a
    /// `system` item, and never `instructions`, which GPT-6.2 bills uncached.
    ///
    /// Falsified by a missing `store: false`, a `system` role in `input`, an
    /// `instructions` field, or
    /// any field the plan route refuses.
    #[test]
    fn a_request_carries_the_plan_contract() {
        let opts = BuildOpts::new("gpt-6.1-sol", 4096)
            .with_system("be brief")
            .with_temperature(0.3)
            .with_top_p(0.9)
            .with_effort("high");
        let v = body(opts, &[Message::user("hi")]);
        assert_eq!(v["store"], false);
        assert_eq!(v["stream"], true);
        assert!(v.get("instructions").is_none(), "{v}");
        assert_eq!(v["input"][0], json!({ "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "be brief" }] }));
        assert_eq!(v["reasoning"], json!({ "summary": "auto", "effort": "high" }));
        assert_eq!(v["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(v["input"][1], json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "hi" }] }));
        for refused in [
            "max_output_tokens", "temperature", "top_p", "previous_response_id", "metadata",
            "truncation", "user", "prompt_cache_retention", "max_tokens",
        ] {
            assert!(v.get(refused).is_none(), "{refused} must not be sent: {v}");
        }
    }

    /// A system message after the first becomes a developer message.
    #[test]
    fn a_later_system_message_is_a_developer_message() {
        let v = body(BuildOpts::new("m", 1), &[Message::user("a"), Message::system("note")]);
        assert_eq!(v["input"][1], json!({ "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "note" }] }));
    }

    /// A tool round trip: the call becomes a `function_call` with string
    /// arguments, its result a `function_call_output` carrying the error in
    /// its text, and our own reasoning goes back with its encrypted content
    /// while another provider's is left out.
    #[test]
    fn tool_calls_results_and_reasoning_round_trip() {
        let assistant = Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Reasoning { text: "think".into(), signature: Some(format!("{REASONING_SIGNATURE_PREFIX}ENC")) },
                ContentBlock::Reasoning { text: "claude's".into(), signature: Some("sig".into()) },
                ContentBlock::Text { text: "looking".into() },
                ContentBlock::ToolUse { id: "call_1".into(), name: "ls".into(), input: json!({ "path": "/" }), signature: None },
            ]),
            turn_scoped: false,
        };
        let results = Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: "denied".into(),
            is_error: true,
        }]);
        let v = body(BuildOpts::new("m", 1), &[Message::user("go"), assistant, results]);
        let input = v["input"].as_array().unwrap();
        assert_eq!(input.len(), 5, "{v}");
        assert_eq!(input[1], json!({ "type": "reasoning", "summary": [{ "type": "summary_text", "text": "think" }], "encrypted_content": "ENC" }));
        assert_eq!(input[2], json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "looking" }] }));
        assert_eq!(input[3], json!({ "type": "function_call", "call_id": "call_1", "name": "ls", "arguments": "{\"path\":\"/\"}" }));
        assert_eq!(input[4], json!({ "type": "function_call_output", "call_id": "call_1", "output": "[tool error]\ndenied" }));
    }

    /// Our reasoning goes back only within the current turn; an earlier
    /// turn's is left out.
    ///
    /// Falsified by a reasoning item from before the last user message.
    #[test]
    fn reasoning_goes_back_only_within_the_turn() {
        let thought = |enc: &str| Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Reasoning { text: String::new(), signature: Some(format!("{REASONING_SIGNATURE_PREFIX}{enc}")) },
                ContentBlock::ToolUse { id: format!("c-{enc}"), name: "ls".into(), input: json!({}), signature: None },
            ]),
            turn_scoped: false,
        };
        let result = |enc: &str| Message::tool_results(vec![ContentBlock::ToolResult { tool_use_id: format!("c-{enc}"), content: "ok".into(), is_error: false }]);
        let history = [Message::user("one"), thought("OLD"), result("OLD"), Message::assistant("done"), Message::user("two"), thought("NEW"), result("NEW")];
        let v = body(BuildOpts::new("m", 1), &history);
        let kept: Vec<&str> = v["input"].as_array().unwrap().iter().filter_map(|i| i["encrypted_content"].as_str()).collect();
        assert_eq!(kept, ["NEW"], "{v}");
    }

    /// Tools are function tools; a request without tools names none.
    #[test]
    fn tools_are_function_tools() {
        let tool = ToolDefinition { name: "ls".into(), description: "list".into(), input_schema: json!({ "type": "object" }) };
        let v = body(BuildOpts::new("m", 1).with_tools(vec![tool]), &[Message::user("a")]);
        assert_eq!(v["tools"], json!([{ "type": "function", "name": "ls", "description": "list", "parameters": { "type": "object" }, "strict": false }]));
        assert_eq!(v["tool_choice"], "auto");
        let bare = body(BuildOpts::new("m", 1), &[Message::user("a")]);
        assert!(bare.get("tools").is_none() && bare.get("tool_choice").is_none(), "{bare}");
    }

    /// An image goes as a data URL; media without bytes leaves a marker.
    #[test]
    fn images_are_data_urls_and_missing_media_is_a_marker() {
        let msg = Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![
                ContentBlock::Media { hash: "h1".into(), media_type: "image/png".into(), data_base64: Some("AAAA".into()) },
                ContentBlock::Media { hash: "h2".into(), media_type: "image/png".into(), data_base64: None },
            ]),
            turn_scoped: false,
        };
        let v = body(BuildOpts::new("m", 1), &[msg]);
        let parts = &v["input"][0]["content"];
        assert_eq!(parts[0], json!({ "type": "input_image", "image_url": "data:image/png;base64,AAAA" }));
        assert!(parts[1]["text"].as_str().unwrap().contains("bytes were not found"), "{v}");
    }
}
