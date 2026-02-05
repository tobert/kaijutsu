//! Tool registry and execution engines.
//!
//! Tools are capabilities that can be equipped to a kernel.
//! Execution engines provide different ways to run code.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

// ============================================================================
// DisplayHint - Dual-format output for humans vs models
// ============================================================================

/// Display hint for command output.
///
/// Tools can specify how their output should be formatted for different audiences:
/// - **Humans** → Pretty columns, colors, traditional tree
/// - **Models** → Token-efficient compact formats (brace notation, JSON)
///
/// This mirrors kaish's DisplayHint but uses serde for wire serialization.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DisplayHint {
    /// No special formatting - use raw output as-is.
    #[default]
    None,

    /// Pre-rendered output for both audiences.
    Formatted {
        /// Pretty format for humans (TTY).
        user: String,
        /// Compact format for models/piping.
        model: String,
    },

    /// Tabular data - client handles column layout.
    Table {
        /// Optional column headers.
        #[serde(skip_serializing_if = "Option::is_none")]
        headers: Option<Vec<String>>,
        /// Table rows (each row is a vector of cell values).
        rows: Vec<Vec<String>>,
        /// Entry metadata for coloring (is_dir, is_executable, etc.).
        #[serde(skip_serializing_if = "Option::is_none")]
        entry_types: Option<Vec<EntryType>>,
    },

    /// Tree structure - client chooses traditional vs compact.
    Tree {
        /// Root directory name.
        root: String,
        /// Tree structure as JSON for flexible rendering.
        structure: serde_json::Value,
        /// Pre-rendered traditional format (for human display).
        traditional: String,
        /// Pre-rendered compact format (for model/piped display).
        compact: String,
    },
}

/// Entry type for colorizing file listings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryType {
    /// Regular file.
    File,
    /// Directory.
    Directory,
    /// Executable file.
    Executable,
    /// Symbolic link.
    Symlink,
}

// ============================================================================
// Tool Info
// ============================================================================

/// Information about a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    /// Tool name (unique identifier).
    pub name: String,
    /// Human-readable description.
    pub description: String,
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
    /// Display hint for richer formatting.
    #[serde(default, skip_serializing_if = "is_display_hint_none")]
    pub hint: DisplayHint,
}

/// Helper for serde skip_serializing_if
fn is_display_hint_none(hint: &DisplayHint) -> bool {
    matches!(hint, DisplayHint::None)
}

impl ExecResult {
    /// Create a successful result.
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
            hint: DisplayHint::None,
        }
    }

    /// Create a failure result.
    pub fn failure(exit_code: i32, stderr: impl Into<String>) -> Self {
        Self {
            stdout: String::new(),
            stderr: stderr.into(),
            exit_code,
            success: false,
            hint: DisplayHint::None,
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
            hint: DisplayHint::None,
        }
    }

    /// Create a result with a display hint.
    pub fn with_hint(mut self, hint: DisplayHint) -> Self {
        self.hint = hint;
        self
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

    /// Get the JSON Schema for tool input parameters.
    ///
    /// Returns the schema used to validate tool input. This enables
    /// models to understand the expected parameters for each tool.
    fn schema(&self) -> Option<serde_json::Value> {
        None // Default: no schema
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
    pub fn register_with_engine(&mut self, info: ToolInfo, engine: Arc<dyn ExecutionEngine>) {
        let name = info.name.clone();
        self.tools.insert(name.clone(), info);
        self.engines.insert(name, engine);
    }

    /// Register an engine for an existing tool.
    pub fn register_engine(&mut self, name: &str, engine: Arc<dyn ExecutionEngine>) -> bool {
        if self.tools.contains_key(name) {
            self.engines.insert(name.to_string(), engine);
            true
        } else {
            false
        }
    }

    /// Remove an engine for a tool.
    pub fn remove_engine(&mut self, name: &str) -> bool {
        self.engines.remove(name).is_some()
    }

    /// Get a tool's info.
    pub fn get(&self, name: &str) -> Option<&ToolInfo> {
        self.tools.get(name)
    }

    /// Get a mutable reference to a tool's info.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut ToolInfo> {
        self.tools.get_mut(name)
    }

    /// Get an engine for a tool (returns it if registered).
    pub fn get_engine(&self, name: &str) -> Option<Arc<dyn ExecutionEngine>> {
        self.engines.get(name).cloned()
    }

    /// List all available tools.
    pub fn list(&self) -> Vec<&ToolInfo> {
        self.tools.values().collect()
    }

    /// List tools that have registered engines.
    pub fn list_with_engines(&self) -> Vec<&ToolInfo> {
        self.tools
            .values()
            .filter(|t| self.engines.contains_key(&t.name))
            .collect()
    }

    /// Check if a tool has an engine registered.
    pub fn has_engine(&self, name: &str) -> bool {
        self.engines.contains_key(name)
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
        assert_eq!(info.category, "shell");
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

        // Register tool without engine
        registry.register(ToolInfo::new("test", "Test tool", "test"));
        assert!(registry.get("test").is_some());
        assert!(!registry.has_engine("test"));
        assert!(registry.get_engine("test").is_none());
        assert!(registry.list_with_engines().is_empty());

        // Register engine for tool
        let engine = Arc::new(NoopEngine);
        registry.register_engine("test", engine);
        assert!(registry.has_engine("test"));
        assert!(registry.get_engine("test").is_some());
        assert_eq!(registry.list_with_engines().len(), 1);

        // Remove engine
        registry.remove_engine("test");
        assert!(!registry.has_engine("test"));
        assert!(registry.get_engine("test").is_none());
        assert!(registry.list_with_engines().is_empty());
    }

    #[tokio::test]
    async fn test_register_with_engine() {
        let mut registry = ToolRegistry::new();

        registry.register_with_engine(
            ToolInfo::new("noop", "Noop", "test"),
            Arc::new(NoopEngine),
        );
        assert!(registry.has_engine("noop"));
        assert!(registry.get_engine("noop").is_some());
        assert_eq!(registry.list_with_engines().len(), 1);
    }

    #[tokio::test]
    async fn test_default_engine() {
        let mut registry = ToolRegistry::new();

        registry.register_with_engine(
            ToolInfo::new("noop", "Noop", "test"),
            Arc::new(NoopEngine),
        );
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

    #[test]
    fn test_display_hint_default() {
        let result = ExecResult::success("test");
        assert_eq!(result.hint, DisplayHint::None);
    }

    #[test]
    fn test_display_hint_with_hint() {
        let result = ExecResult::success("test").with_hint(DisplayHint::Formatted {
            user: "Pretty output".to_string(),
            model: "compact".to_string(),
        });
        match &result.hint {
            DisplayHint::Formatted { user, model } => {
                assert_eq!(user, "Pretty output");
                assert_eq!(model, "compact");
            }
            _ => panic!("Expected Formatted hint"),
        }
    }

    #[test]
    fn test_display_hint_table() {
        let hint = DisplayHint::Table {
            headers: Some(vec!["Name".to_string(), "Size".to_string()]),
            rows: vec![
                vec!["file1.txt".to_string(), "1024".to_string()],
                vec!["file2.txt".to_string(), "2048".to_string()],
            ],
            entry_types: Some(vec![EntryType::File, EntryType::File]),
        };
        let result = ExecResult::success("file1.txt\nfile2.txt").with_hint(hint);
        assert!(matches!(result.hint, DisplayHint::Table { .. }));
    }

    #[test]
    fn test_display_hint_serialization() {
        let hint = DisplayHint::Table {
            headers: None,
            rows: vec![vec!["src".to_string()], vec!["Cargo.toml".to_string()]],
            entry_types: Some(vec![EntryType::Directory, EntryType::File]),
        };
        let json = serde_json::to_string(&hint).unwrap();
        assert!(json.contains("\"type\":\"table\""));
        assert!(json.contains("\"directory\""));
        let parsed: DisplayHint = serde_json::from_str(&json).unwrap();
        assert_eq!(hint, parsed);
    }
}
