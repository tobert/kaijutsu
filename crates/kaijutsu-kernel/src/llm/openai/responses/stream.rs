//! `/v1/responses` SSE → kaijutsu [`StreamEvent`] state machine.
//!
//! The Responses stream brackets each output item, so the mapping is direct:
//!
//! ```text
//! Responses event                          emitted
//! ───────────────                          ───────
//! output_item.added (reasoning)            ThinkingStart
//! reasoning_summary_text.delta             ThinkingDelta ("\n\n" between parts)
//! output_item.done (reasoning)             ThinkingEnd{encrypted_content}
//! output_item.added (message)              TextStart
//! output_text.delta / refusal.delta        TextDelta
//! output_item.done (message)               TextEnd
//! output_item.done (function_call)         held until the response completes
//! response.completed                       ToolUse…, Done{usage}
//! response.incomplete (max_output_tokens)  ToolUse…, Done{stop_reason: "max_tokens"}
//! response.incomplete (other)              Error
//! response.failed / error                  Error
//! end of stream before a terminal event    Error(Truncated)
//! ```
//!
//! Tool calls wait for `response.completed` so a response that fails later
//! runs none of them. A reasoning item's `encrypted_content` becomes the
//! `ThinkingEnd` signature behind
//! [`super::build::REASONING_SIGNATURE_PREFIX`], which is how the next
//! request continues the model's reasoning. Event types this machine does
//! not know are ignored. After the first `Error` it emits nothing more.

use serde_json::Value;

use crate::llm::stream::{OpenAiCompatUsageExtra, SpentUsage, StreamError, StreamErrorKind, StreamEvent, UsageExtra};

use super::build::REASONING_SIGNATURE_PREFIX;

/// Where ChatGPT plan usage and per-app limits are shown.
pub const PLAN_USAGE_URL: &str = "https://chatgpt.com/settings/usage";

/// Which content block is currently open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    None,
    Thinking,
    Text,
}

/// A completed `function_call` item, held until the response completes.
#[derive(Debug)]
struct ToolCall {
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Default)]
pub struct StateMachine {
    phase: Phase,
    /// The reasoning summary part the last thinking delta belonged to.
    summary_index: Option<u64>,
    tool_calls: Vec<ToolCall>,
    usage: Option<Value>,
    ended: bool,
    failed: bool,
}

impl StateMachine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the response ended in an error.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Whether a terminal event has been seen.
    pub fn ended(&self) -> bool {
        self.ended
    }

    /// Take one decoded SSE `data` payload.
    pub fn step(&mut self, event: Value) -> Vec<StreamEvent> {
        if self.ended {
            return Vec::new();
        }
        let mut out = self.dispatch(&event);
        if let Some(position) = out.iter().position(|e| matches!(e, StreamEvent::Error(_))) {
            out.truncate(position + 1);
            self.failed = true;
            self.ended = true;
            if let Some(StreamEvent::Error(error)) = out.last_mut()
                && error.usage.is_none()
            {
                error.usage = self.spent();
            }
        }
        out
    }

    /// The stream closed. A response that never reached a terminal event may
    /// have been cut anywhere: its open text is kept, its tool calls are not
    /// run.
    pub fn end_of_stream(&mut self) -> Vec<StreamEvent> {
        if self.ended {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.close_open(&mut out);
        self.tool_calls.clear();
        self.ended = true;
        self.failed = true;
        let error = StreamError::new(
            StreamErrorKind::Truncated("no_response_completed".into()),
            "the stream ended before response.completed",
        );
        out.push(StreamEvent::Error(match self.spent() {
            Some(spent) => error.with_usage(spent),
            None => error,
        }));
        out
    }

    /// What the call has spent, once a usage object has arrived.
    pub fn spent(&self) -> Option<SpentUsage> {
        let (input_tokens, output_tokens, extra) = self.usage_counts()?;
        Some(SpentUsage { input_tokens, output_tokens, extra })
    }

    fn usage_counts(&self) -> Option<(Option<u64>, Option<u64>, Option<UsageExtra>)> {
        let usage = self.usage.as_ref()?;
        let input = usage.get("input_tokens").and_then(Value::as_u64);
        let output = usage.get("output_tokens").and_then(Value::as_u64);
        let cached = usage.pointer("/input_tokens_details/cached_tokens").and_then(Value::as_u64).unwrap_or(0);
        let written = usage.pointer("/input_tokens_details/cache_write_tokens").and_then(Value::as_u64).unwrap_or(0);
        let reasoning = usage.pointer("/output_tokens_details/reasoning_tokens").and_then(Value::as_u64).unwrap_or(0);
        // `input_tokens` holds reads and writes; the miss is the rest.
        let extra = UsageExtra::OpenAiCompat(OpenAiCompatUsageExtra {
            prompt_cache_hit_tokens: cached,
            prompt_cache_miss_tokens: input.unwrap_or(0).saturating_sub(cached).saturating_sub(written),
            reasoning_tokens: reasoning,
            prompt_cache_write_tokens: written,
        });
        Some((input, output, Some(extra)))
    }

    fn dispatch(&mut self, event: &Value) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "response.output_item.added" => match item_type(event) {
                "reasoning" => self.open(Phase::Thinking, &mut out),
                "message" => self.open(Phase::Text, &mut out),
                _ => self.close_open(&mut out),
            },
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let index = event.get("summary_index").and_then(Value::as_u64);
                if self.phase != Phase::Thinking {
                    self.open(Phase::Thinking, &mut out);
                } else if index.is_some() && self.summary_index.is_some() && index != self.summary_index {
                    out.push(StreamEvent::ThinkingDelta("\n\n".into()));
                }
                if index.is_some() {
                    self.summary_index = index;
                }
                out.push(StreamEvent::ThinkingDelta(delta(event)));
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                if self.phase != Phase::Text {
                    self.open(Phase::Text, &mut out);
                }
                out.push(StreamEvent::TextDelta(delta(event)));
            }
            "response.output_item.done" => self.item_done(event, &mut out),
            "response.completed" => {
                self.usage = event.pointer("/response/usage").cloned();
                self.close_open(&mut out);
                let stop_reason = if self.tool_calls.is_empty() { "completed" } else { "tool_calls" };
                self.finish(stop_reason, &mut out);
            }
            "response.incomplete" => {
                self.usage = event.pointer("/response/usage").cloned();
                self.close_open(&mut out);
                let reason = event
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                match reason {
                    "max_output_tokens" => self.finish("max_tokens", &mut out),
                    "content_filter" => out.push(StreamEvent::Error(StreamError::new(
                        StreamErrorKind::ContentFilter,
                        "the provider's content filter ended the response",
                    ))),
                    other => out.push(StreamEvent::Error(StreamError::new(
                        StreamErrorKind::Truncated(other.to_string()),
                        format!("the response is incomplete: {other}"),
                    ))),
                }
            }
            "response.failed" => {
                self.usage = event.pointer("/response/usage").cloned();
                self.close_open(&mut out);
                let error = event.pointer("/response/error").cloned().unwrap_or(Value::Null);
                out.push(StreamEvent::Error(error_from(&error)));
            }
            "error" => {
                self.close_open(&mut out);
                let error = event.get("error").filter(|e| e.is_object()).unwrap_or(event);
                out.push(StreamEvent::Error(error_from(error)));
            }
            _ => {}
        }
        out
    }

    fn item_done(&mut self, event: &Value, out: &mut Vec<StreamEvent>) {
        let item = event.get("item").unwrap_or(&Value::Null);
        match item_type(event) {
            "reasoning" => {
                // A reasoning item with no summary still carries the content
                // the next request needs.
                if self.phase != Phase::Thinking {
                    self.open(Phase::Thinking, out);
                }
                let signature = item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(|enc| format!("{REASONING_SIGNATURE_PREFIX}{enc}"));
                out.push(StreamEvent::ThinkingEnd { signature });
                self.phase = Phase::None;
                self.summary_index = None;
            }
            "message" => self.close_open(out),
            "function_call" => {
                self.close_open(out);
                let field = |name| item.get(name).and_then(Value::as_str).unwrap_or("").to_string();
                self.tool_calls.push(ToolCall {
                    call_id: field("call_id"),
                    name: field("name"),
                    arguments: field("arguments"),
                });
            }
            _ => {}
        }
    }

    /// Emit the held tool calls and `Done`. A call without a name or call id
    /// fails the response instead.
    fn finish(&mut self, stop_reason: &str, out: &mut Vec<StreamEvent>) {
        let calls = std::mem::take(&mut self.tool_calls);
        if let Some(bad) = calls.iter().find(|c| c.name.is_empty() || c.call_id.is_empty()) {
            out.push(StreamEvent::Error(StreamError::new(
                StreamErrorKind::Protocol,
                format!("a function_call arrived without a name or call_id: {bad:?}"),
            )));
            return;
        }
        for call in calls {
            let input = if call.arguments.trim().is_empty() {
                Ok(Value::Object(serde_json::Map::new()))
            } else {
                serde_json::from_str(&call.arguments)
            };
            out.push(match input {
                Ok(input) => StreamEvent::ToolUse { id: call.call_id, name: call.name, input, signature: None },
                Err(e) => StreamEvent::ToolUseInvalid {
                    id: call.call_id,
                    name: call.name,
                    arguments: call.arguments,
                    error: e.to_string(),
                },
            });
        }
        let (input_tokens, output_tokens, extra) = self.usage_counts().unwrap_or((None, None, None));
        out.push(StreamEvent::Done { stop_reason: Some(stop_reason.to_string()), input_tokens, output_tokens, extra });
        self.ended = true;
    }

    fn open(&mut self, phase: Phase, out: &mut Vec<StreamEvent>) {
        self.close_open(out);
        out.push(match phase {
            Phase::Thinking => StreamEvent::ThinkingStart,
            Phase::Text => StreamEvent::TextStart,
            Phase::None => return,
        });
        self.phase = phase;
    }

    fn close_open(&mut self, out: &mut Vec<StreamEvent>) {
        match std::mem::take(&mut self.phase) {
            Phase::Thinking => out.push(StreamEvent::ThinkingEnd { signature: None }),
            Phase::Text => out.push(StreamEvent::TextEnd),
            Phase::None => {}
        }
        self.summary_index = None;
    }
}

fn item_type(event: &Value) -> &str {
    event.pointer("/item/type").and_then(Value::as_str).unwrap_or("")
}

fn delta(event: &Value) -> String {
    event.get("delta").and_then(Value::as_str).unwrap_or("").to_string()
}

/// The [`StreamErrorKind`] for a Responses error `code` (or `type`).
///
/// A spent ChatGPT plan is [`StreamErrorKind::Billing`] so it is not retried;
/// plan capacity that is briefly unavailable is
/// [`StreamErrorKind::Overloaded`] so it is.
pub fn error_kind(code: &str) -> StreamErrorKind {
    match code {
        "subscription_sharing_usage_limit_exceeded" | "insufficient_quota" | "billing_error" => StreamErrorKind::Billing,
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" | "server_is_overloaded"
        | "overloaded_error" => StreamErrorKind::Overloaded,
        "subscription_sharing_user_not_eligible"
        | "subscription_sharing_invalid_user"
        | "chatpass_v2_scope_not_authorized"
        | "chatpass_v2_invalid_authorization_context"
        | "invalid_api_key"
        | "authentication_error"
        | "permission_error" => StreamErrorKind::Auth,
        "subscription_sharing_unsupported_capability"
        | "subscription_sharing_route_not_supported"
        | "invalid_request_error"
        | "invalid_prompt"
        | "context_length_exceeded" => StreamErrorKind::InvalidRequest,
        "rate_limit_exceeded" | "rate_limit_error" => StreamErrorKind::RateLimited,
        "server_error" | "internal_error" => StreamErrorKind::ServerError,
        other => StreamErrorKind::Unknown(other.to_string()),
    }
}

/// A [`StreamError`] from a Responses error object `{code, message, param}`.
pub fn error_from(error: &Value) -> StreamError {
    let field = |name| error.get(name).and_then(Value::as_str);
    let code = field("code").or_else(|| field("type")).unwrap_or("error");
    let mut message = match field("message") {
        Some(message) => message.to_string(),
        None if error.is_null() => "the response failed without saying why".to_string(),
        None => error.to_string(),
    };
    if let Some(param) = field("param") {
        message = format!("{message} (param: {param})");
    }
    StreamError::new(error_kind(code), with_guidance(code, message))
}

/// `message` with the next step a player can take for a plan error.
pub fn with_guidance(code: &str, message: String) -> String {
    match code {
        "subscription_sharing_usage_limit_exceeded" => {
            format!("{message}; the ChatGPT plan's usage limit is reached, see {PLAN_USAGE_URL}")
        }
        "subscription_sharing_user_not_eligible" => {
            format!("{message}; plan usage needs a ChatGPT Plus or Pro subscription")
        }
        _ => message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(events: Vec<Value>) -> Vec<StreamEvent> {
        let mut machine = StateMachine::new();
        let mut out: Vec<StreamEvent> = events.into_iter().flat_map(|e| machine.step(e)).collect();
        out.extend(machine.end_of_stream());
        out
    }

    fn usage() -> Value {
        json!({
            "input_tokens": 100,
            "input_tokens_details": { "cached_tokens": 60 },
            "output_tokens": 20,
            "output_tokens_details": { "reasoning_tokens": 8 },
        })
    }

    fn completed() -> Value {
        json!({ "type": "response.completed", "response": { "status": "completed", "usage": usage() } })
    }

    /// Reasoning, then text, then a tool call: each item is bracketed, the
    /// reasoning keeps its encrypted content, the summary parts are joined,
    /// and the tool call follows the closed blocks with the usage on `Done`.
    ///
    /// Falsified by a missing bracket, a lost signature, or a tool call
    /// emitted before the response completed.
    #[test]
    fn a_full_response_maps_to_bracketed_events() {
        let events = vec![
            json!({ "type": "response.created", "response": {} }),
            json!({ "type": "response.output_item.added", "item": { "type": "reasoning", "id": "rs_1" } }),
            json!({ "type": "response.reasoning_summary_text.delta", "summary_index": 0, "delta": "plan" }),
            json!({ "type": "response.reasoning_summary_text.delta", "summary_index": 1, "delta": "more" }),
            json!({ "type": "response.output_item.done", "item": { "type": "reasoning", "id": "rs_1", "encrypted_content": "ENC" } }),
            json!({ "type": "response.output_item.added", "item": { "type": "message", "id": "msg_1" } }),
            json!({ "type": "response.output_text.delta", "delta": "Hello" }),
            json!({ "type": "response.output_item.done", "item": { "type": "message", "id": "msg_1" } }),
            json!({ "type": "response.output_item.added", "item": { "type": "function_call", "id": "fc_1" } }),
            json!({ "type": "response.function_call_arguments.delta", "delta": "{\"a\"" }),
            json!({ "type": "response.output_item.done", "item": { "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "ls", "arguments": "{\"a\":1}" } }),
            json!({ "type": "response.metadata" }),
            completed(),
        ];
        assert_eq!(
            run(events),
            vec![
                StreamEvent::ThinkingStart,
                StreamEvent::ThinkingDelta("plan".into()),
                StreamEvent::ThinkingDelta("\n\n".into()),
                StreamEvent::ThinkingDelta("more".into()),
                StreamEvent::ThinkingEnd { signature: Some(format!("{REASONING_SIGNATURE_PREFIX}ENC")) },
                StreamEvent::TextStart,
                StreamEvent::TextDelta("Hello".into()),
                StreamEvent::TextEnd,
                StreamEvent::ToolUse { id: "call_1".into(), name: "ls".into(), input: json!({ "a": 1 }), signature: None },
                StreamEvent::Done {
                    stop_reason: Some("tool_calls".into()),
                    input_tokens: Some(100),
                    output_tokens: Some(20),
                    extra: Some(UsageExtra::OpenAiCompat(OpenAiCompatUsageExtra {
                        prompt_cache_hit_tokens: 60,
                        prompt_cache_miss_tokens: 40,
                        reasoning_tokens: 8,
                        prompt_cache_write_tokens: 0,
                    })),
                },
            ]
        );
    }

    /// A cache write is counted, and the miss is what was neither read nor
    /// written: OpenAI's ordinary input is `input - cached - write`.
    ///
    /// Falsified by a write left at 0, or a miss that still holds the write.
    #[test]
    fn cache_writes_are_counted_apart_from_misses() {
        let usage = json!({
            "input_tokens": 1000,
            "input_tokens_details": { "cached_tokens": 600, "cache_write_tokens": 300 },
            "output_tokens": 5,
        });
        let out = run(vec![json!({ "type": "response.completed", "response": { "usage": usage } })]);
        let Some(StreamEvent::Done { extra: Some(UsageExtra::OpenAiCompat(extra)), .. }) = out.last() else { panic!("{out:?}") };
        assert_eq!(
            (extra.prompt_cache_hit_tokens, extra.prompt_cache_write_tokens, extra.prompt_cache_miss_tokens),
            (600, 300, 100)
        );
    }

    /// A reasoning item with no summary still yields a block carrying its
    /// encrypted content.
    #[test]
    fn a_silent_reasoning_item_keeps_its_signature() {
        let out = run(vec![
            json!({ "type": "response.output_item.done", "item": { "type": "reasoning", "encrypted_content": "E" } }),
            completed(),
        ]);
        assert_eq!(out[..2], [
            StreamEvent::ThinkingStart,
            StreamEvent::ThinkingEnd { signature: Some(format!("{REASONING_SIGNATURE_PREFIX}E")) },
        ]);
    }

    /// A spent plan fails the response as billing, so it is not retried, and
    /// the message says where to look. Tool calls before it do not run.
    #[test]
    fn a_spent_plan_is_a_billing_error_with_guidance() {
        let out = run(vec![
            json!({ "type": "response.output_item.done", "item": { "type": "function_call", "call_id": "c", "name": "n", "arguments": "{}" } }),
            json!({ "type": "response.failed", "response": { "error": { "code": "subscription_sharing_usage_limit_exceeded", "message": "limit" } } }),
        ]);
        let [StreamEvent::Error(error)] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(error.kind, StreamErrorKind::Billing);
        assert!(error.message.contains(PLAN_USAGE_URL), "{}", error.message);
    }

    /// A stream that closes before a terminal event is truncated and runs no
    /// tool calls; an open text block is closed first.
    #[test]
    fn end_of_stream_without_completion_is_truncated() {
        let out = run(vec![
            json!({ "type": "response.output_item.added", "item": { "type": "message" } }),
            json!({ "type": "response.output_text.delta", "delta": "par" }),
        ]);
        assert_eq!(out[..3], [StreamEvent::TextStart, StreamEvent::TextDelta("par".into()), StreamEvent::TextEnd]);
        assert!(matches!(&out[3], StreamEvent::Error(e) if e.kind == StreamErrorKind::Truncated("no_response_completed".into())), "{out:?}");
    }

    /// The output limit completes the response as `max_tokens`; a content
    /// filter fails it.
    #[test]
    fn incomplete_responses_map_by_reason() {
        let out = run(vec![json!({ "type": "response.incomplete", "response": { "incomplete_details": { "reason": "max_output_tokens" } } })]);
        assert!(matches!(&out[0], StreamEvent::Done { stop_reason: Some(r), .. } if r == "max_tokens"), "{out:?}");
        let out = run(vec![json!({ "type": "response.incomplete", "response": { "incomplete_details": { "reason": "content_filter" } } })]);
        assert!(matches!(&out[0], StreamEvent::Error(e) if e.kind == StreamErrorKind::ContentFilter), "{out:?}");
    }

    /// Arguments that do not parse become `ToolUseInvalid`.
    #[test]
    fn unparseable_arguments_are_an_invalid_tool_use() {
        let out = run(vec![
            json!({ "type": "response.output_item.done", "item": { "type": "function_call", "call_id": "c", "name": "n", "arguments": "{\"a\":" } }),
            completed(),
        ]);
        assert!(matches!(&out[0], StreamEvent::ToolUseInvalid { id, .. } if id == "c"), "{out:?}");
    }

    /// A failure with no error object still says something readable.
    #[test]
    fn a_bare_failure_has_a_message() {
        let out = run(vec![json!({ "type": "response.failed", "response": {} })]);
        assert!(matches!(&out[0], StreamEvent::Error(e) if e.message == "the response failed without saying why"), "{out:?}");
    }

    /// A top-level `error` event fails the stream with its code's kind.
    #[test]
    fn an_error_event_fails_the_stream() {
        let out = run(vec![json!({ "type": "error", "code": "subscription_sharing_usage_unavailable", "message": "busy" })]);
        assert!(matches!(&out[0], StreamEvent::Error(e) if e.kind == StreamErrorKind::Overloaded), "{out:?}");
        assert_eq!(out.len(), 1);
    }
}
