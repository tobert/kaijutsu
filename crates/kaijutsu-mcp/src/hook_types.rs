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
    /// Source tool identifier: `claude-code`, `gemini-cli`, `cursor`, etc.
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
    pub agent_id: Option<String>,
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
    pub pending_drifts: u32,
}

/// Known MCP tool names that the hook listener should skip to avoid double-writes.
///
/// When Claude calls a kaijutsu MCP tool, the MCP server already creates the
/// appropriate CRDT blocks. If the PostToolUse hook also fires for that same
/// tool call, we'd create duplicate blocks. This list filters those out.
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
    "doc_undo",
    "drift_push",
    "drift_pull",
    "drift_flush",
    "drift_merge",
    "drift_queue",
    "drift_cancel",
    "drift_ls",
    "kaish_exec",
    "shell",
    "kernel_search",
    "whoami",
    "block_inspect",
    "block_diff",
    "block_history",
    "block_list",
];
