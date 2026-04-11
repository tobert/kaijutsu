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

use dashmap::DashMap;
use std::sync::Arc;

use async_trait::async_trait;
use kaijutsu_types::{ContextId, SessionId};

use kaijutsu_kernel::drift::SharedDriftRouter;
use kaijutsu_kernel::tools::{ExecResult, ExecutionEngine};

// ============================================================================
// Shell Context State - per-session "current context" tracking
// ============================================================================

/// Per-session current-context map. Each SSH session gets independent state
/// so that one session switching context doesn't affect others.
///
/// Uses DashMap for synchronous, concurrent access.
pub type SessionContextMap = Arc<DashMap<SessionId, ContextId>>;

/// Extension trait for SessionContextMap to provide convenient accessors.
pub trait SessionContextExt {
    /// Get the current context for a session.
    fn current(&self, session_id: &SessionId) -> Option<ContextId>;
}

impl SessionContextExt for SessionContextMap {
    fn current(&self, session_id: &SessionId) -> Option<ContextId> {
        self.get(session_id).map(|r| *r)
    }
}

/// Create a new session-context map.
pub fn session_context_map() -> SessionContextMap {
    Arc::new(DashMap::new())
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
    sessions: SessionContextMap,
}

impl ContextEngine {
    /// Create a new context engine with per-session state tracking.
    pub fn new(drift: SharedDriftRouter, sessions: SessionContextMap) -> Self {
        Self { drift, sessions }
    }

    async fn execute_inner(
        &self,
        args: Vec<String>,
        session_id: SessionId,
    ) -> Result<String, String> {
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
                        let label = router
                            .get(ctx_id)
                            .and_then(|h| h.label.as_deref())
                            .unwrap_or("(unlabeled)");
                        let label_owned = label.to_string();
                        drop(router);
                        self.sessions.insert(session_id, ctx_id);
                        Ok(format!("Switched to context '{}'", label_owned))
                    }
                    Err(e) => Err(format!("Failed to resolve context '{}': {}", query, e)),
                }
            }
            "list" | "ls" => {
                let router = self.drift.read().await;
                let current_id = self.sessions.get(&session_id).map(|r| *r);
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
                let current_id = self.sessions.get(&session_id).map(|r| *r);
                match current_id {
                    Some(id) => {
                        let router = self.drift.read().await;
                        let label = router
                            .get(id)
                            .and_then(|h| h.label.as_deref())
                            .unwrap_or("(unlabeled)");
                        Ok(format!("Current context: {} [{}]", label, id.short()))
                    }
                    None => Ok("No active context".to_string()),
                }
            }
            "leave" => {
                let old = self.sessions.remove(&session_id).map(|(_, v)| v);
                match old {
                    Some(id) => {
                        let router = self.drift.read().await;
                        let label = router
                            .get(id)
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

    async fn execute(
        &self,
        params: &str,
        ctx: &kaijutsu_kernel::ToolContext,
    ) -> anyhow::Result<ExecResult> {
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

        match self.execute_inner(args, ctx.session_id).await {
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
        drift.write().await.register(
            ctx_id,
            Some("default"),
            None,
            kaijutsu_types::PrincipalId::system(),
        ).unwrap();

        let sessions = session_context_map();
        let engine = ContextEngine::new(drift, sessions);

        let result = engine
            .execute(
                r#"{"_positional": ["list"]}"#,
                &kaijutsu_kernel::ToolContext::test(),
            )
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.stdout.contains("default"));
    }

    #[tokio::test]
    async fn test_context_engine_switch() {
        let drift = shared_drift_router();
        let ctx_id = ContextId::new();
        drift.write().await.register(
            ctx_id,
            Some("planning"),
            None,
            kaijutsu_types::PrincipalId::system(),
        ).unwrap();

        let sessions = session_context_map();
        let engine = ContextEngine::new(drift, sessions.clone());

        let tool_ctx = kaijutsu_kernel::ToolContext::test();
        let result = engine
            .execute(
                r#"{"_positional": ["switch", "planning"]}"#,
                &tool_ctx,
            )
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.stdout.contains("planning"));
        assert_eq!(
            sessions.get(&tool_ctx.session_id).map(|r| *r),
            Some(ctx_id),
        );
    }

    #[tokio::test]
    async fn test_context_engine_help() {
        let drift = shared_drift_router();
        let sessions = session_context_map();
        let engine = ContextEngine::new(drift, sessions);

        let result = engine
            .execute(
                r#"{"_positional": ["help"]}"#,
                &kaijutsu_kernel::ToolContext::test(),
            )
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.stdout.contains("USAGE"));
    }

    /// Regression: sessions must have independent current-context tracking.
    /// Previously a single Arc was shared across all sessions, so one
    /// session's switch contaminated every other session.
    #[tokio::test]
    async fn test_sessions_have_independent_current_context() {
        let drift = shared_drift_router();
        let id_alpha = ContextId::new();
        let id_beta = ContextId::new();

        {
            let mut d = drift.write().await;
            d.register(
                id_alpha,
                Some("alpha"),
                None,
                kaijutsu_types::PrincipalId::new(),
            ).unwrap();
            d.register(
                id_beta,
                Some("beta"),
                None,
                kaijutsu_types::PrincipalId::new(),
            ).unwrap();
        }

        // Both engines share the same SessionContextMap (as in production),
        // but each session uses a different SessionId via ToolContext.
        let sessions = session_context_map();
        let engine = ContextEngine::new(drift.clone(), sessions.clone());

        let session_a = SessionId::new();
        let ctx_a = kaijutsu_kernel::ToolContext {
            session_id: session_a,
            ..kaijutsu_kernel::ToolContext::test()
        };
        let session_b = SessionId::new();
        let ctx_b = kaijutsu_kernel::ToolContext {
            session_id: session_b,
            ..kaijutsu_kernel::ToolContext::test()
        };

        // Session A switches to alpha
        let result = engine
            .execute(
                r#"{"_positional": ["switch", "alpha"]}"#,
                &ctx_a,
            )
            .await
            .unwrap();
        assert!(result.success, "Session A switch failed: {}", result.stderr);

        // Session B switches to beta
        let result = engine
            .execute(
                r#"{"_positional": ["switch", "beta"]}"#,
                &ctx_b,
            )
            .await
            .unwrap();
        assert!(result.success, "Session B switch failed: {}", result.stderr);

        // Session A's current context should still be alpha.
        assert_eq!(
            sessions.get(&session_a).map(|r| *r),
            Some(id_alpha),
            "Session A's context should still be alpha after Session B switched",
        );
        assert_eq!(
            sessions.get(&session_b).map(|r| *r),
            Some(id_beta),
            "Session B's context should be beta",
        );
    }
}
