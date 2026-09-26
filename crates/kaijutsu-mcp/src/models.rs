//! MCP request and response types.
//!
//! Shell commands run in the current kernel context. Session and peer tools
//! manage connections and exchanges between players.

use rmcp::schemars;
use serde::Deserialize;

/// Execute a kaish command in the caller's kernel context. The shell is
/// context-bound — `.` references the current context in kj commands, durable
/// cwd/env carry across calls, and `kj` builtins are available for
/// context/drift/fork management. Output is written to kernel blocks and
/// observable in kaijutsu-app.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShellRequest {
    /// The kaish command to execute, run in your current kernel context
    /// (e.g., "cargo check", "git status", "kj context list --tree",
    /// "kj fork --name alt"). Standard kaish works: pipes, variables, scripting.
    #[schemars(
        description = "kaish command to execute in the current kernel context (e.g., 'cargo check', 'kj context list --tree')"
    )]
    pub command: String,
    /// Return immediately with an operation receipt instead of waiting for
    /// completion. Defaults to false. Pass true for long-running work, then
    /// read or wait on the receipt.
    #[serde(default)]
    pub run_in_background: bool,
    /// Wait timeout in seconds, used when not running in the background
    /// (default: 300, max: 600). Reaching the timeout leaves the operation
    /// running.
    #[schemars(description = "Wait timeout in seconds, used when not running in the background (default: 300, max: 600); does not cancel the operation")]
    pub timeout_secs: Option<u64>,
}

#[cfg(test)]
mod shell_request_tests {
    use super::ShellRequest;

    #[test]
    fn shell_schema_defaults_to_run_in_background_false_and_rejects_the_retired_foreground_flag() {
        let schema = serde_json::to_value(schemars::schema_for!(ShellRequest)).unwrap();
        assert_eq!(schema["properties"]["run_in_background"]["default"], false);
        assert!(serde_json::from_value::<ShellRequest>(serde_json::json!({
            "command": "echo hello", "foreground": false,
        })).is_err());
    }

    /// Omitting `run_in_background` must deserialize to `false` — the wire
    /// caller waits for completion unless it opts into a receipt.
    #[test]
    fn shell_request_omitted_run_in_background_defaults_to_false() {
        let parsed: ShellRequest =
            serde_json::from_value(serde_json::json!({"command": "echo hello"})).unwrap();
        assert!(!parsed.run_in_background, "omitting run_in_background must wait for completion by default");
    }
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
    /// Context mode bundle (context_type). Selects which rc lifecycle scripts
    /// and tool policy the new context is born with. Defaults to "mcp".
    #[schemars(
        description = "Context mode (context_type) — selects rc lifecycle + tool policy. Defaults to \"mcp\""
    )]
    pub context_type: Option<String>,
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
