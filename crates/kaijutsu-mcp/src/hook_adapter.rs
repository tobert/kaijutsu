//! Native Claude Code, Codex, and Antigravity hook protocol adapters.
//!
//! Source hooks carry the hosting conversation in `session_id` (or
//! `conversationId` for Antigravity). Keep that separate from
//! `agent_id`/`subagent_id`, which identify a principal inside the hosting
//! session and become [`HookEvent::principal_id`].

use serde_json::{Map, Value};

use crate::hook_types::{FileInfo, HookEvent, HookResponse, ToolInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookSource {
    Claude,
    Codex,
    Antigravity,
}

impl HookSource {
    pub fn adapt(self, native: Value) -> Option<HookEvent> {
        let object = native.as_object()?;
        if self == Self::Antigravity {
            return adapt_antigravity(object);
        }
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
            source: match self {
                Self::Claude => "claude-code",
                Self::Codex => "codex",
                Self::Antigravity => "antigravity",
            }
            .to_string(),
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
                Self::Antigravity => None,
            },
            agent_type: match self {
                Self::Claude => owned_string(object, "agent_type"),
                Self::Codex => owned_string(object, "agent_type")
                    .or_else(|| owned_string(object, "subagent_type")),
                Self::Antigravity => None,
            },
            trigger: if self == Self::Codex { owned_string(object, "trigger") } else { None },
        })
    }

    /// The native stdout for `response`, or `None` to print nothing.
    ///
    /// Context goes only to events that feed it to the model beside work
    /// already in flight. On `Stop` and `SubagentStop` the host treats
    /// `additionalContext` as feedback and keeps the turn going, so a mirror
    /// error there starts another turn whose own end fires the hook again.
    pub fn response(self, native_event: &str, response: &HookResponse) -> Option<String> {
        if self == Self::Antigravity {
            if !matches!(
                native_event,
                "PreToolUse" | "PostToolUse" | "PreInvocation" | "SessionStart"
            ) {
                return None;
            }
            return response.context.as_ref().and_then(|context| {
                serde_json::to_string(&serde_json::json!({
                    "additionalContext": context,
                }))
                .ok()
            });
        }
        if !matches!(
            native_event,
            "PreToolUse" | "PostToolUse" | "PostToolUseFailure" | "UserPromptSubmit" | "SessionStart"
        ) {
            return None;
        }
        response.context.as_ref().and_then(|context| serde_json::to_string(&serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": native_event,
                "additionalContext": context,
            }
        })).ok())
    }
}

fn adapt_antigravity(object: &Map<String, Value>) -> Option<HookEvent> {
    let raw_event = string(object, "hookEventName")
        .or_else(|| string(object, "hook_event_name"))
        .or_else(|| string(object, "event"))?;
    let event = match raw_event {
        "SessionStart" => "session.start",
        "SessionEnd" => "session.end",
        "PreToolUse" => "tool.before",
        "PostToolUse" => "tool.after",
        "PostToolUseFailure" => "tool.error",
        "PreInvocation" => "prompt.submit",
        "PostInvocation" | "Stop" => "agent.stop",
        _ => return None,
    };

    let cwd = object
        .get("workspacePaths")
        .and_then(Value::as_array)
        .and_then(|paths| paths.first())
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| owned_string(object, "cwd"));

    let tool = string(object, "toolName")
        .or_else(|| string(object, "tool_name"))
        .map(|name| ToolInfo {
            name: name.to_string(),
            input: object
                .get("toolInput")
                .or_else(|| object.get("tool_input"))
                .cloned()
                .unwrap_or(Value::Null),
            output: value_as_text(
                object
                    .get("toolOutput")
                    .or_else(|| object.get("toolResponse"))
                    .or_else(|| object.get("tool_output"))
                    .or_else(|| object.get("tool_response")),
            ),
            error: owned_string(object, "error"),
            duration_ms: object
                .get("durationMs")
                .or_else(|| object.get("duration_ms"))
                .and_then(Value::as_u64),
        });

    Some(HookEvent {
        event: event.to_string(),
        source: "antigravity".to_string(),
        session_id: owned_string(object, "conversationId")
            .or_else(|| owned_string(object, "session_id")),
        timestamp: owned_string(object, "timestamp"),
        cwd,
        model: owned_string(object, "model"),
        transcript_path: owned_string(object, "transcriptPath")
            .or_else(|| owned_string(object, "transcript_path")),
        tool,
        file: None,
        prompt: owned_string(object, "prompt"),
        response: owned_string(object, "response"),
        reason: owned_string(object, "reason"),
        principal_id: owned_string(object, "agentId").or_else(|| owned_string(object, "agent_id")),
        agent_type: owned_string(object, "agentType").or_else(|| owned_string(object, "agent_type")),
        trigger: owned_string(object, "trigger"),
    })
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
