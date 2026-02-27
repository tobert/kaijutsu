//! Tool registry and execution engines.
//!
//! Tools are capabilities registered on a kernel.
//! Execution engines provide different ways to run code.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use kaijutsu_types::{ContextId, KernelId, PrincipalId, SessionId};

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
    /// Structured output data for richer formatting (tables, trees).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<kaijutsu_types::OutputData>,
}

impl ExecResult {
    /// Create a successful result.
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
            output: None,
        }
    }

    /// Create a failure result.
    pub fn failure(exit_code: i32, stderr: impl Into<String>) -> Self {
        Self {
            stdout: String::new(),
            stderr: stderr.into(),
            exit_code,
            success: false,
            output: None,
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
            output: None,
        }
    }

    /// Attach structured output data for richer formatting.
    pub fn with_output_data(mut self, data: kaijutsu_types::OutputData) -> Self {
        self.output = Some(data);
        self
    }
}

// ============================================================================
// EngineArgs — structured args bridging LLMs and kaish
// ============================================================================

/// Structured arguments for execution engines.
///
/// Bridges two calling conventions:
/// - **LLMs** put everything in `_positional` as raw argv strings
/// - **kaish** splits into `positional` + `named` keys + boolean `flags`
///
/// Use [`to_argv()`](EngineArgs::to_argv) to reconstruct a flat argv for engines
/// that parse Unix-style args. The ordering is:
///
/// 1. `positional[0]` (subcommand)
/// 2. Named args as `-k value` / `--key value`
/// 3. Boolean flags as `-f` / `--flag`
/// 4. `positional[1..]` (remaining positional args)
///
/// This works because git/drift handlers use scan-based flag detection (`.any()`,
/// `.find()`, `extract_m_flag`) rather than strict positional indexing.
#[derive(Debug, Clone, Default)]
pub struct EngineArgs {
    /// Positional arguments (subcommand + trailing args).
    pub positional: Vec<String>,
    /// Named arguments (`-m "msg"`, `--output file`). Deterministic ordering.
    pub named: BTreeMap<String, String>,
    /// Boolean flags (`--cached`, `-s`). Deterministic ordering.
    pub flags: BTreeSet<String>,
}

impl EngineArgs {
    /// Parse from the JSON object that `tool_args_to_json()` produces.
    ///
    /// Extracts:
    /// - `_positional` array → `positional` (String, Number, Bool all coerced)
    /// - String-valued root keys (except `_positional`) → `named`
    /// - `true`-valued root keys → `flags`
    pub fn from_json(value: &serde_json::Value) -> Self {
        let mut args = Self::default();

        let obj = match value.as_object() {
            Some(o) => o,
            None => return args,
        };

        // Extract positional args, coercing all scalar types to String
        if let Some(arr) = obj.get("_positional").and_then(|v| v.as_array()) {
            args.positional = arr
                .iter()
                .filter_map(|v| match v {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Number(n) => Some(n.to_string()),
                    serde_json::Value::Bool(b) => Some(b.to_string()),
                    _ => None,
                })
                .collect();
        }

        // Extract named args and flags from remaining keys
        for (key, value) in obj {
            if key == "_positional" {
                continue;
            }
            match value {
                serde_json::Value::String(s) => {
                    args.named.insert(key.clone(), s.clone());
                }
                serde_json::Value::Bool(true) => {
                    args.flags.insert(key.clone());
                }
                // Number-valued named args (rare but handle gracefully)
                serde_json::Value::Number(n) => {
                    args.named.insert(key.clone(), n.to_string());
                }
                _ => {} // Skip arrays, objects, null, false
            }
        }

        args
    }

    /// Reconstruct a flat argv vector suitable for Unix-style argument parsing.
    ///
    /// Ordering: `subcommand` + `named pairs` + `flags` + `trailing positionals`.
    pub fn to_argv(&self) -> Vec<String> {
        let mut argv = Vec::new();

        // Subcommand (positional[0])
        if let Some(first) = self.positional.first() {
            argv.push(first.clone());
        }

        // Named args: single-char → `-k value`, multi-char → `--key value`
        for (key, value) in &self.named {
            if key.len() == 1 {
                argv.push(format!("-{}", key));
            } else {
                argv.push(format!("--{}", key));
            }
            argv.push(value.clone());
        }

        // Flags: single-char → `-f`, multi-char → `--flag`
        for flag in &self.flags {
            if flag.len() == 1 {
                argv.push(format!("-{}", flag));
            } else {
                argv.push(format!("--{}", flag));
            }
        }

        // Remaining positional args
        if self.positional.len() > 1 {
            argv.extend_from_slice(&self.positional[1..]);
        }

        argv
    }
}

// ============================================================================
// ToolContext — execution context for every tool invocation
// ============================================================================

/// Execution context passed to every tool invocation.
///
/// The "who, where, what" of the call site. Kaijutsu's counterpart to
/// kaish's ExecContext, focused on identity and location rather than
/// shell plumbing (pipes, jobs, aliases).
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Who is executing this tool.
    pub principal_id: PrincipalId,
    /// Which conversation context this occurs in.
    pub context_id: ContextId,
    /// Working directory for file-relative operations (VFS or real path).
    pub cwd: PathBuf,
    /// Session identifier for audit trail.
    pub session_id: SessionId,
    /// Kernel identifier.
    pub kernel_id: KernelId,
}

impl ToolContext {
    pub fn new(
        principal_id: PrincipalId,
        context_id: ContextId,
        cwd: impl Into<PathBuf>,
        session_id: SessionId,
        kernel_id: KernelId,
    ) -> Self {
        Self {
            principal_id,
            context_id,
            cwd: cwd.into(),
            session_id,
            kernel_id,
        }
    }

    /// Minimal context for tests.
    pub fn test() -> Self {
        Self {
            principal_id: PrincipalId::new(),
            context_id: ContextId::new(),
            cwd: PathBuf::from("/"),
            session_id: SessionId::new(),
            kernel_id: KernelId::new(),
        }
    }
}

// ============================================================================
// ExecutionEngine trait
// ============================================================================

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
    async fn execute(&self, code: &str, ctx: &ToolContext) -> anyhow::Result<ExecResult>;

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
    /// Execution engines.
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
    #[tracing::instrument(skip(self, code, ctx))]
    pub async fn execute(&self, code: &str, ctx: &ToolContext) -> anyhow::Result<ExecResult> {
        match self.default_engine() {
            Some(engine) => engine.execute(code, ctx).await,
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

    async fn execute(&self, code: &str, _ctx: &ToolContext) -> anyhow::Result<ExecResult> {
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

        let result = registry.execute("hello", &ToolContext::test()).await.unwrap();
        assert!(result.success);
        assert!(result.stdout.contains("hello"));
    }

    #[tokio::test]
    async fn test_noop_engine() {
        let engine = NoopEngine;
        assert!(engine.is_available().await);

        let result = engine.execute("test", &ToolContext::test()).await.unwrap();
        assert!(result.success);
        assert_eq!(result.stdout, "noop: test");
    }

    #[test]
    fn test_output_default_none() {
        let result = ExecResult::success("test");
        assert!(result.output.is_none());
    }

    #[test]
    fn test_output_with_data() {
        use kaijutsu_types::OutputData;
        let data = OutputData::text("Pretty output");
        let result = ExecResult::success("test").with_output_data(data.clone());
        assert_eq!(result.output, Some(data));
    }

    #[test]
    fn test_output_data_serialization() {
        use kaijutsu_types::{OutputData, OutputNode, OutputEntryType};
        let mut node1 = OutputNode::new("src");
        node1.entry_type = OutputEntryType::Directory;
        let mut node2 = OutputNode::new("Cargo.toml");
        node2.entry_type = OutputEntryType::File;
        let data = OutputData::nodes(vec![node1, node2]);
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("\"directory\""));
        let parsed: OutputData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, parsed);
    }

    // ========================================================================
    // EngineArgs tests
    // ========================================================================

    #[test]
    fn engine_args_llm_passthrough() {
        // LLMs put everything in _positional — to_argv() returns them as-is
        let json = serde_json::json!({"_positional": ["commit", "-m", "hello world"]});
        let args = EngineArgs::from_json(&json);

        assert_eq!(args.positional, vec!["commit", "-m", "hello world"]);
        assert!(args.named.is_empty());
        assert!(args.flags.is_empty());
        assert_eq!(args.to_argv(), vec!["commit", "-m", "hello world"]);
    }

    #[test]
    fn engine_args_kaish_commit_m_flag() {
        // kaish: `git commit -m "msg"` → positional: ["commit","msg"], flags: {"m"}
        let json = serde_json::json!({
            "_positional": ["commit", "msg"],
            "m": true
        });
        let args = EngineArgs::from_json(&json);
        let argv = args.to_argv();

        // Should reconstruct: ["commit", "-m", "msg"]
        assert_eq!(argv[0], "commit");
        assert!(argv.contains(&"-m".to_string()));
        assert!(argv.contains(&"msg".to_string()));
    }

    #[test]
    fn engine_args_kaish_diff_cached() {
        // kaish: `git diff --cached` → positional: ["diff"], flags: {"cached"}
        let json = serde_json::json!({
            "_positional": ["diff"],
            "cached": true
        });
        let argv = EngineArgs::from_json(&json).to_argv();
        assert_eq!(argv, vec!["diff", "--cached"]);
    }

    #[test]
    fn engine_args_kaish_log_numeric_flag() {
        // kaish: `git log -5` → positional: ["log"], flags: {"5"}
        let json = serde_json::json!({
            "_positional": ["log"],
            "5": true
        });
        let argv = EngineArgs::from_json(&json).to_argv();
        assert_eq!(argv, vec!["log", "-5"]);
    }

    #[test]
    fn engine_args_numeric_positional() {
        // drift cancel 1 → positional might include a JSON number
        let json = serde_json::json!({"_positional": ["cancel", 1]});
        let args = EngineArgs::from_json(&json);
        assert_eq!(args.positional, vec!["cancel", "1"]);
        assert_eq!(args.to_argv(), vec!["cancel", "1"]);
    }

    #[test]
    fn engine_args_empty_json() {
        let json = serde_json::json!({});
        let args = EngineArgs::from_json(&json);
        assert!(args.positional.is_empty());
        assert!(args.to_argv().is_empty());
    }

    #[test]
    fn engine_args_named_string_values() {
        let json = serde_json::json!({
            "_positional": ["push"],
            "target": "abc123"
        });
        let args = EngineArgs::from_json(&json);
        assert_eq!(args.named.get("target"), Some(&"abc123".to_string()));
        let argv = args.to_argv();
        assert_eq!(argv[0], "push");
        assert!(argv.contains(&"--target".to_string()));
        assert!(argv.contains(&"abc123".to_string()));
    }
}
