//! Context management engine for kaish shell.
//!
//! Provides the `context` command for creating and switching contexts from
//! the shell interface. Delegates all context state to the kernel's DriftRouter
//! (the single source of truth for context labels and metadata).
//!
//! # Usage
//!
//! ```kaish
//! # Switch to an existing context
//! context switch default
//!
//! # List contexts
//! context list
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use kaijutsu_types::ContextId;
use tokio::sync::RwLock;

use kaijutsu_kernel::drift::SharedDriftRouter;
use kaijutsu_kernel::tools::{ExecResult, ExecutionEngine};

// ============================================================================
// Shell Context State - per-connection "current context" tracking
// ============================================================================

/// Per-connection state tracking which context the shell is currently in.
///
/// All context listing, label resolution, and metadata live in the kernel's
/// DriftRouter. This struct only tracks which context is "active" for this
/// particular shell session.
pub type CurrentContext = Arc<RwLock<Option<ContextId>>>;

/// Create a new current-context tracker.
pub fn current_context() -> CurrentContext {
    Arc::new(RwLock::new(None))
}

// ============================================================================
// Context Engine - ExecutionEngine implementation
// ============================================================================

/// Execution engine for the `context` shell command.
///
/// Provides commands:
/// - `context switch <name>` - Switch to a context
/// - `context list` - List all contexts
/// - `context current` - Show current context
pub struct ContextEngine {
    drift: SharedDriftRouter,
    current: CurrentContext,
}

impl ContextEngine {
    /// Create a new context engine.
    pub fn new(drift: SharedDriftRouter, current: CurrentContext) -> Self {
        Self { drift, current }
    }

    async fn execute_inner(&self, args: Vec<String>) -> Result<String, String> {
        if args.is_empty() {
            return self.show_help();
        }

        match args[0].as_str() {
            "switch" | "join" => {
                if args.len() < 2 {
                    return Err("Usage: context switch <name>".to_string());
                }
                let query = &args[1];
                let router = self.drift.read().await;
                match router.resolve_context(query) {
                    Ok(ctx_id) => {
                        let label = router.get(ctx_id)
                            .and_then(|h| h.label.as_deref())
                            .unwrap_or("(unlabeled)");
                        let label_owned = label.to_string();
                        drop(router);
                        *self.current.write().await = Some(ctx_id);
                        Ok(format!("Switched to context '{}'", label_owned))
                    }
                    Err(e) => Err(format!("Failed to resolve context '{}': {}", query, e)),
                }
            }
            "list" | "ls" => {
                let router = self.drift.read().await;
                let current_id = *self.current.read().await;
                let contexts = router.list_contexts();

                let mut output = String::new();
                for handle in contexts {
                    let label = handle.label.as_deref().unwrap_or("(unlabeled)");
                    let short = handle.id.short();
                    let is_current = current_id == Some(handle.id);
                    if is_current {
                        output.push_str(&format!("* {} [{}] (current)\n", label, short));
                    } else {
                        output.push_str(&format!("  {} [{}]\n", label, short));
                    }
                }
                if output.is_empty() {
                    output.push_str("No contexts registered.\n");
                }
                Ok(output)
            }
            "current" | "show" => {
                let current_id = *self.current.read().await;
                match current_id {
                    Some(id) => {
                        let router = self.drift.read().await;
                        let label = router.get(id)
                            .and_then(|h| h.label.as_deref())
                            .unwrap_or("(unlabeled)");
                        Ok(format!("Current context: {} [{}]", label, id.short()))
                    }
                    None => Ok("No active context".to_string()),
                }
            }
            "leave" => {
                let old = self.current.write().await.take();
                match old {
                    Some(id) => {
                        let router = self.drift.read().await;
                        let label = router.get(id)
                            .and_then(|h| h.label.as_deref())
                            .unwrap_or("(unlabeled)");
                        Ok(format!("Left context '{}' [{}]", label, id.short()))
                    }
                    None => Ok("No active context to leave".to_string()),
                }
            }
            "help" | "-h" | "--help" => self.show_help(),
            other => Err(format!(
                "Unknown subcommand: {}. Use 'context help' for usage.",
                other
            )),
        }
    }

    fn show_help(&self) -> Result<String, String> {
        Ok(r#"context - Manage conversation contexts

USAGE:
    context <command> [args]

COMMANDS:
    switch <name>   Switch to a context (by label or ID prefix)
    list            List all contexts
    current         Show current context
    leave           Leave current context
    help            Show this help

EXAMPLES:
    context switch planning
    context switch def        # prefix match
    context list
"#
        .to_string())
    }
}

#[async_trait]
impl ExecutionEngine for ContextEngine {
    fn name(&self) -> &str {
        "context"
    }

    fn description(&self) -> &str {
        "Manage conversation contexts (switch, list, current, leave)"
    }

    fn schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "_positional": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Subcommand and arguments: switch|list|current|leave [name]"
                }
            },
            "required": []
        }))
    }

    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        // Parse the JSON params
        let parsed: serde_json::Value = match serde_json::from_str(params) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ExecResult::failure(1, format!("Invalid parameters: {}", e)));
            }
        };

        // Extract positional arguments
        let args: Vec<String> = parsed
            .get("_positional")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        match self.execute_inner(args).await {
            Ok(output) => Ok(ExecResult::success(output)),
            Err(e) => Ok(ExecResult::failure(1, e)),
        }
    }

    async fn is_available(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_kernel::drift::shared_drift_router;

    #[tokio::test]
    async fn test_context_engine_list() {
        let drift = shared_drift_router();
        let ctx_id = ContextId::new();
        drift.write().await.register(ctx_id, Some("default"), None);

        let current = current_context();
        let engine = ContextEngine::new(drift, current);

        let result = engine
            .execute(r#"{"_positional": ["list"]}"#)
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.stdout.contains("default"));
    }

    #[tokio::test]
    async fn test_context_engine_switch() {
        let drift = shared_drift_router();
        let ctx_id = ContextId::new();
        drift.write().await.register(ctx_id, Some("planning"), None);

        let current = current_context();
        let engine = ContextEngine::new(drift, current.clone());

        let result = engine
            .execute(r#"{"_positional": ["switch", "planning"]}"#)
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.stdout.contains("planning"));
        assert_eq!(*current.read().await, Some(ctx_id));
    }

    #[tokio::test]
    async fn test_context_engine_help() {
        let drift = shared_drift_router();
        let current = current_context();
        let engine = ContextEngine::new(drift, current);

        let result = engine
            .execute(r#"{"_positional": ["help"]}"#)
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.stdout.contains("USAGE"));
    }
}
