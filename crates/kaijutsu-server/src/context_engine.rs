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
use parking_lot::RwLock;

use kaijutsu_kernel::tools::{ExecResult, ExecutionEngine};

use crate::rpc::ContextState;

// ============================================================================
// Context Manager - Thread-safe state for context operations
// ============================================================================

/// Thread-safe context state shared between RPC handlers and ContextEngine.
///
/// Tracks which contexts exist and which one is currently active for this
/// connection. The seat abstraction has been removed — this is now just
/// lightweight context membership tracking.
#[derive(Debug)]
pub struct ContextManager {
    inner: RwLock<ContextManagerInner>,
}

#[derive(Debug, Default)]
#[allow(dead_code)] // nick/kernel_id/instance used by constructor, useful for future presence
struct ContextManagerInner {
    /// All contexts in the kernel
    contexts: HashMap<String, ContextState>,
    /// Current user identity (nick)
    nick: String,
    /// Kernel ID this manager belongs to
    kernel_id: String,
    /// Current instance ID for this connection
    instance: String,
    /// Currently active context name
    current_context: Option<String>,
}

impl ContextManager {
    /// Create a new context manager.
    pub fn new(nick: String, kernel_id: String, instance: String) -> Self {
        let mut contexts = HashMap::new();
        // Always have a default context
        contexts.insert("default".to_string(), ContextState::new("default".to_string()));

        Self {
            inner: RwLock::new(ContextManagerInner {
                contexts,
                nick,
                kernel_id,
                instance,
                current_context: None,
            }),
        }
    }

    /// Join or create a context, returning the context name.
    pub fn join_context(&self, context_name: &str) -> String {
        let mut inner = self.inner.write();

        // Ensure context exists
        inner.contexts
            .entry(context_name.to_string())
            .or_insert_with(|| ContextState::new(context_name.to_string()));

        inner.current_context = Some(context_name.to_string());

        context_name.to_string()
    }

    /// Leave the current context.
    pub fn leave_context(&self) -> Option<String> {
        let mut inner = self.inner.write();
        inner.current_context.take()
    }

    /// Get the current context name.
    pub fn current_context(&self) -> Option<String> {
        self.inner.read().current_context.clone()
    }

    /// List all context names.
    pub fn list_contexts(&self) -> Vec<String> {
        self.inner.read().contexts.keys().cloned().collect()
    }

    /// Get context state.
    pub fn get_context(&self, name: &str) -> Option<ContextState> {
        self.inner.read().contexts.get(name).cloned()
    }

    /// Attach a document to a context.
    ///
    /// If the context doesn't exist, it will be created first.
    pub fn attach_document(&self, context_name: &str, document_id: &str, attached_by: &str) {
        use crate::rpc::ContextDocument;

        let mut inner = self.inner.write();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_millis() as u64;

        let doc = ContextDocument {
            id: document_id.to_string(),
            attached_by: attached_by.to_string(),
            attached_at: now,
        };

        // Ensure context exists
        inner.contexts
            .entry(context_name.to_string())
            .or_insert_with(|| ContextState::new(context_name.to_string()))
            .documents.push(doc);
    }

    /// Sync state from RPC layer (called when contexts change externally).
    pub fn sync_contexts(&self, contexts: HashMap<String, ContextState>) {
        let mut inner = self.inner.write();
        inner.contexts = contexts;
    }

    /// Get contexts map (for RPC sync back).
    pub fn contexts(&self) -> HashMap<String, ContextState> {
        self.inner.read().contexts.clone()
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
            "current" | "show" => {
                match self.manager.current_context() {
                    Some(ctx) => Ok(format!("Current context: {}", ctx)),
                    None => Ok("No active context".to_string()),
                }
            }
            "leave" => {
                match self.manager.leave_context() {
                    Some(ctx) => Ok(format!("Left context '{}'", ctx)),
                    None => Ok("No active context to leave".to_string()),
                }
            }
            "help" | "-h" | "--help" => self.show_help(),
            other => Err(format!("Unknown subcommand: {}. Use 'context help' for usage.", other)),
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
"#.to_string())
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
            "kernel-1".to_string(),
            "instance-1".to_string(),
        );

        // Should have default context
        assert!(manager.list_contexts().contains(&"default".to_string()));

        // Join a new context
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
            "kernel-2".to_string(),
            "instance-2".to_string(),
        ));
        let engine = ContextEngine::new(manager);

        let result = engine.execute(r#"{"_positional": ["list"]}"#).await.unwrap();
        assert!(result.success);
        assert!(result.stdout.contains("default"));
    }

    #[tokio::test]
    async fn test_context_engine_new() {
        let manager = Arc::new(ContextManager::new(
            "charlie".to_string(),
            "kernel-3".to_string(),
            "instance-3".to_string(),
        ));
        let engine = ContextEngine::new(manager.clone());

        let result = engine.execute(r#"{"_positional": ["new", "testing"]}"#).await.unwrap();
        assert!(result.success);
        assert!(result.stdout.contains("testing"));
        assert_eq!(manager.current_context(), Some("testing".to_string()));
    }

    #[tokio::test]
    async fn test_context_engine_help() {
        let manager = Arc::new(ContextManager::new(
            "dave".to_string(),
            "kernel-4".to_string(),
            "instance-4".to_string(),
        ));
        let engine = ContextEngine::new(manager);

        let result = engine.execute(r#"{"_positional": ["help"]}"#).await.unwrap();
        assert!(result.success);
        assert!(result.stdout.contains("USAGE"));
    }
}
