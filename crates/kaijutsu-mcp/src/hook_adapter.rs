//! Native Claude Code and Codex hook protocol adapters.
//!
//! Source hooks carry the hosting conversation in `session_id`.  Keep that
//! separate from `agent_id`/`subagent_id`, which identify a principal inside
//! the hosting session and become [`HookEvent::principal_id`].

use serde_json::{Map, Value};

use crate::hook_types::{FileInfo, HookEvent, HookResponse, ToolInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookSource {
    Claude,
    Codex,
}

impl HookSource {
    pub fn adapt(self, native: Value) -> Option<HookEvent> {
        let object = native.as_object()?;
        let event = map_event(self, string(object, "hook_event_name")?)?;
        let tool = string(object, "tool_name").map(|name| ToolInfo {
            name: name.to_string(),
            input: object.get("tool_input").cloned().unwrap_or(Value::Null),
            output: value_as_text(object.get("tool_response").or_else(|| object.get("tool_output"))),
            error: owned_string(object, "error"),
            duration_ms: object.get("duration_ms").and_then(Value::as_u64),
        });

        Some(HookEvent {
            event: event.to_string(),
            source: match self { Self::Claude => "claude-code", Self::Codex => "codex" }.to_string(),
            session_id: owned_string(object, "session_id"),
            timestamp: if self == Self::Codex { owned_string(object, "timestamp") } else { None },
            cwd: owned_string(object, "cwd"),
            model: owned_string(object, "model"),
            transcript_path: owned_string(object, "transcript_path"),
            tool,
            file: if self == Self::Codex {
                object.get("file").and_then(|v| serde_json::from_value::<FileInfo>(v.clone()).ok())
            } else { None },
            prompt: owned_string(object, "prompt"),
            response: if self == Self::Codex {
                owned_string(object, "last_assistant_message").or_else(|| owned_string(object, "response"))
            } else { owned_string(object, "response") },
            reason: owned_string(object, "reason"),
            principal_id: match self {
                Self::Claude => owned_string(object, "agent_id"),
                Self::Codex => owned_string(object, "agent_id")
                    .or_else(|| owned_string(object, "subagent_id"))
                    .or_else(|| owned_string(object, "principal_id")),
            },
            agent_type: match self {
                Self::Claude => owned_string(object, "agent_type"),
                Self::Codex => owned_string(object, "agent_type")
                    .or_else(|| owned_string(object, "subagent_type")),
            },
            trigger: if self == Self::Codex { owned_string(object, "trigger") } else { None },
        })
    }

    pub fn response(self, native_event: &str, response: &HookResponse) -> Option<String> {
        response.context.as_ref().and_then(|context| serde_json::to_string(&serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": native_event,
                "additionalContext": context,
            }
        })).ok())
    }
}

fn map_event(source: HookSource, event: &str) -> Option<&'static str> {
    Some(match event {
        "PreToolUse" if source == HookSource::Claude => "tool.before",
        "PostToolUse" => "tool.after",
        "PostToolUseFailure" if source == HookSource::Claude => "tool.error",
        "UserPromptSubmit" => "prompt.submit",
        "Stop" => "agent.stop",
        "SessionStart" => "session.start",
        "SessionEnd" => "session.end",
        "SubagentStart" => "subagent.start",
        "SubagentStop" => "subagent.stop",
        "PreCompact" | "PostCompact" => "agent.compact",
        _ => return None,
    })
}

fn string<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

fn owned_string(object: &Map<String, Value>, key: &str) -> Option<String> {
    string(object, key).map(String::from)
}

fn value_as_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => serde_json::to_string(other).ok(),
    }
}
