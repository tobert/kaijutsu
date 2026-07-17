//! Serde types for the kaijutsu hook protocol.
//!
//! These match the schema defined in `docs/hooks.md`. Pure data —
//! no runtime dependencies beyond serde.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Incoming hook event from an adapter script.
///
/// Every hook invocation sends one of these as a JSON line on stdin (or over
/// the Unix socket). The `event` field uses dotted names like `"tool.after"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEvent {
    /// Event type: `session.start`, `tool.after`, `prompt.submit`, etc.
    pub event: String,
    /// Source tool identifier. `claude-code` is the only adapter today.
    pub source: String,
    /// Opaque session ID from the source tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// ISO 8601 timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// LLM model in use (e.g., `claude-opus-4-6`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Path to source tool's transcript file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,

    // -- Event-specific fields --
    /// Tool info (tool.before, tool.after, tool.error).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolInfo>,
    /// File info (file.edit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileInfo>,
    /// User prompt text (prompt.submit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Agent response text (agent.stop).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    /// Reason for session end or compaction trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Subagent identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<String>,
    /// Subagent type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Compaction trigger (`manual` | `auto`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
}

/// Tool invocation details.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    /// Tool name (e.g., `Bash`, `Edit`, `Write`).
    pub name: String,
    /// Tool input parameters.
    #[serde(default)]
    pub input: Value,
    /// Tool output (tool.after).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Error message (tool.error).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Execution duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// File modification details.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    /// File path.
    pub path: String,
    /// Individual edits within the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edits: Option<Vec<FileEdit>>,
}

/// A single edit within a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEdit {
    /// Old content (before edit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    /// New content (after edit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
}

/// Response from kaijutsu-mcp hook processing.
///
/// Sent back to the adapter as JSON. The adapter maps `context` to the
/// source tool's native injection field (e.g., Claude's `additionalContext`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookResponse {
    /// Drift context to inject into the agent's next turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// `"allow"` or `"deny"` — maps to exit code 0 or 2.
    pub block: String,
    /// Explanation for deny decisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl HookResponse {
    /// Allow the action with no injected context.
    pub fn allow() -> Self {
        Self {
            context: None,
            block: "allow".to_string(),
            reason: None,
        }
    }

    /// Allow the action and inject drift context.
    pub fn allow_with_context(context: impl Into<String>) -> Self {
        Self {
            context: Some(context.into()),
            block: "allow".to_string(),
            reason: None,
        }
    }

    /// Deny the action with a reason.
    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            context: None,
            block: "deny".to_string(),
            reason: Some(reason.into()),
        }
    }

    /// Whether this response denies the action.
    pub fn is_deny(&self) -> bool {
        self.block == "deny"
    }
}

/// Ping response for socket discovery / health checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingResponse {
    pub status: String,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    /// Agent session ID (e.g., Claude Code session UUID).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub pending_drifts: u32,
}

/// Known MCP tool names that the hook listener should skip to avoid double-writes.
///
/// When Claude calls a kaijutsu MCP tool, the MCP server already creates the
/// appropriate CRDT blocks. If the PostToolUse hook also fires for that same
/// tool call, we'd create duplicate blocks. This list filters those out.
/// Compare against [`normalize_tool_name`]'s output, not the raw hook payload
/// name — Claude Code delivers MCP tool calls as `mcp__<server>__<tool>`.
pub const KAIJUTSU_MCP_TOOLS: &[&str] = &[
    "block_create",
    "block_append",
    "block_edit",
    "block_read",
    "block_status",
    "doc_create",
    "doc_delete",
    "doc_list",
    "doc_tree",
    "kaish_exec",
    "shell",
    "kernel_search",
    "list_kernel_tools",
    "whoami",
    "block_inspect",
    "block_diff",
    "block_history",
    "block_list",
    "read_input",
    "write_input",
    "edit_input",
    "submit_input",
    "register_session",
    "invoke_peer",
];

/// Strip a leading `mcp__<server>__` prefix from a hook-reported tool name.
///
/// Claude Code delivers MCP tool calls as `mcp__<server>__<tool>` (e.g.
/// `mcp__kaijutsu__shell`), but [`KAIJUTSU_MCP_TOOLS`] lists bare tool names.
/// Names that aren't `mcp__`-prefixed (kaish invocations, other adapters)
/// pass through unchanged.
pub fn normalize_tool_name(name: &str) -> &str {
    let Some(rest) = name.strip_prefix("mcp__") else {
        return name;
    };
    match rest.find("__") {
        Some(idx) => &rest[idx + 2..],
        None => name,
    }
}

/// First 8 chars (not bytes — session ids aren't guaranteed ASCII) of a
/// session id, for building short suffixes like `-{first 8 chars}` in
/// generated context labels.
pub fn short_session_suffix(session_id: &str) -> &str {
    match session_id.char_indices().nth(8) {
        Some((idx, _)) => &session_id[..idx],
        None => session_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_mcp_prefix() {
        assert_eq!(normalize_tool_name("mcp__kaijutsu__shell"), "shell");
        assert_eq!(
            normalize_tool_name("mcp__kaijutsu__register_session"),
            "register_session"
        );
    }

    #[test]
    fn normalize_strips_any_server_name() {
        // The prefix is `mcp__<anything>__` — not tied to "kaijutsu".
        assert_eq!(normalize_tool_name("mcp__other-server__block_create"), "block_create");
    }

    #[test]
    fn normalize_passes_through_bare_name() {
        assert_eq!(normalize_tool_name("Bash"), "Bash");
        assert_eq!(normalize_tool_name("Edit"), "Edit");
    }

    #[test]
    fn normalize_handles_malformed_prefix_without_panicking() {
        // Starts with `mcp__` but has no second `__` delimiter — not a real
        // MCP-qualified name, so pass it through unchanged rather than panic
        // or silently truncate.
        assert_eq!(normalize_tool_name("mcp__onlyoneprefix"), "mcp__onlyoneprefix");
    }

    #[test]
    fn kaijutsu_mcp_tools_includes_register_session_and_invoke_peer() {
        assert!(KAIJUTSU_MCP_TOOLS.contains(&"register_session"));
        assert!(KAIJUTSU_MCP_TOOLS.contains(&"invoke_peer"));
    }

    #[test]
    fn short_session_suffix_truncates_to_8_chars() {
        assert_eq!(
            short_session_suffix("a1b2c3d4-e5f6-7890-abcd-ef1234567890"),
            "a1b2c3d4"
        );
    }

    #[test]
    fn short_session_suffix_passes_through_shorter_ids() {
        assert_eq!(short_session_suffix("abc"), "abc");
    }

    #[test]
    fn short_session_suffix_is_char_safe_not_byte_safe() {
        // Multi-byte chars near the boundary must not panic or split a
        // codepoint — this session id has fewer than 8 *chars* even though
        // it has more than 8 *bytes*.
        let sid = "日本語テスト123456";
        let suffix = short_session_suffix(sid);
        assert_eq!(suffix.chars().count(), 8);
    }
}
