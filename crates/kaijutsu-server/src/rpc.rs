//! Cap'n Proto RPC server implementation.
//!
//! One shared kernel is created at server startup ([`create_shared_kernel`]),
//! shared across all SSH connections via `Arc`. Per-connection state
//! (principal, kaish, command history) lives in [`ConnectionState`]. The
//! primary RPC surface is [`KernelImpl`], which implements the capnp
//! `kernel::Server` trait.
//!
//! # Why this file is large
//!
//! This file is intentionally kept as a single module rather than decomposed
//! into a `rpc/` directory of per-domain submodules. The `kernel::Server`
//! trait impl is ~6k lines on its own and must be contiguous — a capnp
//! constraint — so a mechanical split would produce one thin file of
//! delegating trait methods plus one-file-per-subject of inherent `impl`
//! blocks, doubling surface area without gaining real modularity. File-size
//! ergonomics matter less in an AI-assisted workflow where grep, LSP, and
//! the editor's outline view handle navigation. Parallel work resolves
//! cleanly per-hunk: moving methods into separate files doesn't reduce
//! merge conflict surface, it just reshuffles it.
//!
//! If a specific chunk of code grows its own identity (the LLM agentic
//! loop is the obvious example — see [`crate::llm_stream`]), extract it.
//! Don't decompose the file just to have smaller files.
//!
//! # Navigation
//!
//! Section banners use `// ========` and are grep-able with `rg '^// ='`.
//! Top-level sections, in order:
//!
//! - **Server State** — [`ConnectionState`], [`ConversationCache`],
//!   [`SharedKernelState`], execution tracking helpers.
//! - **Execute Output Dispatch** — background fan-out of `execute()`
//!   output events to subscribers.
//! - **Semantic Index Integration** — `BlockSource` adapter for kaijutsu-index.
//! - **Shared Kernel Creation** — [`create_shared_kernel`] (server startup).
//! - **World Implementation** — [`WorldImpl`] (top-level capability).
//! - **Kernel Implementation** — [`KernelImpl`], the main trait impl. Its
//!   internal subsections group ~80 RPC methods: lifecycle/info, shell
//!   execution (`execute`/`interrupt`/`complete`), VFS, tools, block CRDT
//!   ops, prompt/LLM, context ops, MCP, shell execution (kaish), shell
//!   state, blob placeholders, MCP resources, agent attachment, timeline
//!   navigation, config, drift, LLM configuration, tool filter
//!   configuration, per-context tool filter, shell variable introspection,
//!   input document operations, context interrupt.
//! - **Shell Value Conversion Helpers** — kaish `Value` ↔ Cap'n Proto.
//! - **OutputData Build Helpers** — structured command output builders.
//! - **Agent Helper Functions** — `AgentInfo` / `AgentCapability` converters.
//! - **Cap'n Proto ↔ Rust Type Helpers** — tool filter (de)serialization.
//! - **Shell Execution Dispatch** — [`execute_shell_command`], shared by
//!   the `shell_execute` RPC and `submit_input`.
//! - **Utility Functions** — block ID / filter / status converters.
//! - **VFS Implementation** — [`VfsImpl`] (filesystem capability).
//! - **Synthesis** — Rhai-driven keyword extraction.
//!
//! LLM streaming (`process_llm_stream` and its agentic loop) lives in
//! [`crate::llm_stream`], not in this file.

#![allow(refining_impl_trait)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::RwLock as TokioRwLock;
use tokio_util::sync::CancellationToken;
// tokio::sync::Mutex used inside ConversationCache for per-context locking

use capnp::capability::Promise;
use capnp_rpc::pry;

use crate::embedded_kaish::EmbeddedKaish;
use crate::interrupt::ContextInterruptState;
use crate::kaijutsu_capnp::*;
use crate::llm_stream::spawn_llm_for_prompt;

use kaijutsu_crdt::{BlockKind, ContentType, Role, Status};
use kaijutsu_kernel::kernel_db::{ContextRow, KernelDb};
use kaijutsu_kernel::{
    AgentActivityEvent,
    // Agents
    AgentCapability,
    AgentConfig,
    AgentInfo,
    AgentStatus,
    // FlowBus
    BlockFlow,
    // Config
    ConfigCrdtBackend,
    ConfigWatcherHandle,
    // File tools (workspace guard helper type only — engines go through broker)
    FileDocumentCache,
    InputDocFlow,
    InvokeRequest,
    InvokeResponse,
    Kernel,
    LlmMessage,
    LocalBackend,
    SharedBlockFlowBus,
    SharedBlockStore,
    SharedConfigFlowBus,
    SharedInputDocFlowBus,
    // Tool filtering (wire type, binding translation)
    ToolFilter,
    VfsOps,
    block_store::BlockStore,
    shared_block_flow_bus,
    shared_config_flow_bus,
    shared_input_doc_flow_bus,
};
use kaijutsu_types::{ContextId, KernelId, Principal, PrincipalId, SessionId};
// Alias to avoid conflict with kaijutsu_capnp::ToolKind (glob-imported)
use kaijutsu_types::ToolKind as TypesToolKind;
use serde_json;
use tracing::Instrument;

/// Extract W3C Trace Context from a Cap'n Proto `TraceContext` reader.
///
/// Returns a tracing span linked to the remote parent (or a root span if empty).
/// Safe to call even when trace is not present — returns a detached span.
fn extract_rpc_trace(
    trace: capnp::Result<trace_context::Reader<'_>>,
    name: &'static str,
) -> tracing::Span {
    let (traceparent, tracestate) = match trace {
        Ok(t) => {
            let tp = t
                .get_traceparent()
                .ok()
                .and_then(|r| r.to_str().ok())
                .unwrap_or("");
            let ts = t
                .get_tracestate()
                .ok()
                .and_then(|r| r.to_str().ok())
                .unwrap_or("");
            (tp.to_string(), ts.to_string())
        }
        Err(_) => (String::new(), String::new()),
    };
    let span = kaijutsu_telemetry::extract_trace_context(&traceparent, &tracestate);
    // Override the default "rpc.request" name with the actual method name
    let named_span = tracing::info_span!(parent: &span, "rpc", method = name);
    named_span
}

// ============================================================================
// Server State
// ============================================================================

/// Per-context LLM conversation cache with per-context locking and LRU eviction.
///
/// Each context gets its own `tokio::sync::Mutex<Vec<LlmMessage>>`, so concurrent
/// prompts to the same context serialize properly. DashMap provides the outer
/// concurrent access. LRU eviction keeps memory bounded.
pub struct ConversationCache {
    entries: dashmap::DashMap<ContextId, Arc<tokio::sync::Mutex<Vec<LlmMessage>>>>,
    last_accessed: dashmap::DashMap<ContextId, std::time::Instant>,
    max_contexts: usize,
}

impl ConversationCache {
    /// Create a new cache with the given capacity.
    pub fn new(max_contexts: usize) -> Self {
        Self {
            entries: dashmap::DashMap::new(),
            last_accessed: dashmap::DashMap::new(),
            max_contexts,
        }
    }

    /// Get or create the per-context lock. Returns an Arc<Mutex> that the caller
    /// holds for the entire `process_llm_stream` — serializing concurrent prompts.
    pub fn get_or_create(&self, ctx: ContextId) -> Arc<tokio::sync::Mutex<Vec<LlmMessage>>> {
        self.last_accessed.insert(ctx, std::time::Instant::now());

        if let Some(entry) = self.entries.get(&ctx) {
            return entry.clone();
        }

        // Evict LRU if at capacity (skip entries with strong_count > 1, they're in active use)
        if self.entries.len() >= self.max_contexts {
            let mut oldest: Option<(ContextId, std::time::Instant)> = None;
            for entry in self.last_accessed.iter() {
                let ctx_id = *entry.key();
                let accessed = *entry.value();
                // Skip entries in active use
                if let Some(e) = self.entries.get(&ctx_id)
                    && Arc::strong_count(&e) > 1
                {
                    continue;
                }
                if oldest.is_none() || accessed < oldest.unwrap().1 {
                    oldest = Some((ctx_id, accessed));
                }
            }
            if let Some((evict_id, _)) = oldest {
                self.entries.remove(&evict_id);
                self.last_accessed.remove(&evict_id);
            }
        }

        let lock = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        self.entries.insert(ctx, lock.clone());
        lock
    }
}

/// Kernel state shared across all connections via Arc.
/// Created once at server startup.
pub struct SharedKernelState {
    pub id: KernelId,
    pub name: String,
    pub kernel: Arc<Kernel>,
    pub documents: SharedBlockStore,
    pub config_backend: Arc<ConfigCrdtBackend>,
    pub config_watcher: Option<ConfigWatcherHandle>,
    pub conversation_cache: Arc<ConversationCache>,
    /// SQLite persistence for context metadata, edges, presets, workspaces.
    /// Arc<parking_lot::Mutex> (not tokio) — shared with KjDispatcher, all ops sync and sub-ms.
    pub kernel_db: Arc<parking_lot::Mutex<KernelDb>>,
    /// Semantic vector index for context search/clustering.
    /// None if embedding model not configured or unavailable.
    pub semantic_index: Option<Arc<kaijutsu_index::SemanticIndex>>,
    /// Per-context interrupt state. Created fresh at the start of each
    /// `process_llm_stream` call; looked up by `interruptContext` RPC.
    pub context_interrupts: Arc<TokioRwLock<HashMap<ContextId, Arc<ContextInterruptState>>>>,
    /// Monotonically increasing generation counter for interrupt state.
    /// Prevents race where stream A's cleanup removes stream B's interrupt.
    pub interrupt_generation: AtomicU64,
    /// kj command dispatcher — shared across all connections.
    pub kj_dispatcher: Arc<kaijutsu_kernel::KjDispatcher>,
    /// Per-session current-context tracking for the `context` shell command.
    pub session_contexts: crate::context_engine::SessionContextMap,
}

pub type SharedKernel = Arc<SharedKernelState>;

impl SharedKernelState {
    /// Create a fresh `ContextInterruptState` for a new prompt, replacing any previous entry.
    ///
    /// `CancellationToken` cannot be reset — so each prompt gets a new one.
    /// Returns the interrupt state and its generation number. The generation
    /// must be passed to `remove_interrupt` to prevent the race where stream A's
    /// cleanup removes stream B's newer interrupt.
    pub async fn create_interrupt(
        &self,
        context_id: ContextId,
    ) -> (Arc<ContextInterruptState>, u64) {
        let generation = self.interrupt_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let state = ContextInterruptState::new(generation);
        let mut map = self.context_interrupts.write().await;
        map.insert(context_id, state.clone());
        (state, generation)
    }

    /// Look up an existing interrupt state for a context.
    ///
    /// Returns `None` if the context has no active interrupt (nothing running).
    pub async fn get_interrupt(&self, context_id: ContextId) -> Option<Arc<ContextInterruptState>> {
        let map = self.context_interrupts.read().await;
        map.get(&context_id).cloned()
    }

    /// Remove the interrupt state for a context (called when stream finishes).
    ///
    /// Only removes the entry if `generation` matches the current state's
    /// generation, preventing a stale stream from removing a newer stream's
    /// interrupt state.
    pub async fn remove_interrupt(&self, context_id: ContextId, generation: u64) {
        let mut map = self.context_interrupts.write().await;
        if let Some(state) = map.get(&context_id)
            && state.generation == generation
        {
            map.remove(&context_id);
        }
    }
}

/// Server-wide state. Shared via Arc across all SSH connections.
pub struct ServerRegistry {
    pub kernel: SharedKernel,
}

/// A background execution tracked by exec_id.
struct RunningExecution {
    cancel: CancellationToken,
}

/// Per-connection state. Lives in each connection's LocalSet.
pub struct ConnectionState {
    pub principal: Principal,
    pub session_id: SessionId,
    /// Global session map for context tracking.
    pub session_contexts: crate::context_engine::SessionContextMap,
    pub kaish: Option<Rc<EmbeddedKaish>>,
    pub command_history: Vec<CommandEntry>,
    next_exec_id: AtomicU64,
    /// Currently running executions, keyed by exec_id.
    running_executions: HashMap<u64, RunningExecution>,
    /// Output subscribers registered via subscribe_output().
    output_subscribers: Vec<kernel_output::Client>,
}

impl ConnectionState {
    pub fn new(
        principal: Principal,
        session_contexts: crate::context_engine::SessionContextMap,
    ) -> Self {
        Self {
            principal,
            session_id: SessionId::new(),
            session_contexts,
            kaish: None,
            command_history: Vec::new(),
            next_exec_id: AtomicU64::new(1),
            running_executions: HashMap::new(),
            output_subscribers: Vec::new(),
        }
    }

    /// Get the connection's active context, or error if none joined.
    pub fn require_context(&self) -> Result<ContextId, capnp::Error> {
        self.session_contexts
            .get(&self.session_id)
            .map(|r| *r)
            .ok_or_else(|| {
                capnp::Error::failed("no context joined — call joinContext first".into())
            })
    }

    fn next_exec_id(&self) -> u64 {
        self.next_exec_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Register a new execution, returning the cancellation token.
    fn register_execution(&mut self, exec_id: u64) -> CancellationToken {
        let token = CancellationToken::new();
        self.running_executions.insert(
            exec_id,
            RunningExecution {
                cancel: token.clone(),
            },
        );
        token
    }

    /// Remove a completed execution from tracking.
    fn complete_execution(&mut self, exec_id: u64) {
        self.running_executions.remove(&exec_id);
    }

    /// Check if any execution is currently in-flight.
    fn has_running_execution(&self) -> bool {
        !self.running_executions.is_empty()
    }

    /// Register an output subscriber callback.
    fn add_output_subscriber(&mut self, client: kernel_output::Client) {
        self.output_subscribers.push(client);
    }
}

/// Get the stable data directory for kernel persistent storage.
/// Creates the directory if it doesn't exist.
/// Returns: ~/.local/share/kaijutsu/kernel/
fn kernel_data_dir() -> std::path::PathBuf {
    let dir = kaish_kernel::xdg_data_home()
        .join("kaijutsu")
        .join("kernel");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("Failed to create kernel data dir {:?}: {}", dir, e);
    }

    // Log a migration hint if old per-kernel directories exist
    let old_dir = kaish_kernel::xdg_data_home()
        .join("kaijutsu")
        .join("kernels");
    if old_dir.is_dir()
        && let Ok(entries) = std::fs::read_dir(&old_dir)
    {
        let count = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .count();
        if count > 0 {
            log::warn!(
                "Found {} old kernel data dir(s) in {:?}. \
                     To recover, copy the most recent data.db to {:?}",
                count,
                old_dir,
                dir
            );
        }
    }

    dir
}

/// Create a BlockStore backed by the shared KernelDb.
fn create_block_store_with_kernel_db(
    db: Arc<parking_lot::Mutex<KernelDb>>,
    kernel_id: KernelId,
    default_workspace_id: kaijutsu_types::WorkspaceId,
    agent_id: PrincipalId,
    block_flows: SharedBlockFlowBus,
    input_flows: SharedInputDocFlowBus,
) -> Result<SharedBlockStore, String> {
    let mut inner =
        BlockStore::with_db_and_flows(db, kernel_id, default_workspace_id, agent_id, block_flows);
    inner.set_input_flows(input_flows);
    let store = Arc::new(inner);
    store.load_from_db().map_err(|e| {
        format!("Failed to load documents from DB (refusing to start with empty store): {e}")
    })?;
    log::info!("Loaded {} documents from database", store.len());
    if let Err(e) = store.load_input_docs_from_db() {
        log::warn!("Failed to load input docs from DB: {}", e);
    }
    Ok(store)
}

/// Get the config directory path.
/// Returns: ~/.config/kaijutsu/
fn config_dir() -> std::path::PathBuf {
    kaish_kernel::xdg_config_home().join("kaijutsu")
}

/// Create and initialize the config CRDT backend.
///
/// This loads config files into CRDT documents and starts the file watcher.
/// Returns the backend and optional watcher handle.
async fn create_config_backend(
    documents: SharedBlockStore,
    config_flows: SharedConfigFlowBus,
    config_path_override: Option<&Path>,
) -> (Arc<ConfigCrdtBackend>, Option<ConfigWatcherHandle>) {
    let config_path = config_path_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(config_dir);

    let backend = Arc::new(ConfigCrdtBackend::with_flows(
        documents,
        config_path,
        config_flows,
    ));

    // Load base theme config
    if let Err(e) = backend.ensure_config("theme.toml").await {
        log::warn!("Failed to load theme.toml: {}", e);
    }

    // Start the file watcher
    let watcher = match backend.start_watcher() {
        Ok(handle) => {
            log::info!("Config file watcher started");
            Some(handle)
        }
        Err(e) => {
            log::warn!("Failed to start config watcher: {}", e);
            None
        }
    };

    (backend, watcher)
}

/// Initialize a kernel's LLM registry from its config backend.
///
/// Loads `models.toml` from the config CRDT, parses it, and populates
/// the kernel's `LlmRegistry` with providers and aliases. Returns the
/// embedding config if present (for semantic index initialization).
async fn initialize_kernel_models(
    kernel: &Arc<Kernel>,
    config_backend: &Arc<ConfigCrdtBackend>,
) -> Option<kaijutsu_kernel::EmbeddingModelConfig> {
    if let Err(e) = config_backend.ensure_config("models.toml").await {
        log::warn!("Failed to load models.toml: {}", e);
        return None;
    }

    let content = match config_backend.get_content("models.toml") {
        Ok(c) => c,
        Err(e) => {
            log::warn!("Failed to read models.toml: {}", e);
            return None;
        }
    };

    match kaijutsu_kernel::load_models_config_toml(&content) {
        Ok(models_config) => match kaijutsu_kernel::initialize_llm_registry(&models_config.llm) {
            Ok(registry) => {
                *kernel.llm().write().await = registry;
                log::info!("Initialized kernel LLM registry from models.toml");
                models_config.embedding
            }
            Err(e) => {
                log::error!("Failed to initialize LLM registry: {}", e);
                None
            }
        },
        Err(e) => {
            log::warn!("Failed to parse models.toml: {}, reloading from disk", e);
            if let Err(reload_err) = config_backend.reload_from_disk("models.toml").await {
                log::error!("Failed to reload models.toml from disk: {}", reload_err);
                return None;
            }
            let content = match config_backend.get_content("models.toml") {
                Ok(c) => c,
                Err(e) => {
                    log::error!("Failed to read reloaded models.toml: {}", e);
                    return None;
                }
            };
            match kaijutsu_kernel::load_models_config_toml(&content) {
                Ok(models_config) => {
                    match kaijutsu_kernel::initialize_llm_registry(&models_config.llm) {
                        Ok(registry) => {
                            *kernel.llm().write().await = registry;
                            log::info!(
                                "Initialized kernel LLM registry from models.toml (reloaded)"
                            );
                            models_config.embedding
                        }
                        Err(e) => {
                            log::error!("Failed to initialize LLM registry after reload: {}", e);
                            None
                        }
                    }
                }
                Err(e) => {
                    log::error!("Failed to parse models.toml even after reload: {}", e);
                    None
                }
            }
        }
    }
}


#[derive(Clone)]
pub struct CommandEntry {
    pub id: u64,
    pub code: String,
    pub timestamp: u64,
}

// ============================================================================
// Execute Output Dispatch
// ============================================================================

/// Dispatch stdout/stderr/exitCode events to all registered output subscribers.
///
/// Sends up to 3 `KernelOutputEvent` messages per execution:
/// - stdout (if non-empty)
/// - stderr (if non-empty)
/// - exitCode (always — signals completion)
///
/// Removes subscribers whose RPC calls fail (capability revoked / client disconnected).
async fn dispatch_output_events(
    exec_id: u64,
    result: &kaish_kernel::interpreter::ExecResult,
    connection: &Rc<RefCell<ConnectionState>>,
) {
    // Clone subscribers out to avoid holding RefCell borrow across await points.
    let subscribers: Vec<kernel_output::Client> =
        { connection.borrow().output_subscribers.clone() };

    if subscribers.is_empty() {
        return;
    }

    let mut failed_indices = Vec::new();

    for (i, subscriber) in subscribers.iter().enumerate() {
        let mut ok = true;

        // stdout
        let stdout = result.text_out();
        if ok && !stdout.is_empty() {
            let mut req = subscriber.on_output_request();
            {
                let mut event = req.get().init_event();
                event.set_exec_id(exec_id);
                event.init_event().set_stdout(&stdout);
            }
            ok = req.send().promise.await.is_ok();
        }

        // stderr
        if ok && !result.err.is_empty() {
            let mut req = subscriber.on_output_request();
            {
                let mut event = req.get().init_event();
                event.set_exec_id(exec_id);
                event.init_event().set_stderr(&result.err);
            }
            ok = req.send().promise.await.is_ok();
        }

        // exitCode (always — signals completion)
        if ok {
            let mut req = subscriber.on_output_request();
            {
                let mut event = req.get().init_event();
                event.set_exec_id(exec_id);
                event.init_event().set_exit_code(result.code as i32);
            }
            if req.send().promise.await.is_err() {
                ok = false;
            }
        }

        if !ok {
            failed_indices.push(i);
        }
    }

    // Remove failed subscribers (iterate in reverse to preserve indices).
    if !failed_indices.is_empty() {
        let mut conn = connection.borrow_mut();
        for &i in failed_indices.iter().rev() {
            if i < conn.output_subscribers.len() {
                conn.output_subscribers.swap_remove(i);
            }
        }
    }
}

// ============================================================================
// Semantic Index Integration
// ============================================================================

/// Adapter: SharedBlockStore → kaijutsu_index::BlockSource
pub(crate) struct BlockStoreSource(pub(crate) SharedBlockStore);

impl kaijutsu_index::BlockSource for BlockStoreSource {
    fn block_snapshots(
        &self,
        ctx: ContextId,
    ) -> Result<Vec<kaijutsu_types::BlockSnapshot>, String> {
        // Try in-memory first; if missing, hydrate from DB on demand.
        if !self.0.contains(ctx) {
            let _ = self.0.load_one_from_db(ctx);
        }
        BlockStore::block_snapshots(&self.0, ctx).map_err(|e| e.to_string())
    }
}

/// Adapter: FlowBus<BlockFlow> subscription → kaijutsu_index::StatusReceiver
struct FlowBusStatusReceiver {
    sub: kaijutsu_kernel::flows::Subscription<BlockFlow>,
}

impl kaijutsu_index::StatusReceiver for FlowBusStatusReceiver {
    fn recv(
        &mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<kaijutsu_index::StatusEvent>> + Send + '_>,
    > {
        Box::pin(async {
            loop {
                let msg = self.sub.recv().await?;
                if let BlockFlow::StatusChanged {
                    context_id, status, ..
                } = msg.payload
                    && status.is_terminal()
                {
                    return Some(kaijutsu_index::StatusEvent { context_id, status });
                }
            }
        })
    }
}

// ============================================================================
// Shared Kernel Creation
// ============================================================================

/// Create the shared kernel at server startup.
///
/// This performs all kernel initialization: VFS mounts, block store with DB
/// persistence, default context, config backend, block tools, LLM, and MCP.
/// The returned `SharedKernel` is shared across all connections via `Arc`.
pub async fn create_shared_kernel(
    config_dir: Option<&Path>,
    data_dir: Option<&Path>,
) -> Result<SharedKernel, capnp::Error> {
    // Create shared FlowBus instances - shared between Kernel and BlockStore
    let block_flows = shared_block_flow_bus(1024);
    let config_flows = shared_config_flow_bus(256);
    let input_flows = shared_input_doc_flow_bus(256);

    // Resolve stable data directory (used for block store DB, kernel DB, semantic index)
    let resolved_data_dir = match data_dir {
        Some(dir) => {
            if let Err(e) = std::fs::create_dir_all(dir) {
                log::warn!("Failed to create data dir {:?}: {}", dir, e);
            }
            dir.to_path_buf()
        }
        None => kernel_data_dir(),
    };

    // Open KernelDb first — it owns the stable kernel ID
    let kernel_db = {
        let db_path = resolved_data_dir.join("kernel.db");
        match KernelDb::open(&db_path) {
            Ok(db) => {
                log::info!("Opened KernelDb at {}", db_path.display());
                db
            }
            Err(e) => {
                log::error!("Failed to open KernelDb at {}: {}", db_path.display(), e);
                KernelDb::in_memory().expect("in-memory KernelDb should never fail")
            }
        }
    };

    // Get stable kernel ID (persisted across restarts so context rows stay joinable)
    let id = match kernel_db.get_or_create_kernel_id() {
        Ok(kid) => {
            log::info!("Kernel ID: {} (from kernel table)", kid.to_hex());
            kid
        }
        Err(e) => {
            log::error!("Failed to get/create kernel ID: {}, using ephemeral", e);
            KernelId::new()
        }
    };
    let id_str = id.to_hex();

    // Create the kaijutsu kernel with shared FlowBus
    let kernel = Kernel::with_flows(&id_str, block_flows.clone(), Some(&resolved_data_dir)).await;

    // Read-only root — whole system visible (ls /usr/bin, cargo, etc.)
    kernel.mount("/", LocalBackend::read_only("/")).await;

    // Read-write ~/src (longest-prefix wins over /)
    let home = kaish_kernel::home_dir();
    let src_dir = home.join("src");
    kernel
        .mount(
            &format!("{}", src_dir.display()),
            LocalBackend::new(&src_dir),
        )
        .await;

    // Read-write /tmp for scratch/interop with external tools
    kernel.mount("/tmp", LocalBackend::new("/tmp")).await;

    // Freeze the mount table — security perimeter is now fixed.
    // No more mount/unmount via RPC after this point.
    kernel.freeze_mounts();

    // Wrap KernelDb in Arc<Mutex> and create auto-workspaces
    let kernel_db_arc = Arc::new(parking_lot::Mutex::new(kernel_db));
    let default_ws_id = {
        let db = kernel_db_arc.lock();
        db.get_or_create_default_workspace(id, PrincipalId::system())
            .unwrap()
    };

    // Create block store backed by unified KernelDb
    let block_flows_for_index = block_flows.clone();
    let documents = create_block_store_with_kernel_db(
        kernel_db_arc.clone(),
        id,
        default_ws_id,
        PrincipalId::system(),
        block_flows,
        input_flows,
    )
    .map_err(capnp::Error::failed)?;

    // Create config backend
    let (config_backend, config_watcher) =
        create_config_backend(documents.clone(), config_flows, config_dir).await;

    let kernel_arc = Arc::new(kernel);
    let workspace_guard = Some(kaijutsu_kernel::file_tools::WorkspaceGuard::new(
        kernel_db_arc.clone(),
    ));
    let session_contexts = crate::context_engine::session_context_map();

    // Phase 1 M4: register virtual MCP servers on the broker so
    // dispatch_tool_via_broker has something to route to. This runs
    // alongside the legacy registry until M5 deletes it.
    let file_cache_for_broker = Arc::new(kaijutsu_kernel::file_tools::FileDocumentCache::new(
        documents.clone(),
        kernel_arc.vfs().clone(),
    ));
    if let Err(e) = kernel_arc
        .register_builtin_mcp_servers(
            documents.clone(),
            file_cache_for_broker,
            workspace_guard,
        )
        .await
    {
        return Err(capnp::Error::failed(format!(
            "Failed to register builtin MCP servers on broker: {}",
            e
        )));
    }

    // Recover contexts: KernelDb is the primary source, with BlockStore discovery as fallback.
    // A failure to read the DB here means we cannot know which contexts should be recovered —
    // refuse to start rather than silently coming up with zero contexts.
    let all_contexts: Vec<ContextRow> = {
        let db = kernel_db_arc.lock();
        // Step 1: Load active contexts from KernelDb
        let db_contexts = db.list_active_contexts(id).map_err(|e| {
            capnp::Error::failed(format!("Failed to load contexts from KernelDb: {}", e))
        })?;
        let db_ids: std::collections::HashSet<ContextId> =
            db_contexts.iter().map(|r| r.context_id).collect();

        // Step 2: Discover Conversation documents not in KernelDb → bootstrap minimal rows.
        // Only conversations are contexts — code/config/text docs are internal.
        let block_store_ids: Vec<ContextId> =
            documents.list_ids_by_kind(kaijutsu_types::DocKind::Conversation);
        for &bs_ctx_id in &block_store_ids {
            if !db_ids.contains(&bs_ctx_id) {
                let row = ContextRow {
                    context_id: bs_ctx_id,
                    kernel_id: id,
                    label: None,
                    provider: None,
                    model: None,
                    system_prompt: None,
                    tool_filter: None,
                    consent_mode: kaijutsu_kernel::control::ConsentMode::Collaborative,
                    context_state: kaijutsu_types::ContextState::Live,
                    created_at: kaijutsu_types::now_millis() as i64,
                    created_by: PrincipalId::system(),
                    forked_from: None,
                    fork_kind: None,
                    archived_at: None,
                    workspace_id: None,
                    preset_id: None,
                };
                let default_ws = db
                    .get_or_create_default_workspace(row.kernel_id, row.created_by)
                    .unwrap_or_else(|_| kaijutsu_types::WorkspaceId::new());
                if let Err(e) = db.insert_context_with_document(&row, default_ws) {
                    log::warn!(
                        "Failed to bootstrap context {} into KernelDb: {}",
                        bs_ctx_id.short(),
                        e
                    );
                } else {
                    log::info!(
                        "Bootstrapped context {} into KernelDb from BlockStore",
                        bs_ctx_id.short()
                    );
                }
            }
        }

        // Step 3: Re-read all active contexts (lock dropped after this block)
        db.list_active_contexts(id).map_err(|e| {
            capnp::Error::failed(format!("Failed to re-read contexts from KernelDb: {}", e))
        })?
    }; // db lock dropped here — safe to await below

    // Register recovered contexts into DriftRouter
    if !all_contexts.is_empty() {
        let mut drift = kernel_arc.drift().write().await;
        for row in &all_contexts {
            if let Err(e) = drift.register(
                row.context_id,
                row.label.as_deref(),
                row.forked_from,
                row.created_by,
            ) {
                log::warn!(
                    "Skipping context {} recovery: {}",
                    row.context_id.short(),
                    e
                );
                continue;
            }
            if let (Some(provider), Some(model)) = (&row.provider, &row.model) {
                let _ = drift.configure_llm(row.context_id, provider, model);
            }
            if row.tool_filter.is_some() {
                let _ = drift.configure_tools(row.context_id, row.tool_filter.clone());
            }
            log::info!(
                "Recovered context {} (label={:?}, provider={:?}) from KernelDb",
                row.context_id.short(),
                row.label,
                row.provider,
            );
        }
    }

    // Initialize LLM registry + embedding config from models.toml
    let embedding_config = initialize_kernel_models(&kernel_arc, &config_backend).await;

    // External MCP admin (register_mcp / list_mcp / etc.) is offline
    // until Phase 2 wires it onto the broker.

    // Initialize semantic index if embedding model is configured
    let semantic_index = if let Some(emb_config) = embedding_config {
        let index_config = kaijutsu_index::IndexConfig::new(
            emb_config.model_dir.clone(),
            emb_config.dimensions,
            emb_config.max_tokens,
            &resolved_data_dir,
        );
        match kaijutsu_index::OnnxEmbedder::new(
            &emb_config.model_dir,
            emb_config.dimensions,
            emb_config.max_tokens,
        ) {
            Ok(embedder) => {
                match kaijutsu_index::SemanticIndex::new(index_config, Box::new(embedder)) {
                    Ok(idx) => {
                        let idx = Arc::new(idx);
                        // Spawn background watcher for re-indexing on block completion
                        let block_source = Arc::new(BlockStoreSource(documents.clone()));
                        let status_receiver = FlowBusStatusReceiver {
                            sub: block_flows_for_index.subscribe("block.status"),
                        };

                        // Build synthesis callback — runs Rhai after each indexing.
                        // Must spawn_blocking: Rhai eval + ONNX embed are CPU-bound.
                        let synth_idx = idx.clone();
                        let synth_blocks: Arc<dyn kaijutsu_index::BlockSource> =
                            Arc::new(BlockStoreSource(documents.clone()));
                        let on_indexed: kaijutsu_index::watcher::OnIndexed =
                            Arc::new(move |ctx_id| {
                                let idx_clone = synth_idx.clone();
                                let blocks_clone = synth_blocks.clone();
                                tokio::task::spawn_blocking(move || {
                                    crate::synthesis::run_synthesis_and_cache(
                                        ctx_id,
                                        idx_clone.embedder_arc(),
                                        blocks_clone,
                                        idx_clone.synthesis_cache(),
                                    );
                                });
                            });

                        kaijutsu_index::watcher::spawn_index_watcher(
                            idx.clone(),
                            block_source,
                            Box::new(status_receiver),
                            Some(on_indexed),
                        );
                        log::info!(
                            "Semantic index initialized with {}",
                            emb_config.model_dir.display()
                        );
                        Some(idx)
                    }
                    Err(e) => {
                        log::warn!("Semantic index unavailable: {}", e);
                        None
                    }
                }
            }
            Err(e) => {
                log::warn!("Embedding model unavailable: {}", e);
                None
            }
        }
    } else {
        None
    };

    // Create kj dispatcher — shared across all connections
    let kj_dispatcher = Arc::new(kaijutsu_kernel::KjDispatcher::new(
        kernel_arc.drift().clone(),
        documents.clone(),
        kernel_db_arc.clone(),
        id,
        kernel_arc.clone(),
    ));

    let shared = SharedKernelState {
        id,
        name: id_str,
        kernel: kernel_arc,
        documents,
        config_backend,
        config_watcher,
        conversation_cache: Arc::new(ConversationCache::new(64)),
        kernel_db: kernel_db_arc,
        semantic_index,
        context_interrupts: Arc::new(TokioRwLock::new(HashMap::new())),
        interrupt_generation: AtomicU64::new(0),
        kj_dispatcher,
        session_contexts,
    };

    Ok(Arc::new(shared))
}

// ============================================================================
// World Implementation
// ============================================================================

/// World capability implementation
pub struct WorldImpl {
    registry: Arc<ServerRegistry>,
    connection: Rc<RefCell<ConnectionState>>,
}

impl WorldImpl {
    pub fn new(registry: Arc<ServerRegistry>, connection: Rc<RefCell<ConnectionState>>) -> Self {
        Self {
            registry,
            connection,
        }
    }
}

impl world::Server for WorldImpl {
    fn whoami(
        self: Rc<Self>,
        _params: world::WhoamiParams,
        mut results: world::WhoamiResults,
    ) -> Promise<(), capnp::Error> {
        let conn = self.connection.borrow();
        let mut identity = results.get().init_identity();
        identity.set_username(&conn.principal.username);
        identity.set_display_name(&conn.principal.display_name);
        Promise::ok(())
    }

    fn list_kernels(
        self: Rc<Self>,
        _params: world::ListKernelsParams,
        mut results: world::ListKernelsResults,
    ) -> Promise<(), capnp::Error> {
        let kernel = &self.registry.kernel;
        let mut kernels = results.get().init_kernels(1);
        let mut k = kernels.reborrow().get(0);
        k.set_id(kernel.id.as_bytes());
        k.set_name(&kernel.name);
        k.set_user_count(1);
        k.set_agent_count(0);
        Promise::ok(())
    }

    fn attach_kernel(
        self: Rc<Self>,
        params: world::AttachKernelParams,
        mut results: world::AttachKernelResults,
    ) -> Promise<(), capnp::Error> {
        let _params_reader = pry!(params.get());
        let _span = tracing::info_span!("rpc", method = "attach_kernel").entered();

        // No kernel creation — just hand out the shared kernel
        let kernel = self.registry.kernel.clone();
        let kernel_impl = KernelImpl::new(
            kernel.clone(),
            self.connection.clone(),
        );
        results.get().set_kernel(capnp_rpc::new_client(kernel_impl));
        results.get().set_kernel_id(kernel.id.as_bytes());
        Promise::ok(())
    }
}

// ============================================================================
// Kernel Implementation
// ============================================================================

struct KernelImpl {
    kernel: SharedKernel,
    connection: Rc<RefCell<ConnectionState>>,
}

impl KernelImpl {
    fn new(
        kernel: SharedKernel,
        connection: Rc<RefCell<ConnectionState>>,
    ) -> Self {
        Self { kernel, connection }
    }
}

impl kernel::Server for KernelImpl {
    fn get_info(
        self: Rc<Self>,
        params: kernel::GetInfoParams,
        mut results: kernel::GetInfoResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _span = extract_rpc_trace(p.get_trace(), "get_info").entered();
        let mut info = results.get().init_info();
        info.set_id(self.kernel.id.as_bytes());
        info.set_name(&self.kernel.name);
        info.set_user_count(1);
        info.set_agent_count(0);
        Promise::ok(())
    }

    // kaish execution methods

    fn execute(
        self: Rc<Self>,
        params: kernel::ExecuteParams,
        mut results: kernel::ExecuteResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let trace_span = extract_rpc_trace(p.get_trace(), "execute");
        let code = pry!(pry!(p.get_code()).to_str()).to_owned();
        let kernel = self.kernel.clone();
        let connection = self.connection.clone();

        // Non-blocking execute: return exec_id immediately, spawn execution in background.

        Promise::from_future(
            async move {
                // Get or create embedded kaish executor with real connection identity
                let (kaish, newly_created) = {
                    let mut conn = connection.borrow_mut();
                    let was_none = conn.kaish.is_none();

                    if was_none {
                        log::info!("Creating embedded kaish for kernel {}", kernel.id.to_hex());
                        let context_id = conn.require_context()?;
                        let kj_disp = kernel.kj_dispatcher.clone();
                        let kj_principal = conn.principal.id;
                        let session_contexts = conn.session_contexts.clone();
                        match EmbeddedKaish::with_identity_and_db(
                            &format!(
                                "{}-{}-{}",
                                kernel.name,
                                conn.principal.username,
                                conn.session_id.short()
                            ),
                            kernel.documents.clone(),
                            kernel.kernel.clone(),
                            None,
                            conn.principal.id,
                            context_id,
                            conn.session_id,
                            kernel.id,
                            Some(&kernel.kernel_db),
                            session_contexts.clone(),
                            |map, sid, tools| {
                                let block_source: Arc<dyn kaijutsu_index::BlockSource> =
                                    Arc::new(BlockStoreSource(kernel.documents.clone()));
                                tools.register(crate::kj_builtin::KjBuiltin::new(
                                    kj_disp,
                                    map,
                                    kj_principal,
                                    sid,
                                    kernel.semantic_index.clone(),
                                    block_source,
                                ));
                            },
                        ) {
                            Ok(kaish) => {
                                conn.kaish = Some(Rc::new(kaish));
                            }
                            Err(e) => {
                                log::error!("Failed to create embedded kaish: {}", e);
                                return Err(capnp::Error::failed(format!(
                                    "kaish creation failed: {}",
                                    e
                                )));
                            }
                        }
                    }

                    (conn.kaish.as_ref().unwrap().clone(), was_none)
                };

                // Apply persisted env vars and init_script on first creation.
                if newly_created {
                    if let Some(ctx_id) = kaish.context_id() {
                        kaish.apply_context_config(&kernel.kernel_db, ctx_id).await;
                    }
                }

                // Reject concurrent executions — kaish kernel is serial.
                {
                    let conn = connection.borrow();
                    if conn.has_running_execution() {
                        return Err(capnp::Error::failed("execution already in progress".into()));
                    }
                }

                // Allocate exec_id and register the execution before spawning.
                let (exec_id, cancel_token) = {
                    let mut conn = connection.borrow_mut();
                    let id = conn.next_exec_id();
                    let token = conn.register_execution(id);

                    let timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .expect("system clock before UNIX epoch")
                        .as_secs();

                    conn.command_history.push(CommandEntry {
                        id,
                        code: code.clone(),
                        timestamp,
                    });

                    (id, token)
                };

                // Return exec_id to the caller immediately.
                results.get().set_exec_id(exec_id);

                // Spawn background execution — Rc<EmbeddedKaish> is fine on LocalSet.
                let connection_bg = connection.clone();
                tokio::task::spawn_local(async move {
                    // Yield so the RPC response is sent before we start executing.
                    tokio::task::yield_now().await;

                    let exec_result = tokio::select! {
                        result = kaish.execute(&code) => {
                            match result {
                                Ok(r) => r,
                                Err(e) => {
                                    log::error!("kaish execute error: {}", e);
                                    kaish_kernel::interpreter::ExecResult::failure(1, e.to_string())
                                }
                            }
                        }
                        _ = cancel_token.cancelled() => {
                            kaish.cancel();
                            kaish_kernel::interpreter::ExecResult::failure(130, "interrupted")
                        }
                    };

                    // Dispatch output events to all subscribers.
                    dispatch_output_events(exec_id, &exec_result, &connection_bg).await;

                    // Clean up execution tracking.
                    connection_bg.borrow_mut().complete_execution(exec_id);
                });

                Ok(())
            }
            .instrument(trace_span),
        )
    }

    fn interrupt(
        self: Rc<Self>,
        params: kernel::InterruptParams,
        _results: kernel::InterruptResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _span = extract_rpc_trace(p.get_trace(), "interrupt").entered();
        let exec_id = p.get_exec_id();

        let conn = self.connection.borrow();
        if let Some(running) = conn.running_executions.get(&exec_id) {
            log::info!("Interrupting execution {}", exec_id);
            running.cancel.cancel();
        }
        // Silently ignore unknown exec_ids (may have already completed).
        Promise::ok(())
    }

    fn complete(
        self: Rc<Self>,
        params: kernel::CompleteParams,
        mut results: kernel::CompleteResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _span = extract_rpc_trace(p.get_trace(), "complete");
        let partial = pry!(pry!(p.get_partial()).to_str()).to_owned();
        let cursor = p.get_cursor() as usize;
        let kernel_arc = self.kernel.kernel.clone();

        Promise::from_future(async move {
            let _guard = _span.entered();
            let completions: Vec<String> = Vec::new();

            // TODO: kaish completions can be added here when available

            let builder = results.get();
            let mut list = builder.init_completions(completions.len() as u32);
            for (i, text) in completions.iter().enumerate() {
                let mut entry = list.reborrow().get(i as u32);
                entry.set_text(text);
                entry.set_display_text(text);
                entry.set_kind(CompletionKind::Keyword);
            }

            Ok(())
        })
    }

    fn subscribe_output(
        self: Rc<Self>,
        params: kernel::SubscribeOutputParams,
        _results: kernel::SubscribeOutputResults,
    ) -> Promise<(), capnp::Error> {
        let _span = tracing::info_span!("rpc", method = "subscribe_output").entered();
        let callback = pry!(pry!(params.get()).get_callback());
        self.connection.borrow_mut().add_output_subscriber(callback);
        log::debug!("Output subscriber registered");
        Promise::ok(())
    }

    fn get_command_history(
        self: Rc<Self>,
        params: kernel::GetCommandHistoryParams,
        mut results: kernel::GetCommandHistoryResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _span = extract_rpc_trace(p.get_trace(), "get_command_history").entered();
        let limit = p.get_limit() as usize;

        let conn = self.connection.borrow();
        let entries: Vec<_> = conn.command_history.iter().rev().take(limit).collect();

        let mut result_entries = results.get().init_entries(entries.len() as u32);
        for (i, entry) in entries.iter().enumerate() {
            let mut e = result_entries.reborrow().get(i as u32);
            e.set_id(entry.id);
            e.set_code(&entry.code);
            e.set_timestamp(entry.timestamp);
        }
        Promise::ok(())
    }

    // VFS methods

    fn vfs(
        self: Rc<Self>,
        _params: kernel::VfsParams,
        mut results: kernel::VfsResults,
    ) -> Promise<(), capnp::Error> {
        let vfs_impl = VfsImpl::new(self.kernel.kernel.clone());
        results.get().set_vfs(capnp_rpc::new_client(vfs_impl));
        Promise::ok(())
    }

    fn list_mounts(
        self: Rc<Self>,
        _params: kernel::ListMountsParams,
        mut results: kernel::ListMountsResults,
    ) -> Promise<(), capnp::Error> {
        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "list_mounts");
        Promise::from_future(
            async move {
                let mounts = kernel_arc.list_mounts().await;
                let mut builder = results.get().init_mounts(mounts.len() as u32);
                for (i, mount) in mounts.iter().enumerate() {
                    let mut m = builder.reborrow().get(i as u32);
                    m.set_path(mount.path.to_string_lossy());
                    m.set_read_only(mount.read_only);
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    fn mount(
        self: Rc<Self>,
        params: kernel::MountParams,
        _results: kernel::MountResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let path = match params.get_path().and_then(|p| get_path_str(p)) {
            Ok(s) => s,
            Err(e) => return Promise::err(e),
        };
        let source = match params.get_source().and_then(|p| get_path_str(p)) {
            Ok(s) => s,
            Err(e) => return Promise::err(e),
        };
        let writable = params.get_writable();

        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "mount");
        Promise::from_future(
            async move {
                // Expand source path (e.g., ~ to home dir)
                let expanded = shellexpand::tilde(&source);
                let source_path = std::path::PathBuf::from(expanded.as_ref());

                if !source_path.exists() {
                    return Err(capnp::Error::failed(format!(
                        "source path does not exist: {}",
                        source_path.display()
                    )));
                }

                let backend = if writable {
                    LocalBackend::new(source_path)
                } else {
                    LocalBackend::read_only(source_path)
                };

                if !kernel_arc.mount(&path, backend).await {
                    return Err(capnp::Error::failed(
                        "mount table is frozen — mounts cannot be changed at runtime".to_string(),
                    ));
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    fn unmount(
        self: Rc<Self>,
        params: kernel::UnmountParams,
        mut results: kernel::UnmountResults,
    ) -> Promise<(), capnp::Error> {
        let path = match params
            .get()
            .and_then(|p| p.get_path())
            .and_then(|p| get_path_str(p))
        {
            Ok(s) => s,
            Err(e) => return Promise::err(e),
        };

        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "unmount");
        Promise::from_future(
            async move {
                let success = kernel_arc.unmount(&path).await;
                results.get().set_success(success);
                Ok(())
            }
            .instrument(span),
        )
    }

    // Tool execution

    fn execute_tool(
        self: Rc<Self>,
        params: kernel::ExecuteToolParams,
        mut results: kernel::ExecuteToolResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let trace_span = extract_rpc_trace(p.get_trace(), "execute_tool");
        let call = pry!(p.get_call());
        let tool_name = pry!(pry!(call.get_tool()).to_str()).to_owned();
        let tool_params = pry!(pry!(call.get_params()).to_str()).to_owned();
        let request_id = pry!(pry!(call.get_request_id()).to_str()).to_owned();

        let kernel_arc = self.kernel.kernel.clone();
        let kernel_id = self.kernel.id;

        // Extract identity and kaish ref for async cwd resolution
        let (principal_id, context_id, session_id, kaish_ref) = {
            let conn = self.connection.borrow();
            (
                conn.principal.id,
                pry!(conn.require_context()),
                conn.session_id,
                conn.kaish.clone(),
            )
        };

        Promise::from_future(
            async move {
                let mut result = results.get().init_result();
                result.set_request_id(&request_id);

                // Resolve cwd from kaish session (now in async context, can await)
                let cwd = match &kaish_ref {
                    Some(k) => k.cwd().await,
                    None => std::path::PathBuf::from("/"),
                };

                let tool_ctx = kaijutsu_kernel::ExecContext::new(
                    principal_id,
                    context_id,
                    cwd,
                    session_id,
                    kernel_id,
                );

                // Kernel-scoped tool filter removed in Phase 1 M5; only the
                // per-context filter remains as a visibility gate.
                if let Some(ctx_filter) = kernel_arc
                    .drift()
                    .read()
                    .await
                    .get(context_id)
                    .and_then(|h| h.tool_filter.as_ref())
                    && !ctx_filter.allows(&tool_name)
                {
                    result.set_success(false);
                    result.set_error(format!(
                        "Tool filtered out by context config: {}",
                        tool_name
                    ));
                    return Ok(());
                }

                // Dispatch through the Phase 1 broker (M4).
                match kernel_arc
                    .dispatch_tool_via_broker(&tool_name, &tool_params, &tool_ctx)
                    .await
                {
                    Ok(exec_result) => {
                        result.set_success(exec_result.success);
                        result.set_output(&exec_result.stdout);
                        if !exec_result.stderr.is_empty() {
                            result.set_error(&exec_result.stderr);
                        }
                    }
                    Err(e) => {
                        result.set_success(false);
                        result.set_error(e.to_string());
                    }
                }
                Ok(())
            }
            .instrument(trace_span),
        )
    }

    fn get_tool_schemas(
        self: Rc<Self>,
        params: kernel::GetToolSchemasParams,
        mut results: kernel::GetToolSchemasResults,
    ) -> Promise<(), capnp::Error> {
        let kernel_arc = self.kernel.kernel.clone();
        let (principal_id, context_id) = {
            let conn = self.connection.borrow();
            // Fall back to a throwaway ContextId when the connection has no
            // active context — the broker will auto-populate a binding for
            // that ephemeral id.
            let ctx = conn.require_context().unwrap_or_else(|_| ContextId::new());
            (conn.principal.id, ctx)
        };

        let span = extract_rpc_trace(pry!(params.get()).get_trace(), "get_tool_schemas");
        Promise::from_future(
            async move {
                let visible = kernel_arc
                    .list_tool_defs_via_broker(context_id, principal_id)
                    .await;
                let mut builder = results.get().init_schemas(visible.len() as u32);
                for (i, (name, schema, description)) in visible.iter().enumerate() {
                    let mut s = builder.reborrow().get(i as u32);
                    s.set_name(name);
                    s.set_description(description.as_deref().unwrap_or(""));
                    s.set_category("mcp");
                    s.set_input_schema(&schema.to_string());
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    // =========================================================================
    // Block-based CRDT operations
    // =========================================================================

    fn subscribe_blocks(
        self: Rc<Self>,
        params: kernel::SubscribeBlocksParams,
        _results: kernel::SubscribeBlocksResults,
    ) -> Promise<(), capnp::Error> {
        let _span = tracing::info_span!("rpc", method = "subscribe_blocks").entered();
        let callback = pry!(pry!(params.get()).get_callback());

        {
            // Get the FlowBus instances from the kernel
            let block_flows = self.kernel.kernel.block_flows().clone();
            let input_flows = self.kernel.documents.input_flows().cloned();
            let kernel_id = self.kernel.id;

            // Spawn a bridge task that forwards FlowBus events to the callback
            // Use spawn_local because Cap'n Proto callbacks are not Send
            // Uses tokio::select! to multiplex block + input doc events on one callback
            tokio::task::spawn_local(async move {
                let mut block_sub = block_flows.subscribe("block.*");
                // Input flows are optional (only present if set_input_flows was called)
                let mut input_sub = input_flows.map(|f| f.subscribe("input.*"));
                log::debug!(
                    "Started FlowBus subscription for kernel {} (input_flows={})",
                    kernel_id.to_hex(),
                    input_sub.is_some()
                );

                loop {
                    let success = tokio::select! {
                        Some(msg) = block_sub.recv() => {
                            match msg.payload {
                                BlockFlow::Inserted { context_id, ref block, ref after_id, ref ops, .. } => {
                                    let mut req = callback.on_block_inserted_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                        params.set_has_after_id(after_id.is_some());
                                        if let Some(after) = after_id {
                                            set_block_id_builder(&mut params.reborrow().init_after_id(), after);
                                        }
                                        // Include CRDT ops for proper sync
                                        params.set_ops(ops);
                                        let mut block_state = params.init_block();
                                        set_block_snapshot(&mut block_state, block);
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                BlockFlow::Deleted { context_id, ref block_id, .. } => {
                                    let mut req = callback.on_block_deleted_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                        set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                BlockFlow::StatusChanged { context_id, ref block_id, status, ref output, .. } => {
                                    let mut req = callback.on_block_status_changed_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                        set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
                                        params.set_status(status_to_capnp(status));
                                        if let Some(output_data) = output {
                                            build_output_data(params.reborrow().init_output_data(), output_data);
                                        }
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                BlockFlow::CollapsedChanged { context_id, ref block_id, collapsed, .. } => {
                                    let mut req = callback.on_block_collapsed_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                        set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
                                        params.set_collapsed(collapsed);
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                BlockFlow::ExcludedChanged { context_id, ref block_id, excluded, .. } => {
                                    let mut req = callback.on_block_excluded_changed_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                        set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
                                        params.set_excluded(excluded);
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                BlockFlow::Moved { context_id, ref block_id, ref after_id, .. } => {
                                    let mut req = callback.on_block_moved_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                        set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
                                        params.set_has_after_id(after_id.is_some());
                                        if let Some(after) = after_id {
                                            set_block_id_builder(&mut params.reborrow().init_after_id(), after);
                                        }
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                BlockFlow::TextOps { context_id, ref block_id, ref ops, .. } => {
                                    let mut req = callback.on_block_text_ops_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                        set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
                                        params.set_ops(ops);
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                BlockFlow::SyncReset { context_id, generation } => {
                                    let mut req = callback.on_sync_reset_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                        params.set_generation(generation);
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                BlockFlow::ContextSwitched { context_id } => {
                                    let mut req = callback.on_context_switched_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                // No wire protocol for these yet — drop silently
                                BlockFlow::OutputChanged { .. }
                                | BlockFlow::MetadataChanged { .. } => true,
                            }
                        }
                        Some(msg) = async {
                            match &mut input_sub {
                                Some(sub) => sub.recv().await,
                                None => std::future::pending().await,
                            }
                        } => {
                            match msg.payload {
                                InputDocFlow::TextOps { context_id, ref ops, .. } => {
                                    let mut req = callback.on_input_text_ops_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                        params.set_ops(ops);
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                InputDocFlow::Cleared { context_id } => {
                                    let mut req = callback.on_input_cleared_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                    }
                                    req.send().promise.await.is_ok()
                                }
                            }
                        }
                        else => break,
                    };

                    // If callback fails (client disconnected), stop the bridge task
                    if !success {
                        log::debug!(
                            "FlowBus bridge task for kernel {} stopping: callback failed",
                            kernel_id
                        );
                        break;
                    }
                }

                log::debug!("FlowBus bridge task for kernel {} ended", kernel_id);
            });
        }
        Promise::ok(())
    }

    fn get_context_state(
        self: Rc<Self>,
        _params: kernel::GetContextStateParams,
        _results: kernel::GetContextStateResults,
    ) -> Promise<(), capnp::Error> {
        // Tombstoned: use getBlocks @82 + getContextSync @83 instead.
        // Schema ordinal @14 is preserved for wire compatibility.
        Promise::err(capnp::Error::failed(
            "getContextState removed: use getBlocks @82 + getContextSync @83".into(),
        ))
    }

    // =========================================================================
    // LLM operations
    // =========================================================================

    fn prompt(
        self: Rc<Self>,
        params: kernel::PromptParams,
        mut results: kernel::PromptResults,
    ) -> Promise<(), capnp::Error> {
        log::debug!("prompt() called for kernel {}", self.kernel.id);
        let params = pry!(params.get());
        let trace_span = extract_rpc_trace(params.get_trace(), "prompt");
        let request = pry!(params.get_request());
        let content = pry!(pry!(request.get_content()).to_str()).to_owned();
        let context_id_bytes = pry!(request.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        log::info!(
            "Received prompt request: context_id={}, content_len={}",
            context_id,
            content.len()
        );
        // Note: Cap'n Proto defaults unset Text fields to "", so we filter empty strings
        let model = request
            .get_model()
            .ok()
            .and_then(|m| m.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned());

        let kernel = self.kernel.clone();
        let (user_agent_id, session_id, kaish_ref) = {
            let conn = self.connection.borrow();
            (conn.principal.id, conn.session_id, conn.kaish.clone())
        };

        Promise::from_future(
            async move {
                log::debug!("prompt future started for context_id={}", context_id);

                // Resolve cwd from kaish session (in async context, can await)
                let cwd = match &kaish_ref {
                    Some(k) => k.cwd().await,
                    None => std::path::PathBuf::from("/"),
                };
                let tool_ctx = kaijutsu_kernel::ExecContext::new(
                    user_agent_id,
                    context_id,
                    cwd,
                    session_id,
                    kernel.id,
                );

                let documents = kernel.documents.clone();

                // Document must exist — join_context is the sole creator
                if documents.get(context_id).is_none() {
                    return Err(capnp::Error::failed(format!(
                        "context {} not found — call join_context first",
                        context_id
                    )));
                }

                // Create user message block at the end of the document
                let last_block = documents.last_block_id(context_id);
                log::info!(
                    "Inserting user block into context {}, after={:?}",
                    context_id,
                    last_block
                );
                let user_block_id = documents
                    .insert_block_as(
                        context_id,
                        None,
                        last_block.as_ref(),
                        Role::User,
                        BlockKind::Text,
                        &content,
                        Status::Done,
                        ContentType::Plain,
                        Some(user_agent_id),
                    )
                    .map_err(|e| {
                        log::error!("Failed to insert user block: {}", e);
                        capnp::Error::failed(format!("failed to insert user block: {}", e))
                    })?;
                log::debug!("Inserted user block: {:?}", user_block_id);

                // Generate prompt ID
                let prompt_id = uuid::Uuid::new_v4().to_string();
                log::debug!("Generated prompt_id={}", prompt_id);

                log::info!("User message block inserted, spawning LLM stream task");
                log::info!(
                    "Using model: {} (requested: {:?})",
                    model.as_deref().unwrap_or("default"),
                    model
                );

                // Spawn LLM streaming in background
                spawn_llm_for_prompt(
                    &kernel,
                    context_id,
                    &content,
                    model.as_deref(),
                    &user_block_id,
                    tool_ctx,
                    user_agent_id,
                )
                .await?;

                // Return immediately with prompt_id - streaming happens in background
                results.get().set_prompt_id(&prompt_id);
                log::debug!(
                    "prompt() returning immediately with prompt_id={}",
                    prompt_id
                );
                Ok(())
            }
            .instrument(trace_span),
        )
    }

    // =========================================================================
    // Context operations
    // =========================================================================

    fn list_contexts(
        self: Rc<Self>,
        params: kernel::ListContextsParams,
        mut results: kernel::ListContextsResults,
    ) -> Promise<(), capnp::Error> {
        let kernel_arc = self.kernel.kernel.clone();
        let kernel_db_arc = self.kernel.kernel_db.clone();
        let kernel_id = self.kernel.id;
        let semantic_index = self.kernel.semantic_index.clone();

        let span = extract_rpc_trace(pry!(params.get()).get_trace(), "list_contexts");
        Promise::from_future(
            async move {
                // Build KernelDb lookup for fork_kind + archived_at (fields not on DriftRouter)
                let db_map: HashMap<ContextId, ContextRow> = {
                    let db = kernel_db_arc.lock();
                    match db.list_all_contexts(kernel_id) {
                        Ok(rows) => rows.into_iter().map(|r| (r.context_id, r)).collect(),
                        Err(_) => HashMap::new(),
                    }
                };

                // Read from the kernel's drift router — runtime authority for provider/model
                let drift = kernel_arc.drift().read().await;
                let contexts = drift.list_contexts();
                let mut ctx_list = results.get().init_contexts(contexts.len() as u32);

                for (i, ctx) in contexts.iter().enumerate() {
                    let mut c = ctx_list.reborrow().get(i as u32);
                    c.set_id(ctx.id.as_bytes());
                    c.set_label(ctx.label.as_deref().unwrap_or(""));
                    // Wire field is still named `parentId` — Rust side renamed to `forked_from`
                    c.set_parent_id(
                        ctx.forked_from
                            .map(|p| *p.as_bytes())
                            .unwrap_or([0u8; 16])
                            .as_slice(),
                    );
                    c.set_provider(ctx.provider.as_deref().unwrap_or(""));
                    c.set_model(ctx.model.as_deref().unwrap_or(""));
                    c.set_created_at(ctx.created_at);
                    c.set_trace_id(&ctx.trace_id);

                    c.set_context_state(ctx.state.as_str());

                    // Supplement with KernelDb metadata
                    if let Some(row) = db_map.get(&ctx.id) {
                        c.set_fork_kind(row.fork_kind.as_ref().map(|fk| fk.as_str()).unwrap_or(""));
                        c.set_archived_at(row.archived_at.map(|ts| ts as u64).unwrap_or(0));
                    }

                    // Supplement with synthesis data (keywords + preview)
                    if let Some(ref idx) = semantic_index
                        && let Some(synth) = idx.synthesis_cache().get_any(ctx.id)
                    {
                        let kw_strs: Vec<&str> =
                            synth.keywords.iter().map(|(k, _)| k.as_str()).collect();
                        let mut kw_list = c.reborrow().init_keywords(kw_strs.len() as u32);
                        for (j, kw) in kw_strs.iter().enumerate() {
                            kw_list.set(j as u32, kw);
                        }
                        if let Some((_, _, preview)) = synth.top_blocks.first() {
                            c.set_top_block_preview(preview);
                        }
                    }
                }

                Ok(())
            }
            .instrument(span),
        )
    }

    /// Create a new context with the given label.
    ///
    /// Generates a fresh ContextId (UUIDv7), creates the document in the
    /// block store, and registers it in the kernel's drift router.
    fn create_context(
        self: Rc<Self>,
        params: kernel::CreateContextParams,
        mut results: kernel::CreateContextResults,
    ) -> Promise<(), capnp::Error> {
        let label = pry!(pry!(pry!(params.get()).get_label()).to_str()).to_owned();

        let kernel = self.kernel.clone();
        let connection = self.connection.clone();

        let session_id = connection.borrow().session_id;
        let parent_ctx = connection
            .borrow()
            .session_contexts
            .get(&session_id)
            .map(|r| *r);

        log::info!(
            "create_context: label='{}' kernel='{}'",
            label,
            kernel.id.to_hex()
        );

        Promise::from_future(async move {
            let context_id = ContextId::new();

            // Create the document for this context
            if let Err(e) = kernel.documents.create_document(
                context_id,
                kaijutsu_types::DocKind::Conversation,
                None,
            ) {
                return Err(capnp::Error::failed(format!(
                    "Failed to create document for context {}: {}",
                    context_id, e
                )));
            }

            // Create the input document for this context
            if let Err(e) = kernel.documents.create_input_doc(context_id) {
                log::warn!(
                    "Failed to create input doc for context {}: {}",
                    context_id,
                    e
                );
            }

            let label_ref = if label.is_empty() {
                None
            } else {
                Some(label.as_str())
            };
            let created_by = connection.borrow().principal.id;

            // Read LLM defaults so new contexts start with a model set.
            // If no provider is configured, don't silently inject a hardcoded model —
            // let both be None so the user gets a clear error when they try to use LLM.
            let (default_provider, default_model) = {
                let registry = kernel.kernel.llm().read().await;
                let provider = registry.default_provider_name().map(|s| s.to_string());
                let model = registry.default_model().map(|s| s.to_string());
                if provider.is_none() && model.is_none() {
                    log::warn!("No LLM provider configured — new context will have no model set");
                }
                (provider, model)
            };

            // Write-through: KernelDb first, then DriftRouter. Both steps must
            // succeed or we roll in-memory state back — we never want a ghost
            // that is live in memory but missing from the DB (lost on restart)
            // nor a DB row without a drift entry.
            {
                let db = kernel.kernel_db.lock();
                let row = ContextRow {
                    context_id,
                    kernel_id: kernel.id,
                    label: label_ref.map(|s| s.to_string()),
                    provider: default_provider.clone(),
                    model: default_model.clone(),
                    system_prompt: None,
                    tool_filter: None,
                    consent_mode: kaijutsu_kernel::control::ConsentMode::Collaborative,
                    context_state: kaijutsu_types::ContextState::Live,
                    created_at: kaijutsu_types::now_millis() as i64,
                    created_by,
                    forked_from: parent_ctx,
                    fork_kind: None,
                    archived_at: None,
                    workspace_id: None,
                    preset_id: None,
                };
                let default_ws = db
                    .get_or_create_default_workspace(row.kernel_id, row.created_by)
                    .unwrap_or_else(|_| kaijutsu_types::WorkspaceId::new());
                if let Err(e) = db.insert_context_with_document(&row, default_ws) {
                    drop(db);
                    let _ = kernel.documents.delete_document(context_id);
                    return Err(capnp::Error::failed(format!(
                        "KernelDb insert_context failed for {}: {}",
                        context_id.short(),
                        e
                    )));
                }
            }

            {
                let mut drift = kernel.kernel.drift().write().await;
                if let Err(e) = drift.register(context_id, label_ref, parent_ctx, created_by) {
                    drop(drift);
                    let _ = kernel.kernel_db.lock().delete_context(context_id);
                    let _ = kernel.documents.delete_document(context_id);
                    return Err(capnp::Error::failed(format!("label conflict: {e}")));
                }
                if let (Some(p), Some(m)) = (&default_provider, &default_model) {
                    let _ = drift.configure_llm(context_id, p, m);
                }
                log::info!(
                    "Created context {} (label={:?}) in kernel DriftRouter",
                    context_id,
                    label_ref
                );
            }

            results.get().set_id(context_id.as_bytes());
            Ok(())
        })
    }

    /// Join an existing context, returning its context_id.
    ///
    /// The context must already exist (created via `createContext`). Returns an
    /// error if the context doesn't exist — no auto-creation.
    fn join_context(
        self: Rc<Self>,
        params: kernel::JoinContextParams,
        mut results: kernel::JoinContextResults,
    ) -> Promise<(), capnp::Error> {
        let params = pry!(params.get());
        // Schema: joinContext(contextId :Data, instance :Text) -> (contextId :Data)
        let context_id_bytes = pry!(params.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes).ok_or_else(|| capnp::Error::failed(
                "invalid context ID (expected 16 bytes)".into()
            ))
        );
        let instance = pry!(pry!(params.get_instance()).to_str()).to_owned();

        let kernel = self.kernel.clone();
        let connection = self.connection.clone();

        log::info!(
            "join_context: context_id={} instance='{}' kernel='{}'",
            context_id,
            instance,
            kernel.id.to_hex()
        );

        let span = extract_rpc_trace(params.get_trace(), "join_context");
        Promise::from_future(
            async move {
                // Context must already exist — no auto-creation
                if !kernel.documents.contains(context_id) {
                    return Err(capnp::Error::failed(format!(
                        "context {} does not exist — use createContext first",
                        context_id
                    )));
                }

                log::debug!("Re-joining existing context {}", context_id);

                // Ensure input doc exists (idempotent)
                if let Err(e) = kernel.documents.create_input_doc(context_id) {
                    log::warn!(
                        "Failed to create input doc for context {}: {}",
                        context_id,
                        e
                    );
                }

                // Verify context is registered in drift router
                {
                    let drift = kernel.kernel.drift().read().await;
                    if drift.get(context_id).is_none() {
                        return Err(capnp::Error::failed(format!(
                            "context {} not registered in drift router — use createContext first",
                            context_id
                        )));
                    }
                    let trace_id = drift
                        .get(context_id)
                        .map(|h| h.trace_id)
                        .unwrap_or([0u8; 16]);
                    let _ctx_span =
                        kaijutsu_telemetry::context_root_span(&trace_id, "join_context").entered();
                }

                // Update connection's active context in the global map
                let session_id = connection.borrow().session_id;
                connection
                    .borrow()
                    .session_contexts
                    .insert(session_id, context_id);

                // Return the context_id
                results.get().set_context_id(context_id.as_bytes());

                Ok(())
            }
            .instrument(span),
        )
    }

    // =========================================================================
    // MCP (legacy — removed in Phase 1 M5)
    // =========================================================================
    //
    // The RPC methods below kept their capnp ordinals so wire compatibility is
    // preserved, but return `Unimplemented` until Phase 2 rewires external MCP
    // administration through the broker.

    fn register_mcp(
        self: Rc<Self>,
        _params: kernel::RegisterMcpParams,
        _results: kernel::RegisterMcpResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented(
            "register_mcp: external MCP admin is offline until Phase 2".into(),
        ))
    }

    fn unregister_mcp(
        self: Rc<Self>,
        _params: kernel::UnregisterMcpParams,
        _results: kernel::UnregisterMcpResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented(
            "unregister_mcp: external MCP admin is offline until Phase 2".into(),
        ))
    }

    fn list_mcp_servers(
        self: Rc<Self>,
        _params: kernel::ListMcpServersParams,
        mut results: kernel::ListMcpServersResults,
    ) -> Promise<(), capnp::Error> {
        // Empty list — no servers registered until Phase 2.
        results.get().init_servers(0);
        Promise::ok(())
    }

    fn call_mcp_tool(
        self: Rc<Self>,
        _params: kernel::CallMcpToolParams,
        _results: kernel::CallMcpToolResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented(
            "call_mcp_tool: external MCP tool calls go through Broker in Phase 2".into(),
        ))
    }

    // =========================================================================
    // Shell execution (kaish REPL)
    // =========================================================================

    fn shell_execute(
        self: Rc<Self>,
        params: kernel::ShellExecuteParams,
        mut results: kernel::ShellExecuteResults,
    ) -> Promise<(), capnp::Error> {
        log::debug!(
            "shell_execute() called for kernel {}",
            self.kernel.id.to_hex()
        );
        let params = pry!(params.get());
        let trace_span = extract_rpc_trace(params.get_trace(), "shell_execute");
        let code = pry!(pry!(params.get_code()).to_str()).to_owned();
        let context_id_bytes = pry!(params.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let user_initiated = params.get_user_initiated();
        log::info!(
            "Shell execute: context_id={}, code={}, user_initiated={}",
            context_id,
            code,
            user_initiated
        );

        let kernel = self.kernel.clone();
        let connection = self.connection.clone();
        let user_agent_id = self.connection.borrow().principal.id;

        Promise::from_future(
            async move {
                let command_block_id = execute_shell_command(
                    &code,
                    context_id,
                    user_agent_id,
                    user_initiated,
                    &kernel,
                    &connection,
                )
                .await?;

                let mut block_id_builder = results.get().init_command_block_id();
                set_block_id_builder(&mut block_id_builder, &command_block_id);
                Ok(())
            }
            .instrument(trace_span),
        )
    }

    // =========================================================================
    // Shell state (cwd, last result)
    // =========================================================================

    fn get_cwd(
        self: Rc<Self>,
        _params: kernel::GetCwdParams,
        mut results: kernel::GetCwdResults,
    ) -> Promise<(), capnp::Error> {
        let connection = self.connection.clone();

        let span = tracing::info_span!("rpc", method = "get_cwd");
        Promise::from_future(
            async move {
                let kaish = {
                    let conn = connection.borrow();
                    conn.kaish.clone()
                };

                let cwd = if let Some(kaish) = kaish {
                    kaish.cwd().await
                } else {
                    std::path::PathBuf::from("/docs")
                };

                results.get().set_path(cwd.to_string_lossy());
                Ok(())
            }
            .instrument(span),
        )
    }

    fn set_cwd(
        self: Rc<Self>,
        params: kernel::SetCwdParams,
        mut results: kernel::SetCwdResults,
    ) -> Promise<(), capnp::Error> {
        let path = match params.get().and_then(|p| p.get_path()) {
            Ok(p) => match p.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => {
                    return Promise::err(capnp::Error::failed(format!("invalid path: {}", e)));
                }
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("missing path: {}", e))),
        };

        let connection = self.connection.clone();

        let span = tracing::info_span!("rpc", method = "set_cwd");
        Promise::from_future(
            async move {
                let kaish = {
                    let conn = connection.borrow();
                    conn.kaish
                        .clone()
                        .ok_or_else(|| capnp::Error::failed("kaish not initialized".into()))?
                };

                // set_cwd doesn't return a Result in kaish
                kaish.set_cwd(std::path::PathBuf::from(&path)).await;
                results.get().set_success(true);
                results.get().set_error("");
                Ok(())
            }
            .instrument(span),
        )
    }

    fn get_last_result(
        self: Rc<Self>,
        _params: kernel::GetLastResultParams,
        mut results: kernel::GetLastResultResults,
    ) -> Promise<(), capnp::Error> {
        let connection = self.connection.clone();

        let span = tracing::info_span!("rpc", method = "get_last_result");
        Promise::from_future(
            async move {
                let kaish = {
                    let conn = connection.borrow();
                    conn.kaish.clone()
                };

                let last_result = if let Some(kaish) = kaish {
                    kaish.last_result().await
                } else {
                    None
                };

                let mut result_builder = results.get().init_result();
                if let Some(exec_result) = last_result {
                    result_builder.set_code(exec_result.code);
                    result_builder.set_ok(exec_result.ok());
                    result_builder.set_stdout(exec_result.text_out().as_bytes());
                    result_builder.set_stderr(&exec_result.err);

                    // Serialize data if present
                    if let Some(ref data) = exec_result.data {
                        value_to_shell_value(result_builder.reborrow().init_data(), data);
                    }

                    // Serialize structured output data
                    if let Some(output_data) = exec_result.output() {
                        build_output_data(
                            result_builder.reborrow().init_output_data(),
                            output_data,
                        );
                    }
                } else {
                    // No last result - return empty/zero values
                    result_builder.set_code(0);
                    result_builder.set_ok(true);
                    result_builder.set_stdout(&[]);
                    result_builder.set_stderr("");
                }

                Ok(())
            }
            .instrument(span),
        )
    }

    // =========================================================================
    // Blob Storage (schema ordinals kept for wire compat, handlers removed)
    // =========================================================================

    fn write_blob(
        self: Rc<Self>,
        _params: kernel::WriteBlobParams,
        _results: kernel::WriteBlobResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("blob API removed".into()))
    }

    fn read_blob(
        self: Rc<Self>,
        _params: kernel::ReadBlobParams,
        _results: kernel::ReadBlobResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("blob API removed".into()))
    }

    fn delete_blob(
        self: Rc<Self>,
        _params: kernel::DeleteBlobParams,
        _results: kernel::DeleteBlobResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("blob API removed".into()))
    }

    fn list_blobs(
        self: Rc<Self>,
        _params: kernel::ListBlobsParams,
        _results: kernel::ListBlobsResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("blob API removed".into()))
    }

    fn push_ops(
        self: Rc<Self>,
        params: kernel::PushOpsParams,
        mut results: kernel::PushOpsResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let _trace_guard = extract_rpc_trace(params_reader.get_trace(), "push_ops").entered();
        let context_id_bytes = pry!(params_reader.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let ops_data = pry!(params_reader.get_ops()).to_vec();

        log::debug!(
            "push_ops called for context {} with {} bytes",
            context_id,
            ops_data.len()
        );

        let _ctx_span = if let Ok(drift) = self.kernel.kernel.drift().try_read() {
            let trace_id = drift.trace_id_for_context(context_id).unwrap_or([0u8; 16]);
            Some(kaijutsu_telemetry::context_root_span(&trace_id, "push_ops").entered())
        } else {
            None
        };

        let documents = &self.kernel.documents;

        // Deserialize the sync payload
        let payload: kaijutsu_crdt::block_store::SyncPayload = match postcard::from_bytes(&ops_data)
        {
            Ok(p) => p,
            Err(e) => {
                return Promise::err(capnp::Error::failed(format!(
                    "failed to deserialize sync payload: {}",
                    e
                )));
            }
        };

        // Merge the sync payload into the document
        let ack_version = match documents.merge_ops(context_id, payload) {
            Ok(version) => version,
            Err(e) => {
                return Promise::err(capnp::Error::failed(format!("failed to merge ops: {}", e)));
            }
        };

        log::debug!("push_ops merged successfully, new version: {}", ack_version);
        results.get().set_ack_version(ack_version);
        Promise::ok(())
    }

    // =========================================================================
    // MCP Resource Operations (legacy — removed in Phase 1 M5)
    // =========================================================================

    fn list_mcp_resources(
        self: Rc<Self>,
        _params: kernel::ListMcpResourcesParams,
        mut results: kernel::ListMcpResourcesResults,
    ) -> Promise<(), capnp::Error> {
        results.get().init_resources(0);
        Promise::ok(())
    }

    fn read_mcp_resource(
        self: Rc<Self>,
        _params: kernel::ReadMcpResourceParams,
        _results: kernel::ReadMcpResourceResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented(
            "read_mcp_resource: offline until Phase 3 resource wiring".into(),
        ))
    }

    fn subscribe_mcp_resources(
        self: Rc<Self>,
        _params: kernel::SubscribeMcpResourcesParams,
        _results: kernel::SubscribeMcpResourcesResults,
    ) -> Promise<(), capnp::Error> {
        Promise::ok(())
    }

    fn subscribe_mcp_elicitations(
        self: Rc<Self>,
        _params: kernel::SubscribeMcpElicitationsParams,
        _results: kernel::SubscribeMcpElicitationsResults,
    ) -> Promise<(), capnp::Error> {
        Promise::ok(())
    }

    // ========================================================================
    // Agent Attachment (Phase 2: Collaborative Canvas)
    // ========================================================================

    fn attach_agent(
        self: Rc<Self>,
        params: kernel::AttachAgentParams,
        mut results: kernel::AttachAgentResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let config_reader = pry!(params_reader.get_config());

        // Extract config fields
        let nick = pry!(config_reader.get_nick())
            .to_str()
            .unwrap_or("unknown")
            .to_owned();
        let instance = pry!(config_reader.get_instance())
            .to_str()
            .unwrap_or("default")
            .to_owned();
        let provider = pry!(config_reader.get_provider())
            .to_str()
            .unwrap_or("unknown")
            .to_owned();
        let model_id = pry!(config_reader.get_model_id())
            .to_str()
            .unwrap_or("unknown")
            .to_owned();

        // Extract capabilities
        let caps_reader = pry!(config_reader.get_capabilities());
        let capabilities: Vec<AgentCapability> = (0..caps_reader.len())
            .filter_map(|i| {
                caps_reader
                    .get(i)
                    .map(capnp_to_agent_capability)
                    .ok()
                    .flatten()
            })
            .collect();

        let config = AgentConfig {
            nick: nick.clone(),
            instance,
            provider,
            model_id,
            capabilities,
        };

        // Extract optional AgentCommands callback for reverse invocation.
        // get_commands() returns Err for null/missing capability pointers —
        // this is the standard capnp pattern for optional capabilities.
        let commands_callback = params_reader.get_commands().ok();

        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "attach_agent");
        Promise::from_future(
            async move {
                // Create invoke channel if callback provided
                let invoke_sender = if let Some(callback) = commands_callback {
                    let (tx, mut rx) = tokio::sync::mpsc::channel::<InvokeRequest>(32);
                    let nick_for_task = nick.clone();

                    // Bridge task: recv InvokeRequest from channel, call capnp callback
                    tokio::task::spawn_local(async move {
                        while let Some(request) = rx.recv().await {
                            let mut req = callback.invoke_request();
                            {
                                let mut p = req.get();
                                p.set_action(&request.action);
                                p.set_params(&request.params);
                            }
                            let result = match req.send().promise.await {
                                Ok(response) => match response.get().and_then(|r| r.get_result()) {
                                    Ok(data) => Ok(data.to_vec()),
                                    Err(e) => Err(format!("capnp read error: {e}")),
                                },
                                Err(e) => Err(format!("RPC error: {e}")),
                            };
                            if request.reply.send(InvokeResponse { result }).is_err() {
                                tracing::debug!(
                                    nick = %nick_for_task,
                                    "Agent invoke reply dropped (caller likely timed out)",
                                );
                            }
                        }
                        log::debug!("Agent invoke bridge for '{}' ended", nick_for_task);
                    });

                    Some(tx)
                } else {
                    None
                };

                let agent_info = kernel_arc
                    .attach_agent(config, invoke_sender)
                    .await
                    .map_err(|e| capnp::Error::failed(format!("failed to attach agent: {}", e)))?;

                // Build response
                let mut info = results.get().init_info();
                set_agent_info(&mut info, &agent_info);

                Ok(())
            }
            .instrument(span),
        )
    }

    fn list_agents(
        self: Rc<Self>,
        _params: kernel::ListAgentsParams,
        mut results: kernel::ListAgentsResults,
    ) -> Promise<(), capnp::Error> {
        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "list_agents");
        Promise::from_future(
            async move {
                let agents = kernel_arc.list_agents().await;

                let mut list = results.get().init_agents(agents.len() as u32);
                for (i, agent) in agents.iter().enumerate() {
                    let mut a = list.reborrow().get(i as u32);
                    set_agent_info(&mut a, agent);
                }

                Ok(())
            }
            .instrument(span),
        )
    }

    fn detach_agent(
        self: Rc<Self>,
        params: kernel::DetachAgentParams,
        _results: kernel::DetachAgentResults,
    ) -> Promise<(), capnp::Error> {
        let nick = pry!(pry!(pry!(params.get()).get_nick()).to_str()).to_owned();

        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "detach_agent");
        Promise::from_future(
            async move {
                kernel_arc.detach_agent(&nick).await;
                Ok(())
            }
            .instrument(span),
        )
    }

    fn set_agent_capabilities(
        self: Rc<Self>,
        params: kernel::SetAgentCapabilitiesParams,
        _results: kernel::SetAgentCapabilitiesResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let nick = pry!(pry!(params_reader.get_nick()).to_str()).to_owned();
        let caps_reader = pry!(params_reader.get_capabilities());

        let capabilities: Vec<AgentCapability> = (0..caps_reader.len())
            .filter_map(|i| {
                caps_reader
                    .get(i)
                    .map(capnp_to_agent_capability)
                    .ok()
                    .flatten()
            })
            .collect();

        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "set_agent_capabilities");
        Promise::from_future(
            async move {
                kernel_arc
                    .set_agent_capabilities(&nick, capabilities)
                    .await
                    .map_err(|e| {
                        capnp::Error::failed(format!("failed to set capabilities: {}", e))
                    })?;

                Ok(())
            }
            .instrument(span),
        )
    }

    fn invoke_agent(
        self: Rc<Self>,
        params: kernel::InvokeAgentParams,
        mut results: kernel::InvokeAgentResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let nick = pry!(pry!(params_reader.get_nick()).to_str()).to_owned();
        let action = pry!(pry!(params_reader.get_action()).to_str()).to_owned();
        let invoke_params = pry!(params_reader.get_params()).to_vec();

        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "invoke_agent");
        Promise::from_future(
            async move {
                // Emit started event
                kernel_arc
                    .emit_agent_event(AgentActivityEvent::Started {
                        agent: nick.clone(),
                        block_id: String::new(), // TODO: thread block_id through invoke_agent RPC
                        action: action.clone(),
                    })
                    .await;

                // Dispatch to the target agent via its registered channel
                let result = kernel_arc.invoke_agent(&nick, &action, invoke_params).await;

                match result {
                    Ok(data) => {
                        kernel_arc
                            .emit_agent_event(AgentActivityEvent::Completed {
                                agent: nick,
                                block_id: String::new(), // TODO: thread block_id through invoke_agent RPC
                                success: true,
                            })
                            .await;
                        results.get().set_result(&data);
                        Ok(())
                    }
                    Err(e) => {
                        kernel_arc
                            .emit_agent_event(AgentActivityEvent::Completed {
                                agent: nick,
                                block_id: String::new(), // TODO: thread block_id through invoke_agent RPC
                                success: false,
                            })
                            .await;
                        Err(capnp::Error::failed(format!("invoke_agent: {e}")))
                    }
                }
            }
            .instrument(span),
        )
    }

    fn subscribe_agent_events(
        self: Rc<Self>,
        params: kernel::SubscribeAgentEventsParams,
        _results: kernel::SubscribeAgentEventsResults,
    ) -> Promise<(), capnp::Error> {
        let _span = tracing::info_span!("rpc", method = "subscribe_agent_events").entered();
        let callback = pry!(pry!(params.get()).get_callback());

        let kernel_arc = self.kernel.kernel.clone();
        let kernel_id = self.kernel.id;

        // Spawn a bridge task that forwards AgentActivityEvent to the callback
        tokio::task::spawn_local(async move {
            let mut receiver = kernel_arc.subscribe_agent_events().await;

            log::debug!("Started agent event subscription for kernel {}", kernel_id);

            while let Ok(event) = receiver.recv().await {
                let success = match &event {
                    AgentActivityEvent::Started {
                        agent,
                        block_id: _,
                        action,
                    } => {
                        let mut req = callback.on_activity_request();
                        {
                            let mut params = req.get().init_event();
                            params.set_agent(agent);
                            let mut started = params.init_started();
                            // Parse block_id string back to components - simplified for now
                            started.reborrow().init_block_id();
                            started.set_action(action);
                        }
                        req.send().promise.await.is_ok()
                    }
                    AgentActivityEvent::Progress {
                        agent,
                        block_id: _,
                        message,
                        percent,
                    } => {
                        let mut req = callback.on_activity_request();
                        {
                            let mut params = req.get().init_event();
                            params.set_agent(agent);
                            let mut progress = params.init_progress();
                            progress.reborrow().init_block_id();
                            progress.set_message(message);
                            progress.set_percent(*percent);
                        }
                        req.send().promise.await.is_ok()
                    }
                    AgentActivityEvent::Completed {
                        agent,
                        block_id: _,
                        success: ok,
                    } => {
                        let mut req = callback.on_activity_request();
                        {
                            let mut params = req.get().init_event();
                            params.set_agent(agent);
                            let mut completed = params.init_completed();
                            completed.reborrow().init_block_id();
                            completed.set_success(*ok);
                        }
                        req.send().promise.await.is_ok()
                    }
                    AgentActivityEvent::CursorMoved {
                        agent,
                        block_id: _,
                        offset,
                    } => {
                        let mut req = callback.on_activity_request();
                        {
                            let mut params = req.get().init_event();
                            params.set_agent(agent);
                            let mut cursor = params.init_cursor_moved();
                            cursor.reborrow().init_block_id();
                            cursor.set_offset(*offset);
                        }
                        req.send().promise.await.is_ok()
                    }
                };

                if !success {
                    log::debug!(
                        "Agent event bridge task for kernel {} stopping: callback failed",
                        kernel_id
                    );
                    break;
                }
            }

            log::debug!("Agent event bridge task for kernel {} ended", kernel_id);
        });

        Promise::ok(())
    }

    // ========================================================================
    // Timeline Navigation
    // ========================================================================

    // forkFromVersion removed — consolidated to kj fork

    fn cherry_pick_block(
        self: Rc<Self>,
        params: kernel::CherryPickBlockParams,
        mut results: kernel::CherryPickBlockResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let source_block_id_reader = pry!(params_reader.get_source_block_id());
        let source_block_id = pry!(parse_block_id_from_reader(&source_block_id_reader));
        // Schema: cherryPickBlock(sourceBlockId, targetContextId :Data)
        let target_ctx_bytes = pry!(params_reader.get_target_context_id());
        let target_ctx_id =
            pry!(
                ContextId::try_from_slice(target_ctx_bytes).ok_or_else(|| capnp::Error::failed(
                    "invalid target context ID (expected 16 bytes)".into()
                ))
            );

        log::info!(
            "Cherry-pick request: block={} to context={}",
            source_block_id.to_key(),
            target_ctx_id
        );

        let documents = self.kernel.documents.clone();
        let kernel_arc = self.kernel.kernel.clone();
        let user_agent_id = self.connection.borrow().principal.id;

        // Get the source document and extract block snapshot (DashMap access is sync)
        let doc_entry = match documents.get(source_block_id.context_id) {
            Some(entry) => entry,
            None => return Promise::err(capnp::Error::failed("Source document not found".into())),
        };
        let block_snapshot = match doc_entry.doc.get_block_snapshot(&source_block_id) {
            Some(snapshot) => snapshot,
            None => return Promise::err(capnp::Error::failed("Block not found".into())),
        };
        drop(doc_entry);

        let span = extract_rpc_trace(params_reader.get_trace(), "cherry_pick_block");
        Promise::from_future(
            async move {
                // Look up target context in drift router for trace linkage
                let drift = kernel_arc.drift().read().await;
                let target_handle = drift.get(target_ctx_id).ok_or_else(|| {
                    capnp::Error::failed(format!("target context {} not found", target_ctx_id))
                })?;
                let trace_id = target_handle.trace_id;
                drop(drift);
                let _ctx_span =
                    kaijutsu_telemetry::context_root_span(&trace_id, "cherry_pick_block").entered();

                // Target document must exist
                if !documents.contains(target_ctx_id) {
                    return Err(capnp::Error::failed(format!(
                        "target context {} not found — join target context first",
                        target_ctx_id
                    )));
                }

                // Insert the block into target document (authored by calling user)
                let after_id = documents.last_block_id(target_ctx_id);
                let new_block_id = documents
                    .insert_block_as(
                        target_ctx_id,
                        None,
                        after_id.as_ref(),
                        block_snapshot.role,
                        block_snapshot.kind,
                        block_snapshot.content,
                        Status::Done,
                        block_snapshot.content_type,
                        Some(user_agent_id),
                    )
                    .map_err(|e| capnp::Error::failed(format!("Failed to insert block: {}", e)))?;

                // Build result
                let mut new_block_builder = results.get().init_new_block_id();
                set_block_id_builder(&mut new_block_builder, &new_block_id);

                log::info!(
                    "Cherry-pick complete: {} -> {}",
                    source_block_id.to_key(),
                    new_block_id.to_key()
                );
                Ok(())
            }
            .instrument(span),
        )
    }

    fn get_context_history(
        self: Rc<Self>,
        params: kernel::GetContextHistoryParams,
        mut results: kernel::GetContextHistoryResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let _span = extract_rpc_trace(params_reader.get_trace(), "get_context_history").entered();
        let context_id_bytes = pry!(params_reader.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let limit = params_reader.get_limit() as usize;

        let _ctx_span = if let Ok(drift) = self.kernel.kernel.drift().try_read() {
            let trace_id = drift.trace_id_for_context(context_id).unwrap_or([0u8; 16]);
            Some(kaijutsu_telemetry::context_root_span(&trace_id, "get_document_history").entered())
        } else {
            None
        };

        // Get the document (DashMap access is sync)
        let doc_entry = self.kernel.documents.get(context_id);
        let doc_entry = match doc_entry {
            Some(entry) => entry,
            None => return Promise::err(capnp::Error::failed("Document not found".into())),
        };

        // Get blocks ordered by creation time to build version history
        let blocks = doc_entry.doc.blocks_ordered();
        let current_version = doc_entry.version();

        // For now, each block addition is a "version snapshot"
        // In the future, this could be more granular (edits, etc.)
        let snapshot_count = blocks.len().min(limit);
        let mut snapshots = results.get().init_snapshots(snapshot_count as u32);

        for (i, block) in blocks.iter().take(limit).enumerate() {
            let mut snapshot = snapshots.reborrow().get(i as u32);
            snapshot.set_version(i as u64 + 1);
            snapshot.set_timestamp(block.created_at);
            snapshot.set_block_count((i + 1) as u32);
            snapshot.set_change_kind(crate::kaijutsu_capnp::ChangeKind::BlockAdded);

            let mut block_id_builder = snapshot.init_changed_block_id();
            set_block_id_builder(&mut block_id_builder, &block.id);
        }

        log::debug!(
            "Document history: {} snapshots (current version: {})",
            snapshot_count,
            current_version
        );
        Promise::ok(())
    }

    // =========================================================================
    // Config Methods (Phase 2: Config as CRDT)
    // =========================================================================

    fn list_configs(
        self: Rc<Self>,
        _params: kernel::ListConfigsParams,
        mut results: kernel::ListConfigsResults,
    ) -> Promise<(), capnp::Error> {
        let _span = tracing::info_span!("rpc", method = "list_configs").entered();

        let configs = self.kernel.config_backend.list_configs();
        let mut builder = results.get().init_configs(configs.len() as u32);
        for (i, config) in configs.iter().enumerate() {
            builder.set(i as u32, config);
        }

        Promise::ok(())
    }

    fn reload_config(
        self: Rc<Self>,
        params: kernel::ReloadConfigParams,
        mut results: kernel::ReloadConfigResults,
    ) -> Promise<(), capnp::Error> {
        let path = pry!(pry!(pry!(params.get()).get_path()).to_str()).to_owned();
        let config_backend = self.kernel.config_backend.clone();

        let span = tracing::info_span!("rpc", method = "reload_config");
        Promise::from_future(
            async move {
                match config_backend.reload_from_disk(&path).await {
                    Ok(()) => {
                        results.get().set_success(true);
                        results.get().set_error("");
                    }
                    Err(e) => {
                        results.get().set_success(false);
                        results.get().set_error(format!("{}", e));
                    }
                }

                Ok(())
            }
            .instrument(span),
        )
    }

    fn reset_config(
        self: Rc<Self>,
        params: kernel::ResetConfigParams,
        mut results: kernel::ResetConfigResults,
    ) -> Promise<(), capnp::Error> {
        let path = pry!(pry!(pry!(params.get()).get_path()).to_str()).to_owned();
        let config_backend = self.kernel.config_backend.clone();

        let span = tracing::info_span!("rpc", method = "reset_config");
        Promise::from_future(
            async move {
                match config_backend.reset_to_default(&path).await {
                    Ok(()) => {
                        results.get().set_success(true);
                        results.get().set_error("");
                    }
                    Err(e) => {
                        results.get().set_success(false);
                        results.get().set_error(format!("{}", e));
                    }
                }

                Ok(())
            }
            .instrument(span),
        )
    }

    fn get_config(
        self: Rc<Self>,
        params: kernel::GetConfigParams,
        mut results: kernel::GetConfigResults,
    ) -> Promise<(), capnp::Error> {
        let _span = tracing::info_span!("rpc", method = "get_config").entered();
        let path = pry!(pry!(pry!(params.get()).get_path()).to_str()).to_owned();

        match self.kernel.config_backend.get_content(&path) {
            Ok(content) => {
                results.get().set_content(&content);
                results.get().set_error("");
            }
            Err(e) => {
                results.get().set_content("");
                results.get().set_error(format!("{}", e));
            }
        }

        Promise::ok(())
    }

    // ========================================================================
    // Drift: Cross-Context Communication
    // ========================================================================

    fn get_context_id(
        self: Rc<Self>,
        params: kernel::GetContextIdParams,
        mut results: kernel::GetContextIdResults,
    ) -> Promise<(), capnp::Error> {
        let ctx_id = pry!(self.connection.borrow().require_context());
        let kernel_arc = self.kernel.kernel.clone();

        let span = extract_rpc_trace(pry!(params.get()).get_trace(), "get_context_id");
        Promise::from_future(
            async move {
                results.get().set_id(ctx_id.as_bytes());
                let drift = kernel_arc.drift().read().await;
                let label = drift
                    .get(ctx_id)
                    .and_then(|h| h.label.as_deref())
                    .unwrap_or("");
                results.get().set_label(label);
                Ok(())
            }
            .instrument(span),
        )
    }

    fn configure_llm(
        self: Rc<Self>,
        params: kernel::ConfigureLlmParams,
        mut results: kernel::ConfigureLlmResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let provider_name = pry!(pry!(params_reader.get_provider()).to_str()).to_owned();
        let model = pry!(pry!(params_reader.get_model()).to_str()).to_owned();
        let kernel_arc = self.kernel.kernel.clone();

        // Use explicit contextId if provided (16 bytes), otherwise connection's current
        let ctx_id_bytes = pry!(params_reader.get_context_id());
        let ctx_id = if ctx_id_bytes.len() == 16 {
            pry!(
                ContextId::try_from_slice(ctx_id_bytes)
                    .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
            )
        } else {
            pry!(self.connection.borrow().require_context())
        };

        let shared_kernel = self.kernel.clone();
        let span = extract_rpc_trace(params_reader.get_trace(), "configure_llm");
        Promise::from_future(
            async move {
                // Validate provider before persisting — never write bad data
                let config = kaijutsu_kernel::llm::ProviderConfig::new(&provider_name)
                    .with_default_model(&model);
                match kaijutsu_kernel::llm::RigProvider::from_config(&config) {
                    Ok(new_provider) => {
                        // Provider is valid — now persist
                        {
                            let db = shared_kernel.kernel_db.lock();
                            if let Err(e) =
                                db.update_model(ctx_id, Some(&provider_name), Some(&model))
                            {
                                log::warn!(
                                    "KernelDb update_model failed for {}: {}",
                                    ctx_id.short(),
                                    e
                                );
                            }
                        }
                        {
                            let mut drift = kernel_arc.drift().write().await;
                            let _ = drift.configure_llm(ctx_id, &provider_name, &model);
                        }
                        // Ensure provider is registered in LLM registry (for API client),
                        // but do NOT change kernel-wide defaults — model is per-context
                        let mut registry = kernel_arc.llm().write().await;
                        if registry.get(&provider_name).is_none() {
                            registry.register(&provider_name, Arc::new(new_provider));
                        }
                        results.get().set_success(true);
                        results.get().set_error("");
                        log::info!(
                            "Context {} model set: provider={}, model={}",
                            ctx_id.short(),
                            provider_name,
                            model
                        );
                    }
                    Err(e) => {
                        results.get().set_success(false);
                        results.get().set_error(format!("{}", e));
                        log::warn!(
                            "Failed to configure LLM for context {}: provider={}, model={}, err={}",
                            ctx_id.short(),
                            provider_name,
                            model,
                            e
                        );
                    }
                }

                Ok(())
            }
            .instrument(span),
        )
    }

    // REMOVED pre-1.0: use shell_execute("kj drift push ...") instead
    fn drift_push(
        self: Rc<Self>,
        _params: kernel::DriftPushParams,
        _results: kernel::DriftPushResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::failed(
            "driftPush removed — use shell_execute(\"kj drift push <ctx> <content>\")".into(),
        ))
    }

    // REMOVED pre-1.0: use shell_execute("kj drift flush") instead
    fn drift_flush(
        self: Rc<Self>,
        _params: kernel::DriftFlushParams,
        _results: kernel::DriftFlushResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::failed(
            "driftFlush removed — use shell_execute(\"kj drift flush\")".into(),
        ))
    }

    fn drift_queue(
        self: Rc<Self>,
        _params: kernel::DriftQueueParams,
        mut results: kernel::DriftQueueResults,
    ) -> Promise<(), capnp::Error> {
        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "drift_queue");
        Promise::from_future(
            async move {
                let drift = kernel_arc.drift().read().await;
                let queue = drift.queue();

                let mut list = results.get().init_staged(queue.len() as u32);
                for (i, drift_item) in queue.iter().enumerate() {
                    let mut entry = list.reborrow().get(i as u32);
                    entry.set_id(drift_item.id);
                    entry.set_source_ctx(drift_item.source_ctx.as_bytes());
                    entry.set_target_ctx(drift_item.target_ctx.as_bytes());
                    entry.set_content(&drift_item.content);
                    entry.set_source_model(drift_item.source_model.as_deref().unwrap_or(""));
                    entry.set_drift_kind(drift_kind_to_capnp(drift_item.drift_kind));
                    entry.set_created_at(drift_item.created_at);
                }

                Ok(())
            }
            .instrument(span),
        )
    }

    fn drift_cancel(
        self: Rc<Self>,
        params: kernel::DriftCancelParams,
        mut results: kernel::DriftCancelResults,
    ) -> Promise<(), capnp::Error> {
        let staged_id = pry!(params.get()).get_staged_id();
        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "drift_cancel");
        Promise::from_future(
            async move {
                let mut drift = kernel_arc.drift().write().await;
                let success = drift.cancel(staged_id);
                results.get().set_success(success);
                Ok(())
            }
            .instrument(span),
        )
    }

    // REMOVED pre-1.0: use shell_execute("kj drift pull ...") instead
    fn drift_pull(
        self: Rc<Self>,
        _params: kernel::DriftPullParams,
        _results: kernel::DriftPullResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::failed(
            "driftPull removed — use shell_execute(\"kj drift pull <ctx> [prompt]\")".into(),
        ))
    }

    // REMOVED pre-1.0: use shell_execute("kj drift merge ...") instead
    fn drift_merge(
        self: Rc<Self>,
        _params: kernel::DriftMergeParams,
        _results: kernel::DriftMergeResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::failed(
            "driftMerge removed — use shell_execute(\"kj drift merge [ctx]\")".into(),
        ))
    }

    // listAllContexts was removed — listContexts now reads from kernel's drift router

    // ========================================================================
    // LLM Configuration (Phase 5)
    // ========================================================================

    fn get_llm_config(
        self: Rc<Self>,
        params: kernel::GetLlmConfigParams,
        mut results: kernel::GetLlmConfigResults,
    ) -> Promise<(), capnp::Error> {
        let kernel_arc = self.kernel.kernel.clone();

        let span = extract_rpc_trace(pry!(params.get()).get_trace(), "get_llm_config");
        Promise::from_future(
            async move {
                let registry = kernel_arc.llm().read().await;

                let mut config = results.get().init_config();
                config.set_default_provider(registry.default_provider_name().unwrap_or(""));
                config.set_default_model(registry.default_model().unwrap_or(""));

                // Only include available providers (skip disabled/unconfigured)
                let provider_names: Vec<&str> = registry
                    .list()
                    .into_iter()
                    .filter(|name| registry.get(name).is_some())
                    .collect();
                let mut providers = config.init_providers(provider_names.len() as u32);
                for (i, name) in provider_names.iter().enumerate() {
                    let mut entry = providers.reborrow().get(i as u32);
                    entry.set_name(name);
                    entry.set_available(true);
                    if let Some(p) = registry.get(name) {
                        let avail = p.available_models();
                        entry.set_default_model(avail.first().copied().unwrap_or(""));
                    }
                    // Populate full models list from aliases + default
                    let model_ids = registry.models_for_provider(name);
                    let mut models_list = entry.init_models(model_ids.len() as u32);
                    for (j, model_id) in model_ids.iter().enumerate() {
                        models_list.set(j as u32, model_id);
                    }
                }

                Ok(())
            }
            .instrument(span),
        )
    }

    fn set_default_provider(
        self: Rc<Self>,
        params: kernel::SetDefaultProviderParams,
        mut results: kernel::SetDefaultProviderResults,
    ) -> Promise<(), capnp::Error> {
        let provider_name = pry!(pry!(pry!(params.get()).get_provider()).to_str()).to_owned();
        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "set_default_provider");
        Promise::from_future(
            async move {
                let mut registry = kernel_arc.llm().write().await;
                if registry.set_default(&provider_name) {
                    results.get().set_success(true);
                    results.get().set_error("");
                    log::info!("Default LLM provider set to: {}", provider_name);
                } else {
                    results.get().set_success(false);
                    results
                        .get()
                        .set_error(format!("provider '{}' not found", provider_name));
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    fn set_default_model(
        self: Rc<Self>,
        params: kernel::SetDefaultModelParams,
        mut results: kernel::SetDefaultModelResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let provider_name = pry!(pry!(params_reader.get_provider()).to_str()).to_owned();
        let model = pry!(pry!(params_reader.get_model()).to_str()).to_owned();
        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "set_default_model");
        Promise::from_future(
            async move {
                let mut registry = kernel_arc.llm().write().await;
                // Verify the provider exists
                if registry.get(&provider_name).is_none() {
                    results.get().set_success(false);
                    results
                        .get()
                        .set_error(format!("provider '{}' not found", provider_name));
                    return Ok(());
                }
                registry.set_default_model(&model);
                results.get().set_success(true);
                results.get().set_error("");
                log::info!(
                    "Default model set to: {} (provider: {})",
                    model,
                    provider_name
                );
                Ok(())
            }
            .instrument(span),
        )
    }

    // ========================================================================
    // Tool Filter Configuration (Phase 5)
    // ========================================================================

    fn get_tool_filter(
        self: Rc<Self>,
        _params: kernel::GetToolFilterParams,
        mut results: kernel::GetToolFilterResults,
    ) -> Promise<(), capnp::Error> {
        let _ = self.kernel.kernel.clone();
        let span = tracing::info_span!("rpc", method = "get_tool_filter");
        Promise::from_future(
            async move {
                // Kernel-scoped filter removed in Phase 1 M5; always report
                // `All` so existing clients see the permissive default.
                results.get().init_filter().set_all(());
                Ok(())
            }
            .instrument(span),
        )
    }

    fn set_tool_filter(
        self: Rc<Self>,
        _params: kernel::SetToolFilterParams,
        mut results: kernel::SetToolFilterResults,
    ) -> Promise<(), capnp::Error> {
        // Kernel-scoped filter removed in Phase 1 M5. Accept the call for
        // wire compat but no-op the state change; Phase 2 will reroute
        // through broker ContextToolBinding updates.
        results.get().set_success(true);
        results.get().set_error("kernel-scoped tool filter removed in Phase 1 M5 — no-op");
        Promise::ok(())
    }

    // ========================================================================
    // Per-Context Tool Filter
    // ========================================================================

    fn set_context_tool_filter(
        self: Rc<Self>,
        params: kernel::SetContextToolFilterParams,
        mut results: kernel::SetContextToolFilterResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let context_id_bytes = pry!(params_reader.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );

        let filter_reader = pry!(params_reader.get_filter());
        let filter = pry!(parse_tool_filter_from_capnp(filter_reader));

        let kernel_arc = self.kernel.kernel.clone();
        let shared_kernel = self.kernel.clone();

        let span = extract_rpc_trace(params_reader.get_trace(), "set_context_tool_filter");
        Promise::from_future(
            async move {
                // Write-through: KernelDb first
                {
                    let db = shared_kernel.kernel_db.lock();
                    if let Err(e) = db.update_tool_filter(context_id, &Some(filter.clone())) {
                        log::warn!(
                            "KernelDb update_tool_filter failed for {}: {}",
                            context_id.short(),
                            e
                        );
                    }
                }

                let mut drift = kernel_arc.drift().write().await;
                match drift.configure_tools(context_id, Some(filter)) {
                    Ok(()) => {
                        results.get().set_success(true);
                        results.get().set_error("");
                        log::info!("Context tool filter updated: {}", context_id);
                    }
                    Err(e) => {
                        results.get().set_success(false);
                        results.get().set_error(e.to_string());
                    }
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    fn get_context_tool_filter(
        self: Rc<Self>,
        params: kernel::GetContextToolFilterParams,
        mut results: kernel::GetContextToolFilterResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let context_id_bytes = pry!(params_reader.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );

        let kernel_arc = self.kernel.kernel.clone();

        let span = extract_rpc_trace(params_reader.get_trace(), "get_context_tool_filter");
        Promise::from_future(
            async move {
                let drift = kernel_arc.drift().read().await;
                let handle = drift.get(context_id);

                let mut res = results.get();
                match handle.and_then(|h| h.tool_filter.as_ref()) {
                    Some(filter) => {
                        res.set_has_filter(true);
                        serialize_tool_filter_to_capnp(filter, res.init_filter());
                    }
                    None => {
                        res.set_has_filter(false);
                        res.init_filter().set_all(());
                    }
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    // forkFiltered removed — consolidated to kj fork

    // =========================================================================
    // Shell Variable Introspection
    // =========================================================================

    fn get_shell_var(
        self: Rc<Self>,
        params: kernel::GetShellVarParams,
        mut results: kernel::GetShellVarResults,
    ) -> Promise<(), capnp::Error> {
        let name = match params.get().and_then(|p| p.get_name()) {
            Ok(n) => match n.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => {
                    return Promise::err(capnp::Error::failed(format!("invalid name: {}", e)));
                }
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("missing name: {}", e))),
        };

        let connection = self.connection.clone();

        let span = tracing::info_span!("rpc", method = "get_shell_var");
        Promise::from_future(
            async move {
                let kaish = {
                    let conn = connection.borrow();
                    conn.kaish
                        .clone()
                        .ok_or_else(|| capnp::Error::failed("kaish not initialized".into()))?
                };

                let value = kaish.get_var(&name).await;
                let mut builder = results.get();
                if let Some(val) = value {
                    builder.set_found(true);
                    value_to_shell_value(builder.init_value(), &val);
                } else {
                    builder.set_found(false);
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    fn set_shell_var(
        self: Rc<Self>,
        params: kernel::SetShellVarParams,
        mut results: kernel::SetShellVarResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let name = pry!(pry!(reader.get_name()).to_str()).to_owned();
        let value_reader = pry!(reader.get_value());
        let value = match shell_value_to_value(value_reader) {
            Ok(v) => v,
            Err(e) => {
                results.get().set_success(false);
                results.get().set_error(format!("invalid value: {}", e));
                return Promise::ok(());
            }
        };

        let connection = self.connection.clone();

        let span = tracing::info_span!("rpc", method = "set_shell_var");
        Promise::from_future(
            async move {
                let kaish = {
                    let conn = connection.borrow();
                    conn.kaish
                        .clone()
                        .ok_or_else(|| capnp::Error::failed("kaish not initialized".into()))?
                };

                kaish.set_var(&name, value).await;
                results.get().set_success(true);
                results.get().set_error("");
                Ok(())
            }
            .instrument(span),
        )
    }

    fn list_shell_vars(
        self: Rc<Self>,
        _params: kernel::ListShellVarsParams,
        mut results: kernel::ListShellVarsResults,
    ) -> Promise<(), capnp::Error> {
        let connection = self.connection.clone();

        let span = tracing::info_span!("rpc", method = "list_shell_vars");
        Promise::from_future(
            async move {
                let kaish = {
                    let conn = connection.borrow();
                    conn.kaish
                        .clone()
                        .ok_or_else(|| capnp::Error::failed("kaish not initialized".into()))?
                };

                let var_names = kaish.list_vars().await;
                let mut list_builder = results.get().init_vars(var_names.len() as u32);

                for (i, name) in var_names.iter().enumerate() {
                    let mut entry = list_builder.reborrow().get(i as u32);
                    entry.set_name(name);

                    // Fetch each variable's value for the full listing
                    if let Some(val) = kaish.get_var(name).await {
                        value_to_shell_value(entry.init_value(), &val);
                    }
                }

                Ok(())
            }
            .instrument(span),
        )
    }

    fn compact_context(
        self: Rc<Self>,
        params: kernel::CompactContextParams,
        mut results: kernel::CompactContextResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _span = extract_rpc_trace(p.get_trace(), "compact_context").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );

        let _ctx_span = if let Ok(drift) = self.kernel.kernel.drift().try_read() {
            let trace_id = drift.trace_id_for_context(context_id).unwrap_or([0u8; 16]);
            Some(kaijutsu_telemetry::context_root_span(&trace_id, "compact_document").entered())
        } else {
            None
        };

        // Per-block DTE stores don't need compaction — each block's DTE
        // is already minimal. This is intentionally a no-op; sync_generation
        // stays 0 and SyncReset is never emitted. If compaction is ever
        // reintroduced, bump DocumentEntry.sync_generation and emit
        // BlockFlow::SyncReset so clients can resync their frontier.
        let mut r = results.get();
        r.set_new_size(0);
        r.set_generation(0);
        Promise::ok(())
    }

    // =========================================================================
    // Input document operations (compose scratchpad)
    // =========================================================================

    fn edit_input(
        self: Rc<Self>,
        params: kernel::EditInputParams,
        mut results: kernel::EditInputResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "edit_input").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let pos = p.get_pos() as usize;
        let insert = pry!(pry!(p.get_insert()).to_str()).to_owned();
        let delete = p.get_delete() as usize;

        log::debug!(
            "edit_input: context={}, pos={}, insert_len={}, delete={}",
            context_id,
            pos,
            insert.len(),
            delete
        );

        let documents = &self.kernel.documents;

        match documents.edit_input(context_id, pos, &insert, delete) {
            Ok(_ops) => {
                // edit_input returns the ops and emits InputDocFlow::TextOps via FlowBus.
                // The version is implicit from the DTE document; return 0 as ack.
                // Clients use the FlowBus subscription for real-time sync.
                results.get().set_ack_version(0);
                Promise::ok(())
            }
            Err(e) => Promise::err(capnp::Error::failed(format!("edit_input failed: {}", e))),
        }
    }

    fn get_input_state(
        self: Rc<Self>,
        params: kernel::GetInputStateParams,
        mut results: kernel::GetInputStateResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "get_input_state").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );

        log::debug!("get_input_state: context={}", context_id);

        let documents = &self.kernel.documents;

        match documents.get_input_state(context_id) {
            Ok((content, ops, version)) => {
                let mut r = results.get();
                r.set_content(&content);
                r.set_ops(&ops);
                r.set_version(version);
                Promise::ok(())
            }
            Err(e) => Promise::err(capnp::Error::failed(format!(
                "get_input_state failed: {}",
                e
            ))),
        }
    }

    fn push_input_ops(
        self: Rc<Self>,
        params: kernel::PushInputOpsParams,
        mut results: kernel::PushInputOpsResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "push_input_ops").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let ops_data = pry!(p.get_ops()).to_vec();

        log::debug!(
            "push_input_ops: context={}, ops_len={}",
            context_id,
            ops_data.len()
        );

        let documents = &self.kernel.documents;

        match documents.merge_input_ops(context_id, &ops_data) {
            Ok(version) => {
                results.get().set_ack_version(version);
                Promise::ok(())
            }
            Err(e) => Promise::err(capnp::Error::failed(format!(
                "push_input_ops failed: {}",
                e
            ))),
        }
    }

    fn submit_input(
        self: Rc<Self>,
        params: kernel::SubmitInputParams,
        mut results: kernel::SubmitInputResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let trace_span = extract_rpc_trace(p.get_trace(), "submit_input");
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let is_shell = pry!(p.get_mode()) == InputMode::Shell;

        log::info!("submit_input: context={} shell={}", context_id, is_shell);

        let kernel = self.kernel.clone();
        let connection = self.connection.clone();
        let user_agent_id = self.connection.borrow().principal.id;

        Promise::from_future(
            async move {
                let documents = kernel.documents.clone();

                // Read text first, validate, THEN clear — avoids clearing compose
                // on whitespace-only input (InputCleared would fire with no block created).
                let text = documents
                    .get_input_text(context_id)
                    .map_err(|e| capnp::Error::failed(format!("get_input_text: {}", e)))?;
                let text = text.trim().to_string();
                if text.is_empty() {
                    return Err(capnp::Error::failed("input is empty".into()));
                }
                // Input has content — now clear it
                documents
                    .clear_input(context_id)
                    .map_err(|e| capnp::Error::failed(format!("clear_input failed: {}", e)))?;

                if is_shell {
                    let command_block_id = execute_shell_command(
                        &text,
                        context_id,
                        user_agent_id,
                        true,
                        &kernel,
                        &connection,
                    )
                    .await?;

                    let mut block_id_builder = results.get().init_command_block_id();
                    set_block_id_builder(&mut block_id_builder, &command_block_id);
                } else {
                    // Chat prompt — create user message block and invoke LLM

                    // Document must exist — join_context is the sole creator
                    if documents.get(context_id).is_none() {
                        return Err(capnp::Error::failed(format!(
                            "context {} not found — call join_context first",
                            context_id
                        )));
                    }

                    // Build ToolContext from connection state
                    let (session_id, kaish_ref) = {
                        let conn = connection.borrow();
                        (conn.session_id, conn.kaish.clone())
                    };
                    let cwd = match &kaish_ref {
                        Some(k) => k.cwd().await,
                        None => std::path::PathBuf::from("/"),
                    };
                    let tool_ctx = kaijutsu_kernel::ExecContext::new(
                        user_agent_id,
                        context_id,
                        cwd,
                        session_id,
                        kernel.id,
                    );

                    // Create user message block at the end of the document
                    let last_block = documents.last_block_id(context_id);
                    let user_block_id = documents
                        .insert_block_as(
                            context_id,
                            None,
                            last_block.as_ref(),
                            Role::User,
                            BlockKind::Text,
                            &text,
                            Status::Done,
                            ContentType::Plain,
                            Some(user_agent_id),
                        )
                        .map_err(|e| {
                            capnp::Error::failed(format!("failed to insert user block: {}", e))
                        })?;

                    // Spawn LLM streaming in background
                    spawn_llm_for_prompt(
                        &kernel,
                        context_id,
                        &text,
                        None,
                        &user_block_id,
                        tool_ctx,
                        user_agent_id,
                    )
                    .await?;

                    let mut block_id_builder = results.get().init_command_block_id();
                    set_block_id_builder(&mut block_id_builder, &user_block_id);
                }

                Ok(())
            }
            .instrument(trace_span),
        )
    }

    fn clear_input(
        self: Rc<Self>,
        params: kernel::ClearInputParams,
        _results: kernel::ClearInputResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "clear_input").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );

        log::info!("clear_input: context={}", context_id);

        match self.kernel.documents.clear_input(context_id) {
            Ok(_text) => Promise::ok(()),
            Err(e) => Promise::err(capnp::Error::failed(format!("clear_input failed: {}", e))),
        }
    }

    fn search_similar(
        self: Rc<Self>,
        params: kernel::SearchSimilarParams,
        mut results: kernel::SearchSimilarResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let span = extract_rpc_trace(p.get_trace(), "search_similar");
        let query = pry!(pry!(p.get_query()).to_str()).to_string();
        let k = p.get_k() as usize;
        let kernel = self.kernel.clone();

        Promise::from_future(
            async move {
                let search_results = match &kernel.semantic_index {
                    Some(idx) => {
                        let idx = idx.clone();
                        let q = query.clone();
                        tokio::task::spawn_blocking(move || idx.search(&q, k))
                            .await
                            .map_err(|e| capnp::Error::failed(format!("spawn_blocking: {}", e)))?
                            .map_err(|e| capnp::Error::failed(format!("search: {}", e)))?
                    }
                    None => vec![],
                };

                // Populate labels from drift router
                let drift = kernel.kernel.drift().read().await;

                let mut list = results.get().init_results(search_results.len() as u32);
                for (i, r) in search_results.iter().enumerate() {
                    let mut entry = list.reborrow().get(i as u32);
                    entry.set_context_id(r.context_id.as_bytes());
                    entry.set_score(r.score);
                    if let Some(handle) = drift.get(r.context_id)
                        && let Some(ref label) = handle.label
                    {
                        entry.set_label(label);
                    }
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    fn get_neighbors(
        self: Rc<Self>,
        params: kernel::GetNeighborsParams,
        mut results: kernel::GetNeighborsResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let span = extract_rpc_trace(p.get_trace(), "get_neighbors");
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let k = p.get_k() as usize;
        let kernel = self.kernel.clone();

        Promise::from_future(
            async move {
                let search_results = match &kernel.semantic_index {
                    Some(idx) => {
                        let idx = idx.clone();
                        tokio::task::spawn_blocking(move || idx.neighbors(context_id, k))
                            .await
                            .map_err(|e| capnp::Error::failed(format!("spawn_blocking: {}", e)))?
                            .map_err(|e| capnp::Error::failed(format!("neighbors: {}", e)))?
                    }
                    None => vec![],
                };

                let drift = kernel.kernel.drift().read().await;

                let mut list = results.get().init_results(search_results.len() as u32);
                for (i, r) in search_results.iter().enumerate() {
                    let mut entry = list.reborrow().get(i as u32);
                    entry.set_context_id(r.context_id.as_bytes());
                    entry.set_score(r.score);
                    if let Some(handle) = drift.get(r.context_id)
                        && let Some(ref label) = handle.label
                    {
                        entry.set_label(label);
                    }
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    fn get_clusters(
        self: Rc<Self>,
        params: kernel::GetClustersParams,
        mut results: kernel::GetClustersResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let span = extract_rpc_trace(p.get_trace(), "get_clusters");
        let min_cluster_size = p.get_min_cluster_size() as usize;
        let kernel = self.kernel.clone();

        Promise::from_future(
            async move {
                let clusters = match &kernel.semantic_index {
                    Some(idx) => {
                        let idx = idx.clone();
                        tokio::task::spawn_blocking(move || idx.clusters(min_cluster_size))
                            .await
                            .map_err(|e| capnp::Error::failed(format!("spawn_blocking: {}", e)))?
                            .map_err(|e| capnp::Error::failed(format!("clusters: {}", e)))?
                    }
                    None => vec![],
                };

                let mut list = results.get().init_clusters(clusters.len() as u32);
                for (i, c) in clusters.iter().enumerate() {
                    let mut entry = list.reborrow().get(i as u32);
                    entry.set_cluster_id(c.cluster_id as u32);
                    let mut ids = entry.init_context_ids(c.context_ids.len() as u32);
                    for (j, ctx_id) in c.context_ids.iter().enumerate() {
                        ids.set(j as u32, ctx_id.as_bytes());
                    }
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    fn get_blocks(
        self: Rc<Self>,
        params: kernel::GetBlocksParams,
        mut results: kernel::GetBlocksResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "get_blocks").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );

        let query_reader = pry!(p.get_query());
        let query = pry!(parse_block_query(&query_reader));

        let documents = &self.kernel.documents;

        let blocks = match query {
            kaijutsu_types::BlockQuery::All => {
                pry!(
                    documents
                        .block_snapshots(context_id)
                        .map_err(|e| capnp::Error::failed(e.to_string()))
                )
            }
            kaijutsu_types::BlockQuery::ByIds(ids) => {
                if ids.is_empty() {
                    return Promise::err(capnp::Error::failed(
                        "byIds requires at least one block ID".into(),
                    ));
                }
                pry!(
                    documents
                        .get_blocks_by_ids(context_id, &ids)
                        .map_err(|e| capnp::Error::failed(e.to_string()))
                )
            }
            kaijutsu_types::BlockQuery::ByFilter(filter) => {
                pry!(
                    filter
                        .validate()
                        .map_err(|e| capnp::Error::failed(e.to_string()))
                );
                pry!(
                    documents
                        .query_blocks(context_id, &filter)
                        .map_err(|e| capnp::Error::failed(e.to_string()))
                )
            }
        };

        let mut block_list = results.get().init_blocks(blocks.len() as u32);
        for (i, block) in blocks.iter().enumerate() {
            let mut block_builder = block_list.reborrow().get(i as u32);
            set_block_snapshot(&mut block_builder, block);
        }

        Promise::ok(())
    }

    fn get_context_sync(
        self: Rc<Self>,
        params: kernel::GetContextSyncParams,
        mut results: kernel::GetContextSyncResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "get_context_sync").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );

        let documents = &self.kernel.documents;
        let (ops, version) = pry!(
            documents
                .context_sync_state(context_id)
                .map_err(|e| capnp::Error::failed(e.to_string()))
        );

        let mut r = results.get();
        r.set_context_id(context_id.as_bytes());
        r.set_ops(&ops);
        r.set_version(version);

        Promise::ok(())
    }

    fn subscribe_blocks_filtered(
        self: Rc<Self>,
        params: kernel::SubscribeBlocksFilteredParams,
        _results: kernel::SubscribeBlocksFilteredResults,
    ) -> Promise<(), capnp::Error> {
        let _span = tracing::info_span!("rpc", method = "subscribe_blocks_filtered").entered();
        let p = pry!(params.get());
        let callback = pry!(p.get_callback());

        // Parse the BlockEventFilter from the capnp struct
        let filter = if p.has_filter() {
            let f = pry!(p.get_filter());
            parse_block_event_filter(f)
        } else {
            kaijutsu_types::BlockEventFilter::default()
        };

        let has_filter = filter.has_active_constraint();

        // Determine which FlowBus topics to subscribe to based on event_types filter.
        // If event_types is set, only subscribe to matching topics (leveraging Phase 2 topic partitioning).
        let subscribe_pattern = if !filter.event_types.is_empty() {
            // If only one event type, subscribe to just that topic for maximum efficiency.
            // Otherwise fall back to wildcard.
            if filter.event_types.len() == 1 {
                match filter.event_types[0] {
                    kaijutsu_types::BlockFlowKind::Inserted => "block.inserted",
                    kaijutsu_types::BlockFlowKind::TextOps => "block.text_ops",
                    kaijutsu_types::BlockFlowKind::Deleted => "block.deleted",
                    kaijutsu_types::BlockFlowKind::StatusChanged => "block.status",
                    kaijutsu_types::BlockFlowKind::CollapsedChanged => "block.collapsed",
                    kaijutsu_types::BlockFlowKind::ExcludedChanged => "block.excluded",
                    kaijutsu_types::BlockFlowKind::Moved => "block.moved",
                    kaijutsu_types::BlockFlowKind::SyncReset => "block.sync_reset",
                    kaijutsu_types::BlockFlowKind::OutputChanged => "block.output",
                    kaijutsu_types::BlockFlowKind::MetadataChanged => "block.metadata",
                    kaijutsu_types::BlockFlowKind::ContextSwitched => "block.context_switched",
                }
            } else {
                "block.*"
            }
        } else {
            "block.*"
        };

        {
            let block_flows = self.kernel.kernel.block_flows().clone();
            let input_flows = self.kernel.documents.input_flows().cloned();
            let kernel_id = self.kernel.id;

            tokio::task::spawn_local(async move {
                let mut block_sub = block_flows.subscribe(subscribe_pattern);
                let mut input_sub = input_flows.map(|f| f.subscribe("input.*"));
                log::debug!(
                    "Started filtered FlowBus subscription for kernel {} (filter_active={}, pattern={})",
                    kernel_id.to_hex(),
                    has_filter,
                    subscribe_pattern
                );

                loop {
                    let success = tokio::select! {
                        Some(msg) = block_sub.recv() => {
                            // Apply server-side filter before serializing to wire
                            if has_filter && !msg.payload.matches_filter(&filter) {
                                continue;
                            }

                            // Same dispatch as subscribe_blocks — forward to callback
                            match msg.payload {
                                BlockFlow::Inserted { context_id, ref block, ref after_id, ref ops, .. } => {
                                    let mut req = callback.on_block_inserted_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                        params.set_has_after_id(after_id.is_some());
                                        if let Some(after) = after_id {
                                            set_block_id_builder(&mut params.reborrow().init_after_id(), after);
                                        }
                                        params.set_ops(ops);
                                        let mut block_state = params.init_block();
                                        set_block_snapshot(&mut block_state, block);
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                BlockFlow::Deleted { context_id, ref block_id, .. } => {
                                    let mut req = callback.on_block_deleted_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                        set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                BlockFlow::StatusChanged { context_id, ref block_id, status, ref output, .. } => {
                                    let mut req = callback.on_block_status_changed_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                        set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
                                        params.set_status(status_to_capnp(status));
                                        if let Some(output_data) = output {
                                            build_output_data(params.reborrow().init_output_data(), output_data);
                                        }
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                BlockFlow::CollapsedChanged { context_id, ref block_id, collapsed, .. } => {
                                    let mut req = callback.on_block_collapsed_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                        set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
                                        params.set_collapsed(collapsed);
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                BlockFlow::ExcludedChanged { context_id, ref block_id, excluded, .. } => {
                                    let mut req = callback.on_block_excluded_changed_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                        set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
                                        params.set_excluded(excluded);
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                BlockFlow::Moved { context_id, ref block_id, ref after_id, .. } => {
                                    let mut req = callback.on_block_moved_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                        set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
                                        params.set_has_after_id(after_id.is_some());
                                        if let Some(after) = after_id {
                                            set_block_id_builder(&mut params.reborrow().init_after_id(), after);
                                        }
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                BlockFlow::TextOps { context_id, ref block_id, ref ops, .. } => {
                                    let mut req = callback.on_block_text_ops_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                        set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
                                        params.set_ops(ops);
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                BlockFlow::SyncReset { context_id, generation } => {
                                    let mut req = callback.on_sync_reset_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                        params.set_generation(generation);
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                BlockFlow::ContextSwitched { context_id } => {
                                    let mut req = callback.on_context_switched_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                // No wire protocol for these yet — drop silently
                                BlockFlow::OutputChanged { .. }
                                | BlockFlow::MetadataChanged { .. } => true,
                            }
                        }
                        Some(msg) = async {
                            match &mut input_sub {
                                Some(sub) => sub.recv().await,
                                None => std::future::pending().await,
                            }
                        } => {
                            match msg.payload {
                                InputDocFlow::TextOps { context_id, ref ops, .. } => {
                                    let mut req = callback.on_input_text_ops_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                        params.set_ops(ops);
                                    }
                                    req.send().promise.await.is_ok()
                                }
                                InputDocFlow::Cleared { context_id } => {
                                    let mut req = callback.on_input_cleared_request();
                                    {
                                        let mut params = req.get();
                                        params.set_context_id(context_id.as_bytes());
                                    }
                                    req.send().promise.await.is_ok()
                                }
                            }
                        }
                        else => break,
                    };

                    if !success {
                        log::debug!(
                            "Filtered FlowBus bridge task for kernel {} stopping: callback failed",
                            kernel_id
                        );
                        break;
                    }
                }

                log::debug!(
                    "Filtered FlowBus bridge task for kernel {} ended",
                    kernel_id
                );
            });
        }
        Promise::ok(())
    }

    // =========================================================================
    // Context Interrupt
    // =========================================================================

    fn interrupt_context(
        self: Rc<Self>,
        params: kernel::InterruptContextParams,
        mut results: kernel::InterruptContextResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let context_id_bytes = pry!(params_reader.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let immediate = params_reader.get_immediate();

        // Cancel kaish synchronously — cancel() is a sync method.
        if immediate && let Some(kaish) = self.connection.borrow().kaish.as_ref() {
            kaish.cancel();
        }

        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let success = if let Some(interrupt) = kernel.get_interrupt(context_id).await {
                if immediate {
                    interrupt.hard();
                } else {
                    interrupt.soft();
                }
                true
            } else {
                // No active stream for this context — no-op (idempotent).
                false
            };

            log::info!(
                "interruptContext: context={}, immediate={}, success={}",
                context_id,
                immediate,
                success
            );

            results.get().set_success(success);
            Ok(())
        })
    }

    fn list_presets(
        self: Rc<Self>,
        params: kernel::ListPresetsParams,
        mut results: kernel::ListPresetsResults,
    ) -> Promise<(), capnp::Error> {
        let _span = extract_rpc_trace(pry!(params.get()).get_trace(), "list_presets");
        let kernel_id = self.kernel.id;

        let presets = {
            let db = self.kernel.kernel_db.lock();
            db.list_presets(kernel_id).unwrap_or_default()
        };

        let mut list = results.get().init_presets(presets.len() as u32);
        for (i, preset) in presets.iter().enumerate() {
            let mut p = list.reborrow().get(i as u32);
            p.set_id(preset.preset_id.as_bytes());
            p.set_label(&preset.label);
            p.set_description(preset.description.as_deref().unwrap_or(""));
            p.set_provider(preset.provider.as_deref().unwrap_or(""));
            p.set_model(preset.model.as_deref().unwrap_or(""));
        }

        Promise::ok(())
    }

    fn set_context_state(
        self: Rc<Self>,
        params: kernel::SetContextStateParams,
        mut results: kernel::SetContextStateResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "set_context_state").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let state_str = pry!(pry!(p.get_state()).to_str());
        let new_state = match kaijutsu_types::ContextState::from_str(state_str) {
            Ok(s) => s,
            Err(_) => {
                results.get().set_success(false);
                results
                    .get()
                    .set_error(&format!("unknown state '{state_str}'"));
                return Promise::ok(());
            }
        };

        // Validate transition: only Staging → Live allowed in v1
        let drift_router = self.kernel.kernel.drift().clone();
        {
            let drift = match drift_router.try_read() {
                Ok(d) => d,
                Err(_) => {
                    results.get().set_success(false);
                    results.get().set_error("drift router busy");
                    return Promise::ok(());
                }
            };
            if let Some(current) = drift.context_state(context_id) {
                use kaijutsu_types::ContextState::*;
                match (current, new_state) {
                    (Staging, Live) => {} // allowed
                    (same, target) if same == target => {
                        results.get().set_success(true);
                        return Promise::ok(());
                    }
                    (from, to) => {
                        results.get().set_success(false);
                        results
                            .get()
                            .set_error(&format!("transition {from} → {to} not allowed"));
                        return Promise::ok(());
                    }
                }
            } else {
                results.get().set_success(false);
                results.get().set_error("context not found in drift router");
                return Promise::ok(());
            }
        }

        // Update DriftRouter
        {
            let mut drift = match drift_router.try_write() {
                Ok(d) => d,
                Err(_) => {
                    results.get().set_success(false);
                    results.get().set_error("drift router busy (write)");
                    return Promise::ok(());
                }
            };
            if let Err(e) = drift.set_state(context_id, new_state) {
                results.get().set_success(false);
                results.get().set_error(&e.to_string());
                return Promise::ok(());
            }
        }

        // Update KernelDb
        {
            let db = self.kernel.kernel_db.lock();
            if let Err(e) = db.update_context_state(context_id, new_state) {
                log::error!("set_context_state: KernelDb update failed: {e}");
                // DriftRouter was already updated — log but don't fail
            }
        }

        log::info!(
            "set_context_state: context={} state={}",
            context_id.short(),
            new_state
        );
        results.get().set_success(true);
        Promise::ok(())
    }

    fn set_block_excluded(
        self: Rc<Self>,
        params: kernel::SetBlockExcludedParams,
        mut results: kernel::SetBlockExcludedResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "set_block_excluded").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let block_id_reader = pry!(p.get_block_id());
        let block_id = pry!(parse_block_id_from_reader(&block_id_reader));
        let excluded = p.get_excluded();

        // Enforce: excluded toggling allowed in Live or Staging state
        {
            let drift = match self.kernel.kernel.drift().try_read() {
                Ok(d) => d,
                Err(_) => {
                    return Promise::err(capnp::Error::failed("drift router busy".into()));
                }
            };
            match drift.context_state(context_id) {
                Some(kaijutsu_types::ContextState::Live)
                | Some(kaijutsu_types::ContextState::Staging) => {} // allowed
                Some(state) => {
                    return Promise::err(capnp::Error::failed(format!(
                        "cannot toggle excluded in {state} state (only live/staging)"
                    )));
                }
                None => {
                    return Promise::err(capnp::Error::failed("context not found".into()));
                }
            }
        }

        if let Err(e) = self
            .kernel
            .documents
            .set_excluded(context_id, &block_id, excluded)
        {
            return Promise::err(capnp::Error::failed(e.to_string()));
        }

        match self.kernel.documents.version(context_id) {
            Ok(ack) => {
                results.get().set_ack_version(ack);
                Promise::ok(())
            }
            Err(e) => Promise::err(capnp::Error::failed(e.to_string())),
        }
    }
}

// ============================================================================
// Shell Value Conversion Helpers
// ============================================================================

/// Convert kaish `ast::Value` → Cap'n Proto `ShellValue` builder.
fn value_to_shell_value(mut builder: shell_value::Builder<'_>, value: &kaish_kernel::ast::Value) {
    use kaish_kernel::ast::Value;
    match value {
        Value::Null => builder.set_null(()),
        Value::Bool(b) => builder.set_bool(*b),
        Value::Int(i) => builder.set_int(*i),
        Value::Float(f) => builder.set_float(*f),
        Value::String(s) => builder.set_string(s),
        Value::Json(j) => builder.set_json(serde_json::to_string(j).unwrap_or_default()),
        Value::Blob(b) => builder.set_blob(&b.id),
    }
}

/// Convert Cap'n Proto `ShellValue` reader → kaish `ast::Value`.
fn shell_value_to_value(
    reader: shell_value::Reader<'_>,
) -> Result<kaish_kernel::ast::Value, capnp::Error> {
    use kaish_kernel::ast::Value;
    match reader.which()? {
        shell_value::Null(()) => Ok(Value::Null),
        shell_value::Bool(b) => Ok(Value::Bool(b)),
        shell_value::Int(i) => Ok(Value::Int(i)),
        shell_value::Float(f) => Ok(Value::Float(f)),
        shell_value::String(s) => Ok(Value::String(s?.to_str()?.to_owned())),
        shell_value::Json(j) => {
            let json_str = j?.to_str()?;
            let parsed: serde_json::Value = serde_json::from_str(json_str)
                .map_err(|e| capnp::Error::failed(format!("invalid JSON: {}", e)))?;
            Ok(Value::Json(parsed))
        }
        shell_value::Blob(b) => {
            let id = b?.to_str()?.to_owned();
            Ok(Value::Blob(kaish_kernel::ast::BlobRef {
                id,
                size: 0,
                content_type: String::new(),
                hash: None,
            }))
        }
    }
}

// ============================================================================
// OutputData Build Helpers
// ============================================================================

fn entry_type_to_capnp(et: kaijutsu_types::OutputEntryType) -> crate::kaijutsu_capnp::EntryType {
    use crate::kaijutsu_capnp::EntryType;
    use kaijutsu_types::OutputEntryType;
    match et {
        OutputEntryType::Text => EntryType::Text,
        OutputEntryType::File => EntryType::File,
        OutputEntryType::Directory => EntryType::Directory,
        OutputEntryType::Executable => EntryType::Executable,
        OutputEntryType::Symlink => EntryType::Symlink,
        _ => EntryType::Text,
    }
}

fn build_output_node(
    mut builder: crate::kaijutsu_capnp::output_node::Builder<'_>,
    node: &kaijutsu_types::OutputNode,
) {
    builder.set_name(&node.name);
    builder.set_entry_type(entry_type_to_capnp(node.entry_type));
    if let Some(ref text) = node.text {
        builder.set_has_text(true);
        builder.set_text(text);
    }
    if !node.cells.is_empty() {
        let mut cells = builder.reborrow().init_cells(node.cells.len() as u32);
        for (i, cell) in node.cells.iter().enumerate() {
            cells.set(i as u32, cell);
        }
    }
    if !node.children.is_empty() {
        let mut children = builder.reborrow().init_children(node.children.len() as u32);
        for (i, child) in node.children.iter().enumerate() {
            build_output_node(children.reborrow().get(i as u32), child);
        }
    }
}

fn build_output_data(
    mut builder: crate::kaijutsu_capnp::output_data::Builder<'_>,
    data: &kaijutsu_types::OutputData,
) {
    if let Some(ref headers) = data.headers {
        builder.set_has_headers(true);
        let mut hlist = builder.reborrow().init_headers(headers.len() as u32);
        for (i, h) in headers.iter().enumerate() {
            hlist.set(i as u32, h);
        }
    }
    if !data.root.is_empty() {
        let mut root = builder.reborrow().init_root(data.root.len() as u32);
        for (i, node) in data.root.iter().enumerate() {
            build_output_node(root.reborrow().get(i as u32), node);
        }
    }
}

// ============================================================================
// Agent Helper Functions
// ============================================================================

use crate::kaijutsu_capnp::{
    AgentCapability as CapnpAgentCapability, AgentStatus as CapnpAgentStatus,
};

/// Convert capnp AgentCapability to kernel AgentCapability.
fn capnp_to_agent_capability(cap: CapnpAgentCapability) -> Option<AgentCapability> {
    match cap {
        CapnpAgentCapability::SpellCheck => Some(AgentCapability::SpellCheck),
        CapnpAgentCapability::Grammar => Some(AgentCapability::Grammar),
        CapnpAgentCapability::Format => Some(AgentCapability::Format),
        CapnpAgentCapability::Review => Some(AgentCapability::Review),
        CapnpAgentCapability::Generate => Some(AgentCapability::Generate),
        CapnpAgentCapability::Refactor => Some(AgentCapability::Refactor),
        CapnpAgentCapability::Explain => Some(AgentCapability::Explain),
        CapnpAgentCapability::Translate => Some(AgentCapability::Translate),
        CapnpAgentCapability::Summarize => Some(AgentCapability::Summarize),
        CapnpAgentCapability::Custom => Some(AgentCapability::Custom),
    }
}

/// Set AgentInfo fields on a Cap'n Proto builder.
fn set_agent_info(builder: &mut agent_info::Builder, info: &AgentInfo) {
    builder.set_nick(&info.nick);
    builder.set_instance(&info.instance);
    builder.set_provider(&info.provider);
    builder.set_model_id(&info.model_id);

    // Set capabilities
    let caps_len = info.capabilities.len() as u32;
    let mut caps = builder.reborrow().init_capabilities(caps_len);
    for (i, cap) in info.capabilities.iter().enumerate() {
        caps.set(i as u32, agent_capability_to_capnp(*cap));
    }

    // Set status
    builder.set_status(agent_status_to_capnp(info.status));

    // Set timestamps
    builder.set_attached_at(info.attached_at);
    builder.set_last_activity(info.last_activity);
}

/// Convert kernel AgentCapability to capnp.
fn agent_capability_to_capnp(cap: AgentCapability) -> CapnpAgentCapability {
    match cap {
        AgentCapability::SpellCheck => CapnpAgentCapability::SpellCheck,
        AgentCapability::Grammar => CapnpAgentCapability::Grammar,
        AgentCapability::Format => CapnpAgentCapability::Format,
        AgentCapability::Review => CapnpAgentCapability::Review,
        AgentCapability::Generate => CapnpAgentCapability::Generate,
        AgentCapability::Refactor => CapnpAgentCapability::Refactor,
        AgentCapability::Explain => CapnpAgentCapability::Explain,
        AgentCapability::Translate => CapnpAgentCapability::Translate,
        AgentCapability::Summarize => CapnpAgentCapability::Summarize,
        AgentCapability::Custom => CapnpAgentCapability::Custom,
    }
}

/// Convert kernel AgentStatus to capnp.
fn agent_status_to_capnp(status: AgentStatus) -> CapnpAgentStatus {
    match status {
        AgentStatus::Ready => CapnpAgentStatus::Ready,
        AgentStatus::Busy => CapnpAgentStatus::Busy,
        AgentStatus::Offline => CapnpAgentStatus::Offline,
    }
}

// ============================================================================
// Cap'n Proto ↔ Rust Type Helpers
// ============================================================================

/// Parse a ToolFilter from a Cap'n Proto ToolFilterConfig reader.
fn parse_tool_filter_from_capnp(
    reader: tool_filter_config::Reader<'_>,
) -> Result<ToolFilter, capnp::Error> {
    match reader.which()? {
        tool_filter_config::All(()) => Ok(ToolFilter::All),
        tool_filter_config::AllowList(list) => {
            let list = list?;
            let mut tools = std::collections::HashSet::new();
            for i in 0..list.len() {
                let name = list.get(i)?.to_str()?;
                tools.insert(name.to_string());
            }
            Ok(ToolFilter::AllowList(tools))
        }
        tool_filter_config::DenyList(list) => {
            let list = list?;
            let mut tools = std::collections::HashSet::new();
            for i in 0..list.len() {
                let name = list.get(i)?.to_str()?;
                tools.insert(name.to_string());
            }
            Ok(ToolFilter::DenyList(tools))
        }
    }
}

/// Serialize a ToolFilter into a Cap'n Proto ToolFilterConfig builder.
fn serialize_tool_filter_to_capnp(
    filter: &ToolFilter,
    mut builder: tool_filter_config::Builder<'_>,
) {
    match filter {
        ToolFilter::All => builder.set_all(()),
        ToolFilter::AllowList(tools) => {
            let tools_vec: Vec<&str> = tools.iter().map(|s| s.as_str()).collect();
            let mut list = builder.init_allow_list(tools_vec.len() as u32);
            for (i, tool) in tools_vec.iter().enumerate() {
                list.set(i as u32, tool);
            }
        }
        ToolFilter::DenyList(tools) => {
            let tools_vec: Vec<&str> = tools.iter().map(|s| s.as_str()).collect();
            let mut list = builder.init_deny_list(tools_vec.len() as u32);
            for (i, tool) in tools_vec.iter().enumerate() {
                list.set(i as u32, tool);
            }
        }
    }
}

// ============================================================================
// Shell Execution Dispatch
// ============================================================================
//
// LLM streaming + agentic loop moved to `crate::llm_stream`
// (build_tool_definitions, spawn_llm_for_prompt, tool_kind_for_category,
// process_llm_stream all live there).

/// Create kaish, insert ToolCall + ToolResult blocks, spawn execution.
///
/// Shared by `shell_execute` (direct RPC) and `submit_input` (shell mode).
/// Exit codes 0/2/3 map to Done; everything else is Error.
async fn execute_shell_command(
    code: &str,
    context_id: ContextId,
    user_agent_id: PrincipalId,
    user_initiated: bool,
    kernel: &SharedKernelState,
    connection: &Rc<RefCell<ConnectionState>>,
) -> Result<kaijutsu_crdt::BlockId, capnp::Error> {
    // Get or create embedded kaish executor with real connection identity
    let (kaish, newly_created) = {
        let mut conn = connection.borrow_mut();
        let was_none = conn.kaish.is_none();

        if was_none {
            log::info!("Creating embedded kaish for kernel {}", kernel.id.to_hex());
            let kj_disp = kernel.kj_dispatcher.clone();
            let kj_principal = conn.principal.id;
            let session_contexts = conn.session_contexts.clone();
            match EmbeddedKaish::with_identity_and_db(
                &format!(
                    "{}-{}-{}",
                    kernel.name,
                    conn.principal.username,
                    conn.session_id.short()
                ),
                kernel.documents.clone(),
                kernel.kernel.clone(),
                None,
                conn.principal.id,
                conn.require_context()?,
                conn.session_id,
                kernel.id,
                Some(&kernel.kernel_db),
                session_contexts.clone(),
                |map, sid, tools| {
                    let block_source: Arc<dyn kaijutsu_index::BlockSource> =
                        Arc::new(BlockStoreSource(kernel.documents.clone()));
                    tools.register(crate::kj_builtin::KjBuiltin::new(
                        kj_disp,
                        map,
                        kj_principal,
                        sid,
                        kernel.semantic_index.clone(),
                        block_source,
                    ));
                },
            ) {
                Ok(kaish) => {
                    conn.kaish = Some(Rc::new(kaish));
                }
                Err(e) => {
                    log::error!("Failed to create embedded kaish: {}", e);
                    return Err(capnp::Error::failed(format!(
                        "kaish creation failed: {}",
                        e
                    )));
                }
            }
        }

        (conn.kaish.as_ref().unwrap().clone(), was_none)
    };

    // Apply persisted env vars and init_script on first creation.
    if newly_created {
        if let Some(ctx_id) = kaish.context_id() {
            kaish.apply_context_config(&kernel.kernel_db, ctx_id).await;
        }
    }

    let documents = kernel.documents.clone();
    let kernel_arc = kernel.kernel.clone();

    // Link to context's long-running trace
    let trace_id = {
        let drift = kernel_arc.drift().read().await;
        drift.trace_id_for_context(context_id).unwrap_or([0u8; 16])
    };
    let _ctx_span = kaijutsu_telemetry::context_root_span(&trace_id, "shell_execute").entered();

    // Document must exist — join_context is the sole creator
    if documents.get(context_id).is_none() {
        return Err(capnp::Error::failed(format!(
            "context {} not found — call join_context first",
            context_id
        )));
    }

    // Create ToolCall block for the shell command (authored by user if user_initiated)
    let last_block = documents.last_block_id(context_id);
    let role = if user_initiated {
        Some(Role::User)
    } else {
        None
    };
    let command_block_id = documents
        .insert_tool_call_as(
            context_id,
            None,
            last_block.as_ref(),
            "shell",
            serde_json::json!({"code": code}),
            Some(TypesToolKind::Shell),
            Some(user_agent_id),
            None,
            role,
        )
        .map_err(|e| capnp::Error::failed(format!("failed to insert shell command: {}", e)))?;

    // Create ToolResult block (empty, will be filled by execution — system-authored)
    let output_block_id = documents
        .insert_tool_result_as(
            context_id,
            &command_block_id,
            Some(&command_block_id),
            "",
            false,
            None,
            Some(TypesToolKind::Shell),
            Some(PrincipalId::system()),
            None,
        )
        .map_err(|e| capnp::Error::failed(format!("failed to insert shell output: {}", e)))?;

    // Mark output block as Running — clients poll this to detect completion
    if let Err(e) = documents.set_status(context_id, &output_block_id, Status::Running) {
        log::warn!("Failed to set output block to Running: {}", e);
    }

    // User-initiated shell blocks are excluded from conversation by default.
    // Users can toggle inclusion via the block gutter controls.
    if user_initiated {
        if let Err(e) = documents.set_excluded(context_id, &command_block_id, true) {
            log::warn!("Failed to set shell command block excluded: {}", e);
        }
        if let Err(e) = documents.set_excluded(context_id, &output_block_id, true) {
            log::warn!("Failed to set shell output block excluded: {}", e);
        }
    }

    // Spawn execution in background
    let code = code.to_owned();
    let documents_clone = documents.clone();
    let output_block_id_clone = output_block_id;
    let command_block_id_clone = command_block_id;
    let block_flows = kernel_arc.block_flows().clone();

    tokio::task::spawn_local(async move {
        // Yield to let the event loop flush BlockInserted events to clients
        // before we start producing text ops. Without this, fast commands
        // (like `ls`) can emit edit_text before the client has processed the
        // BlockInserted, causing DataMissing errors on the client side.
        tokio::task::yield_now().await;

        log::info!(
            "shell_execute: executing code via EmbeddedKaish: {:?}",
            code
        );
        match kaish.execute(&code).await {
            Ok(result) => {
                log::info!(
                    "shell_execute: kaish returned code={} original_code={:?} did_spill={} out_len={} err_len={}",
                    result.code,
                    result.original_code,
                    result.did_spill,
                    result.text_out().len(),
                    result.err.len()
                );

                let out_text = result.text_out().into_owned();
                let output_text = if result.err.is_empty() {
                    out_text
                } else if out_text.is_empty() {
                    result.err.clone()
                } else {
                    format!("{}\n{}", out_text, result.err)
                };

                if let Err(e) = documents_clone.edit_text_as(
                    context_id,
                    &output_block_id_clone,
                    0,
                    &output_text,
                    0,
                    Some(PrincipalId::system()),
                ) {
                    log::error!("Failed to update shell output: {}", e);
                }

                if let Some(output_data) = result.output()
                    && let Err(e) = documents_clone.set_output(
                        context_id,
                        &output_block_id_clone,
                        Some(output_data),
                    )
                {
                    log::error!("Failed to set output data: {}", e);
                }

                if let Some(ref ct_str) = result.content_type {
                    let ct = ContentType::from_mime(ct_str);
                    if ct != ContentType::Plain {
                        if let Err(e) =
                            documents_clone.set_content_type(context_id, &output_block_id_clone, ct)
                        {
                            log::error!("Failed to set content_type: {}", e);
                        }
                    }
                }

                // Read baggage: mark blocks ephemeral if tool signaled it
                if result
                    .baggage
                    .get("kaijutsu.ephemeral")
                    .map(|v| v == "true")
                    .unwrap_or(false)
                {
                    for bid in [&command_block_id_clone, &output_block_id_clone] {
                        if let Err(e) = documents_clone.set_ephemeral(context_id, bid, true) {
                            log::error!("Failed to set ephemeral on block: {}", e);
                        }
                    }
                }

                // Exit 2: latch gate (rm/trash) — confirmation message shown, not a failure
                // Exit 3 / did_spill: output truncated to spill file — command ran, not a failure
                let final_status = match result.code {
                    0 | 2 | 3 => Status::Done,
                    _ => Status::Error,
                };
                if let Err(e) =
                    documents_clone.set_status(context_id, &output_block_id_clone, final_status)
                {
                    log::error!("Failed to set output block status: {}", e);
                }
                if let Err(e) =
                    documents_clone.set_status(context_id, &command_block_id_clone, final_status)
                {
                    log::error!("Failed to set command block status: {}", e);
                }

                // Detect context switch (kj fork, kj context switch).
                // If the kaish session has no active context (e.g. post-leave),
                // we suppress the event rather than publish a nil sentinel —
                // subscribers keep their last known good context.
                if let Some(new_context_id) = kaish.context_id()
                    && new_context_id != context_id
                {
                    log::info!(
                        "shell_execute: context switched {} → {}",
                        context_id,
                        new_context_id
                    );
                    block_flows.publish(kaijutsu_kernel::flows::BlockFlow::ContextSwitched {
                        context_id: new_context_id,
                    });
                }
            }
            Err(e) => {
                let error_msg = format!("Error: {}", e);
                log::error!("Shell execution failed: {}", e);
                if let Err(e) = documents_clone.edit_text_as(
                    context_id,
                    &output_block_id_clone,
                    0,
                    &error_msg,
                    0,
                    Some(PrincipalId::system()),
                ) {
                    log::error!("Failed to update shell output with error: {}", e);
                }
                if let Err(e) =
                    documents_clone.set_status(context_id, &output_block_id_clone, Status::Error)
                {
                    log::error!("Failed to set output block error status: {}", e);
                }
                if let Err(e) =
                    documents_clone.set_status(context_id, &command_block_id_clone, Status::Error)
                {
                    log::error!("Failed to set command block error status: {}", e);
                }
            }
        }
    });

    Ok(command_block_id)
}

// ============================================================================
// Utility Functions
// ============================================================================

/// Parse a BlockId from a Cap'n Proto BlockId reader (binary format).
fn parse_block_id_from_reader(
    reader: &crate::kaijutsu_capnp::block_id::Reader<'_>,
) -> Result<kaijutsu_crdt::BlockId, capnp::Error> {
    let context_id = ContextId::try_from_slice(reader.get_context_id()?)
        .ok_or_else(|| capnp::Error::failed("invalid context_id in BlockId".into()))?;
    let agent_id = PrincipalId::try_from_slice(reader.get_agent_id()?)
        .ok_or_else(|| capnp::Error::failed("invalid agent_id in BlockId".into()))?;
    Ok(kaijutsu_crdt::BlockId {
        context_id,
        agent_id,
        seq: reader.get_seq(),
    })
}

/// Set BlockId fields on a Cap'n Proto builder (binary format).
fn set_block_id_builder(
    builder: &mut crate::kaijutsu_capnp::block_id::Builder,
    block_id: &kaijutsu_crdt::BlockId,
) {
    builder.set_context_id(block_id.context_id.as_bytes());
    builder.set_agent_id(block_id.agent_id.as_bytes());
    builder.set_seq(block_id.seq);
}

/// Set BlockSnapshot fields on a Cap'n Proto builder.
fn set_block_snapshot(
    builder: &mut crate::kaijutsu_capnp::block_snapshot::Builder,
    block: &kaijutsu_crdt::BlockSnapshot,
) {
    // Set ID
    {
        let mut id = builder.reborrow().init_id();
        set_block_id_builder(&mut id, &block.id);
    }

    // Set parent_id if present
    if let Some(ref parent) = block.parent_id {
        builder.set_has_parent_id(true);
        let mut pid = builder.reborrow().init_parent_id();
        set_block_id_builder(&mut pid, parent);
    } else {
        builder.set_has_parent_id(false);
    }

    // Set role
    builder.set_role(match block.role {
        kaijutsu_crdt::Role::User => crate::kaijutsu_capnp::Role::User,
        kaijutsu_crdt::Role::Model => crate::kaijutsu_capnp::Role::Model,
        kaijutsu_crdt::Role::System => crate::kaijutsu_capnp::Role::System,
        kaijutsu_crdt::Role::Tool => crate::kaijutsu_capnp::Role::Tool,
        kaijutsu_crdt::Role::Asset => crate::kaijutsu_capnp::Role::Asset,
    });

    // Set status
    builder.set_status(match block.status {
        kaijutsu_crdt::Status::Pending => crate::kaijutsu_capnp::Status::Pending,
        kaijutsu_crdt::Status::Running => crate::kaijutsu_capnp::Status::Running,
        kaijutsu_crdt::Status::Done => crate::kaijutsu_capnp::Status::Done,
        kaijutsu_crdt::Status::Error => crate::kaijutsu_capnp::Status::Error,
    });

    // Set kind
    builder.set_kind(match block.kind {
        kaijutsu_crdt::BlockKind::Text => crate::kaijutsu_capnp::BlockKind::Text,
        kaijutsu_crdt::BlockKind::Thinking => crate::kaijutsu_capnp::BlockKind::Thinking,
        kaijutsu_crdt::BlockKind::ToolCall => crate::kaijutsu_capnp::BlockKind::ToolCall,
        kaijutsu_crdt::BlockKind::ToolResult => crate::kaijutsu_capnp::BlockKind::ToolResult,
        kaijutsu_crdt::BlockKind::Drift => crate::kaijutsu_capnp::BlockKind::Drift,
        kaijutsu_crdt::BlockKind::File => crate::kaijutsu_capnp::BlockKind::File,
        kaijutsu_crdt::BlockKind::Error => crate::kaijutsu_capnp::BlockKind::Error,
    });

    // Set basic fields (no author — derived from id.agent_id)
    builder.set_content(&block.content);
    builder.set_collapsed(block.collapsed);
    builder.set_created_at(block.created_at);

    // Set tool-specific fields
    if let Some(ref name) = block.tool_name {
        builder.set_tool_name(name);
    }
    if let Some(ref input) = block.tool_input {
        builder.set_tool_input(input);
    }
    if let Some(ref tc_id) = block.tool_call_id {
        let mut tcid = builder.reborrow().init_tool_call_id();
        set_block_id_builder(&mut tcid, tc_id);
    }
    if let Some(code) = block.exit_code {
        builder.set_has_exit_code(true);
        builder.set_exit_code(code);
    }
    builder.set_is_error(block.is_error);

    // Set output data if present
    if let Some(ref output) = block.output {
        build_output_data(builder.reborrow().init_output_data(), output);
    }

    // Set tool mechanism metadata
    if let Some(tk) = block.tool_kind {
        builder.set_has_tool_kind(true);
        builder.set_tool_kind(tool_kind_to_capnp(tk));
    }

    // Set file metadata
    if let Some(ref path) = block.file_path {
        builder.set_file_path(path);
    }

    // Set tool_use_id (LLM-assigned tool invocation ID)
    if let Some(ref tui) = block.tool_use_id {
        builder.set_tool_use_id(tui);
    }

    // Set content_type hint (MIME type)
    if block.content_type != ContentType::Plain {
        builder.set_content_type(block.content_type.as_mime());
    }

    // Set ephemeral flag
    builder.set_ephemeral(block.ephemeral);
    builder.set_excluded(block.excluded);

    // Set drift-specific fields if present
    if let Some(ref ctx) = block.source_context {
        builder.set_source_context(ctx.as_bytes());
    }
    if let Some(ref model) = block.source_model {
        builder.set_source_model(model);
    }
    if let Some(dk) = block.drift_kind {
        builder.set_has_drift_kind(true);
        builder.set_drift_kind(drift_kind_to_capnp(dk));
    }

    // Set error payload if present
    if let Some(ref payload) = block.error {
        builder.set_has_error_payload(true);
        let mut ep = builder.reborrow().init_error_payload();
        ep.set_category(error_category_to_capnp(payload.category));
        ep.set_severity(error_severity_to_capnp(payload.severity));
        if let Some(ref code) = payload.code {
            ep.set_code(code);
        }
        if let Some(ref detail) = payload.detail {
            ep.set_detail(detail);
        }
        if let Some(ref span) = payload.span {
            ep.set_has_span(true);
            ep.set_span_line(span.line);
            ep.set_span_column(span.column);
            ep.set_span_length(span.length);
        }
        if let Some(sk) = payload.source_kind {
            ep.set_has_source_kind(true);
            ep.set_source_kind(match sk {
                kaijutsu_crdt::BlockKind::Text => crate::kaijutsu_capnp::BlockKind::Text,
                kaijutsu_crdt::BlockKind::Thinking => crate::kaijutsu_capnp::BlockKind::Thinking,
                kaijutsu_crdt::BlockKind::ToolCall => crate::kaijutsu_capnp::BlockKind::ToolCall,
                kaijutsu_crdt::BlockKind::ToolResult => {
                    crate::kaijutsu_capnp::BlockKind::ToolResult
                }
                kaijutsu_crdt::BlockKind::Drift => crate::kaijutsu_capnp::BlockKind::Drift,
                kaijutsu_crdt::BlockKind::File => crate::kaijutsu_capnp::BlockKind::File,
                kaijutsu_crdt::BlockKind::Error => crate::kaijutsu_capnp::BlockKind::Error,
            });
        }
    }
}

/// Convert a CRDT ErrorCategory to Cap'n Proto.
fn error_category_to_capnp(
    cat: kaijutsu_crdt::ErrorCategory,
) -> crate::kaijutsu_capnp::ErrorCategory {
    match cat {
        kaijutsu_crdt::ErrorCategory::Tool => crate::kaijutsu_capnp::ErrorCategory::Tool,
        kaijutsu_crdt::ErrorCategory::Stream => crate::kaijutsu_capnp::ErrorCategory::Stream,
        kaijutsu_crdt::ErrorCategory::Rpc => crate::kaijutsu_capnp::ErrorCategory::Rpc,
        kaijutsu_crdt::ErrorCategory::Render => crate::kaijutsu_capnp::ErrorCategory::Render,
        kaijutsu_crdt::ErrorCategory::Parse => crate::kaijutsu_capnp::ErrorCategory::Parse,
        kaijutsu_crdt::ErrorCategory::Validation => {
            crate::kaijutsu_capnp::ErrorCategory::Validation
        }
        kaijutsu_crdt::ErrorCategory::Kernel => crate::kaijutsu_capnp::ErrorCategory::Kernel,
    }
}

/// Convert a CRDT ErrorSeverity to Cap'n Proto.
fn error_severity_to_capnp(
    sev: kaijutsu_crdt::ErrorSeverity,
) -> crate::kaijutsu_capnp::ErrorSeverity {
    match sev {
        kaijutsu_crdt::ErrorSeverity::Warning => crate::kaijutsu_capnp::ErrorSeverity::Warning,
        kaijutsu_crdt::ErrorSeverity::Error => crate::kaijutsu_capnp::ErrorSeverity::Error,
        kaijutsu_crdt::ErrorSeverity::Fatal => crate::kaijutsu_capnp::ErrorSeverity::Fatal,
    }
}

/// Convert a CRDT ToolKind to Cap'n Proto ToolKind.
fn tool_kind_to_capnp(tk: kaijutsu_crdt::ToolKind) -> crate::kaijutsu_capnp::ToolKind {
    match tk {
        kaijutsu_crdt::ToolKind::Shell => crate::kaijutsu_capnp::ToolKind::Shell,
        kaijutsu_crdt::ToolKind::Mcp => crate::kaijutsu_capnp::ToolKind::Mcp,
        kaijutsu_crdt::ToolKind::Builtin => crate::kaijutsu_capnp::ToolKind::Builtin,
    }
}

/// Convert a CRDT DriftKind to Cap'n Proto DriftKind.
fn drift_kind_to_capnp(dk: kaijutsu_crdt::DriftKind) -> crate::kaijutsu_capnp::DriftKind {
    match dk {
        kaijutsu_crdt::DriftKind::Push => crate::kaijutsu_capnp::DriftKind::Push,
        kaijutsu_crdt::DriftKind::Pull => crate::kaijutsu_capnp::DriftKind::Pull,
        kaijutsu_crdt::DriftKind::Merge => crate::kaijutsu_capnp::DriftKind::Merge,
        kaijutsu_crdt::DriftKind::Distill => crate::kaijutsu_capnp::DriftKind::Distill,
        kaijutsu_crdt::DriftKind::Commit => crate::kaijutsu_capnp::DriftKind::Commit,
        kaijutsu_crdt::DriftKind::Notification => crate::kaijutsu_capnp::DriftKind::Notification,
        kaijutsu_crdt::DriftKind::Fork => crate::kaijutsu_capnp::DriftKind::Fork,
    }
}

/// Parse a Cap'n Proto BlockQuery union into a Rust BlockQuery.
fn parse_block_query(
    reader: &crate::kaijutsu_capnp::block_query::Reader<'_>,
) -> Result<kaijutsu_types::BlockQuery, capnp::Error> {
    match reader.which()? {
        crate::kaijutsu_capnp::block_query::All(()) => Ok(kaijutsu_types::BlockQuery::All),
        crate::kaijutsu_capnp::block_query::ByIds(ids_reader) => {
            let ids_reader = ids_reader?;
            let mut ids = Vec::with_capacity(ids_reader.len() as usize);
            for id_reader in ids_reader.iter() {
                ids.push(parse_block_id_from_reader(&id_reader)?);
            }
            Ok(kaijutsu_types::BlockQuery::ByIds(ids))
        }
        crate::kaijutsu_capnp::block_query::ByFilter(filter_reader) => {
            let filter = parse_block_filter(&filter_reader?)?;
            Ok(kaijutsu_types::BlockQuery::ByFilter(filter))
        }
    }
}

/// Parse a Cap'n Proto BlockFilter into a Rust BlockFilter.
fn parse_block_filter(
    reader: &crate::kaijutsu_capnp::block_filter::Reader<'_>,
) -> Result<kaijutsu_types::BlockFilter, capnp::Error> {
    let kinds = if reader.get_has_kinds() {
        let kinds_reader = reader.get_kinds()?;
        let mut kinds = Vec::with_capacity(kinds_reader.len() as usize);
        for k in kinds_reader.iter() {
            kinds.push(match k? {
                crate::kaijutsu_capnp::BlockKind::Text => BlockKind::Text,
                crate::kaijutsu_capnp::BlockKind::Thinking => BlockKind::Thinking,
                crate::kaijutsu_capnp::BlockKind::ToolCall => BlockKind::ToolCall,
                crate::kaijutsu_capnp::BlockKind::ToolResult => BlockKind::ToolResult,
                crate::kaijutsu_capnp::BlockKind::Drift => BlockKind::Drift,
                crate::kaijutsu_capnp::BlockKind::File => BlockKind::File,
                crate::kaijutsu_capnp::BlockKind::Error => BlockKind::Error,
            });
        }
        if kinds.is_empty() {
            return Err(capnp::Error::failed(
                "hasKinds=true but kinds list is empty".into(),
            ));
        }
        kinds
    } else {
        vec![]
    };

    let roles = if reader.get_has_roles() {
        let roles_reader = reader.get_roles()?;
        let mut roles = Vec::with_capacity(roles_reader.len() as usize);
        for r in roles_reader.iter() {
            roles.push(match r? {
                crate::kaijutsu_capnp::Role::User => Role::User,
                crate::kaijutsu_capnp::Role::Model => Role::Model,
                crate::kaijutsu_capnp::Role::System => Role::System,
                crate::kaijutsu_capnp::Role::Tool => Role::Tool,
                crate::kaijutsu_capnp::Role::Asset => Role::Asset,
            });
        }
        if roles.is_empty() {
            return Err(capnp::Error::failed(
                "hasRoles=true but roles list is empty".into(),
            ));
        }
        roles
    } else {
        vec![]
    };

    let statuses = if reader.get_has_statuses() {
        let statuses_reader = reader.get_statuses()?;
        let mut statuses = Vec::with_capacity(statuses_reader.len() as usize);
        for s in statuses_reader.iter() {
            statuses.push(match s? {
                crate::kaijutsu_capnp::Status::Pending => Status::Pending,
                crate::kaijutsu_capnp::Status::Running => Status::Running,
                crate::kaijutsu_capnp::Status::Done => Status::Done,
                crate::kaijutsu_capnp::Status::Error => Status::Error,
            });
        }
        if statuses.is_empty() {
            return Err(capnp::Error::failed(
                "hasStatuses=true but statuses list is empty".into(),
            ));
        }
        statuses
    } else {
        vec![]
    };

    let parent_id = if reader.get_has_parent_id() {
        Some(parse_block_id_from_reader(&reader.get_parent_id()?)?)
    } else {
        None
    };

    Ok(kaijutsu_types::BlockFilter {
        kinds,
        roles,
        statuses,
        exclude_compacted: reader.get_exclude_compacted(),
        limit: reader.get_limit(),
        max_depth: reader.get_max_depth(),
        parent_id,
    })
}

/// Parse a BlockEventFilter from a Cap'n Proto reader.
fn parse_block_event_filter(
    reader: crate::kaijutsu_capnp::block_event_filter::Reader<'_>,
) -> kaijutsu_types::BlockEventFilter {
    let context_ids = if reader.get_has_context_ids() {
        reader
            .get_context_ids()
            .map(|list| {
                list.iter()
                    .filter_map(|bytes| bytes.ok().and_then(ContextId::try_from_slice))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };

    let event_types = if reader.get_has_event_types() {
        reader
            .get_event_types()
            .map(|list| {
                list.iter()
                    .filter_map(|k| {
                        k.ok().map(|k| match k {
                            crate::kaijutsu_capnp::BlockFlowKind::Inserted => {
                                kaijutsu_types::BlockFlowKind::Inserted
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::TextOps => {
                                kaijutsu_types::BlockFlowKind::TextOps
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::Deleted => {
                                kaijutsu_types::BlockFlowKind::Deleted
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::StatusChanged => {
                                kaijutsu_types::BlockFlowKind::StatusChanged
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::CollapsedChanged => {
                                kaijutsu_types::BlockFlowKind::CollapsedChanged
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::ExcludedChanged => {
                                kaijutsu_types::BlockFlowKind::ExcludedChanged
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::Moved => {
                                kaijutsu_types::BlockFlowKind::Moved
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::SyncReset => {
                                kaijutsu_types::BlockFlowKind::SyncReset
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::OutputChanged => {
                                kaijutsu_types::BlockFlowKind::OutputChanged
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::MetadataChanged => {
                                kaijutsu_types::BlockFlowKind::MetadataChanged
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::ContextSwitched => {
                                kaijutsu_types::BlockFlowKind::ContextSwitched
                            }
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };

    let block_kinds = if reader.get_has_block_kinds() {
        reader
            .get_block_kinds()
            .map(|list| {
                list.iter()
                    .filter_map(|k| {
                        k.ok().map(|k| match k {
                            crate::kaijutsu_capnp::BlockKind::Text => BlockKind::Text,
                            crate::kaijutsu_capnp::BlockKind::Thinking => BlockKind::Thinking,
                            crate::kaijutsu_capnp::BlockKind::ToolCall => BlockKind::ToolCall,
                            crate::kaijutsu_capnp::BlockKind::ToolResult => BlockKind::ToolResult,
                            crate::kaijutsu_capnp::BlockKind::Drift => BlockKind::Drift,
                            crate::kaijutsu_capnp::BlockKind::File => BlockKind::File,
                            crate::kaijutsu_capnp::BlockKind::Error => BlockKind::Error,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };

    kaijutsu_types::BlockEventFilter {
        context_ids,
        event_types,
        block_kinds,
    }
}

/// Convert a CRDT Status to Cap'n Proto Status.
fn status_to_capnp(status: kaijutsu_crdt::Status) -> crate::kaijutsu_capnp::Status {
    match status {
        kaijutsu_crdt::Status::Pending => crate::kaijutsu_capnp::Status::Pending,
        kaijutsu_crdt::Status::Running => crate::kaijutsu_capnp::Status::Running,
        kaijutsu_crdt::Status::Done => crate::kaijutsu_capnp::Status::Done,
        kaijutsu_crdt::Status::Error => crate::kaijutsu_capnp::Status::Error,
    }
}

/// Parse a BlockSnapshot from a Cap'n Proto reader.
// SeatHandle interface removed — replaced by ContextMembership.

// ============================================================================
// VFS Implementation
// ============================================================================

struct VfsImpl {
    kernel: Arc<Kernel>,
}

impl VfsImpl {
    fn new(kernel: Arc<Kernel>) -> Self {
        Self { kernel }
    }
}

/// Convert VfsError to capnp Error
fn vfs_err_to_capnp(e: kaijutsu_kernel::VfsError) -> capnp::Error {
    capnp::Error::failed(format!("{}", e))
}

/// Helper to extract path string from capnp text reader
fn get_path_str(text: capnp::text::Reader<'_>) -> Result<String, capnp::Error> {
    text.to_str()
        .map(|s| s.to_owned())
        .map_err(|e| capnp::Error::failed(format!("invalid UTF-8: {}", e)))
}

/// Helper to build FileAttr result
fn set_file_attr(
    builder: &mut crate::kaijutsu_capnp::file_attr::Builder,
    attr: &kaijutsu_kernel::FileAttr,
) {
    builder.set_size(attr.size);
    builder.set_kind(match attr.kind {
        kaijutsu_kernel::FileType::File => crate::kaijutsu_capnp::FileType::File,
        kaijutsu_kernel::FileType::Directory => crate::kaijutsu_capnp::FileType::Directory,
        kaijutsu_kernel::FileType::Symlink => crate::kaijutsu_capnp::FileType::Symlink,
    });
    builder.set_perm(attr.perm);
    // Convert SystemTime to duration since epoch
    let duration = attr
        .mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    builder.set_mtime_secs(duration.as_secs());
    builder.set_mtime_nanos(duration.subsec_nanos());
    builder.set_nlink(attr.nlink);
}

impl vfs::Server for VfsImpl {
    fn getattr(
        self: Rc<Self>,
        params: vfs::GetattrParams,
        mut results: vfs::GetattrResults,
    ) -> Promise<(), capnp::Error> {
        let path = match params.get().and_then(|p| p.get_path()) {
            Ok(p) => match p.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let attr = kernel
                .getattr(Path::new(&path))
                .await
                .map_err(vfs_err_to_capnp)?;
            let mut builder = results.get().init_attr();
            set_file_attr(&mut builder, &attr);
            Ok(())
        })
    }

    fn readdir(
        self: Rc<Self>,
        params: vfs::ReaddirParams,
        mut results: vfs::ReaddirResults,
    ) -> Promise<(), capnp::Error> {
        let path = match params.get().and_then(|p| p.get_path()) {
            Ok(p) => match p.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let entries = kernel
                .readdir(Path::new(&path))
                .await
                .map_err(vfs_err_to_capnp)?;
            let mut builder = results.get().init_entries(entries.len() as u32);
            for (i, entry) in entries.iter().enumerate() {
                let mut e = builder.reborrow().get(i as u32);
                e.set_name(&entry.name);
                e.set_kind(match entry.kind {
                    kaijutsu_kernel::FileType::File => crate::kaijutsu_capnp::FileType::File,
                    kaijutsu_kernel::FileType::Directory => {
                        crate::kaijutsu_capnp::FileType::Directory
                    }
                    kaijutsu_kernel::FileType::Symlink => crate::kaijutsu_capnp::FileType::Symlink,
                });
            }
            Ok(())
        })
    }

    fn read(
        self: Rc<Self>,
        params: vfs::ReadParams,
        mut results: vfs::ReadResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let path = match params.get_path().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let offset = params.get_offset();
        let size = params.get_size();
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let data = kernel
                .read(Path::new(&path), offset, size)
                .await
                .map_err(vfs_err_to_capnp)?;
            results.get().set_data(&data);
            Ok(())
        })
    }

    fn readlink(
        self: Rc<Self>,
        params: vfs::ReadlinkParams,
        mut results: vfs::ReadlinkResults,
    ) -> Promise<(), capnp::Error> {
        let path = match params.get().and_then(|p| p.get_path()) {
            Ok(p) => match p.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let target = kernel
                .readlink(Path::new(&path))
                .await
                .map_err(vfs_err_to_capnp)?;
            results.get().set_target(target.to_string_lossy());
            Ok(())
        })
    }

    fn write(
        self: Rc<Self>,
        params: vfs::WriteParams,
        mut results: vfs::WriteResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let path = match params.get_path().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let offset = params.get_offset();
        let data = match params.get_data() {
            Ok(d) => d.to_vec(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let written = kernel
                .write(Path::new(&path), offset, &data)
                .await
                .map_err(vfs_err_to_capnp)?;
            results.get().set_written(written);
            Ok(())
        })
    }

    fn create(
        self: Rc<Self>,
        params: vfs::CreateParams,
        mut results: vfs::CreateResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let path = match params.get_path().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let mode = params.get_mode();
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let attr = kernel
                .create(Path::new(&path), mode)
                .await
                .map_err(vfs_err_to_capnp)?;
            let mut builder = results.get().init_attr();
            set_file_attr(&mut builder, &attr);
            Ok(())
        })
    }

    fn mkdir(
        self: Rc<Self>,
        params: vfs::MkdirParams,
        mut results: vfs::MkdirResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let path = match params.get_path().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let mode = params.get_mode();
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let attr = kernel
                .mkdir(Path::new(&path), mode)
                .await
                .map_err(vfs_err_to_capnp)?;
            let mut builder = results.get().init_attr();
            set_file_attr(&mut builder, &attr);
            Ok(())
        })
    }

    fn unlink(
        self: Rc<Self>,
        params: vfs::UnlinkParams,
        _results: vfs::UnlinkResults,
    ) -> Promise<(), capnp::Error> {
        let path = match params.get().and_then(|p| p.get_path()) {
            Ok(p) => match p.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            kernel
                .unlink(Path::new(&path))
                .await
                .map_err(vfs_err_to_capnp)?;
            Ok(())
        })
    }

    fn rmdir(
        self: Rc<Self>,
        params: vfs::RmdirParams,
        _results: vfs::RmdirResults,
    ) -> Promise<(), capnp::Error> {
        let path = match params.get().and_then(|p| p.get_path()) {
            Ok(p) => match p.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            kernel
                .rmdir(Path::new(&path))
                .await
                .map_err(vfs_err_to_capnp)?;
            Ok(())
        })
    }

    fn rename(
        self: Rc<Self>,
        params: vfs::RenameParams,
        _results: vfs::RenameResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let from = match params.get_from().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let to = match params.get_to().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            kernel
                .rename(Path::new(&from), Path::new(&to))
                .await
                .map_err(vfs_err_to_capnp)?;
            Ok(())
        })
    }

    fn truncate(
        self: Rc<Self>,
        params: vfs::TruncateParams,
        _results: vfs::TruncateResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let path = match params.get_path().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let size = params.get_size();
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            kernel
                .truncate(Path::new(&path), size)
                .await
                .map_err(vfs_err_to_capnp)?;
            Ok(())
        })
    }

    fn setattr(
        self: Rc<Self>,
        params: vfs::SetattrParams,
        mut results: vfs::SetattrResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let path = match params.get_path().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let attr_reader = match params.get_attr() {
            Ok(a) => a,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };

        // Convert to kernel SetAttr
        let set_attr = kaijutsu_kernel::SetAttr {
            size: if attr_reader.get_has_size() {
                Some(attr_reader.get_size())
            } else {
                None
            },
            perm: if attr_reader.get_has_perm() {
                Some(attr_reader.get_perm())
            } else {
                None
            },
            mtime: if attr_reader.get_has_mtime() {
                Some(
                    std::time::UNIX_EPOCH
                        + std::time::Duration::from_secs(attr_reader.get_mtime_secs()),
                )
            } else {
                None
            },
            atime: None, // Not in capnp schema
            uid: None,   // Not in capnp schema
            gid: None,   // Not in capnp schema
        };

        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let attr = kernel
                .setattr(Path::new(&path), set_attr)
                .await
                .map_err(vfs_err_to_capnp)?;
            let mut builder = results.get().init_new_attr();
            set_file_attr(&mut builder, &attr);
            Ok(())
        })
    }

    fn symlink(
        self: Rc<Self>,
        params: vfs::SymlinkParams,
        mut results: vfs::SymlinkResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let path = match params.get_path().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let target = match params.get_target().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let attr = kernel
                .symlink(Path::new(&path), Path::new(&target))
                .await
                .map_err(vfs_err_to_capnp)?;
            let mut builder = results.get().init_attr();
            set_file_attr(&mut builder, &attr);
            Ok(())
        })
    }

    fn read_only(
        self: Rc<Self>,
        _params: vfs::ReadOnlyParams,
        mut results: vfs::ReadOnlyResults,
    ) -> Promise<(), capnp::Error> {
        results.get().set_read_only(self.kernel.vfs().read_only());
        Promise::ok(())
    }

    fn statfs(
        self: Rc<Self>,
        _params: vfs::StatfsParams,
        mut results: vfs::StatfsResults,
    ) -> Promise<(), capnp::Error> {
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let stat = kernel.statfs().await.map_err(vfs_err_to_capnp)?;
            let mut builder = results.get().init_stat();
            builder.set_blocks(stat.blocks);
            builder.set_bfree(stat.bfree);
            builder.set_bavail(stat.bavail);
            builder.set_files(stat.files);
            builder.set_ffree(stat.ffree);
            builder.set_bsize(stat.bsize);
            builder.set_namelen(stat.namelen);
            Ok(())
        })
    }

    fn real_path(
        self: Rc<Self>,
        params: vfs::RealPathParams,
        mut results: vfs::RealPathResults,
    ) -> Promise<(), capnp::Error> {
        let path = match params.get().and_then(|p| p.get_path()) {
            Ok(p) => match p.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            match kernel.real_path(Path::new(&path)).await {
                Ok(Some(real)) => {
                    results.get().set_real_path(real.to_string_lossy());
                    Ok(())
                }
                Ok(None) => {
                    // Virtual backend (MemoryBackend) - return empty string
                    results.get().set_real_path("");
                    Ok(())
                }
                Err(e) => Err(vfs_err_to_capnp(e)),
            }
        })
    }
}

// ============================================================================
// Synthesis (Rhai-driven keyword extraction + representative block selection)
// ============================================================================
