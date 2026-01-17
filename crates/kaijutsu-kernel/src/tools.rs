//! Tool registry and execution engines.
//!
//! Tools are capabilities that can be equipped to a kernel.
//! Execution engines provide different ways to run code.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Information about a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    /// Tool name (unique identifier).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Whether this tool is currently equipped.
    pub equipped: bool,
    /// Tool category (e.g., "shell", "mcp", "builtin").
    pub category: String,
}

impl ToolInfo {
    /// Create a new tool info.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        category: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            equipped: false,
            category: category.into(),
        }
    }
}

/// Result of executing code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
    /// Exit code (0 = success).
    pub exit_code: i32,
    /// Whether execution succeeded.
    pub success: bool,
}

impl ExecResult {
    /// Create a successful result.
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        }
    }

    /// Create a failure result.
    pub fn failure(exit_code: i32, stderr: impl Into<String>) -> Self {
        Self {
            stdout: String::new(),
            stderr: stderr.into(),
            exit_code,
            success: false,
        }
    }

    /// Create a result with both stdout and stderr.
    pub fn with_output(
        stdout: impl Into<String>,
        stderr: impl Into<String>,
        exit_code: i32,
    ) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_code,
            success: exit_code == 0,
        }
    }
}

/// Trait for execution engines.
///
/// Execution engines run code in different environments (kaish, bash, lua, etc.).
#[async_trait]
pub trait ExecutionEngine: Send + Sync {
    /// Get the engine name.
    fn name(&self) -> &str;

    /// Get the engine description.
    fn description(&self) -> &str;

    /// Execute code and return the result.
    async fn execute(&self, code: &str) -> anyhow::Result<ExecResult>;

    /// Check if this engine is available/ready.
    async fn is_available(&self) -> bool;

    /// Get completions for partial input.
    async fn complete(&self, partial: &str, cursor: usize) -> Vec<String> {
        let _ = (partial, cursor);
        Vec::new()
    }

    /// Interrupt a running execution.
    async fn interrupt(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Registry of tools and execution engines.
#[derive(Default)]
pub struct ToolRegistry {
    /// Available tools.
    tools: HashMap<String, ToolInfo>,
    /// Execution engines (stored separately from equipped state).
    engines: HashMap<String, Arc<dyn ExecutionEngine>>,
    /// Default execution engine name.
    default_engine: Option<String>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools)
            .field("engines", &self.engines.keys().collect::<Vec<_>>())
            .field("default_engine", &self.default_engine)
            .finish()
    }
}

impl ToolRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool as available (without an engine).
    pub fn register(&mut self, info: ToolInfo) {
        self.tools.insert(info.name.clone(), info);
    }

    /// Register a tool with an execution engine.
    /// The tool is registered but not equipped by default.
    pub fn register_with_engine(&mut self, info: ToolInfo, engine: Arc<dyn ExecutionEngine>) {
        let name = info.name.clone();
        self.tools.insert(name.clone(), info);
        self.engines.insert(name, engine);
    }

    /// Equip a tool (must have an engine already registered).
    /// Returns true if the tool was equipped, false if the tool doesn't exist
    /// or doesn't have an engine registered.
    pub fn equip(&mut self, name: &str) -> bool {
        // Check if tool has a registered engine
        if !self.engines.contains_key(name) {
            return false;
        }
        if let Some(info) = self.tools.get_mut(name) {
            info.equipped = true;
            true
        } else {
            false
        }
    }

    /// Equip a tool with an execution engine (for backwards compatibility).
    /// Registers the engine and marks the tool as equipped.
    pub fn equip_with_engine(&mut self, name: &str, engine: Arc<dyn ExecutionEngine>) -> bool {
        if let Some(info) = self.tools.get_mut(name) {
            info.equipped = true;
            self.engines.insert(name.to_string(), engine);
            true
        } else {
            false
        }
    }

    /// Unequip a tool (keeps the engine registered for later re-equipping).
    pub fn unequip(&mut self, name: &str) -> bool {
        if let Some(info) = self.tools.get_mut(name) {
            info.equipped = false;
            true
        } else {
            false
        }
    }

    /// Get a tool's info.
    pub fn get(&self, name: &str) -> Option<&ToolInfo> {
        self.tools.get(name)
    }

    /// Get a mutable reference to a tool's info.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut ToolInfo> {
        self.tools.get_mut(name)
    }

    /// Get an equipped engine.
    /// Returns the engine only if the tool is equipped.
    pub fn get_engine(&self, name: &str) -> Option<Arc<dyn ExecutionEngine>> {
        // Check if tool is equipped
        let is_equipped = self.tools.get(name).map(|t| t.equipped).unwrap_or(false);
        if is_equipped {
            self.engines.get(name).cloned()
        } else {
            None
        }
    }

    /// List all available tools.
    pub fn list(&self) -> Vec<&ToolInfo> {
        self.tools.values().collect()
    }

    /// List equipped tools.
    pub fn list_equipped(&self) -> Vec<&ToolInfo> {
        self.tools
            .values()
            .filter(|t| t.equipped)
            .collect()
    }

    /// Set the default execution engine.
    pub fn set_default_engine(&mut self, name: &str) {
        if self.engines.contains_key(name) {
            self.default_engine = Some(name.to_string());
        }
    }

    /// Get the default execution engine.
    pub fn default_engine(&self) -> Option<Arc<dyn ExecutionEngine>> {
        self.default_engine
            .as_ref()
            .and_then(|name| self.get_engine(name))
    }

    /// Execute code using the default engine.
    pub async fn execute(&self, code: &str) -> anyhow::Result<ExecResult> {
        match self.default_engine() {
            Some(engine) => engine.execute(code).await,
            None => Ok(ExecResult::failure(1, "no execution engine available")),
        }
    }
}

/// A no-op execution engine for testing.
#[derive(Debug)]
pub struct NoopEngine;

#[async_trait]
impl ExecutionEngine for NoopEngine {
    fn name(&self) -> &str {
        "noop"
    }

    fn description(&self) -> &str {
        "No-op engine for testing"
    }

    async fn execute(&self, code: &str) -> anyhow::Result<ExecResult> {
        Ok(ExecResult::success(format!("noop: {}", code)))
    }

    async fn is_available(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_info() {
        let info = ToolInfo::new("bash", "Bash shell", "shell");
        assert_eq!(info.name, "bash");
        assert!(!info.equipped);
    }

    #[test]
    fn test_exec_result() {
        let success = ExecResult::success("output");
        assert!(success.success);
        assert_eq!(success.exit_code, 0);

        let failure = ExecResult::failure(1, "error");
        assert!(!failure.success);
        assert_eq!(failure.exit_code, 1);
    }

    #[tokio::test]
    async fn test_registry() {
        let mut registry = ToolRegistry::new();

        registry.register(ToolInfo::new("test", "Test tool", "test"));
        assert!(registry.get("test").is_some());
        assert!(!registry.get("test").unwrap().equipped);

        let engine = Arc::new(NoopEngine);
        registry.equip_with_engine("test", engine);
        assert!(registry.get("test").unwrap().equipped);
        assert!(registry.get_engine("test").is_some());

        registry.unequip("test");
        assert!(!registry.get("test").unwrap().equipped);
        // Engine is still registered, just not equipped
        assert!(registry.get_engine("test").is_none());

        // Re-equip the tool (engine is still there)
        registry.equip("test");
        assert!(registry.get("test").unwrap().equipped);
        assert!(registry.get_engine("test").is_some());
    }

    #[tokio::test]
    async fn test_default_engine() {
        let mut registry = ToolRegistry::new();

        registry.register(ToolInfo::new("noop", "Noop", "test"));
        registry.equip_with_engine("noop", Arc::new(NoopEngine));
        registry.set_default_engine("noop");

        let result = registry.execute("hello").await.unwrap();
        assert!(result.success);
        assert!(result.stdout.contains("hello"));
    }

    #[tokio::test]
    async fn test_noop_engine() {
        let engine = NoopEngine;
        assert!(engine.is_available().await);

        let result = engine.execute("test").await.unwrap();
        assert!(result.success);
        assert_eq!(result.stdout, "noop: test");
    }
}
