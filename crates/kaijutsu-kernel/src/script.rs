//! Rhai scripting engine for kaijutsu kernels.
//!
//! Provides a sandboxed scripting environment for:
//! - Cell manipulation (create, read, update, delete)
//! - Hook callbacks on events (cell created, changed, executed)
//! - Custom automation and macros
//!
//! Scripts run synchronously within the kernel context and have
//! access to the BlockStore for CRDT operations.

use rhai::{Dynamic, Engine, EvalAltResult, Scope};
use tracing::{debug, info, warn};

use crate::block_store::SharedBlockStore;
use crate::db::CellKind;

/// A scripting engine instance for a kernel.
pub struct ScriptEngine {
    engine: Engine,
    scope: Scope<'static>,
    hooks: HookRegistry,
}

/// Registry of hook callbacks.
#[derive(Default)]
pub struct HookRegistry {
    on_cell_created: Vec<String>,
    on_cell_changed: Vec<String>,
    on_before_execute: Vec<String>,
    on_after_execute: Vec<String>,
}

/// Events that can trigger hooks.
#[derive(Debug, Clone)]
pub enum HookEvent {
    /// A new cell was created
    CellCreated { cell_id: String, kind: String },
    /// A cell's content changed
    CellChanged { cell_id: String },
    /// About to execute a cell
    BeforeExecute { cell_id: String },
    /// Cell execution completed
    AfterExecute { cell_id: String, success: bool },
}

/// Result of script evaluation.
pub type ScriptResult<T> = Result<T, Box<EvalAltResult>>;

impl ScriptEngine {
    /// Create a new script engine with block store access.
    pub fn new(block_store: SharedBlockStore) -> Self {
        let mut engine = Engine::new();
        let scope = Scope::new();
        let hooks = HookRegistry::default();

        // Configure engine safety limits
        engine.set_max_expr_depths(64, 64);
        engine.set_max_operations(100_000);
        engine.set_max_modules(10);
        engine.set_max_string_size(1_000_000);
        engine.set_max_array_size(10_000);
        engine.set_max_map_size(10_000);

        // Register cell manipulation functions
        Self::register_cell_functions(&mut engine, block_store);

        Self { engine, scope, hooks }
    }

    /// Register cell manipulation functions with the engine.
    fn register_cell_functions(engine: &mut Engine, block_store: SharedBlockStore) {
        // Clone Arc for each closure
        let store_create = block_store.clone();
        let store_get = block_store.clone();
        let store_set = block_store.clone();
        let store_list = block_store.clone();
        let store_delete = block_store.clone();

        // create_cell(kind: &str) -> String
        // Creates a new cell with the given kind and returns its ID
        engine.register_fn("create_cell", move |kind: String| -> String {
            let cell_kind = match kind.as_str() {
                "code" => CellKind::Code,
                "markdown" => CellKind::Markdown,
                "output" => CellKind::Output,
                "system" => CellKind::System,
                "user_message" => CellKind::UserMessage,
                "agent_message" => CellKind::AgentMessage,
                _ => CellKind::Code,
            };

            let id = uuid::Uuid::new_v4().to_string();

            match store_create.create_cell(id.clone(), cell_kind, None) {
                Ok(_) => {
                    debug!("Script: created cell {} ({:?})", id, cell_kind);
                    id
                }
                Err(e) => {
                    warn!("Script: failed to create cell: {}", e);
                    String::new()
                }
            }
        });

        // get_content(cell_id: &str) -> String
        // Returns the content of a cell, or empty string if not found
        engine.register_fn("get_content", move |cell_id: String| -> String {
            match store_get.get(&cell_id) {
                Some(cell) => {
                    let content = cell.content();
                    debug!("Script: get_content({}) -> {} chars", cell_id, content.len());
                    content
                }
                None => {
                    debug!("Script: get_content({}) -> cell not found", cell_id);
                    String::new()
                }
            }
        });

        // set_content(cell_id: &str, content: &str)
        // Replaces the entire content of a cell with a single text block
        engine.register_fn("set_content", move |cell_id: String, content: String| {
            // Get block IDs to delete
            let block_ids: Vec<_> = store_set
                .get(&cell_id)
                .map(|cell| cell.doc.blocks_ordered().iter().map(|b| b.id.clone()).collect())
                .unwrap_or_default();

            // Delete all existing blocks
            for id in block_ids {
                let _ = store_set.delete_block(&cell_id, &id);
            }

            // Insert new content as a single text block
            if !content.is_empty() {
                match store_set.insert_text_block(&cell_id, None, &content) {
                    Ok(_) => {
                        debug!("Script: set_content({}, {} chars)", cell_id, content.len());
                    }
                    Err(e) => {
                        warn!("Script: set_content({}) error: {}", cell_id, e);
                    }
                }
            }
        });

        // cells() -> Array
        // Returns an array of all cell IDs
        engine.register_fn("cells", move || -> rhai::Array {
            let ids: rhai::Array = store_list
                .list_ids()
                .into_iter()
                .map(|id| Dynamic::from(id))
                .collect();
            debug!("Script: cells() -> {} cells", ids.len());
            ids
        });

        // delete_cell(cell_id: &str) -> bool
        // Deletes a cell, returns true if successful
        engine.register_fn("delete_cell", move |cell_id: String| -> bool {
            match store_delete.delete_cell(&cell_id) {
                Ok(()) => {
                    debug!("Script: delete_cell({}) -> success", cell_id);
                    true
                }
                Err(e) => {
                    warn!("Script: delete_cell({}) -> error: {}", cell_id, e);
                    false
                }
            }
        });

        // Additional cell query functions
        let store_kind = block_store.clone();
        let store_len = block_store.clone();
        let store_append = block_store.clone();

        // get_kind(cell_id) -> String
        // Returns the kind of a cell ("code", "markdown", etc.)
        engine.register_fn("get_kind", move |cell_id: String| -> String {
            match store_kind.get(&cell_id) {
                Some(cell) => {
                    let kind = match cell.kind {
                        CellKind::Code => "code",
                        CellKind::Markdown => "markdown",
                        CellKind::Output => "output",
                        CellKind::System => "system",
                        CellKind::UserMessage => "user_message",
                        CellKind::AgentMessage => "agent_message",
                    };
                    kind.to_string()
                }
                None => String::new(),
            }
        });

        // cell_len(cell_id) -> i64
        // Returns the length of a cell's content in characters
        engine.register_fn("cell_len", move |cell_id: String| -> i64 {
            match store_len.get(&cell_id) {
                Some(cell) => cell.content().len() as i64,
                None => -1,
            }
        });

        // append_text(cell_id, text)
        // Append text to the last text block, or create a new one
        engine.register_fn("append_text", move |cell_id: String, text: String| {
            let text_len = text.len();

            // Find the last text block
            let last_text_block = store_append
                .get(&cell_id)
                .and_then(|cell| {
                    cell.doc
                        .blocks_ordered()
                        .iter()
                        .rev()
                        .find(|b| matches!(b.content, kaijutsu_crdt::BlockContentSnapshot::Text { .. }))
                        .map(|b| b.id.clone())
                });

            match last_text_block {
                Some(block_id) => {
                    if let Err(e) = store_append.append_text(&cell_id, &block_id, &text) {
                        warn!("Script: append_text({}) error: {}", cell_id, e);
                    }
                }
                None => {
                    // No text block exists, create one
                    if let Err(e) = store_append.insert_text_block(&cell_id, None, &text) {
                        warn!("Script: append_text({}) - create block error: {}", cell_id, e);
                    }
                }
            }
            debug!("Script: append_text({}, {} chars)", cell_id, text_len);
        });

        // Utility functions - use 'println' to avoid conflict with Rhai's built-in 'print'
        engine.register_fn("println", |msg: String| {
            info!("[script] {}", msg);
        });

        engine.register_fn("log", |level: String, msg: String| {
            match level.as_str() {
                "debug" => debug!("[script] {}", msg),
                "info" => info!("[script] {}", msg),
                "warn" => warn!("[script] {}", msg),
                "error" => tracing::error!("[script] {}", msg),
                _ => info!("[script] {}", msg),
            }
        });
    }

    /// Evaluate a script string.
    pub fn eval(&mut self, script: &str) -> ScriptResult<Dynamic> {
        self.engine.eval_with_scope(&mut self.scope, script)
    }

    /// Evaluate a script and return a specific type.
    pub fn eval_as<T: Clone + 'static>(&mut self, script: &str) -> ScriptResult<T>
    where
        T: Clone + Send + Sync,
    {
        self.engine.eval_with_scope(&mut self.scope, script)
    }

    /// Run a script file (by content).
    pub fn run(&mut self, script: &str) -> ScriptResult<()> {
        self.engine.run_with_scope(&mut self.scope, script)
    }

    /// Register a hook callback script.
    pub fn register_hook(&mut self, event: &str, callback_script: String) {
        match event {
            "cell_created" => self.hooks.on_cell_created.push(callback_script),
            "cell_changed" => self.hooks.on_cell_changed.push(callback_script),
            "before_execute" => self.hooks.on_before_execute.push(callback_script),
            "after_execute" => self.hooks.on_after_execute.push(callback_script),
            _ => warn!("Unknown hook event: {}", event),
        }
    }

    /// Fire a hook event.
    pub fn fire_hook(&mut self, event: HookEvent) {
        let scripts = match &event {
            HookEvent::CellCreated { cell_id, kind } => {
                self.scope.push("event_cell_id", cell_id.clone());
                self.scope.push("event_kind", kind.clone());
                self.hooks.on_cell_created.clone()
            }
            HookEvent::CellChanged { cell_id } => {
                self.scope.push("event_cell_id", cell_id.clone());
                self.hooks.on_cell_changed.clone()
            }
            HookEvent::BeforeExecute { cell_id } => {
                self.scope.push("event_cell_id", cell_id.clone());
                self.hooks.on_before_execute.clone()
            }
            HookEvent::AfterExecute { cell_id, success } => {
                self.scope.push("event_cell_id", cell_id.clone());
                self.scope.push("event_success", *success);
                self.hooks.on_after_execute.clone()
            }
        };

        for script in scripts {
            if let Err(e) = self.run(&script) {
                warn!("Hook script error: {}", e);
            }
        }

        // Clean up event variables
        let _ = self.scope.remove::<String>("event_cell_id");
        let _ = self.scope.remove::<String>("event_kind");
        let _ = self.scope.remove::<bool>("event_success");
    }

    /// Get a variable from the scope.
    pub fn get_var<T: Clone + 'static>(&self, name: &str) -> Option<T> {
        self.scope.get_value(name)
    }

    /// Set a variable in the scope.
    pub fn set_var(&mut self, name: impl Into<String>, value: impl Into<Dynamic>) {
        let name = name.into();
        if self.scope.contains(&name) {
            self.scope.set_value(&name, value.into());
        } else {
            self.scope.push(name, value.into());
        }
    }

    /// Clear all variables from scope.
    pub fn clear_scope(&mut self) {
        self.scope.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::shared_block_store;

    #[test]
    fn test_basic_eval() {
        let store = shared_block_store("test");
        let mut engine = ScriptEngine::new(store);

        let result: i64 = engine.eval_as("40 + 2").unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn test_variables() {
        let store = shared_block_store("test");
        let mut engine = ScriptEngine::new(store);

        engine.set_var("x", 10_i64);
        let result: i64 = engine.eval_as("x * 4").unwrap();
        assert_eq!(result, 40);
    }

    #[test]
    fn test_print_function() {
        let store = shared_block_store("test");
        let mut engine = ScriptEngine::new(store);

        // Should not panic
        engine.run(r#"println("Hello from Rhai!");"#).unwrap();
    }

    #[test]
    fn test_create_cell_function() {
        let store = shared_block_store("test");
        let mut engine = ScriptEngine::new(store.clone());

        // create_cell returns a UUID string
        let result: String = engine.eval_as(r#"create_cell("code")"#).unwrap();
        assert!(!result.is_empty());
        // UUID format: 8-4-4-4-12
        assert!(result.contains('-'));

        // Verify cell actually exists in store
        assert!(store.get(&result).is_some());
    }

    #[test]
    fn test_cell_content_roundtrip() {
        let store = shared_block_store("test");
        let mut engine = ScriptEngine::new(store.clone());

        // Create a cell and set its content
        engine
            .run(
                r#"
            let id = create_cell("markdown");
            set_content(id, "Hello World from Rhai!");
        "#,
            )
            .unwrap();

        // Get the cell ID from the store and verify content
        let ids = store.list_ids();
        assert_eq!(ids.len(), 1);

        let cell = store.get(&ids[0]).unwrap();
        assert_eq!(cell.content(), "Hello World from Rhai!");
    }

    #[test]
    fn test_get_content_via_script() {
        let store = shared_block_store("test");
        let mut engine = ScriptEngine::new(store);

        // Create cell, set content, get it back
        let content: String = engine
            .eval_as(
                r#"
            let id = create_cell("code");
            set_content(id, "fn main() {}");
            get_content(id)
        "#,
            )
            .unwrap();

        assert_eq!(content, "fn main() {}");
    }

    #[test]
    fn test_cells_list() {
        let store = shared_block_store("test");
        let mut engine = ScriptEngine::new(store);

        // Create multiple cells
        engine
            .run(
                r#"
            create_cell("code");
            create_cell("markdown");
            create_cell("output");
        "#,
            )
            .unwrap();

        // Get the list
        let count: i64 = engine.eval_as("cells().len()").unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn test_delete_cell() {
        let store = shared_block_store("test");
        let mut engine = ScriptEngine::new(store.clone());

        // Create and then delete a cell
        let deleted: bool = engine
            .eval_as(
                r#"
            let id = create_cell("code");
            delete_cell(id)
        "#,
            )
            .unwrap();

        assert!(deleted);

        // Verify cell is gone
        assert!(store.list_ids().is_empty());
    }

    #[test]
    fn test_get_kind() {
        let store = shared_block_store("test");
        let mut engine = ScriptEngine::new(store);

        let kind: String = engine
            .eval_as(
                r#"
            let id = create_cell("markdown");
            get_kind(id)
        "#,
            )
            .unwrap();

        assert_eq!(kind, "markdown");
    }

    #[test]
    fn test_cell_len() {
        let store = shared_block_store("test");
        let mut engine = ScriptEngine::new(store);

        let len: i64 = engine
            .eval_as(
                r#"
            let id = create_cell("code");
            set_content(id, "hello");
            cell_len(id)
        "#,
            )
            .unwrap();

        assert_eq!(len, 5);
    }

    #[test]
    fn test_append_text() {
        let store = shared_block_store("test");
        let mut engine = ScriptEngine::new(store);

        let content: String = engine
            .eval_as(
                r#"
            let id = create_cell("code");
            set_content(id, "hello");
            append_text(id, " world");
            get_content(id)
        "#,
            )
            .unwrap();

        assert_eq!(content, "hello world");
    }

    #[test]
    fn test_hook_registration() {
        let store = shared_block_store("test");
        let mut engine = ScriptEngine::new(store);

        engine.register_hook(
            "cell_created",
            r#"println("Cell created: " + event_cell_id);"#.to_string(),
        );

        // Fire the hook
        engine.fire_hook(HookEvent::CellCreated {
            cell_id: "test-cell-123".to_string(),
            kind: "code".to_string(),
        });
    }

    #[test]
    fn test_script_safety_limits() {
        let store = shared_block_store("test");
        let mut engine = ScriptEngine::new(store);

        // This should hit the operation limit
        let result = engine.run(
            r#"
            let x = 0;
            loop {
                x += 1;
            }
        "#,
        );

        assert!(result.is_err());
    }
}
