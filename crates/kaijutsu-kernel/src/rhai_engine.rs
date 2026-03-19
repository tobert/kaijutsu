//! Async-safe Rhai scripting engine for kaijutsu kernels.
//!
//! This module provides a production-ready Rhai execution engine that:
//! - Wraps synchronous Rhai execution in `spawn_blocking` for async safety
//! - Implements the `ExecutionEngine` trait for tool integration
//! - Provides CRDT-aware block operations (insert_block, edit_text, delete_block)
//! - Supports execution interruption
//! - Maintains persistent per-context `Scope` so variables and functions survive
//!   across tool calls within the same conversation
//!
//! # Example
//!
//! ```ignore
//! let engine = RhaiEngine::new(block_store);
//! let result = engine.execute(r#"
//!     let cell = create_cell("markdown");
//!     insert_block(cell, "", "text", "# Hello World");
//!     cell
//! "#, &ctx).await?;
//! ```

use crate::block_store::SharedBlockStore;
use crate::tools::{ExecResult, ExecutionEngine, ToolContext};
use async_trait::async_trait;
use kaijutsu_crdt::{BlockKind, Role, Status};
use kaijutsu_types::ContextId;
use kaijutsu_types::DocKind;
use rhai::{Dynamic, Engine, Scope};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

/// Async-safe Rhai execution engine implementing ExecutionEngine.
///
/// Maintains a per-context `Scope` so variables and helper functions persist
/// across tool calls within the same conversation. The Rhai `Engine` itself is
/// stateless and recreated each call (it only holds registered functions, which
/// are the same every time).
/// Optional callback to register additional Rhai functions on engine creation.
///
/// Used by the server crate to inject synthesis functions (`embed`, `cosine_sim`,
/// etc.) that require `kaijutsu-index` — not available in this crate.
pub type ExtraRegistrar = Arc<dyn Fn(&mut Engine) + Send + Sync>;

pub struct RhaiEngine {
    /// Block store for CRDT operations.
    block_store: SharedBlockStore,
    /// Interrupt flag for cancellation.
    interrupted: Arc<AtomicBool>,
    /// Per-context persistent scopes. `Scope<'static>` is the rhai convention.
    /// `Mutex` (not `RwLock`) because `eval_with_scope` requires `&mut Scope`.
    scopes: Arc<Mutex<HashMap<ContextId, Scope<'static>>>>,
    /// Optional extension point for registering extra functions (e.g. synthesis).
    extra_registrar: Option<ExtraRegistrar>,
}

impl RhaiEngine {
    /// Create a new Rhai engine with the given block store.
    pub fn new(block_store: SharedBlockStore) -> Self {
        Self {
            block_store,
            interrupted: Arc::new(AtomicBool::new(false)),
            scopes: Arc::new(Mutex::new(HashMap::new())),
            extra_registrar: None,
        }
    }

    /// Register additional functions on the Rhai engine (builder pattern).
    ///
    /// The registrar is called on every `create_engine()` invocation, after all
    /// standard functions are registered. This is how server-crate synthesis
    /// functions (embed, cosine_sim, etc.) get injected.
    pub fn with_extra_registrar(mut self, registrar: ExtraRegistrar) -> Self {
        self.extra_registrar = Some(registrar);
        self
    }

    /// Create a configured Rhai engine with all functions registered.
    fn create_engine(
        block_store: SharedBlockStore,
        interrupted: Arc<AtomicBool>,
        context_id: ContextId,
        extra_registrar: Option<&ExtraRegistrar>,
    ) -> (Engine, kaijutsu_rhai::OutputCollector) {
        let mut engine = Engine::new();

        // Configure safety limits
        engine.set_max_expr_depths(64, 64);
        engine.set_max_operations(10_000_000);
        engine.set_max_modules(10);
        engine.set_max_string_size(1_000_000);
        engine.set_max_array_size(10_000);
        engine.set_max_map_size(10_000);

        // Register shared stdlib (math, color, format) + output callbacks
        kaijutsu_rhai::register_stdlib(&mut engine);
        let collector = kaijutsu_rhai::register_output_callbacks(&mut engine);

        // Register cell functions
        Self::register_cell_functions(&mut engine, block_store.clone());

        // Register block-level CRDT functions
        Self::register_block_functions(&mut engine, block_store.clone());

        // Register context-aware functions (emit into current context)
        Self::register_context_functions(&mut engine, block_store, context_id);

        // Register utility functions
        Self::register_utility_functions(&mut engine, interrupted);

        // Server-injected functions (synthesis, etc.)
        if let Some(registrar) = extra_registrar {
            registrar(&mut engine);
        }

        (engine, collector)
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
                "code" => DocKind::Code,
                "markdown" | "text" => DocKind::Text,
                // Legacy kinds map to Conversation
                "output" | "system" | "user_message" | "agent_message" | "conversation" => {
                    DocKind::Conversation
                }
                _ => DocKind::Code,
            };

            let ctx = ContextId::new();

            match store_create.create_document(ctx, doc_kind, None) {
                Ok(_) => {
                    debug!("Rhai: created document {} ({:?})", ctx, doc_kind);
                    ctx.to_string()
                }
                Err(e) => {
                    warn!("Rhai: failed to create document: {}", e);
                    String::new()
                }
            }
        });

        // get_content(cell_id: &str) -> String
        engine.register_fn("get_content", move |cell_id: String| -> String {
            let Ok(ctx) = ContextId::parse(&cell_id) else {
                warn!("Rhai: get_content invalid cell_id: {}", cell_id);
                return String::new();
            };
            match store_get.get(ctx) {
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
            let Ok(ctx) = ContextId::parse(&cell_id) else {
                warn!("Rhai: set_content invalid cell_id: {}", cell_id);
                return;
            };

            // Get block IDs to delete
            let block_ids: Vec<_> = store_set
                .get(ctx)
                .map(|cell| cell.doc.blocks_ordered().iter().map(|b| b.id).collect())
                .unwrap_or_default();

            // Delete all existing blocks
            for id in block_ids {
                let _ = store_set.delete_block(ctx, &id);
            }

            // Insert new content as a single text block
            if !content.is_empty() {
                match store_set.insert_block(
                    ctx,
                    None,
                    None,
                    Role::User,
                    BlockKind::Text,
                    &content,
                    Status::Done,
                ) {
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
                .map(|ctx| Dynamic::from(ctx.to_string()))
                .collect();
            debug!("Rhai: cells() -> {} cells", ids.len());
            ids
        });

        // delete_cell(cell_id: &str) -> bool
        // Note: Keeps old function name for Rhai script compatibility
        engine.register_fn("delete_cell", move |cell_id: String| -> bool {
            let Ok(ctx) = ContextId::parse(&cell_id) else {
                warn!("Rhai: delete_cell invalid cell_id: {}", cell_id);
                return false;
            };
            match store_delete.delete_document(ctx) {
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
            let Ok(ctx) = ContextId::parse(&cell_id) else {
                warn!("Rhai: get_kind invalid cell_id: {}", cell_id);
                return String::new();
            };
            match store_kind.get(ctx) {
                Some(doc) => doc.kind.as_str().to_string(),
                None => String::new(),
            }
        });

        // cell_len(cell_id) -> i64
        engine.register_fn("cell_len", move |cell_id: String| -> i64 {
            let Ok(ctx) = ContextId::parse(&cell_id) else {
                warn!("Rhai: cell_len invalid cell_id: {}", cell_id);
                return -1;
            };
            match store_len.get(ctx) {
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
                let Ok(ctx) = ContextId::parse(&cell_id) else {
                    warn!("Rhai: insert_block invalid cell_id: {}", cell_id);
                    return String::new();
                };

                // Parse after_id string to BlockId
                let after = if after_id.is_empty() {
                    None
                } else {
                    kaijutsu_crdt::BlockId::from_key(&after_id)
                };
                let after_ref = after.as_ref();

                let result = match kind.as_str() {
                    "text" => store_insert.insert_block(
                        ctx,
                        None,
                        after_ref,
                        Role::User,
                        BlockKind::Text,
                        &content,
                        Status::Done,
                    ),
                    "thinking" => store_insert.insert_block(
                        ctx,
                        None,
                        after_ref,
                        Role::Model,
                        BlockKind::Thinking,
                        &content,
                        Status::Done,
                    ),
                    "tool_use" | "tool_call" => {
                        // Parse content as JSON, or use as tool name
                        let input =
                            serde_json::from_str(&content).unwrap_or(serde_json::Value::Null);
                        store_insert.insert_tool_call(ctx, None, after_ref, "unknown", input, None)
                    }
                    "tool_result" => {
                        // For tool_result, we need a tool_call_id. Use after_ref if available.
                        let tool_call_id = after.as_ref();
                        if let Some(tc_id) = tool_call_id {
                            store_insert.insert_tool_result(
                                ctx, tc_id, after_ref, &content, false, None, None,
                            )
                        } else {
                            // Fallback to text if no tool_call_id
                            store_insert.insert_block(
                                ctx,
                                None,
                                after_ref,
                                Role::Tool,
                                BlockKind::Text,
                                &content,
                                Status::Done,
                            )
                        }
                    }
                    _ => store_insert.insert_block(
                        ctx,
                        None,
                        after_ref,
                        Role::User,
                        BlockKind::Text,
                        &content,
                        Status::Done,
                    ),
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

                let Ok(ctx) = ContextId::parse(&cell_id) else {
                    warn!("Rhai: edit_text invalid cell_id: {}", cell_id);
                    return;
                };

                // Parse block_id string to BlockId
                let Some(bid) = kaijutsu_crdt::BlockId::from_key(&block_id) else {
                    warn!("Rhai: edit_text invalid block_id format: {}", block_id);
                    return;
                };

                match store_edit.edit_text(ctx, &bid, pos as usize, &insert, delete as usize) {
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
                let Ok(ctx) = ContextId::parse(&cell_id) else {
                    warn!("Rhai: append_text invalid cell_id: {}", cell_id);
                    return;
                };

                // Parse block_id string to BlockId
                let Some(bid) = kaijutsu_crdt::BlockId::from_key(&block_id) else {
                    warn!("Rhai: append_text invalid block_id format: {}", block_id);
                    return;
                };

                match store_append.append_text(ctx, &bid, &text) {
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
                let Ok(ctx) = ContextId::parse(&cell_id) else {
                    warn!("Rhai: delete_block invalid cell_id: {}", cell_id);
                    return false;
                };

                // Parse block_id string to BlockId
                let Some(bid) = kaijutsu_crdt::BlockId::from_key(&block_id) else {
                    warn!("Rhai: delete_block invalid block_id format: {}", block_id);
                    return false;
                };

                match store_delete.delete_block(ctx, &bid) {
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
            let Ok(ctx) = ContextId::parse(&cell_id) else {
                warn!("Rhai: list_blocks invalid cell_id: {}", cell_id);
                return rhai::Array::new();
            };
            match store_list.get(ctx) {
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
                let Ok(ctx) = ContextId::parse(&cell_id) else {
                    warn!("Rhai: get_block_content invalid cell_id: {}", cell_id);
                    return String::new();
                };

                // Parse block_id string to BlockId
                let Some(target_bid) = kaijutsu_crdt::BlockId::from_key(&block_id) else {
                    return String::new();
                };

                match store_get.get(ctx) {
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

    /// Register context-aware functions that insert blocks into the current context.
    fn register_context_functions(
        engine: &mut Engine,
        block_store: SharedBlockStore,
        context_id: ContextId,
    ) {
        // abc_block(abc_text) -> String
        // Parses ABC notation, engraves to SVG, inserts parent ABC block + child SVG block.
        let abc_store = block_store.clone();
        engine.register_fn("abc_block", move |abc_text: String| -> String {
            // Parse ABC
            let result = kaijutsu_abc::parse(&abc_text);
            if result.has_errors() {
                let errs: Vec<_> = result.errors().map(|e| e.message.clone()).collect();
                warn!("Rhai: abc_block parse errors: {:?}", errs);
                return format!("ABC parse error: {}", errs.join("; "));
            }

            // Engrave to SVG
            let options = kaijutsu_abc::engrave::EngravingOptions::default();
            let svg = kaijutsu_abc::engrave::engrave_to_svg(&result.value, &options);

            // Find last block for ordering
            let last_block_id = abc_store
                .get(context_id)
                .and_then(|doc| doc.doc.blocks_ordered().last().map(|b| b.id));

            // Insert ABC parent block
            let abc_id = match abc_store.insert_block(
                context_id,
                None,
                last_block_id.as_ref(),
                Role::Tool,
                BlockKind::Text,
                &abc_text,
                Status::Done,
            ) {
                Ok(id) => id,
                Err(e) => {
                    warn!("Rhai: abc_block parent insert error: {}", e);
                    return String::new();
                }
            };

            // Set content type on parent
            if let Some(mut entry) = abc_store.get_mut(context_id) {
                let _ = entry
                    .doc
                    .set_content_type(&abc_id, Some("text/vnd.abc".into()));
            }

            // Insert SVG child block (parent = abc_id, after = abc_id)
            match abc_store.insert_block(
                context_id,
                Some(&abc_id),
                Some(&abc_id),
                Role::Tool,
                BlockKind::Text,
                &svg,
                Status::Done,
            ) {
                Ok(svg_id) => {
                    if let Some(mut entry) = abc_store.get_mut(context_id) {
                        let _ = entry
                            .doc
                            .set_content_type(&svg_id, Some("image/svg+xml".into()));
                    }
                    let key = svg_id.to_key();
                    info!(
                        "Rhai: abc_block inserted parent {} + SVG child {} ({} bytes)",
                        abc_id.to_key(),
                        key,
                        svg.len()
                    );
                    key
                }
                Err(e) => {
                    warn!("Rhai: abc_block SVG child insert error: {}", e);
                    // Best-effort cleanup: delete parent
                    let _ = abc_store.delete_block(context_id, &abc_id);
                    String::new()
                }
            }
        });

        // svg_block(svg_content) -> String
        // Inserts SVG content as a Text block at the end of the current context, returns block ID.
        engine.register_fn("svg_block", move |content: String| -> String {
            // Find the last block so we append at the end, not the beginning
            let last_block_id = block_store
                .get(context_id)
                .and_then(|doc| doc.doc.blocks_ordered().last().map(|b| b.id));

            match block_store.insert_block(
                context_id,
                None,
                last_block_id.as_ref(),
                Role::Tool,
                BlockKind::Text,
                &content,
                Status::Done,
            ) {
                Ok(id) => {
                    // Tag the block with SVG content type so the app skips heuristic detection
                    if let Some(mut entry) = block_store.get_mut(context_id) {
                        let _ = entry
                            .doc
                            .set_content_type(&id, Some("image/svg+xml".into()));
                    }
                    let key = id.to_key();
                    info!("Rhai: svg_block inserted {} ({} bytes)", key, content.len());
                    key
                }
                Err(e) => {
                    warn!("Rhai: svg_block error: {}", e);
                    String::new()
                }
            }
        });
    }

    /// Register utility functions.
    fn register_utility_functions(engine: &mut Engine, interrupted: Arc<AtomicBool>) {
        // println(msg: &str) - avoid conflict with Rhai's built-in 'print'
        engine.register_fn("println", |msg: String| {
            info!("[rhai] {}", msg);
        });

        // log(level: &str, msg: &str)
        engine.register_fn("log", |level: String, msg: String| match level.as_str() {
            "debug" => debug!("[rhai] {}", msg),
            "info" => info!("[rhai] {}", msg),
            "warn" => warn!("[rhai] {}", msg),
            "error" => tracing::error!("[rhai] {}", msg),
            _ => info!("[rhai] {}", msg),
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
    ///
    /// Takes the scope out of the map for the duration of execution so we don't
    /// hold the mutex while eval runs. Puts it back afterward regardless of
    /// success/failure.
    fn execute_sync(
        block_store: &SharedBlockStore,
        code: &str,
        interrupted: Arc<AtomicBool>,
        scopes: &Mutex<HashMap<ContextId, Scope<'static>>>,
        context_id: ContextId,
        extra_registrar: Option<&ExtraRegistrar>,
    ) -> ExecResult {
        let (engine, collector) = Self::create_engine(
            block_store.clone(),
            interrupted,
            context_id,
            extra_registrar,
        );

        // Take scope out of the map (or create a new one)
        let mut scope = scopes
            .lock()
            .expect("rhai scope mutex poisoned")
            .remove(&context_id)
            .unwrap_or_default();

        let result = match engine.eval_with_scope::<Dynamic>(&mut scope, code) {
            Ok(result) => {
                let captured_print = collector.take_stdout();

                // Output precedence: SVG > return value
                let output = if let Some(svg) = collector.take_svg() {
                    if !result.is_unit() {
                        warn!("Rhai: svg() set AND non-unit return value — using SVG");
                    }
                    if captured_print.is_empty() {
                        svg
                    } else {
                        format!("{captured_print}{svg}")
                    }
                } else {
                    let val = format!("{}", result);
                    if captured_print.is_empty() {
                        val
                    } else {
                        format!("{captured_print}{val}")
                    }
                };

                debug!("Rhai execution success: {}", output);
                ExecResult::success(output)
            }
            Err(e) => {
                // Still capture any partial output
                let captured_print = collector.take_stdout();
                let svg = collector.take_svg();
                let error_msg = format!("{}", e);
                warn!("Rhai execution error: {}", error_msg);

                let mut result = ExecResult::failure(1, error_msg);
                if !captured_print.is_empty() || svg.is_some() {
                    let partial = match svg {
                        Some(s) => format!("{captured_print}{s}"),
                        None => captured_print,
                    };
                    if !partial.is_empty() {
                        result.stdout = partial;
                    }
                }
                result
            }
        };

        // Put the scope back so the next call in this context picks it up
        scopes
            .lock()
            .expect("rhai scope mutex poisoned")
            .insert(context_id, scope);

        result
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
        "Rhai scripting engine with persistent per-context state. \
         Key functions: svg_block(svg_string) — inserts SVG as a visible block; \
         abc_block(abc_string) — inserts ABC music notation as sheet music. \
         Stdlib: math (sin, cos, lerp, clamp, PI, sqrt, etc.), \
         color (hex, oklch, hsl, rgb, color_mix, color_lighten, hue_shift, etc.), \
         format (xml_escape, fmt_f, to_float, to_int), \
         output (svg(), print()). Variables persist across calls."
    }

    #[tracing::instrument(skip(self, code, ctx), fields(context_id = %ctx.context_id), name = "engine.rhai")]
    async fn execute(&self, code: &str, ctx: &ToolContext) -> anyhow::Result<ExecResult> {
        // Clear interrupt flag before execution
        self.interrupted.store(false, Ordering::SeqCst);

        // The RPC layer passes raw JSON params (e.g. {"code": "..."}).
        // Extract the "code" field if present, otherwise treat input as raw Rhai.
        let code = match serde_json::from_str::<serde_json::Value>(code) {
            Ok(v) => v
                .get("code")
                .and_then(|c| c.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| code.to_string()),
            Err(_) => code.to_string(),
        };

        let block_store = Arc::clone(&self.block_store);
        let interrupted = Arc::clone(&self.interrupted);
        let scopes = Arc::clone(&self.scopes);
        let context_id = ctx.context_id;
        let extra_registrar = self.extra_registrar.clone();

        // Execute in spawn_blocking for async safety
        let result = tokio::task::spawn_blocking(move || {
            Self::execute_sync(
                &block_store,
                &code,
                interrupted,
                &scopes,
                context_id,
                extra_registrar.as_ref(),
            )
        })
        .await?;

        Ok(result)
    }

    async fn is_available(&self) -> bool {
        true
    }

    async fn complete(&self, partial: &str, _cursor: usize) -> Vec<String> {
        // CRDT functions + utility functions + stdlib functions
        let functions = [
            // CRDT cell/block functions
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
            // Context-aware
            "svg_block",
            "abc_block",
            // Utility
            "println",
            "log",
            "is_interrupted",
            "sleep_ms",
            // Stdlib — math
            "sin",
            "cos",
            "tan",
            "asin",
            "acos",
            "atan",
            "atan2",
            "sqrt",
            "abs_f",
            "floor",
            "ceil",
            "round",
            "min_f",
            "max_f",
            "PI",
            "TAU",
            "E",
            "pow",
            "exp",
            "ln",
            "log2",
            "log10",
            "sinh",
            "cosh",
            "tanh",
            "hypot",
            "lerp",
            "clamp",
            "degrees",
            "radians",
            "fract",
            "signum",
            "rem_euclid",
            "copysign",
            // Stdlib — color
            "hex",
            "hexa",
            "rgb",
            "rgba",
            "hsl",
            "hsla",
            "oklch",
            "oklcha",
            "color_mix",
            "color_lighten",
            "color_darken",
            "color_saturate",
            "color_desaturate",
            "hue_shift",
            // Stdlib — format
            "to_float",
            "to_int",
            "xml_escape",
            "fmt_f",
            // Stdlib — output
            "svg",
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

    fn schema(&self) -> Option<serde_json::Value> {
        let catalog = kaijutsu_rhai::function_catalog();
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Rhai script to execute. State (variables, functions) persists across calls within the same context. Use svg_block(svg_string) to insert SVG as a visible block, or abc_block(abc_string) to insert ABC music notation as sheet music.",
                },
            },
            "required": ["code"],
            "additionalProperties": false,
            "context_functions": [
                { "name": "svg_block", "sig": "svg_block(content: string) -> string", "doc": "Insert SVG content as a block in the current conversation. Returns block ID." },
                { "name": "abc_block", "sig": "abc_block(content: string) -> string", "doc": "Insert ABC music notation as a block in the current conversation. Renders to sheet music SVG. Returns block ID." },
            ],
            "stdlib": catalog["functions"],
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::shared_block_store;
    use kaijutsu_types::PrincipalId;

    #[tokio::test]
    async fn test_basic_execution() {
        let store = shared_block_store(PrincipalId::new());
        let engine = RhaiEngine::new(store);

        let result = engine
            .execute("40 + 2", &ToolContext::test())
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.stdout, "42");
    }

    #[tokio::test]
    async fn test_string_result() {
        let store = shared_block_store(PrincipalId::new());
        let engine = RhaiEngine::new(store);

        let result = engine
            .execute(r#""hello" + " " + "world""#, &ToolContext::test())
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.stdout, "hello world");
    }

    #[tokio::test]
    async fn test_create_cell() {
        let store = shared_block_store(PrincipalId::new());
        let engine = RhaiEngine::new(store.clone());

        let result = engine
            .execute(r#"create_cell("markdown")"#, &ToolContext::test())
            .await
            .unwrap();
        assert!(result.success);
        assert!(!result.stdout.is_empty());

        // Verify cell exists
        let ids = store.list_ids();
        assert_eq!(ids.len(), 1);
    }

    #[tokio::test]
    async fn test_cell_content_roundtrip() {
        let store = shared_block_store(PrincipalId::new());
        let engine = RhaiEngine::new(store.clone());

        let result = engine
            .execute(
                r#"
            let id = create_cell("code");
            set_content(id, "fn main() {}");
            get_content(id)
        "#,
                &ToolContext::test(),
            )
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.stdout, "fn main() {}");
    }

    #[tokio::test]
    async fn test_block_operations() {
        let store = shared_block_store(PrincipalId::new());
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
                &ToolContext::test(),
            )
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.stdout, "2");
    }

    #[tokio::test]
    async fn test_edit_text() {
        let store = shared_block_store(PrincipalId::new());
        let engine = RhaiEngine::new(store.clone());

        let result = engine
            .execute(
                r#"
            let cell = create_cell("code");
            let block = insert_block(cell, "", "text", "Hello World");
            edit_text(cell, block, 6, "Rhai", 5);
            get_block_content(cell, block)
        "#,
                &ToolContext::test(),
            )
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.stdout, "Hello Rhai");
    }

    #[tokio::test]
    async fn test_delete_block() {
        let store = shared_block_store(PrincipalId::new());
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
                &ToolContext::test(),
            )
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.stdout, "1");
    }

    #[tokio::test]
    async fn test_execution_error() {
        let store = shared_block_store(PrincipalId::new());
        let engine = RhaiEngine::new(store);

        let result = engine
            .execute("undefined_function()", &ToolContext::test())
            .await
            .unwrap();
        assert!(!result.success);
        assert!(!result.stderr.is_empty());
    }

    #[tokio::test]
    async fn test_interrupt() {
        let store = shared_block_store(PrincipalId::new());
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
                &ToolContext::test(),
            )
            .await
            .unwrap();

        // Note: interrupt is cleared at start of execute, so this should run
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_safety_limits() {
        let store = shared_block_store(PrincipalId::new());
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
                &ToolContext::test(),
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
        let store = shared_block_store(PrincipalId::new());
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
        let store = shared_block_store(PrincipalId::new());
        let engine = RhaiEngine::new(store);
        let debug_str = format!("{:?}", engine);
        assert!(debug_str.contains("RhaiEngine"));
    }

    #[tokio::test]
    async fn test_scope_persists_across_calls() {
        let store = shared_block_store(PrincipalId::new());
        let engine = RhaiEngine::new(store);

        // Use a fixed context so both calls share the same scope
        let ctx = ToolContext::test();

        // First call: define a variable
        let r1 = engine.execute("let x = 42;", &ctx).await.unwrap();
        assert!(r1.success, "define failed: {}", r1.stderr);

        // Second call: read it back
        let r2 = engine.execute("x", &ctx).await.unwrap();
        assert!(r2.success, "read failed: {}", r2.stderr);
        assert_eq!(r2.stdout, "42");
    }

    #[tokio::test]
    async fn test_scope_isolated_between_contexts() {
        let store = shared_block_store(PrincipalId::new());
        let engine = RhaiEngine::new(store);

        let ctx_a = ToolContext::test(); // unique ContextId
        let ctx_b = ToolContext::test(); // different ContextId

        // Define variable in context A
        let r1 = engine.execute("let secret = 99;", &ctx_a).await.unwrap();
        assert!(r1.success);

        // Context B should NOT see it
        let r2 = engine.execute("secret", &ctx_b).await.unwrap();
        assert!(!r2.success, "context B should not see context A's variable");
    }

    #[tokio::test]
    async fn test_schema_includes_code_property() {
        let store = shared_block_store(PrincipalId::new());
        let engine = RhaiEngine::new(store);

        let schema = engine.schema().expect("schema should be Some");
        let props = schema["properties"]
            .as_object()
            .expect("properties should be object");
        assert!(props.contains_key("code"), "schema missing 'code' property");
        assert!(
            schema["stdlib"].is_array(),
            "schema should include stdlib catalog"
        );
        assert!(
            schema["context_functions"].is_array(),
            "schema should include context_functions"
        );
    }

    #[tokio::test]
    async fn test_svg_block_inserts_into_context() {
        let store = shared_block_store(PrincipalId::new());
        let engine = RhaiEngine::new(store.clone());
        let ctx = ToolContext::test();
        let context_id = ctx.context_id;

        // Create the document first so insert_block has a target
        store
            .create_document(context_id, DocKind::Conversation, None)
            .unwrap();

        let result = engine
            .execute(r#"svg_block("<svg><circle r='10'/></svg>")"#, &ctx)
            .await
            .unwrap();

        assert!(result.success, "svg_block failed: {}", result.stderr);
        // Return value is the block ID key
        assert!(!result.stdout.is_empty(), "should return block ID");

        // Verify the block actually landed in the document
        let doc = store.get(context_id).expect("document should exist");
        let blocks = doc.doc.blocks_ordered();
        assert_eq!(blocks.len(), 1, "should have exactly one block");
        assert!(
            blocks[0].content.contains("<svg>"),
            "block content should be SVG"
        );
    }

    #[tokio::test]
    async fn test_json_wrapped_code_extraction() {
        // This is what the RPC layer actually sends: JSON with a "code" field
        let store = shared_block_store(PrincipalId::new());
        let engine = RhaiEngine::new(store);

        let json_input = r#"{"code": "40 + 2"}"#;
        let result = engine
            .execute(json_input, &ToolContext::test())
            .await
            .unwrap();
        assert!(
            result.success,
            "JSON-wrapped code failed: {}",
            result.stderr
        );
        assert_eq!(result.stdout, "42");
    }
}
