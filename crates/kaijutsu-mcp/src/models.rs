//! MCP request and response types.
//!
//! Slim surface after the MCP cleanup (see docs/kj-cleanup.md):
//! - shell / context_shell for command execution
//! - kaish_exec as the escape hatch into kernel tools
//! - {read,write,edit,submit}_input for the shared input scratchpad
//! - register_session, invoke_peer for peer/session concerns
//!
//! The block_*, doc_*, kernel_search, and stage_commit request types
//! were removed when their corresponding tools moved to `kj`.

use rmcp::schemars;
use serde::Deserialize;

// ============================================================================
// Kaish Execution Types
// ============================================================================

/// Execute a tool through the kernel's tool registry (git, drift, etc.).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KaishExecRequest {
    /// Exact tool name to execute (use list_kernel_tools to discover names)
    #[schemars(
        description = "Exact tool name (use list_kernel_tools to discover available names, e.g., 'drift_ls', 'glob', 'grep')"
    )]
    pub tool: String,
    /// JSON parameters for the tool
    #[schemars(description = "JSON parameters for the tool (tool-specific)")]
    #[serde(default = "default_empty_params")]
    pub params: String,
}

fn default_empty_params() -> String {
    "{}".to_string()
}

/// Execute a kaish command through the kernel's embedded shell.
/// Output is written to CRDT blocks and observable in kaijutsu-app.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ShellRequest {
    /// The kaish command to execute (e.g., "cargo check", "git status", "ls -la")
    #[schemars(description = "The kaish command to execute (e.g., 'cargo check', 'git status')")]
    pub command: String,
    /// Timeout in seconds (default: 300)
    #[schemars(description = "Timeout in seconds (default: 300, max: 600)")]
    pub timeout_secs: Option<u64>,
}

/// Context-bound kaish shell. Commands execute in the caller's kernel context.
/// kj builtins available for context/drift/fork management.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ContextShellRequest {
    /// kaish command to execute. The shell is bound to your current context.
    /// kj commands are available (e.g., "kj context list --tree",
    /// "kj fork --name alt", "kj drift push impl 'found it'").
    /// Standard kaish also works (pipes, variables, scripting).
    #[schemars(description = "kaish command to execute in the current kernel context")]
    pub command: String,
    /// Timeout in seconds (default: 300, max: 600)
    #[schemars(description = "Timeout in seconds (default: 300, max: 600)")]
    pub timeout_secs: Option<u64>,
}

// ============================================================================
// Input Document Types
// ============================================================================

/// Read the current input document text for a context.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InputReadRequest {
    /// Context ID (hex or label). Omit to use the current context.
    #[schemars(description = "Context ID (hex UUID or label). Omit to use the current context.")]
    pub context_id: Option<String>,
}

/// Replace the entire input document text.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InputWriteRequest {
    /// Context ID (hex or label). Omit to use the current context.
    #[schemars(description = "Context ID (hex UUID or label). Omit to use the current context.")]
    pub context_id: Option<String>,
    /// The text to write (replaces all existing content).
    #[schemars(description = "The text to write (replaces all existing content)")]
    pub text: String,
}

/// Surgical edit on the input document: insert and/or delete at a position.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InputEditRequest {
    /// Context ID (hex or label). Omit to use the current context.
    #[schemars(description = "Context ID (hex UUID or label). Omit to use the current context.")]
    pub context_id: Option<String>,
    /// Character position to start the edit (0-indexed).
    #[schemars(description = "Character position to start the edit (0-indexed)")]
    pub pos: u64,
    /// Text to insert at the position (empty string for delete-only).
    #[schemars(description = "Text to insert at the position (empty string for delete-only)")]
    #[serde(default)]
    pub insert: String,
    /// Number of characters to delete starting at the position.
    #[schemars(description = "Number of characters to delete starting at the position")]
    #[serde(default)]
    pub delete: u64,
}

/// Submit the input document: snapshot to a conversation block and clear.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InputSubmitRequest {
    /// Context ID (hex or label). Omit to use the current context.
    #[schemars(description = "Context ID (hex UUID or label). Omit to use the current context.")]
    pub context_id: Option<String>,
    /// Input mode: "chat" (default) or "shell".
    #[serde(default)]
    #[schemars(description = "Input mode: 'chat' (default) or 'shell'.")]
    pub mode: Option<String>,
}

// ============================================================================
// Session Registration
// ============================================================================

/// Register this agent session and create a context.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RegisterSessionRequest {
    /// Human-readable label for the context (auto-generated if omitted).
    #[schemars(description = "Human-readable label for the context (auto-generated if omitted)")]
    pub label: Option<String>,
}

// ============================================================================
// Peer Coordination
// ============================================================================

/// Invoke a peer through the kernel.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InvokePeerRequest {
    /// Peer nick to invoke (e.g., "kaijutsu-app")
    #[schemars(description = "Peer nick to invoke (e.g., \"kaijutsu-app\")")]
    pub nick: String,
    /// Action to perform (e.g., "switch_context", "active_context")
    #[schemars(description = "Action to perform (e.g., \"switch_context\", \"active_context\")")]
    pub action: String,
    /// JSON parameters for the action
    #[schemars(description = "JSON parameters for the action (e.g., {\"context_id\": \"019d1631\"})")]
    #[serde(default)]
    pub params: serde_json::Value,
}
