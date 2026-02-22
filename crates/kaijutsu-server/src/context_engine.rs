//! Context management engine for kaish shell.
//!
//! Provides the `context` command for creating and switching contexts from
//! the shell interface. This bridges the kaish tool dispatch with the server's
//! context management.
//!
//! # Usage
//!
//! ```kaish
//! # Create or join a context
//! context new planning
//!
//! # Switch to an existing context
//! context switch default
//!
//! # List contexts
//! context list
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use kaijutsu_types::{ContextId, KernelId};
use parking_lot::RwLock;

use kaijutsu_kernel::tools::{ExecResult, ExecutionEngine};

// ============================================================================
// Context Manager - Thread-safe state for context operations
// ============================================================================

/// Thread-safe context state shared between RPC handlers and ContextEngine.
///
/// Tracks which contexts exist and which one is currently active for this
/// connection. Lightweight context membership tracking.
#[derive(Debug)]
pub struct ContextManager {
    inner: RwLock<ContextManagerInner>,
}

#[derive(Debug, Default)]
#[allow(dead_code)] // nick/kernel_id/instance used by constructor, useful for future presence
struct ContextManagerInner {
    /// All known context labels
    context_labels: HashMap<ContextId, String>,
    /// Current user identity (username)
    nick: String,
    /// Kernel ID this manager belongs to
    kernel_id: KernelId,
    /// Current instance ID for this connection
    instance: String,
    /// Currently active context
    current_context: Option<ContextId>,
}

impl ContextManager {
    /// Create a new context manager.
    pub fn new(nick: String, kernel_id: KernelId, instance: String) -> Self {
        Self {
            inner: RwLock::new(ContextManagerInner {
                context_labels: HashMap::new(),
                nick,
                kernel_id,
                instance,
                current_context: None,
            }),
        }
    }

    /// Register a context label.
    pub fn register_context(&self, id: ContextId, label: &str) {
        let mut inner = self.inner.write();
        inner.context_labels.insert(id, label.to_string());
    }

    /// Join a context by label, returning the label.
    pub fn join_context(&self, label: &str) -> String {
        let mut inner = self.inner.write();

        // Find context by label
        let ctx_id = inner
            .context_labels
            .iter()
            .find(|(_, l)| l.as_str() == label)
            .map(|(id, _)| *id);

        if let Some(id) = ctx_id {
            inner.current_context = Some(id);
        }

        label.to_string()
    }

    /// Leave the current context.
    pub fn leave_context(&self) -> Option<String> {
        let mut inner = self.inner.write();
        let old = inner.current_context.take();
        old.and_then(|id| inner.context_labels.get(&id).cloned())
    }

    /// Get the current context label.
    pub fn current_context(&self) -> Option<String> {
        let inner = self.inner.read();
        inner
            .current_context
            .and_then(|id| inner.context_labels.get(&id).cloned())
    }

    /// List all context labels.
    pub fn list_contexts(&self) -> Vec<String> {
        self.inner
            .read()
            .context_labels
            .values()
            .cloned()
            .collect()
    }
}

// ============================================================================
// Context Engine - ExecutionEngine implementation
// ============================================================================

/// Execution engine for the `context` shell command.
///
/// Provides commands:
/// - `context new <name>` - Create or join a context
/// - `context switch <name>` - Switch to a context (same as new)
/// - `context list` - List all contexts
/// - `context current` - Show current context
pub struct ContextEngine {
    manager: Arc<ContextManager>,
}

impl ContextEngine {
    /// Create a new context engine.
    pub fn new(manager: Arc<ContextManager>) -> Self {
        Self { manager }
    }

    fn execute_inner(&self, args: Vec<String>) -> Result<String, String> {
        if args.is_empty() {
            return self.show_help();
        }

        match args[0].as_str() {
            "new" | "switch" | "join" => {
                if args.len() < 2 {
                    return Err("Usage: context new <name>".to_string());
                }
                let name = &args[1];
                let ctx = self.manager.join_context(name);
                Ok(format!("Joined context '{}'", ctx))
            }
            "list" | "ls" => {
                let contexts = self.manager.list_contexts();
                let current = self.manager.current_context();

                let mut output = String::new();
                for ctx in contexts {
                    if Some(&ctx) == current.as_ref() {
                        output.push_str(&format!("* {} (current)\n", ctx));
                    } else {
                        output.push_str(&format!("  {}\n", ctx));
                    }
                }
                Ok(output)
            }
            "current" | "show" => match self.manager.current_context() {
                Some(ctx) => Ok(format!("Current context: {}", ctx)),
                None => Ok("No active context".to_string()),
            },
            "leave" => match self.manager.leave_context() {
                Some(ctx) => Ok(format!("Left context '{}'", ctx)),
                None => Ok("No active context to leave".to_string()),
            },
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
    new <name>      Create or join a context
    switch <name>   Switch to a context (alias for new)
    list            List all contexts
    current         Show current context
    leave           Leave current context
    help            Show this help

EXAMPLES:
    context new planning
    context switch default
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
        "Manage conversation contexts (new, switch, list, current, leave)"
    }

    fn schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "_positional": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Subcommand and arguments: new|switch|list|current|leave [name]"
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

        match self.execute_inner(args) {
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

    #[test]
    fn test_context_manager_basic() {
        let manager = ContextManager::new(
            "alice".to_string(),
            KernelId::new(),
            "instance-1".to_string(),
        );

        // Register a context
        let default_id = ContextId::new();
        manager.register_context(default_id, "default");

        // Should have default context
        assert!(manager.list_contexts().contains(&"default".to_string()));

        // Register and join a new context
        let planning_id = ContextId::new();
        manager.register_context(planning_id, "planning");
        let ctx = manager.join_context("planning");
        assert_eq!(ctx, "planning");
        assert_eq!(manager.current_context(), Some("planning".to_string()));

        // List should include both
        let contexts = manager.list_contexts();
        assert!(contexts.contains(&"default".to_string()));
        assert!(contexts.contains(&"planning".to_string()));
    }

    #[tokio::test]
    async fn test_context_engine_list() {
        let manager = Arc::new(ContextManager::new(
            "bob".to_string(),
            KernelId::new(),
            "instance-2".to_string(),
        ));
        manager.register_context(ContextId::new(), "default");
        let engine = ContextEngine::new(manager);

        let result = engine
            .execute(r#"{"_positional": ["list"]}"#)
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.stdout.contains("default"));
    }

    #[tokio::test]
    async fn test_context_engine_help() {
        let manager = Arc::new(ContextManager::new(
            "dave".to_string(),
            KernelId::new(),
            "instance-4".to_string(),
        ));
        let engine = ContextEngine::new(manager);

        let result = engine
            .execute(r#"{"_positional": ["help"]}"#)
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.stdout.contains("USAGE"));
    }
}
