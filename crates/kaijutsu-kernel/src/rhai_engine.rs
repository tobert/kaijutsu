//! Async-safe Rhai scripting engine for kaijutsu kernels.
//!
//! This module provides a production-ready Rhai execution engine that:
//! - Wraps synchronous Rhai execution in `spawn_blocking` for async safety
//! - Implements the `ExecutionEngine` trait for tool integration
//! - Provides CRDT-aware block operations (insert_block, edit_text, delete_block)
//! - Supports execution interruption
//!
//! Note: Script caching is not implemented because Rhai's AST type is not
//! `Send + Sync`. Scripts are compiled fresh on each execution.
//!
//! # Example
//!
//! ```ignore
//! let engine = RhaiEngine::new(block_store);
//! let result = engine.execute(r#"
//!     let cell = create_cell("markdown");
//!     insert_block(cell, "", "text", "# Hello World");
//!     cell
//! "#).await?;
//! ```

use crate::block_store::SharedBlockStore;
use crate::db::DocumentKind;
use crate::tools::{ExecResult, ExecutionEngine};
use async_trait::async_trait;
use kaijutsu_crdt::{BlockKind, Role};
use rhai::{Dynamic, Engine, Scope};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Async-safe Rhai execution engine implementing ExecutionEngine.
pub struct RhaiEngine {
    /// Block store for CRDT operations.
    block_store: SharedBlockStore,
    /// Interrupt flag for cancellation.
    interrupted: Arc<AtomicBool>,
}

impl RhaiEngine {
    /// Create a new Rhai engine with the given block store.
    pub fn new(block_store: SharedBlockStore) -> Self {
        Self {
            block_store,
            interrupted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Create a configured Rhai engine with all functions registered.
    fn create_engine(block_store: SharedBlockStore, interrupted: Arc<AtomicBool>) -> Engine {
        let mut engine = Engine::new();

        // Configure safety limits
        engine.set_max_expr_depths(64, 64);
        engine.set_max_operations(100_000);
        engine.set_max_modules(10);
        engine.set_max_string_size(1_000_000);
        engine.set_max_array_size(10_000);
        engine.set_max_map_size(10_000);

        // Register cell functions
        Self::register_cell_functions(&mut engine, block_store.clone());

        // Register block-level CRDT functions
        Self::register_block_functions(&mut engine, block_store);

        // Register utility functions
        Self::register_utility_functions(&mut engine, interrupted);

        engine
    }

    /// Register cell-level manipulation functions.
    fn register_cell_functions(engine: &mut Engine, block_store: SharedBlockStore) {
        let store_create = block_store.clone();
        let store_get = block_store.clone();
        let store_set = block_store.clone();
        let store_list = block_store.clone();
        let store_delete = block_store.clone();
        let store_kind = block_store.clone();
        let store_len = block_store.clone();

        // create_cell(kind: &str) -> String
        // Note: Keeps old function name for Rhai script compatibility
        engine.register_fn("create_cell", move |kind: String| -> String {
            let doc_kind = match kind.as_str() {
                "code" => DocumentKind::Code,
                "markdown" | "text" => DocumentKind::Text,
                // Legacy kinds map to Conversation
                "output" | "system" | "user_message" | "agent_message" | "conversation" => DocumentKind::Conversation,
                _ => DocumentKind::Code,
            };

            let id = uuid::Uuid::new_v4().to_string();

            match store_create.create_document(id.clone(), doc_kind, None) {
                Ok(_) => {
                    debug!("Rhai: created document {} ({:?})", id, doc_kind);
                    id
                }
                Err(e) => {
                    warn!("Rhai: failed to create document: {}", e);
                    String::new()
                }
            }
        });

        // get_content(cell_id: &str) -> String
        engine.register_fn("get_content", move |cell_id: String| -> String {
            match store_get.get(&cell_id) {
                Some(doc) => {
                    let content = doc.content();
                    debug!("Rhai: get_content({}) -> {} chars", cell_id, content.len());
                    content
                }
                None => {
                    debug!("Rhai: get_content({}) -> document not found", cell_id);
                    String::new()
                }
            }
        });

        // set_content(cell_id: &str, content: &str)
        engine.register_fn("set_content", move |cell_id: String, content: String| {
            // Get block IDs to delete
            let block_ids: Vec<_> = store_set
                .get(&cell_id)
                .map(|cell| {
                    cell.doc
                        .blocks_ordered()
                        .iter()
                        .map(|b| b.id.clone())
                        .collect()
                })
                .unwrap_or_default();

            // Delete all existing blocks
            for id in block_ids {
                let _ = store_set.delete_block(&cell_id, &id);
            }

            // Insert new content as a single text block
            if !content.is_empty() {
                match store_set.insert_block(&cell_id, None, None, Role::User, BlockKind::Text, &content) {
                    Ok(_) => {
                        debug!("Rhai: set_content({}, {} chars)", cell_id, content.len());
                    }
                    Err(e) => {
                        warn!("Rhai: set_content({}) error: {}", cell_id, e);
                    }
                }
            }
        });

        // cells() -> Array
        engine.register_fn("cells", move || -> rhai::Array {
            let ids: rhai::Array = store_list
                .list_ids()
                .into_iter()
                .map(Dynamic::from)
                .collect();
            debug!("Rhai: cells() -> {} cells", ids.len());
            ids
        });

        // delete_cell(cell_id: &str) -> bool
        // Note: Keeps old function name for Rhai script compatibility
        engine.register_fn("delete_cell", move |cell_id: String| -> bool {
            match store_delete.delete_document(&cell_id) {
                Ok(()) => {
                    debug!("Rhai: delete_cell({}) -> success", cell_id);
                    true
                }
                Err(e) => {
                    warn!("Rhai: delete_cell({}) -> error: {}", cell_id, e);
                    false
                }
            }
        });

        // get_kind(cell_id) -> String
        engine.register_fn("get_kind", move |cell_id: String| -> String {
            match store_kind.get(&cell_id) {
                Some(doc) => doc.kind.as_str().to_string(),
                None => String::new(),
            }
        });

        // cell_len(cell_id) -> i64
        engine.register_fn("cell_len", move |cell_id: String| -> i64 {
            match store_len.get(&cell_id) {
                Some(doc) => doc.content().len() as i64,
                None => -1,
            }
        });
    }

    /// Register block-level CRDT manipulation functions.
    fn register_block_functions(engine: &mut Engine, block_store: SharedBlockStore) {
        let store_insert = block_store.clone();
        let store_edit = block_store.clone();
        let store_append = block_store.clone();
        let store_delete = block_store.clone();
        let store_list = block_store.clone();
        let store_get = block_store.clone();

        // insert_block(cell_id: &str, after_id: &str, kind: &str, content: &str) -> String
        // Inserts a new block after the specified block (or at the start if empty).
        // Returns the new block ID (as key string).
        engine.register_fn(
            "insert_block",
            move |cell_id: String, after_id: String, kind: String, content: String| -> String {
                // Parse after_id string to BlockId
                let after = if after_id.is_empty() {
                    None
                } else {
                    kaijutsu_crdt::BlockId::from_key(&after_id)
                };
                let after_ref = after.as_ref();

                let result = match kind.as_str() {
                    "text" => store_insert.insert_block(&cell_id, None, after_ref, Role::User, BlockKind::Text, &content),
                    "thinking" => store_insert.insert_block(&cell_id, None, after_ref, Role::Model, BlockKind::Thinking, &content),
                    "tool_use" | "tool_call" => {
                        // Parse content as JSON, or use as tool name
                        let input = serde_json::from_str(&content).unwrap_or(serde_json::Value::Null);
                        store_insert.insert_tool_call(&cell_id, None, after_ref, "unknown", input)
                    }
                    "tool_result" => {
                        // For tool_result, we need a tool_call_id. Use after_ref if available.
                        let tool_call_id = after.as_ref();
                        if let Some(tc_id) = tool_call_id {
                            store_insert.insert_tool_result(&cell_id, tc_id, after_ref, &content, false, None)
                        } else {
                            // Fallback to text if no tool_call_id
                            store_insert.insert_block(&cell_id, None, after_ref, Role::Tool, BlockKind::Text, &content)
                        }
                    }
                    _ => store_insert.insert_block(&cell_id, None, after_ref, Role::User, BlockKind::Text, &content),
                };

                match result {
                    Ok(id) => {
                        let key = id.to_key();
                        debug!(
                            "Rhai: insert_block({}, after={:?}, kind={}) -> {}",
                            cell_id, after_ref, kind, key
                        );
                        key
                    }
                    Err(e) => {
                        warn!(
                            "Rhai: insert_block({}, after={:?}, kind={}) error: {}",
                            cell_id, after_ref, kind, e
                        );
                        String::new()
                    }
                }
            },
        );

        // edit_text(cell_id: &str, block_id: &str, pos: i64, insert: &str, delete: i64)
        // Edit text content within a block at the specified position.
        // block_id should be in BlockId.to_key() format.
        engine.register_fn(
            "edit_text",
            move |cell_id: String, block_id: String, pos: i64, insert: String, delete: i64| {
                if pos < 0 || delete < 0 {
                    warn!("Rhai: edit_text invalid pos={} or delete={}", pos, delete);
                    return;
                }

                // Parse block_id string to BlockId
                let Some(bid) = kaijutsu_crdt::BlockId::from_key(&block_id) else {
                    warn!("Rhai: edit_text invalid block_id format: {}", block_id);
                    return;
                };

                match store_edit.edit_text(&cell_id, &bid, pos as usize, &insert, delete as usize) {
                    Ok(_) => {
                        debug!(
                            "Rhai: edit_text({}, {}, pos={}, del={}, ins={})",
                            cell_id,
                            block_id,
                            pos,
                            delete,
                            insert.len()
                        );
                    }
                    Err(e) => {
                        warn!("Rhai: edit_text({}, {}) error: {}", cell_id, block_id, e);
                    }
                }
            },
        );

        // append_text(cell_id: &str, block_id: &str, text: &str)
        // Append text to an existing text block.
        // block_id should be in BlockId.to_key() format.
        engine.register_fn(
            "append_text",
            move |cell_id: String, block_id: String, text: String| {
                // Parse block_id string to BlockId
                let Some(bid) = kaijutsu_crdt::BlockId::from_key(&block_id) else {
                    warn!("Rhai: append_text invalid block_id format: {}", block_id);
                    return;
                };

                match store_append.append_text(&cell_id, &bid, &text) {
                    Ok(_) => {
                        debug!(
                            "Rhai: append_text({}, {}, {} chars)",
                            cell_id,
                            block_id,
                            text.len()
                        );
                    }
                    Err(e) => {
                        warn!("Rhai: append_text({}, {}) error: {}", cell_id, block_id, e);
                    }
                }
            },
        );

        // delete_block(cell_id: &str, block_id: &str) -> bool
        // block_id should be in BlockId.to_key() format.
        engine.register_fn(
            "delete_block",
            move |cell_id: String, block_id: String| -> bool {
                // Parse block_id string to BlockId
                let Some(bid) = kaijutsu_crdt::BlockId::from_key(&block_id) else {
                    warn!("Rhai: delete_block invalid block_id format: {}", block_id);
                    return false;
                };

                match store_delete.delete_block(&cell_id, &bid) {
                    Ok(_) => {
                        debug!("Rhai: delete_block({}, {}) -> success", cell_id, block_id);
                        true
                    }
                    Err(e) => {
                        warn!("Rhai: delete_block({}, {}) error: {}", cell_id, block_id, e);
                        false
                    }
                }
            },
        );

        // list_blocks(cell_id: &str) -> Array
        // Returns an array of block IDs in order (as strings in BlockId.to_key() format).
        engine.register_fn("list_blocks", move |cell_id: String| -> rhai::Array {
            match store_list.get(&cell_id) {
                Some(cell) => cell
                    .doc
                    .blocks_ordered()
                    .iter()
                    .map(|b| Dynamic::from(b.id.to_key()))
                    .collect(),
                None => rhai::Array::new(),
            }
        });

        // get_block_content(cell_id: &str, block_id: &str) -> String
        // Returns the text content of a specific block.
        // block_id should be in BlockId.to_key() format.
        engine.register_fn(
            "get_block_content",
            move |cell_id: String, block_id: String| -> String {
                // Parse block_id string to BlockId
                let Some(target_bid) = kaijutsu_crdt::BlockId::from_key(&block_id) else {
                    return String::new();
                };

                match store_get.get(&cell_id) {
                    Some(cell) => cell
                        .doc
                        .blocks_ordered()
                        .iter()
                        .find(|b| b.id == target_bid)
                        .map(|b| b.content.clone())
                        .unwrap_or_default(),
                    None => String::new(),
                }
            },
        );
    }

    /// Register utility functions.
    fn register_utility_functions(engine: &mut Engine, interrupted: Arc<AtomicBool>) {
        // println(msg: &str) - avoid conflict with Rhai's built-in 'print'
        engine.register_fn("println", |msg: String| {
            info!("[rhai] {}", msg);
        });

        // log(level: &str, msg: &str)
        engine.register_fn("log", |level: String, msg: String| {
            match level.as_str() {
                "debug" => debug!("[rhai] {}", msg),
                "info" => info!("[rhai] {}", msg),
                "warn" => warn!("[rhai] {}", msg),
                "error" => tracing::error!("[rhai] {}", msg),
                _ => info!("[rhai] {}", msg),
            }
        });

        // is_interrupted() -> bool
        // Check if execution has been interrupted.
        let int_check = interrupted.clone();
        engine.register_fn("is_interrupted", move || -> bool {
            int_check.load(Ordering::SeqCst)
        });

        // sleep_ms(ms: i64)
        // Sleep for the given number of milliseconds. Checks for interrupt.
        let int_sleep = interrupted;
        engine.register_fn("sleep_ms", move |ms: i64| {
            if ms <= 0 {
                return;
            }
            // Sleep in small increments to allow interrupt checking
            let remaining = ms as u64;
            let chunk = 100u64;
            let mut slept = 0u64;
            while slept < remaining {
                if int_sleep.load(Ordering::SeqCst) {
                    return;
                }
                let to_sleep = (remaining - slept).min(chunk);
                std::thread::sleep(std::time::Duration::from_millis(to_sleep));
                slept += to_sleep;
            }
        });
    }

    /// Execute a script synchronously (called from spawn_blocking).
    fn execute_sync(
        block_store: &SharedBlockStore,
        code: &str,
        interrupted: Arc<AtomicBool>,
    ) -> ExecResult {
        let engine = Self::create_engine(block_store.clone(), interrupted);
        let mut scope = Scope::new();

        match engine.eval_with_scope::<Dynamic>(&mut scope, code) {
            Ok(result) => {
                // Format the result as string
                let output = format!("{}", result);
                debug!("Rhai execution success: {}", output);
                ExecResult::success(output)
            }
            Err(e) => {
                let error_msg = format!("{}", e);
                warn!("Rhai execution error: {}", error_msg);
                ExecResult::failure(1, error_msg)
            }
        }
    }
}

impl std::fmt::Debug for RhaiEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RhaiEngine")
            .field("interrupted", &self.interrupted.load(Ordering::SeqCst))
            .finish()
    }
}

#[async_trait]
impl ExecutionEngine for RhaiEngine {
    fn name(&self) -> &str {
        "rhai"
    }

    fn description(&self) -> &str {
        "Rhai scripting engine with CRDT-aware cell and block operations"
    }

    #[tracing::instrument(skip(self, code), name = "engine.rhai")]
    async fn execute(&self, code: &str) -> anyhow::Result<ExecResult> {
        // Clear interrupt flag before execution
        self.interrupted.store(false, Ordering::SeqCst);

        let block_store = Arc::clone(&self.block_store);
        let code = code.to_string();
        let interrupted = Arc::clone(&self.interrupted);

        // Execute in spawn_blocking for async safety
        let result = tokio::task::spawn_blocking(move || {
            Self::execute_sync(&block_store, &code, interrupted)
        })
        .await?;

        Ok(result)
    }

    async fn is_available(&self) -> bool {
        true
    }

    async fn complete(&self, partial: &str, _cursor: usize) -> Vec<String> {
        // Basic completion for cell functions
        let functions = [
            "create_cell",
            "get_content",
            "set_content",
            "cells",
            "delete_cell",
            "get_kind",
            "cell_len",
            "insert_block",
            "edit_text",
            "append_text",
            "delete_block",
            "list_blocks",
            "get_block_content",
            "println",
            "log",
            "is_interrupted",
            "sleep_ms",
        ];

        functions
            .iter()
            .filter(|f| f.starts_with(partial))
            .map(|s| s.to_string())
            .collect()
    }

    async fn interrupt(&self) -> anyhow::Result<()> {
        self.interrupted.store(true, Ordering::SeqCst);
        debug!("Rhai engine interrupted");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::shared_block_store;

    #[tokio::test]
    async fn test_basic_execution() {
        let store = shared_block_store("test");
        let engine = RhaiEngine::new(store);

        let result = engine.execute("40 + 2").await.unwrap();
        assert!(result.success);
        assert_eq!(result.stdout, "42");
    }

    #[tokio::test]
    async fn test_string_result() {
        let store = shared_block_store("test");
        let engine = RhaiEngine::new(store);

        let result = engine.execute(r#""hello" + " " + "world""#).await.unwrap();
        assert!(result.success);
        assert_eq!(result.stdout, "hello world");
    }

    #[tokio::test]
    async fn test_create_cell() {
        let store = shared_block_store("test");
        let engine = RhaiEngine::new(store.clone());

        let result = engine.execute(r#"create_cell("markdown")"#).await.unwrap();
        assert!(result.success);
        assert!(!result.stdout.is_empty());

        // Verify cell exists
        let ids = store.list_ids();
        assert_eq!(ids.len(), 1);
    }

    #[tokio::test]
    async fn test_cell_content_roundtrip() {
        let store = shared_block_store("test");
        let engine = RhaiEngine::new(store.clone());

        let result = engine
            .execute(
                r#"
            let id = create_cell("code");
            set_content(id, "fn main() {}");
            get_content(id)
        "#,
            )
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.stdout, "fn main() {}");
    }

    #[tokio::test]
    async fn test_block_operations() {
        let store = shared_block_store("test");
        let engine = RhaiEngine::new(store.clone());

        let result = engine
            .execute(
                r#"
            let cell = create_cell("markdown");
            let b1 = insert_block(cell, "", "text", "First block");
            let b2 = insert_block(cell, b1, "text", "Second block");
            let blocks = list_blocks(cell);
            blocks.len()
        "#,
            )
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.stdout, "2");
    }

    #[tokio::test]
    async fn test_edit_text() {
        let store = shared_block_store("test");
        let engine = RhaiEngine::new(store.clone());

        let result = engine
            .execute(
                r#"
            let cell = create_cell("code");
            let block = insert_block(cell, "", "text", "Hello World");
            edit_text(cell, block, 6, "Rhai", 5);
            get_block_content(cell, block)
        "#,
            )
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.stdout, "Hello Rhai");
    }

    #[tokio::test]
    async fn test_delete_block() {
        let store = shared_block_store("test");
        let engine = RhaiEngine::new(store.clone());

        let result = engine
            .execute(
                r#"
            let cell = create_cell("code");
            let b1 = insert_block(cell, "", "text", "Keep");
            let b2 = insert_block(cell, b1, "text", "Delete");
            delete_block(cell, b2);
            list_blocks(cell).len()
        "#,
            )
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.stdout, "1");
    }

    #[tokio::test]
    async fn test_execution_error() {
        let store = shared_block_store("test");
        let engine = RhaiEngine::new(store);

        let result = engine.execute("undefined_function()").await.unwrap();
        assert!(!result.success);
        assert!(!result.stderr.is_empty());
    }

    #[tokio::test]
    async fn test_interrupt() {
        let store = shared_block_store("test");
        let engine = RhaiEngine::new(store);

        // Interrupt before execution
        engine.interrupt().await.unwrap();

        // Script that checks interrupt
        let result = engine
            .execute(
                r#"
            if is_interrupted() {
                "interrupted"
            } else {
                "running"
            }
        "#,
            )
            .await
            .unwrap();

        // Note: interrupt is cleared at start of execute, so this should run
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_safety_limits() {
        let store = shared_block_store("test");
        let engine = RhaiEngine::new(store);

        // This should hit the operation limit
        let result = engine
            .execute(
                r#"
            let x = 0;
            loop {
                x += 1;
            }
        "#,
            )
            .await
            .unwrap();

        assert!(!result.success);
        // Rhai error messages vary - check for key terms
        assert!(
            result.stderr.contains("operations")
                || result.stderr.contains("limit")
                || result.stderr.contains("exceeded"),
            "Expected resource limit error, got: {}",
            result.stderr
        );
    }

    #[tokio::test]
    async fn test_completions() {
        let store = shared_block_store("test");
        let engine = RhaiEngine::new(store);

        let completions = engine.complete("create_", 7).await;
        assert!(completions.contains(&"create_cell".to_string()));

        let completions = engine.complete("get_", 4).await;
        assert!(completions.contains(&"get_content".to_string()));
        assert!(completions.contains(&"get_kind".to_string()));
        assert!(completions.contains(&"get_block_content".to_string()));
    }

    #[test]
    fn test_engine_debug() {
        let store = shared_block_store("test");
        let engine = RhaiEngine::new(store);
        let debug_str = format!("{:?}", engine);
        assert!(debug_str.contains("RhaiEngine"));
    }
}
