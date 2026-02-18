//! Cap'n Proto RPC server implementation
//!
//! Implements World and Kernel capabilities.
//! Each kernel owns a kaijutsu_kernel::Kernel for VFS and state,
//! plus a kaish subprocess for code execution.

#![allow(refining_impl_trait)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use capnp::capability::Promise;
use capnp_rpc::pry;

use crate::kaijutsu_capnp::*;
use crate::context_engine::{ContextEngine, ContextManager};
use crate::embedded_kaish::EmbeddedKaish;

use kaijutsu_kernel::{
    DocumentDb, DocumentKind, Kernel,
    LocalBackend, SharedBlockStore, ToolInfo, VfsOps,
    RigProvider, LlmMessage, llm::stream::{LlmStream, StreamRequest, StreamEvent},
    // Block tools
    BlockAppendEngine, BlockCreateEngine, BlockEditEngine, BlockListEngine, BlockReadEngine,
    BlockSearchEngine, BlockSpliceEngine, BlockStatusEngine, KernelSearchEngine, GitEngine,
    // Drift engines (split)
    DriftLsEngine, DriftPushEngine, DriftPullEngine, DriftFlushEngine, DriftMergeEngine,
    // File tools
    FileDocumentCache, ReadEngine, EditEngine, WriteEngine, GlobEngine, GrepEngine, WhoamiEngine,
    // MCP
    McpServerPool, McpServerConfig, McpTransport, McpToolEngine, extract_tool_result_text,
    // FlowBus
    BlockFlow, SharedBlockFlowBus, shared_block_flow_bus,
    SharedConfigFlowBus, shared_config_flow_bus,
    block_store::BlockStore,
    // Agents
    AgentCapability, AgentConfig, AgentInfo, AgentStatus, AgentActivityEvent,
    // Config
    ConfigCrdtBackend, ConfigWatcherHandle,
    // Tool filtering
    ToolFilter,
};
use kaijutsu_crdt::{BlockKind, Role, Status};
use serde_json;
use tracing::Instrument;

/// Extract W3C Trace Context from a Cap'n Proto `TraceContext` reader.
///
/// Returns a tracing span linked to the remote parent (or a root span if empty).
/// Safe to call even when trace is not present — returns a detached span.
fn extract_rpc_trace(trace: capnp::Result<trace_context::Reader<'_>>, name: &'static str) -> tracing::Span {
    let (traceparent, tracestate) = match trace {
        Ok(t) => {
            let tp = t.get_traceparent().ok().and_then(|r| r.to_str().ok()).unwrap_or("");
            let ts = t.get_tracestate().ok().and_then(|r| r.to_str().ok()).unwrap_or("");
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

/// Register block tools with a kernel.
/// Tools with engines are automatically available via the ToolFilter system.
async fn register_block_tools(
    kernel: &Arc<Kernel>,
    documents: SharedBlockStore,
    context_manager: Arc<ContextManager>,
) {
    kernel.register_tool_with_engine(
        ToolInfo::new("context", "Manage conversation contexts (new, switch, list)", "kernel"),
        Arc::new(ContextEngine::new(context_manager)),
    ).await;

    kernel.register_tool_with_engine(
        ToolInfo::new("block_create", "Create a new block with role, kind, content", "block"),
        Arc::new(BlockCreateEngine::new(documents.clone(), "server")),
    ).await;

    kernel.register_tool_with_engine(
        ToolInfo::new("block_append", "Append text to a block", "block"),
        Arc::new(BlockAppendEngine::new(documents.clone())),
    ).await;

    kernel.register_tool_with_engine(
        ToolInfo::new("block_edit", "Line-based editing with atomic ops and CAS validation", "block"),
        Arc::new(BlockEditEngine::new(documents.clone(), "server")),
    ).await;

    kernel.register_tool_with_engine(
        ToolInfo::new("block_splice", "Character-based splice editing", "block"),
        Arc::new(BlockSpliceEngine::new(documents.clone())),
    ).await;

    kernel.register_tool_with_engine(
        ToolInfo::new("block_read", "Read block content with optional line numbers", "block"),
        Arc::new(BlockReadEngine::new(documents.clone())),
    ).await;

    kernel.register_tool_with_engine(
        ToolInfo::new("block_search", "Search within a block using regex", "block"),
        Arc::new(BlockSearchEngine::new(documents.clone())),
    ).await;

    kernel.register_tool_with_engine(
        ToolInfo::new("block_list", "List blocks with optional filters", "block"),
        Arc::new(BlockListEngine::new(documents.clone())),
    ).await;

    kernel.register_tool_with_engine(
        ToolInfo::new("block_status", "Set block status", "block"),
        Arc::new(BlockStatusEngine::new(documents.clone())),
    ).await;

    kernel.register_tool_with_engine(
        ToolInfo::new("kernel_search", "Search across blocks using regex", "kernel"),
        Arc::new(KernelSearchEngine::new(documents.clone())),
    ).await;

    // ── Drift tools (split into individual engines) ──
    kernel.register_tool_with_engine(
        ToolInfo::new("drift_ls", "List all contexts in this kernel's drift router", "drift"),
        Arc::new(DriftLsEngine::new(kernel, "default")),
    ).await;
    kernel.register_tool_with_engine(
        ToolInfo::new("drift_push", "Stage content for transfer to another context", "drift"),
        Arc::new(DriftPushEngine::new(kernel, documents.clone(), "default")),
    ).await;
    kernel.register_tool_with_engine(
        ToolInfo::new("drift_pull", "Read and LLM-summarize another context's conversation", "drift"),
        Arc::new(DriftPullEngine::new(kernel, documents.clone(), "default")),
    ).await;
    kernel.register_tool_with_engine(
        ToolInfo::new("drift_flush", "Deliver all staged drifts to target documents", "drift"),
        Arc::new(DriftFlushEngine::new(kernel, documents.clone(), "default")),
    ).await;
    kernel.register_tool_with_engine(
        ToolInfo::new("drift_merge", "Summarize a forked context back into its parent", "drift"),
        Arc::new(DriftMergeEngine::new(kernel, documents.clone(), "default")),
    ).await;

    // ── File tools (CRDT-backed) ──
    let file_cache = Arc::new(FileDocumentCache::new(documents.clone(), kernel.vfs().clone()));
    kernel.register_tool_with_engine(
        ToolInfo::new("read", "Read file content with optional line numbers", "file"),
        Arc::new(ReadEngine::new(file_cache.clone())),
    ).await;
    kernel.register_tool_with_engine(
        ToolInfo::new("edit", "Edit a file by exact string replacement", "file"),
        Arc::new(EditEngine::new(file_cache.clone())),
    ).await;
    kernel.register_tool_with_engine(
        ToolInfo::new("write", "Write or create a file with the given content", "file"),
        Arc::new(WriteEngine::new(file_cache.clone())),
    ).await;
    kernel.register_tool_with_engine(
        ToolInfo::new("glob", "Find files matching a glob pattern", "file"),
        Arc::new(GlobEngine::new(kernel.vfs().clone())),
    ).await;
    kernel.register_tool_with_engine(
        ToolInfo::new("grep", "Search file content with regex", "file"),
        Arc::new(GrepEngine::new(file_cache.clone(), kernel.vfs().clone())),
    ).await;
    kernel.register_tool_with_engine(
        ToolInfo::new("whoami", "Show current context identity", "drift"),
        Arc::new(WhoamiEngine::new(kernel.drift().clone(), "default")),
    ).await;

    // ── VCS ──
    kernel.register_tool_with_engine(
        ToolInfo::new("git", "Context-aware git with LLM commit summaries", "vcs"),
        Arc::new(GitEngine::new(
            kernel,
            documents,
            "default",
        )),
    ).await;
}

// ============================================================================
// Context Types (Rust-side mirrors of Cap'n Proto types)
// ============================================================================

/// A document attached to a context
#[derive(Debug, Clone)]
pub struct ContextDocument {
    pub id: String,
    pub attached_by: String,  // nick:instance
    pub attached_at: u64,     // Unix timestamp ms
}

/// A context within a kernel
#[derive(Debug, Clone)]
pub struct ContextState {
    pub name: String,
    pub documents: Vec<ContextDocument>,
}

impl ContextState {
    pub fn new(name: String) -> Self {
        Self {
            name,
            documents: Vec::new(),
        }
    }
}

/// Server state shared across all capabilities
pub struct ServerState {
    pub identity: Identity,
    pub kernels: HashMap<String, KernelState>,
    next_kernel_id: AtomicU64,
    next_exec_id: AtomicU64,
    /// Shared MCP server pool
    pub mcp_pool: Arc<McpServerPool>,
    /// Cross-context drift router (context registry + staging queue)
    pub drift_router: kaijutsu_kernel::DriftRouter,
    /// Config directory override. None = use XDG default (~/.config/kaijutsu).
    pub config_dir: Option<std::path::PathBuf>,
    /// Data directory override. None = use XDG default (~/.local/share/kaijutsu).
    pub data_dir: Option<std::path::PathBuf>,
}

impl ServerState {
    pub fn new(username: String, config_dir: Option<std::path::PathBuf>) -> Self {
        // If config_dir is overridden (test mode), also use a tempdir for data
        // to avoid loading stale config from the persistent database.
        let data_dir = config_dir.as_ref().map(|c| c.join("data"));

        Self {
            identity: Identity {
                username: username.clone(),
                display_name: username,
            },
            kernels: HashMap::new(),
            next_kernel_id: AtomicU64::new(1),
            next_exec_id: AtomicU64::new(1),
            mcp_pool: Arc::new(McpServerPool::new()),
            drift_router: kaijutsu_kernel::DriftRouter::new(),
            config_dir,
            data_dir,
        }
    }

    fn next_kernel_id(&self) -> String {
        format!("kernel-{}", self.next_kernel_id.fetch_add(1, Ordering::SeqCst))
    }

    fn next_exec_id(&self) -> u64 {
        self.next_exec_id.fetch_add(1, Ordering::SeqCst)
    }
}

#[derive(Clone)]
pub struct Identity {
    pub username: String,
    pub display_name: String,
}

/// Get the data directory for a kernel's persistent storage.
/// Creates the directory if it doesn't exist.
/// Returns: ~/.local/share/kaijutsu/kernels/{kernel_id}/
fn kernel_data_dir(kernel_id: &str) -> std::path::PathBuf {
    let dir = kaish_kernel::xdg_data_home()
        .join("kaijutsu")
        .join("kernels")
        .join(kernel_id);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("Failed to create kernel data dir {:?}: {}", dir, e);
    }
    dir
}

/// Open or create a BlockStore with database persistence for a kernel.
///
/// If `data_dir_override` is set, uses that directory for the database.
/// Otherwise uses the XDG default (~/.local/share/kaijutsu/kernels/{kernel_id}/).
fn create_block_store_with_db(kernel_id: &str, block_flows: SharedBlockFlowBus, data_dir_override: Option<&Path>) -> SharedBlockStore {
    let db_dir = match data_dir_override {
        Some(dir) => {
            let d = dir.join(kernel_id);
            if let Err(e) = std::fs::create_dir_all(&d) {
                log::warn!("Failed to create data dir {:?}: {}", d, e);
            }
            d
        }
        None => kernel_data_dir(kernel_id),
    };
    let db_path = db_dir.join("data.db");
    match DocumentDb::open(&db_path) {
        Ok(db) => {
            log::info!("Opened document database at {:?}", db_path);
            let store = Arc::new(BlockStore::with_db_and_flows(db, "server", block_flows));
            if let Err(e) = store.load_from_db() {
                log::warn!("Failed to load documents from DB: {}", e);
            } else {
                log::info!("Loaded {} documents from database", store.len());
            }
            store
        }
        Err(e) => {
            log::warn!("Failed to open document database at {:?}: {}, using in-memory", db_path, e);
            Arc::new(BlockStore::with_flows("server", block_flows))
        }
    }
}

/// Ensure main document exists for a kernel. Returns the main document ID.
/// Uses `@` separator (invalid in UUIDs, unlikely in kernel names) for clear visual distinction.
fn ensure_main_document(
    documents: &SharedBlockStore,
    kernel_id: &str,
) -> Result<String, capnp::Error> {
    let main_document_id = format!("{}@main", kernel_id);
    if !documents.contains(&main_document_id) {
        log::info!("Creating main document {} for kernel {}", main_document_id, kernel_id);
        documents
            .create_document(main_document_id.clone(), DocumentKind::Conversation, None)
            .map_err(|e| capnp::Error::failed(e))?;
    }
    Ok(main_document_id)
}

/// Get the config directory path.
/// Returns: ~/.config/kaijutsu/
fn config_dir() -> std::path::PathBuf {
    kaish_kernel::xdg_config_home()
        .join("kaijutsu")
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
    if let Err(e) = backend.ensure_config("theme.rhai").await {
        log::warn!("Failed to load theme.rhai: {}", e);
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
/// Loads `llm.rhai` from the config CRDT, parses it, and populates
/// the kernel's `LlmRegistry` with providers and aliases.
async fn initialize_kernel_llm(
    kernel: &Arc<Kernel>,
    config_backend: &Arc<ConfigCrdtBackend>,
) {
    // Ensure llm.rhai is loaded
    if let Err(e) = config_backend.ensure_config("llm.rhai").await {
        log::warn!("Failed to load llm.rhai: {}", e);
        return;
    }

    // Get the content
    let script = match config_backend.get_content("llm.rhai") {
        Ok(content) => content,
        Err(e) => {
            log::warn!("Failed to read llm.rhai content: {}", e);
            return;
        }
    };

    // Parse and build registry
    match kaijutsu_kernel::load_llm_config(&script) {
        Ok(config) => {
            let registry = kaijutsu_kernel::initialize_llm_registry(&config);
            *kernel.llm().write().await = registry;
            log::info!("Initialized kernel LLM registry from llm.rhai");
        }
        Err(e) => {
            log::warn!("Failed to parse llm.rhai: {}", e);
        }
    }
}

/// Initialize MCP servers from kernel's `mcp.rhai` config.
///
/// Loads `mcp.rhai` from the config CRDT, parses server definitions,
/// and registers them concurrently in the MCP pool.
async fn initialize_kernel_mcp(
    kernel: &Arc<Kernel>,
    config_backend: &Arc<ConfigCrdtBackend>,
    mcp_pool: &Arc<McpServerPool>,
) {
    if let Err(e) = config_backend.ensure_config("mcp.rhai").await {
        log::warn!("Failed to load mcp.rhai: {}", e);
        return;
    }
    let script = match config_backend.get_content("mcp.rhai") {
        Ok(content) => content,
        Err(e) => {
            log::warn!("Failed to read mcp.rhai content: {}", e);
            return;
        }
    };

    let config = match kaijutsu_kernel::load_mcp_config(&script) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("Failed to parse mcp.rhai: {}", e);
            return;
        }
    };

    if config.servers.is_empty() {
        return;
    }

    log::info!("Registering {} MCP servers from mcp.rhai", config.servers.len());

    // Launch all servers concurrently with per-server timeout
    let timeout = std::time::Duration::from_secs(5);
    let futs: Vec<_> = config.servers.into_iter().map(|server_config| {
        let pool = mcp_pool.clone();
        let kernel = kernel.clone();
        let name = server_config.name.clone();
        async move {
            match tokio::time::timeout(timeout, pool.register(server_config)).await {
                Ok(Ok(info)) => {
                    let tools = McpToolEngine::from_server_tools(pool.clone(), &name, &info.tools);
                    for (qualified_name, engine) in tools {
                        let desc = engine.description().to_string();
                        kernel.register_tool_with_engine(
                            ToolInfo::new(&qualified_name, &desc, "mcp"),
                            engine,
                        ).await;
                    }
                    log::info!("MCP server '{}' registered ({} tools)", name, info.tools.len());
                }
                Ok(Err(e)) => {
                    log::warn!("MCP server '{}' failed to register: {}", name, e);
                }
                Err(_) => {
                    log::warn!("MCP server '{}' timed out during registration ({}s)", name, timeout.as_secs());
                }
            }
        }
    }).collect();

    futures::future::join_all(futs).await;
}

pub struct KernelState {
    pub id: String,
    pub name: String,
    pub consent_mode: ConsentMode,
    pub command_history: Vec<CommandEntry>,
    /// Embedded kaish executor (created lazily) - routes through CRDT backend
    pub kaish: Option<Rc<EmbeddedKaish>>,
    /// The kernel (VFS, state, tools, control plane)
    pub kernel: Arc<Kernel>,
    /// Block-based CRDT store (wrapped for sharing with tools)
    pub documents: SharedBlockStore,
    /// Main document ID for this kernel (convention: {kernel_id}@main)
    pub main_document_id: String,
    /// Contexts within this kernel
    pub contexts: HashMap<String, ContextState>,
    /// Thread-safe context manager for shell access
    pub context_manager: Arc<ContextManager>,
    /// Config CRDT backend (manages theme.rhai, seats/*.rhai)
    pub config_backend: Arc<ConfigCrdtBackend>,
    /// Config watcher handle (stops when kernel is dropped)
    pub config_watcher: Option<ConfigWatcherHandle>,
}

#[derive(Clone, Copy)]
pub enum ConsentMode {
    Collaborative,
    Autonomous,
}

#[derive(Clone)]
pub struct CommandEntry {
    pub id: u64,
    pub code: String,
    pub timestamp: u64,
}

// ============================================================================
// World Implementation
// ============================================================================

/// World capability implementation
pub struct WorldImpl {
    state: Rc<RefCell<ServerState>>,
}

impl WorldImpl {
    pub fn new(state: Rc<RefCell<ServerState>>) -> Self {
        Self { state }
    }

    /// Create a new World capability for use in RPC
    pub fn new_client(username: String) -> world::Client {
        let state = Rc::new(RefCell::new(ServerState::new(username, None)));
        capnp_rpc::new_client(WorldImpl::new(state))
    }
}

impl world::Server for WorldImpl {
    fn whoami(
        self: Rc<Self>,
        _params: world::WhoamiParams,
        mut results: world::WhoamiResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.borrow();
        let mut identity = results.get().init_identity();
        identity.set_username(&state.identity.username);
        identity.set_display_name(&state.identity.display_name);
        Promise::ok(())
    }

    fn list_kernels(
        self: Rc<Self>,
        _params: world::ListKernelsParams,
        mut results: world::ListKernelsResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.borrow();
        let mut kernels = results.get().init_kernels(state.kernels.len() as u32);
        for (i, kernel) in state.kernels.values().enumerate() {
            let mut k = kernels.reborrow().get(i as u32);
            k.set_id(&kernel.id);
            k.set_name(&kernel.name);
            k.set_user_count(1);
            k.set_agent_count(0);
        }
        Promise::ok(())
    }

    fn attach_kernel(
        self: Rc<Self>,
        params: world::AttachKernelParams,
        mut results: world::AttachKernelResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let id = pry!(pry!(params_reader.get_id()).to_str()).to_owned();

        // seat_id param is vestigial — ignored
        let state = self.state.clone();

        Promise::from_future(async move {
            // Create kernel entry if it doesn't exist
            let needs_create = {
                let state_ref = state.borrow();
                !state_ref.kernels.contains_key(&id)
            };

            if needs_create {
                // Create shared FlowBus first - shared between Kernel and BlockStore
                let block_flows = shared_block_flow_bus(1024);
                let config_flows = shared_config_flow_bus(256);

                // Create the kaijutsu kernel with shared FlowBus
                let kernel = Kernel::with_flows(&id, block_flows.clone()).await;

                // Read-only root — whole system visible (ls /usr/bin, cargo, etc.)
                kernel.mount("/", LocalBackend::read_only("/")).await;

                // Read-write ~/src (longest-prefix wins over /)
                let home = kaish_kernel::home_dir();
                let src_dir = home.join("src");
                kernel
                    .mount(&format!("{}", src_dir.display()), LocalBackend::new(&src_dir))
                    .await;

                // Read-write /tmp for scratch/interop with external tools
                kernel.mount("/tmp", LocalBackend::new("/tmp")).await;

                // Create block store with database persistence and shared FlowBus
                let documents = create_block_store_with_db(&id, block_flows, state.borrow().data_dir.as_deref());

                // Ensure main document exists (convention ID)
                let main_document_id = ensure_main_document(&documents, &id)?;

                // Create config backend
                let (config_backend, config_watcher) =
                    create_config_backend(documents.clone(), config_flows, state.borrow().config_dir.as_deref()).await;

                // Get identity for context manager
                let nick = {
                    let state_ref = state.borrow();
                    state_ref.identity.username.clone()
                };

                // Create context manager for this kernel
                let context_manager = Arc::new(ContextManager::new(
                    nick,
                    id.clone(),
                    uuid::Uuid::new_v4().to_string(), // instance ID
                ));

                // Register block tools (including context engine + drift)
                let kernel_arc = Arc::new(kernel);
                register_block_tools(&kernel_arc, documents.clone(), context_manager.clone()).await;

                // Register "default" context in kernel's own DriftRouter
                {
                    let mut drift = kernel_arc.drift().write().await;
                    drift.register("default", &main_document_id, None);
                }

                // Initialize LLM registry from llm.rhai config
                initialize_kernel_llm(&kernel_arc, &config_backend).await;

                // Initialize MCP servers from mcp.rhai config
                let mcp_pool = state.borrow().mcp_pool.clone();
                initialize_kernel_mcp(&kernel_arc, &config_backend, &mcp_pool).await;

                // Create default context
                let mut contexts = HashMap::new();
                contexts.insert("default".to_string(), ContextState::new("default".to_string()));

                let mut state_ref = state.borrow_mut();

                // Register in server-level drift router (for listAllContexts).
                // Use kernel_id as context name — drift RPCs look up by kernel_id.
                state_ref.drift_router.register(&id, &main_document_id, None);

                state_ref.kernels.insert(
                    id.clone(),
                    KernelState {
                        id: id.clone(),
                        name: id.clone(),
                        consent_mode: ConsentMode::Collaborative,
                        command_history: Vec::new(),
                        kaish: None, // Spawned lazily
                        kernel: kernel_arc,
                        documents,
                        main_document_id,
                        contexts,
                        context_manager,
                        config_backend,
                        config_watcher,
                    },
                );
            }

            let kernel_impl = KernelImpl::new(state.clone(), id);
            results.get().set_kernel(capnp_rpc::new_client(kernel_impl));
            Ok(())
        })
    }

    fn create_kernel(
        self: Rc<Self>,
        params: world::CreateKernelParams,
        mut results: world::CreateKernelResults,
    ) -> Promise<(), capnp::Error> {
        let config = pry!(pry!(params.get()).get_config());
        let name = pry!(pry!(config.get_name()).to_str()).to_owned();
        let consent_mode = match config.get_consent_mode() {
            Ok(crate::kaijutsu_capnp::ConsentMode::Autonomous) => ConsentMode::Autonomous,
            _ => ConsentMode::Collaborative,
        };

        let state = self.state.clone();

        Promise::from_future(async move {
            let id = {
                let state_ref = state.borrow();
                state_ref.next_kernel_id()
            };

            // Create shared FlowBus first - shared between Kernel and BlockStore
            let block_flows = shared_block_flow_bus(1024);
            let config_flows = shared_config_flow_bus(256);

            // Create the kaijutsu kernel with shared FlowBus
            let kernel = Kernel::with_flows(&name, block_flows.clone()).await;

            // Read-only root — whole system visible (ls /usr/bin, cargo, etc.)
            kernel.mount("/", LocalBackend::read_only("/")).await;

            // Read-write ~/src (longest-prefix wins over /)
            let home = kaish_kernel::home_dir();
            let src_dir = home.join("src");
            kernel
                .mount(&format!("{}", src_dir.display()), LocalBackend::new(&src_dir))
                .await;

            // Read-write /tmp for scratch/interop with external tools
            kernel.mount("/tmp", LocalBackend::new("/tmp")).await;

            // Create block store with database persistence and shared FlowBus
            let documents = create_block_store_with_db(&id, block_flows, state.borrow().data_dir.as_deref());

            // Ensure main document exists (convention ID)
            let main_document_id = ensure_main_document(&documents, &id)?;

            // Create config backend
            let (config_backend, config_watcher) =
                create_config_backend(documents.clone(), config_flows, state.borrow().config_dir.as_deref()).await;

            // Get identity for context manager
            let nick = {
                let state_ref = state.borrow();
                state_ref.identity.username.clone()
            };

            // Create context manager for this kernel
            let context_manager = Arc::new(ContextManager::new(
                nick,
                id.clone(),
                uuid::Uuid::new_v4().to_string(), // instance ID
            ));

            // Register block tools (including context engine + drift)
            let kernel_arc = Arc::new(kernel);
            register_block_tools(&kernel_arc, documents.clone(), context_manager.clone()).await;

            // Register "default" context in kernel's own DriftRouter
            {
                let mut drift = kernel_arc.drift().write().await;
                drift.register("default", &main_document_id, None);
            }

            // Initialize LLM registry from llm.rhai config
            initialize_kernel_llm(&kernel_arc, &config_backend).await;

            // Initialize MCP servers from mcp.rhai config
            let mcp_pool = state.borrow().mcp_pool.clone();
            initialize_kernel_mcp(&kernel_arc, &config_backend, &mcp_pool).await;

            // Create default context
            let mut contexts = HashMap::new();
            contexts.insert("default".to_string(), ContextState::new("default".to_string()));

            {
                let mut state_ref = state.borrow_mut();

                // Register in server-level drift router for cross-kernel communication
                state_ref.drift_router.register(&id, &main_document_id, None);

                state_ref.kernels.insert(
                    id.clone(),
                    KernelState {
                        id: id.clone(),
                        name,
                        consent_mode,
                        command_history: Vec::new(),
                        kaish: None, // Spawned lazily
                        kernel: kernel_arc,
                        documents,
                        main_document_id,
                        contexts,
                        context_manager,
                        config_backend,
                        config_watcher,
                    },
                );
            }

            let kernel_impl = KernelImpl::new(state.clone(), id);
            results.get().set_kernel(capnp_rpc::new_client(kernel_impl));
            Ok(())
        })
    }

}

// ============================================================================
// Kernel Implementation
// ============================================================================

struct KernelImpl {
    state: Rc<RefCell<ServerState>>,
    kernel_id: String,
}

impl KernelImpl {
    fn new(state: Rc<RefCell<ServerState>>, kernel_id: String) -> Self {
        Self { state, kernel_id }
    }
}

impl kernel::Server for KernelImpl {
    fn get_info(
        self: Rc<Self>,
        _params: kernel::GetInfoParams,
        mut results: kernel::GetInfoResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.borrow();
        if let Some(kernel) = state.kernels.get(&self.kernel_id) {
            let mut info = results.get().init_info();
            info.set_id(&kernel.id);
            info.set_name(&kernel.name);
            info.set_user_count(1);
            info.set_agent_count(0);
        }
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
        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        // Use Promise::from_future for async execution

        Promise::from_future(async move {
            // Get or create embedded kaish executor
            let kaish = {
                let mut state_ref = state.borrow_mut();
                let kernel = state_ref.kernels.get_mut(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;

                if kernel.kaish.is_none() {
                    log::info!("Creating embedded kaish for kernel {}", kernel_id);
                    match EmbeddedKaish::new(
                        &kernel_id,
                        kernel.documents.clone(),
                        kernel.kernel.clone(),
                        None,
                    ) {
                        Ok(kaish) => {
                            kernel.kaish = Some(Rc::new(kaish));
                        }
                        Err(e) => {
                            log::error!("Failed to create embedded kaish: {}", e);
                            return Err(capnp::Error::failed(format!("kaish creation failed: {}", e)));
                        }
                    }
                }

                kernel.kaish.as_ref().unwrap().clone()
            };

            // Execute code via embedded kaish (routes through CRDT backend)
            let _exec_result = match kaish.execute(&code).await {
                Ok(result) => result,
                Err(e) => {
                    log::error!("kaish execute error: {}", e);
                    kaish_kernel::interpreter::ExecResult::failure(1, e.to_string())
                }
            };

            // Record in state and build response
            {
                let mut state_ref = state.borrow_mut();
                let exec_id = state_ref.next_exec_id();
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock before UNIX epoch")
                    .as_secs();

                if let Some(kernel) = state_ref.kernels.get_mut(&kernel_id) {
                    // Record command history
                    kernel.command_history.push(CommandEntry {
                        id: exec_id,
                        code: code.clone(),
                        timestamp,
                    });
                }

                results.get().set_exec_id(exec_id);
            }

            Ok(())
        }.instrument(trace_span))
    }

    fn interrupt(
        self: Rc<Self>,
        _params: kernel::InterruptParams,
        _results: kernel::InterruptResults,
    ) -> Promise<(), capnp::Error> {
        Promise::ok(())
    }

    fn complete(
        self: Rc<Self>,
        _params: kernel::CompleteParams,
        mut results: kernel::CompleteResults,
    ) -> Promise<(), capnp::Error> {
        results.get().init_completions(0);
        Promise::ok(())
    }

    fn subscribe_output(
        self: Rc<Self>,
        _params: kernel::SubscribeOutputParams,
        _results: kernel::SubscribeOutputResults,
    ) -> Promise<(), capnp::Error> {
        Promise::ok(())
    }

    fn get_command_history(
        self: Rc<Self>,
        params: kernel::GetCommandHistoryParams,
        mut results: kernel::GetCommandHistoryResults,
    ) -> Promise<(), capnp::Error> {
        let limit = pry!(params.get()).get_limit() as usize;

        let state = self.state.borrow();
        if let Some(kernel) = state.kernels.get(&self.kernel_id) {
            let entries: Vec<_> = kernel.command_history.iter()
                .rev()
                .take(limit)
                .collect();

            let mut result_entries = results.get().init_entries(entries.len() as u32);
            for (i, entry) in entries.iter().enumerate() {
                let mut e = result_entries.reborrow().get(i as u32);
                e.set_id(entry.id);
                e.set_code(&entry.code);
                e.set_timestamp(entry.timestamp);
            }
        }
        Promise::ok(())
    }

    // Lifecycle

    fn fork(
        self: Rc<Self>,
        params: kernel::ForkParams,
        mut results: kernel::ForkResults,
    ) -> Promise<(), capnp::Error> {
        let name = pry!(pry!(pry!(params.get()).get_name()).to_str()).to_owned();
        let parent_kernel_id = self.kernel_id.clone();
        let state = self.state.clone();

        Promise::from_future(async move {
            // Get parent kernel
            let parent_kernel = {
                let state_ref = state.borrow();
                let parent = state_ref.kernels.get(&parent_kernel_id)
                    .ok_or_else(|| capnp::Error::failed("parent kernel not found".into()))?;
                parent.kernel.clone()
            };

            // Create forked kernel (deep copy, isolated VFS)
            let forked_kernel = parent_kernel.fork(&name).await;

            // Generate new kernel ID
            let id = {
                let state_ref = state.borrow();
                state_ref.next_kernel_id()
            };

            // Create shared FlowBus for the new kernel
            let block_flows = shared_block_flow_bus(1024);
            let config_flows = shared_config_flow_bus(256);

            // Create block store with database persistence
            let documents = create_block_store_with_db(&id, block_flows, state.borrow().data_dir.as_deref());

            // Ensure main document exists
            let main_document_id = ensure_main_document(&documents, &id)?;

            // Create config backend
            let (config_backend, config_watcher) =
                create_config_backend(documents.clone(), config_flows, state.borrow().config_dir.as_deref()).await;

            // Get identity for context manager
            let nick = {
                let state_ref = state.borrow();
                state_ref.identity.username.clone()
            };

            // Create context manager for forked kernel
            let context_manager = Arc::new(ContextManager::new(
                nick,
                id.clone(),
                uuid::Uuid::new_v4().to_string(),
            ));

            // Register block tools on forked kernel (including drift)
            let kernel_arc = Arc::new(forked_kernel);
            register_block_tools(&kernel_arc, documents.clone(), context_manager.clone()).await;

            // LLM registry is inherited from parent via Kernel::fork()
            // (includes runtime setDefaultProvider/setDefaultModel changes)

            // Register "default" context in kernel's own DriftRouter
            {
                let mut drift = kernel_arc.drift().write().await;
                drift.register("default", &main_document_id, None);
            }

            // Create default context
            let mut contexts = HashMap::new();
            contexts.insert("default".to_string(), ContextState::new("default".to_string()));

            // Get parent's consent mode
            let consent_mode = {
                let state_ref = state.borrow();
                state_ref.kernels.get(&parent_kernel_id)
                    .map(|k| k.consent_mode)
                    .unwrap_or(ConsentMode::Collaborative)
            };

            // Register the forked kernel
            {
                let mut state_ref = state.borrow_mut();

                // Register in server-level drift router with parent lineage
                let parent_short = state_ref.drift_router
                    .short_id_for_context(&parent_kernel_id)
                    .map(|s| s.to_string());
                state_ref.drift_router.register(
                    &id,
                    &main_document_id,
                    parent_short.as_deref(),
                );

                state_ref.kernels.insert(
                    id.clone(),
                    KernelState {
                        id: id.clone(),
                        name,
                        consent_mode,
                        command_history: Vec::new(),
                        kaish: None,
                        kernel: kernel_arc,
                        documents,
                        main_document_id,
                        contexts,
                        context_manager,
                        config_backend,
                        config_watcher,
                    },
                );
            }

            log::info!("Forked kernel {} from {}", id, parent_kernel_id);

            let kernel_impl = KernelImpl::new(state.clone(), id);
            results.get().set_kernel(capnp_rpc::new_client(kernel_impl));
            Ok(())
        })
    }

    fn thread(
        self: Rc<Self>,
        params: kernel::ThreadParams,
        mut results: kernel::ThreadResults,
    ) -> Promise<(), capnp::Error> {
        let name = pry!(pry!(pry!(params.get()).get_name()).to_str()).to_owned();
        let parent_kernel_id = self.kernel_id.clone();
        let state = self.state.clone();

        Promise::from_future(async move {
            // Get parent kernel and documents (thread shares VFS + documents)
            let (parent_kernel, parent_documents, parent_config_backend) = {
                let state_ref = state.borrow();
                let parent = state_ref.kernels.get(&parent_kernel_id)
                    .ok_or_else(|| capnp::Error::failed("parent kernel not found".into()))?;
                (parent.kernel.clone(), parent.documents.clone(), parent.config_backend.clone())
            };

            // Create threaded kernel (light copy, shared VFS + FlowBus)
            let threaded_kernel = parent_kernel.thread(&name).await;

            // Generate new kernel ID
            let id = {
                let state_ref = state.borrow();
                state_ref.next_kernel_id()
            };

            // Thread shares the parent's documents (same block store)
            let documents = parent_documents;

            // Create main document for this thread (separate conversation)
            let main_document_id = ensure_main_document(&documents, &id)?;

            // Get identity for context manager
            let nick = {
                let state_ref = state.borrow();
                state_ref.identity.username.clone()
            };

            // Create context manager for threaded kernel
            let context_manager = Arc::new(ContextManager::new(
                nick,
                id.clone(),
                uuid::Uuid::new_v4().to_string(),
            ));

            // Register block tools on threaded kernel (including drift)
            let kernel_arc = Arc::new(threaded_kernel);
            register_block_tools(&kernel_arc, documents.clone(), context_manager.clone()).await;

            // LLM registry is inherited from parent via Kernel::thread()
            // (includes runtime setDefaultProvider/setDefaultModel changes)

            // Register "default" context in kernel's own DriftRouter
            {
                let mut drift = kernel_arc.drift().write().await;
                drift.register("default", &main_document_id, None);
            }

            // Create default context
            let mut contexts = HashMap::new();
            contexts.insert("default".to_string(), ContextState::new("default".to_string()));

            // Get parent's consent mode
            let consent_mode = {
                let state_ref = state.borrow();
                state_ref.kernels.get(&parent_kernel_id)
                    .map(|k| k.consent_mode)
                    .unwrap_or(ConsentMode::Collaborative)
            };

            // Register the threaded kernel (shares config backend with parent)
            {
                let mut state_ref = state.borrow_mut();

                // Register in server-level drift router with parent lineage
                let parent_short = state_ref.drift_router
                    .short_id_for_context(&parent_kernel_id)
                    .map(|s| s.to_string());
                state_ref.drift_router.register(
                    &id,
                    &main_document_id,
                    parent_short.as_deref(),
                );

                state_ref.kernels.insert(
                    id.clone(),
                    KernelState {
                        id: id.clone(),
                        name,
                        consent_mode,
                        command_history: Vec::new(),
                        kaish: None,
                        kernel: kernel_arc,
                        documents,
                        main_document_id,
                        contexts,
                        context_manager,
                        config_backend: parent_config_backend,
                        config_watcher: None, // Thread doesn't own the watcher
                    },
                );
            }

            log::info!("Threaded kernel {} from {}", id, parent_kernel_id);

            let kernel_impl = KernelImpl::new(state.clone(), id);
            results.get().set_kernel(capnp_rpc::new_client(kernel_impl));
            Ok(())
        })
    }

    fn detach(
        self: Rc<Self>,
        _params: kernel::DetachParams,
        _results: kernel::DetachResults,
    ) -> Promise<(), capnp::Error> {
        Promise::ok(())
    }

    // VFS methods

    fn vfs(
        self: Rc<Self>,
        _params: kernel::VfsParams,
        mut results: kernel::VfsResults,
    ) -> Promise<(), capnp::Error> {
        // Get the kernel and return a VFS capability backed by it
        let state = self.state.borrow();
        let kernel = state
            .kernels
            .get(&self.kernel_id)
            .map(|k| k.kernel.clone());

        match kernel {
            Some(kernel) => {
                let vfs_impl = VfsImpl::new(kernel);
                results.get().set_vfs(capnp_rpc::new_client(vfs_impl));
                Promise::ok(())
            }
            None => Promise::err(capnp::Error::failed("kernel not found".into())),
        }
    }

    fn list_mounts(
        self: Rc<Self>,
        _params: kernel::ListMountsParams,
        mut results: kernel::ListMountsResults,
    ) -> Promise<(), capnp::Error> {
        // Get the kernel
        let state = self.state.borrow();
        let kernel = match state.kernels.get(&self.kernel_id) {
            Some(k) => k.kernel.clone(),
            None => return Promise::err(capnp::Error::failed("kernel not found".into())),
        };
        drop(state);

        Promise::from_future(async move {
            let mounts = kernel.list_mounts().await;
            let mut builder = results.get().init_mounts(mounts.len() as u32);
            for (i, mount) in mounts.iter().enumerate() {
                let mut m = builder.reborrow().get(i as u32);
                m.set_path(&mount.path.to_string_lossy());
                m.set_read_only(mount.read_only);
            }
            Ok(())
        })
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

        // Get the kernel
        let state = self.state.borrow();
        let kernel = match state.kernels.get(&self.kernel_id) {
            Some(k) => k.kernel.clone(),
            None => return Promise::err(capnp::Error::failed("kernel not found".into())),
        };
        drop(state);

        Promise::from_future(async move {
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

            kernel.mount(&path, backend).await;
            Ok(())
        })
    }

    fn unmount(
        self: Rc<Self>,
        params: kernel::UnmountParams,
        mut results: kernel::UnmountResults,
    ) -> Promise<(), capnp::Error> {
        let path = match params.get().and_then(|p| p.get_path()).and_then(|p| get_path_str(p)) {
            Ok(s) => s,
            Err(e) => return Promise::err(e),
        };

        // Get the kernel
        let state = self.state.borrow();
        let kernel = match state.kernels.get(&self.kernel_id) {
            Some(k) => k.kernel.clone(),
            None => return Promise::err(capnp::Error::failed("kernel not found".into())),
        };
        drop(state);

        Promise::from_future(async move {
            let success = kernel.unmount(&path).await;
            results.get().set_success(success);
            Ok(())
        })
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

        let state = self.state.borrow();
        let kernel = match state.kernels.get(&self.kernel_id) {
            Some(k) => k.kernel.clone(),
            None => return Promise::err(capnp::Error::failed("kernel not found".into())),
        };
        drop(state);


        Promise::from_future(async move {
            let mut result = results.get().init_result();
            result.set_request_id(&request_id);

            // Check if tool is allowed by kernel's tool filter
            if !kernel.tool_allowed(&tool_name).await {
                result.set_success(false);
                result.set_error(&format!("Tool filtered out by kernel config: {}", tool_name));
                return Ok(());
            }

            // Get the engine for this tool
            let engine = kernel.tools().read().await.get_engine(&tool_name);
            match engine {
                Some(engine) => {
                    match engine.execute(&tool_params).await {
                        Ok(exec_result) => {
                            result.set_success(exec_result.success);
                            result.set_output(&exec_result.stdout);
                            if !exec_result.stderr.is_empty() {
                                result.set_error(&exec_result.stderr);
                            }
                        }
                        Err(e) => {
                            result.set_success(false);
                            result.set_error(&e.to_string());
                        }
                    }
                }
                None => {
                    result.set_success(false);
                    result.set_error(&format!("No engine registered for tool: {}", tool_name));
                }
            }
            Ok(())
        }.instrument(trace_span))
    }

    fn get_tool_schemas(
        self: Rc<Self>,
        _params: kernel::GetToolSchemasParams,
        mut results: kernel::GetToolSchemasResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.borrow();
        let kernel = match state.kernels.get(&self.kernel_id) {
            Some(k) => k.kernel.clone(),
            None => return Promise::err(capnp::Error::failed("kernel not found".into())),
        };
        drop(state);

        Promise::from_future(async move {
            let registry = kernel.tools().read().await;
            let tools = registry.list();
            let mut builder = results.get().init_schemas(tools.len() as u32);
            for (i, tool) in tools.iter().enumerate() {
                let mut s = builder.reborrow().get(i as u32);
                s.set_name(&tool.name);
                s.set_description(&tool.description);
                s.set_category(&tool.category);
                // Get schema from the engine if available
                let schema = registry
                    .get_engine(&tool.name)
                    .and_then(|e| e.schema())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "{}".to_string());
                s.set_input_schema(&schema);
            }
            Ok(())
        })
    }

    // =========================================================================
    // Block-based CRDT operations (new architecture)
    // =========================================================================

    fn apply_block_op(
        self: Rc<Self>,
        params: kernel::ApplyBlockOpParams,
        mut results: kernel::ApplyBlockOpResults,
    ) -> Promise<(), capnp::Error> {
        let params = pry!(params.get());
        let cell_id = pry!(pry!(params.get_document_id()).to_str()).to_owned();
        let op = pry!(params.get_op());

        log::debug!("apply_block_op called for cell {}", cell_id);

        let mut state = self.state.borrow_mut();
        let kernel = match state.kernels.get_mut(&self.kernel_id) {
            Some(k) => k,
            None => return Promise::err(capnp::Error::failed("kernel not found".into())),
        };

        // Handle each operation variant
        use crate::kaijutsu_capnp::block_doc_op::Which;
        match pry!(op.which()) {
            Which::InsertBlock(group) => {
                let block_reader = pry!(group.get_block());
                let block = pry!(parse_block_snapshot(&block_reader));
                let after_id = if group.get_has_after_id() {
                    let after_reader = pry!(group.get_after_id());
                    Some(kaijutsu_crdt::BlockId {
                        document_id: pry!(pry!(after_reader.get_document_id()).to_str()).to_owned(),
                        agent_id: pry!(pry!(after_reader.get_agent_id()).to_str()).to_owned(),
                        seq: after_reader.get_seq(),
                    })
                } else {
                    None
                };

                // Insert block using the snapshot data
                if let Err(e) = kernel.documents.insert_block(
                    &cell_id,
                    block.parent_id.as_ref(),
                    after_id.as_ref(),
                    block.role,
                    block.kind,
                    &block.content,
                ) {
                    return Promise::err(capnp::Error::failed(e));
                }
            }
            Which::DeleteBlock(id_result) => {
                let id_reader = pry!(id_result);
                let block_id = kaijutsu_crdt::BlockId {
                    document_id: pry!(pry!(id_reader.get_document_id()).to_str()).to_owned(),
                    agent_id: pry!(pry!(id_reader.get_agent_id()).to_str()).to_owned(),
                    seq: id_reader.get_seq(),
                };
                if let Err(e) = kernel.documents.delete_block(&cell_id, &block_id) {
                    return Promise::err(capnp::Error::failed(e));
                }
            }
            Which::EditBlockText(_) => {
                // Deprecated: position-based edits replaced by CRDT ops (25b2bc6)
            }
            Which::SetCollapsed(group) => {
                let id_reader = pry!(group.get_id());
                let block_id = kaijutsu_crdt::BlockId {
                    document_id: pry!(pry!(id_reader.get_document_id()).to_str()).to_owned(),
                    agent_id: pry!(pry!(id_reader.get_agent_id()).to_str()).to_owned(),
                    seq: id_reader.get_seq(),
                };
                let collapsed = group.get_collapsed();
                if let Err(e) = kernel.documents.set_collapsed(&cell_id, &block_id, collapsed) {
                    return Promise::err(capnp::Error::failed(e));
                }
            }
            Which::SetStatus(group) => {
                let id_reader = pry!(group.get_id());
                let block_id = kaijutsu_crdt::BlockId {
                    document_id: pry!(pry!(id_reader.get_document_id()).to_str()).to_owned(),
                    agent_id: pry!(pry!(id_reader.get_agent_id()).to_str()).to_owned(),
                    seq: id_reader.get_seq(),
                };
                let status = match pry!(group.get_status()) {
                    crate::kaijutsu_capnp::Status::Pending => kaijutsu_crdt::Status::Pending,
                    crate::kaijutsu_capnp::Status::Running => kaijutsu_crdt::Status::Running,
                    crate::kaijutsu_capnp::Status::Done => kaijutsu_crdt::Status::Done,
                    crate::kaijutsu_capnp::Status::Error => kaijutsu_crdt::Status::Error,
                };
                if let Err(e) = kernel.documents.set_status(&cell_id, &block_id, status) {
                    return Promise::err(capnp::Error::failed(e));
                }
            }
            Which::MoveBlock(_group) => {
                // Move operation not yet implemented in BlockStore
                log::warn!("MoveBlock operation not yet implemented");
            }
        };

        // Return the new version
        let new_version = kernel.documents.get(&cell_id)
            .map(|entry| entry.version())
            .unwrap_or(0);
        results.get().set_new_version(new_version);
        Promise::ok(())
    }

    fn subscribe_blocks(
        self: Rc<Self>,
        params: kernel::SubscribeBlocksParams,
        _results: kernel::SubscribeBlocksResults,
    ) -> Promise<(), capnp::Error> {
        let callback = pry!(pry!(params.get()).get_callback());

        let state = self.state.borrow();
        if let Some(kernel_state) = state.kernels.get(&self.kernel_id) {
            // Get the FlowBus from the kernel
            let block_flows = kernel_state.kernel.block_flows().clone();
            let kernel_id = self.kernel_id.clone();
            drop(state);

            // Spawn a bridge task that forwards FlowBus events to the callback
            // Use spawn_local because Cap'n Proto callbacks are not Send
            tokio::task::spawn_local(async move {
                let mut sub = block_flows.subscribe("block.*");
                log::debug!("Started FlowBus subscription for kernel {}", kernel_id);

                while let Some(msg) = sub.recv().await {
                    // Each branch sends its own request type; we convert the result to bool
                    let success = match msg.payload {
                        BlockFlow::Inserted { ref document_id, ref block, ref after_id, ref ops, .. } => {
                            let mut req = callback.on_block_inserted_request();
                            {
                                let mut params = req.get();
                                params.set_document_id(document_id);
                                params.set_has_after_id(after_id.is_some());
                                if let Some(after) = after_id {
                                    let mut aid = params.reborrow().init_after_id();
                                    aid.set_document_id(&after.document_id);
                                    aid.set_agent_id(&after.agent_id);
                                    aid.set_seq(after.seq);
                                }
                                // Include CRDT ops for proper sync
                                params.set_ops(ops);
                                let mut block_state = params.init_block();
                                set_block_snapshot(&mut block_state, block);
                            }
                            req.send().promise.await.is_ok()
                        }
                        BlockFlow::Deleted { ref document_id, ref block_id, .. } => {
                            let mut req = callback.on_block_deleted_request();
                            {
                                let mut params = req.get();
                                params.set_document_id(document_id);
                                let mut id = params.reborrow().init_block_id();
                                id.set_document_id(&block_id.document_id);
                                id.set_agent_id(&block_id.agent_id);
                                id.set_seq(block_id.seq);
                            }
                            req.send().promise.await.is_ok()
                        }
                        BlockFlow::StatusChanged { ref document_id, ref block_id, status, .. } => {
                            let mut req = callback.on_block_status_changed_request();
                            {
                                let mut params = req.get();
                                params.set_document_id(document_id);
                                let mut id = params.reborrow().init_block_id();
                                id.set_document_id(&block_id.document_id);
                                id.set_agent_id(&block_id.agent_id);
                                id.set_seq(block_id.seq);
                                params.set_status(status_to_capnp(status));
                            }
                            req.send().promise.await.is_ok()
                        }
                        BlockFlow::CollapsedChanged { ref document_id, ref block_id, collapsed, .. } => {
                            let mut req = callback.on_block_collapsed_request();
                            {
                                let mut params = req.get();
                                params.set_document_id(document_id);
                                let mut id = params.reborrow().init_block_id();
                                id.set_document_id(&block_id.document_id);
                                id.set_agent_id(&block_id.agent_id);
                                id.set_seq(block_id.seq);
                                params.set_collapsed(collapsed);
                            }
                            req.send().promise.await.is_ok()
                        }
                        BlockFlow::Moved { ref document_id, ref block_id, ref after_id, .. } => {
                            let mut req = callback.on_block_moved_request();
                            {
                                let mut params = req.get();
                                params.set_document_id(document_id);
                                let mut id = params.reborrow().init_block_id();
                                id.set_document_id(&block_id.document_id);
                                id.set_agent_id(&block_id.agent_id);
                                id.set_seq(block_id.seq);
                                params.set_has_after_id(after_id.is_some());
                                if let Some(after) = after_id {
                                    let mut aid = params.reborrow().init_after_id();
                                    aid.set_document_id(&after.document_id);
                                    aid.set_agent_id(&after.agent_id);
                                    aid.set_seq(after.seq);
                                }
                            }
                            req.send().promise.await.is_ok()
                        }
                        BlockFlow::TextOps { ref document_id, ref block_id, ref ops, .. } => {
                            let mut req = callback.on_block_text_ops_request();
                            {
                                let mut params = req.get();
                                params.set_document_id(document_id);
                                let mut id = params.reborrow().init_block_id();
                                id.set_document_id(&block_id.document_id);
                                id.set_agent_id(&block_id.agent_id);
                                id.set_seq(block_id.seq);
                                params.set_ops(ops);
                            }
                            req.send().promise.await.is_ok()
                        }
                        BlockFlow::SyncReset { ref document_id, generation } => {
                            let mut req = callback.on_sync_reset_request();
                            {
                                let mut params = req.get();
                                params.set_document_id(document_id);
                                params.set_generation(generation);
                            }
                            req.send().promise.await.is_ok()
                        }
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

    fn get_document_state(
        self: Rc<Self>,
        params: kernel::GetDocumentStateParams,
        mut results: kernel::GetDocumentStateResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_span = extract_rpc_trace(p.get_trace(), "get_document_state");
        let cell_id = pry!(pry!(p.get_document_id()).to_str()).to_owned();

        log::debug!("get_document_state called for cell {}", cell_id);

        let state = self.state.borrow();
        let kernel = match state.kernels.get(&self.kernel_id) {
            Some(k) => k,
            None => {
                return Promise::err(capnp::Error::failed(format!(
                    "kernel '{}' not found",
                    self.kernel_id
                )));
            }
        };

        let doc = match kernel.documents.get(&cell_id) {
            Some(d) => d,
            None => {
                return Promise::err(capnp::Error::failed(format!(
                    "cell '{}' not found in kernel '{}'",
                    cell_id, self.kernel_id
                )));
            }
        };

        // Check oplog size — compact if over 100KB to keep initial sync fast
        let oplog_bytes = doc.doc.oplog_bytes().unwrap_or_default();
        let needs_compaction = oplog_bytes.len() > 100_000;
        drop(doc); // Release read ref before potential compaction

        if needs_compaction {
            if let Err(e) = kernel.documents.compact_document_silent(&cell_id) {
                log::warn!("Failed to compact document {}: {}", cell_id, e);
            }
        }

        // Re-acquire read ref (post-compaction if it happened)
        let doc = match kernel.documents.get(&cell_id) {
            Some(d) => d,
            None => {
                return Promise::err(capnp::Error::failed(format!(
                    "cell '{}' not found in kernel '{}'",
                    cell_id, self.kernel_id
                )));
            }
        };

        let mut cell_state = results.get().init_state();
        cell_state.set_document_id(&cell_id);
        cell_state.reborrow().set_version(doc.version());

        // Get actual blocks from BlockDocument
        let blocks = doc.doc.blocks_ordered();
        let mut block_list = cell_state.reborrow().init_blocks(blocks.len() as u32);
        for (i, block) in blocks.iter().enumerate() {
            let mut block_builder = block_list.reborrow().get(i as u32);
            set_block_snapshot(&mut block_builder, block);
        }

        // Send full oplog for proper CRDT sync
        // This enables clients to merge subsequent incremental ops
        let oplog_bytes = doc.doc.oplog_bytes().unwrap_or_default();
        cell_state.set_ops(&oplog_bytes);
        log::debug!(
            "Sending DocumentState for cell {} with {} blocks, {} bytes oplog",
            cell_id,
            blocks.len(),
            oplog_bytes.len()
        );

        Promise::ok(())
    }

    // =========================================================================
    // LLM operations
    // =========================================================================

    fn prompt(
        self: Rc<Self>,
        params: kernel::PromptParams,
        mut results: kernel::PromptResults,
    ) -> Promise<(), capnp::Error> {
        log::debug!("prompt() called for kernel {}", self.kernel_id);
        let params = pry!(params.get());
        let trace_span = extract_rpc_trace(params.get_trace(), "prompt");
        let request = pry!(params.get_request());
        let content = pry!(pry!(request.get_content()).to_str()).to_owned();
        log::info!("Received prompt request: cell_id={}, content_len={}",
            pry!(request.get_document_id()).to_str().unwrap_or("?"), content.len());
        // Note: Cap'n Proto defaults unset Text fields to "", so we filter empty strings
        let model = request.get_model().ok()
            .and_then(|m| m.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned());
        let cell_id = pry!(pry!(request.get_document_id()).to_str()).to_owned();

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();


        Promise::from_future(async move {
            log::debug!("prompt future started for cell_id={}", cell_id);

            // Get LLM provider and kernel references from the kernel's own registry
            let (documents, kernel_arc, config_backend) = {
                let state_ref = state.borrow();
                let kernel_state = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| {
                        log::error!("kernel {} not found", kernel_id);
                        capnp::Error::failed("kernel not found".into())
                    })?;
                log::debug!("Got kernel state");

                (kernel_state.documents.clone(), kernel_state.kernel.clone(), kernel_state.config_backend.clone())
            };

            // Load system prompt from config
            let system_prompt = {
                if let Err(e) = config_backend.ensure_config("system.md").await {
                    log::warn!("Failed to ensure system.md config: {}", e);
                }
                config_backend.get_content("system.md")
                    .unwrap_or_else(|_| kaijutsu_kernel::DEFAULT_SYSTEM_PROMPT.to_string())
            };

            // Resolve provider from kernel's LLM registry
            let (provider, model_name, max_output_tokens) = {
                let registry = kernel_arc.llm().read().await;
                let requested_model = model.as_deref();
                let max_tokens = registry.max_output_tokens();

                // Try alias resolution first, then default
                if let Some(name) = requested_model {
                    if let Some((p, m)) = registry.resolve_model(name) {
                        (p, m, max_tokens)
                    } else {
                        let p = registry.default_provider().ok_or_else(|| {
                            log::error!("No LLM provider configured");
                            capnp::Error::failed("No LLM provider configured (check llm.rhai)".into())
                        })?;
                        (p, name.to_string(), max_tokens)
                    }
                } else {
                    let p = registry.default_provider().ok_or_else(|| {
                        log::error!("No LLM provider configured");
                        capnp::Error::failed("No LLM provider configured (check llm.rhai)".into())
                    })?;
                    let m = registry.default_model()
                        .unwrap_or(kaijutsu_kernel::DEFAULT_MODEL)
                        .to_string();
                    (p, m, max_tokens)
                }
            };

            // Build tool definitions from equipped tools (async)
            let tools = build_tool_definitions(&kernel_arc).await;

            // Generate prompt ID
            let prompt_id = uuid::Uuid::new_v4().to_string();
            log::debug!("Generated prompt_id={}", prompt_id);

            // Document must exist — join_context is the sole creator
            if documents.get(&cell_id).is_none() {
                return Err(capnp::Error::failed(
                    format!("document {} not found — call join_context first", cell_id)
                ));
            }

            // Create user message block at the end of the document
            let last_block = documents.last_block_id(&cell_id);
            log::info!("Inserting user block into document {}, after={:?}", cell_id, last_block);
            let user_block_id = documents.insert_block(&cell_id, None, last_block.as_ref(), Role::User, BlockKind::Text, &content)
                .map_err(|e| {
                    log::error!("Failed to insert user block: {}", e);
                    capnp::Error::failed(format!("failed to insert user block: {}", e))
                })?;
            log::debug!("Inserted user block: {:?}", user_block_id);

            log::info!("User message block inserted, spawning LLM stream task");
            log::info!("Using model: {} (requested: {:?})", model_name, model);

            // Spawn LLM streaming in background task with agentic loop
            // Pass user block_id so streaming blocks are inserted AFTER it
            tokio::task::spawn_local(process_llm_stream(
                provider,
                documents,
                cell_id,
                content,
                model_name,
                kernel_arc,
                tools,
                user_block_id, // last_block_id - streaming blocks appear after this
                system_prompt,
                max_output_tokens,
            ));

            // Return immediately with prompt_id - streaming happens in background
            results.get().set_prompt_id(&prompt_id);
            log::debug!("prompt() returning immediately with prompt_id={}", prompt_id);
            Ok(())
        }.instrument(trace_span))
    }

    // =========================================================================
    // Context operations
    // =========================================================================

    fn list_contexts(
        self: Rc<Self>,
        _params: kernel::ListContextsParams,
        mut results: kernel::ListContextsResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.borrow();
        if let Some(kernel) = state.kernels.get(&self.kernel_id) {
            let contexts: Vec<_> = kernel.contexts.values().collect();
            let mut ctx_list = results.get().init_contexts(contexts.len() as u32);

            for (i, ctx) in contexts.iter().enumerate() {
                let mut c = ctx_list.reborrow().get(i as u32);
                c.set_name(&ctx.name);

                // Populate documents list
                let mut docs = c.reborrow().init_documents(ctx.documents.len() as u32);
                for (j, doc) in ctx.documents.iter().enumerate() {
                    let mut d = docs.reborrow().get(j as u32);
                    d.set_id(&doc.id);
                    d.set_attached_by(&doc.attached_by);
                    d.set_attached_at(doc.attached_at);
                }

            }
        }
        Promise::ok(())
    }

    /// Join a context, returning its document_id.
    ///
    /// Creates the context and document if they don't exist. Registers in
    /// both kernel-level and server-level drift routers. The `instance` param
    /// identifies which client connected (for logging/debugging).
    ///
    /// Note: If multi-user presence is needed later, this is the place to
    /// reintroduce a thinner presence protocol.
    fn join_context(
        self: Rc<Self>,
        params: kernel::JoinContextParams,
        mut results: kernel::JoinContextResults,
    ) -> Promise<(), capnp::Error> {
        let params = pry!(params.get());
        let context_name = pry!(pry!(params.get_context_name()).to_str()).to_owned();
        let instance = pry!(pry!(params.get_instance()).to_str()).to_owned();

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        log::info!("join_context: context='{}' instance='{}' kernel='{}'",
            context_name, instance, kernel_id);

        let state2 = state.clone();
        let kernel_id2 = kernel_id.clone();
        let context_name2 = context_name.clone();

        Promise::from_future(async move {
            let doc_id = format!("{}@{}", kernel_id2, context_name2);

            // Update state — grab kernel Arc, then drop borrow before async drift registration
            let kernel_arc = {
                let mut state_ref = state2.borrow_mut();

                // Ensure context exists (create if not)
                if let Some(kernel) = state_ref.kernels.get_mut(&kernel_id2) {
                    kernel.contexts
                        .entry(context_name2.clone())
                        .or_insert_with(|| ContextState::new(context_name2.clone()));

                    // Create the document for this context (join_context is the sole creator)
                    if !kernel.documents.contains(&doc_id) {
                        log::info!("Creating document {} for context {}", doc_id, context_name2);
                        if let Err(e) = kernel.documents.create_document(
                            doc_id.clone(),
                            DocumentKind::Conversation,
                            None,
                        ) {
                            log::error!("Failed to create document {}: {}", doc_id, e);
                        }
                    } else {
                        log::debug!("Re-joining existing context document {}", doc_id);
                    }

                    Some(kernel.kernel.clone())
                } else {
                    None
                }
            };
            // borrow dropped here — safe to .await

            // Register context in both drift routers so drift operations work
            if let Some(kernel) = kernel_arc {
                // Kernel-level drift router (used by push/pull/flush/merge RPCs)
                {
                    let mut drift = kernel.drift().write().await;
                    let exists = drift.list_contexts().iter()
                        .any(|c| c.context_name == context_name2);
                    if !exists {
                        drift.register(&context_name2, &doc_id, None);
                    }
                }

                // Server-level drift router (used by listAllContexts)
                {
                    let mut state_ref = state2.borrow_mut();
                    let already_in_server = state_ref.drift_router.list_contexts().iter()
                        .any(|c| c.context_name == context_name2);
                    if !already_in_server {
                        let short_id = state_ref.drift_router.register(&context_name2, &doc_id, None);
                        log::info!("Registered context '{}' (doc: {}, short_id: {}) in server DriftRouter",
                            context_name2, doc_id, short_id);
                    }
                }
            }

            // Return the document_id
            results.get().set_document_id(&doc_id);

            Ok(())
        })
    }

    // ========================================================================
    // MCP (Model Context Protocol) methods
    // ========================================================================

    fn register_mcp(
        self: Rc<Self>,
        params: kernel::RegisterMcpParams,
        mut results: kernel::RegisterMcpResults,
    ) -> Promise<(), capnp::Error> {
        let config_reader = pry!(pry!(params.get()).get_config());
        let name = pry!(pry!(config_reader.get_name()).to_str()).to_owned();
        let command = pry!(pry!(config_reader.get_command()).to_str()).to_owned();

        // Get arguments
        let args: Vec<String> = if config_reader.has_args() {
            pry!(config_reader.get_args())
                .iter()
                .filter_map(|a| a.ok().and_then(|s| s.to_str().ok()).map(|s| s.to_owned()))
                .collect()
        } else {
            Vec::new()
        };

        // Get environment variables
        let mut env = std::collections::HashMap::new();
        if config_reader.has_env() {
            for env_var in pry!(config_reader.get_env()).iter() {
                if let (Ok(key_reader), Ok(value_reader)) = (env_var.get_key(), env_var.get_value()) {
                    if let (Ok(key), Ok(value)) = (key_reader.to_str(), value_reader.to_str()) {
                        env.insert(key.to_owned(), value.to_owned());
                    }
                }
            }
        }

        // Get working directory
        let cwd = if config_reader.has_cwd() {
            let cwd_reader = pry!(config_reader.get_cwd());
            if let Ok(cwd_str) = cwd_reader.to_str() {
                if cwd_str.is_empty() { None } else { Some(cwd_str.to_owned()) }
            } else {
                None
            }
        } else {
            None
        };

        // Get transport type (default: stdio)
        let transport = if config_reader.has_transport() {
            match pry!(config_reader.get_transport()).to_str() {
                Ok("stdio") | Ok("") => McpTransport::Stdio,
                Ok("streamable_http") => McpTransport::StreamableHttp,
                Ok(other) => {
                    log::warn!("Unknown MCP transport '{}' for '{}', defaulting to stdio", other, name);
                    McpTransport::Stdio
                }
                Err(_) => McpTransport::Stdio,
            }
        } else {
            McpTransport::Stdio
        };

        // Get URL (for streamable HTTP transport)
        let url = if config_reader.has_url() {
            let url_reader = pry!(config_reader.get_url());
            if let Ok(url_str) = url_reader.to_str() {
                if url_str.is_empty() { None } else { Some(url_str.to_owned()) }
            } else {
                None
            }
        } else {
            None
        };

        let config = McpServerConfig {
            name: name.clone(),
            command,
            args,
            env,
            cwd,
            transport,
            url,
            fork_mode: Default::default(),
        };

        let mcp_pool = self.state.borrow().mcp_pool.clone();
        let kernel_arc = self.state.borrow().kernels.get(&self.kernel_id)
            .map(|k| k.kernel.clone());

        Promise::from_future(async move {
            let info = mcp_pool.register(config).await
                .map_err(|e| capnp::Error::failed(format!("Failed to register MCP server: {}", e)))?;

            // Register MCP tools with the kernel if we have one
            // Tools with engines are automatically available via ToolFilter
            if let Some(kernel) = kernel_arc {
                let tools = McpToolEngine::from_server_tools(mcp_pool.clone(), &name, &info.tools);
                for (qualified_name, engine) in tools {
                    let desc = engine.description().to_string();
                    kernel.register_tool_with_engine(
                        ToolInfo::new(&qualified_name, &desc, "mcp"),
                        engine,
                    ).await;
                }
            }

            // Build the result
            let mut info_builder = results.get().init_info();
            info_builder.set_name(&info.name);
            info_builder.set_protocol_version(&info.protocol_version);
            info_builder.set_server_name(&info.server_name);
            info_builder.set_server_version(&info.server_version);

            let mut tools_builder = info_builder.init_tools(info.tools.len() as u32);
            for (i, tool) in info.tools.iter().enumerate() {
                let mut tool_builder = tools_builder.reborrow().get(i as u32);
                tool_builder.set_name(&tool.name);
                tool_builder.set_description(&tool.description.clone().unwrap_or_default());
                tool_builder.set_input_schema(&tool.input_schema.to_string());
            }

            Ok(())
        })
    }

    fn unregister_mcp(
        self: Rc<Self>,
        params: kernel::UnregisterMcpParams,
        _results: kernel::UnregisterMcpResults,
    ) -> Promise<(), capnp::Error> {
        let name = pry!(pry!(pry!(params.get()).get_name()).to_str()).to_owned();
        let mcp_pool = self.state.borrow().mcp_pool.clone();
        let kernel_arc = self.state.borrow().kernels.get(&self.kernel_id)
            .map(|k| k.kernel.clone());

        Promise::from_future(async move {
            // Remove engines for MCP tools before unregistering the server
            if let Some(kernel) = kernel_arc {
                if let Ok(info) = mcp_pool.get_server_info(&name).await {
                    let mut registry = kernel.tools().write().await;
                    for tool in &info.tools {
                        let qualified_name = format!("{}:{}", name, tool.name);
                        registry.remove_engine(&qualified_name);
                    }
                }
            }

            mcp_pool.unregister(&name).await
                .map_err(|e| capnp::Error::failed(format!("Failed to unregister MCP server: {}", e)))?;
            Ok(())
        })
    }

    fn list_mcp_servers(
        self: Rc<Self>,
        _params: kernel::ListMcpServersParams,
        mut results: kernel::ListMcpServersResults,
    ) -> Promise<(), capnp::Error> {
        let mcp_pool = self.state.borrow().mcp_pool.clone();

        Promise::from_future(async move {
            let server_names = mcp_pool.list_servers();
            let mut servers_info = Vec::new();

            for name in server_names {
                if let Ok(info) = mcp_pool.get_server_info(&name).await {
                    servers_info.push(info);
                }
            }

            // Build the result
            let mut servers_builder = results.get().init_servers(servers_info.len() as u32);
            for (i, info) in servers_info.iter().enumerate() {
                let mut server_builder = servers_builder.reborrow().get(i as u32);
                server_builder.set_name(&info.name);
                server_builder.set_protocol_version(&info.protocol_version);
                server_builder.set_server_name(&info.server_name);
                server_builder.set_server_version(&info.server_version);

                let mut tools_builder = server_builder.init_tools(info.tools.len() as u32);
                for (j, tool) in info.tools.iter().enumerate() {
                    let mut tool_builder = tools_builder.reborrow().get(j as u32);
                    tool_builder.set_name(&tool.name);
                    tool_builder.set_description(&tool.description.clone().unwrap_or_default());
                    tool_builder.set_input_schema(&tool.input_schema.to_string());
                }
            }

            Ok(())
        })
    }

    fn call_mcp_tool(
        self: Rc<Self>,
        params: kernel::CallMcpToolParams,
        mut results: kernel::CallMcpToolResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let trace_span = extract_rpc_trace(p.get_trace(), "call_mcp_tool");
        let call_reader = pry!(p.get_call());
        let server = pry!(pry!(call_reader.get_server()).to_str()).to_owned();
        let tool = pry!(pry!(call_reader.get_tool()).to_str()).to_owned();
        let arguments_str = pry!(pry!(call_reader.get_arguments()).to_str()).to_owned();

        // Parse JSON arguments
        let arguments: serde_json::Value = if arguments_str.is_empty() {
            serde_json::json!({})
        } else {
            match serde_json::from_str(&arguments_str) {
                Ok(v) => v,
                Err(e) => {
                    let mut result_builder = results.get().init_result();
                    result_builder.set_success(false);
                    result_builder.set_is_error(true);
                    result_builder.set_content(&format!("Invalid JSON arguments: {}", e));
                    return Promise::ok(());
                }
            }
        };

        let mcp_pool = self.state.borrow().mcp_pool.clone();


        Promise::from_future(async move {
            let mcp_result = mcp_pool.call_tool(&server, &tool, arguments).await;

            let mut result_builder = results.get().init_result();
            match mcp_result {
                Ok(r) => {
                    let is_error = r.is_error.unwrap_or(false);
                    result_builder.set_success(!is_error);
                    result_builder.set_is_error(is_error);

                    let content = extract_tool_result_text(&r);
                    result_builder.set_content(&content);
                }
                Err(e) => {
                    result_builder.set_success(false);
                    result_builder.set_is_error(true);
                    result_builder.set_content(&e.to_string());
                }
            }

            Ok(())
        }.instrument(trace_span))
    }

    // =========================================================================
    // Shell execution (kaish REPL)
    // =========================================================================

    fn shell_execute(
        self: Rc<Self>,
        params: kernel::ShellExecuteParams,
        mut results: kernel::ShellExecuteResults,
    ) -> Promise<(), capnp::Error> {
        log::debug!("shell_execute() called for kernel {}", self.kernel_id);
        let params = pry!(params.get());
        let trace_span = extract_rpc_trace(params.get_trace(), "shell_execute");
        let code = pry!(pry!(params.get_code()).to_str()).to_owned();
        let cell_id = pry!(pry!(params.get_document_id()).to_str()).to_owned();
        log::info!("Shell execute: cell_id={}, code={}", cell_id, code);

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();


        Promise::from_future(async move {
            // Get or create embedded kaish executor (same pattern as execute RPC)
            let (documents, kaish) = {
                let mut state_ref = state.borrow_mut();
                let kernel = state_ref.kernels.get_mut(&kernel_id)
                    .ok_or_else(|| {
                        log::error!("kernel {} not found", kernel_id);
                        capnp::Error::failed("kernel not found".into())
                    })?;

                if kernel.kaish.is_none() {
                    log::info!("Creating embedded kaish for kernel {}", kernel_id);
                    match EmbeddedKaish::new(
                        &kernel_id,
                        kernel.documents.clone(),
                        kernel.kernel.clone(),
                        None,
                    ) {
                        Ok(kaish) => {
                            kernel.kaish = Some(Rc::new(kaish));
                        }
                        Err(e) => {
                            log::error!("Failed to create embedded kaish: {}", e);
                            return Err(capnp::Error::failed(format!("kaish creation failed: {}", e)));
                        }
                    }
                }

                (kernel.documents.clone(), kernel.kaish.as_ref().unwrap().clone())
            };

            // Document must exist — join_context is the sole creator
            if documents.get(&cell_id).is_none() {
                return Err(capnp::Error::failed(
                    format!("document {} not found — call join_context first", cell_id)
                ));
            }

            // Create ShellCommand block at the end of the document
            let last_block = documents.last_block_id(&cell_id);
            log::info!("Inserting shell command into document {}, after={:?}", cell_id, last_block);
            let command_block_id = documents.insert_block(
                &cell_id, None, last_block.as_ref(),
                Role::User, BlockKind::ShellCommand,
                &code
            ).map_err(|e| capnp::Error::failed(format!("failed to insert shell command: {}", e)))?;
            log::info!("Created shell command block: {:?}", command_block_id);

            // Create ShellOutput block (empty, will be filled by execution)
            // Note: after = command_block_id ensures output appears after command in order
            let output_block_id = documents.insert_block(
                &cell_id, Some(&command_block_id), Some(&command_block_id),
                Role::System, BlockKind::ShellOutput,
                ""
            ).map_err(|e| capnp::Error::failed(format!("failed to insert shell output: {}", e)))?;
            log::debug!("Created shell output block: {:?}", output_block_id);

            // Mark output block as Running — clients poll this to detect completion
            if let Err(e) = documents.set_status(&cell_id, &output_block_id, Status::Running) {
                log::warn!("Failed to set output block to Running: {}", e);
            }

            // Spawn execution in background
            let cell_id_clone = cell_id.clone();
            let output_block_id_clone = output_block_id.clone();
            let documents_clone = documents.clone();

            tokio::task::spawn_local(async move {
                // Yield to let the event loop flush BlockInserted events to clients
                // before we start producing text ops. Without this, fast commands
                // (like `ls`) can emit edit_text before the client has processed the
                // BlockInserted, causing DataMissing errors on the client side.
                tokio::task::yield_now().await;

                // Execute via embedded kaish (routes through CRDT backend)
                log::info!("shell_execute: executing code via EmbeddedKaish: {:?}", code);
                match kaish.execute(&code).await {
                    Ok(result) => {
                        log::info!("shell_execute: kaish returned code={} out_len={} err_len={}",
                            result.code, result.out.len(), result.err.len());
                        log::debug!("shell_execute: out={:?} err={:?}", result.out, result.err);

                        // Convert kaish OutputData to kaijutsu DisplayHint and serialize
                        let display_hint = crate::embedded_kaish::convert_output_data(result.output.as_ref());
                        let hint_json = crate::embedded_kaish::serialize_display_hint(&display_hint);

                        // Combine out and err (kaish uses out/err/code fields)
                        let output = if result.err.is_empty() {
                            result.out
                        } else if result.out.is_empty() {
                            result.err
                        } else {
                            format!("{}\n{}", result.out, result.err)
                        };
                        log::debug!("Shell execution completed: {} bytes, exit_code={}, has_hint={}",
                            output.len(), result.code, hint_json.is_some());

                        // Update output block with result text
                        if let Err(e) = documents_clone.edit_text(&cell_id_clone, &output_block_id_clone, 0, &output, 0) {
                            log::error!("Failed to update shell output: {}", e);
                        }

                        // Store display hint if present
                        if let Some(hint) = hint_json.as_deref() {
                            if let Err(e) = documents_clone.set_display_hint(&cell_id_clone, &output_block_id_clone, Some(hint)) {
                                log::error!("Failed to set display hint: {}", e);
                            }
                        }

                        // Mark complete — status reflects exit code
                        let final_status = if result.code == 0 { Status::Done } else { Status::Error };
                        if let Err(e) = documents_clone.set_status(&cell_id_clone, &output_block_id_clone, final_status) {
                            log::error!("Failed to set output block status: {}", e);
                        }
                    }
                    Err(e) => {
                        let error_msg = format!("Error: {}", e);
                        log::error!("Shell execution failed: {}", e);
                        // Write error to output block
                        if let Err(e) = documents_clone.edit_text(&cell_id_clone, &output_block_id_clone, 0, &error_msg, 0) {
                            log::error!("Failed to update shell output with error: {}", e);
                        }
                        // Mark as error
                        if let Err(e) = documents_clone.set_status(&cell_id_clone, &output_block_id_clone, Status::Error) {
                            log::error!("Failed to set output block error status: {}", e);
                        }
                    }
                }
            });

            // Return command block ID
            let mut block_id_builder = results.get().init_command_block_id();
            block_id_builder.set_document_id(&command_block_id.document_id);
            block_id_builder.set_agent_id(&command_block_id.agent_id);
            block_id_builder.set_seq(command_block_id.seq);

            log::debug!("shell_execute() returning command_block_id={:?}", command_block_id);
            Ok(())
        }.instrument(trace_span))
    }

    // =========================================================================
    // Shell state (cwd, last result)
    // =========================================================================

    fn get_cwd(
        self: Rc<Self>,
        _params: kernel::GetCwdParams,
        mut results: kernel::GetCwdResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kaish = {
                let state_ref = state.borrow();
                let kernel = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                kernel.kaish.clone()
            };

            let cwd = if let Some(kaish) = kaish {
                kaish.cwd().await
            } else {
                std::path::PathBuf::from("/docs")
            };

            results.get().set_path(&cwd.to_string_lossy());
            Ok(())
        })
    }

    fn set_cwd(
        self: Rc<Self>,
        params: kernel::SetCwdParams,
        mut results: kernel::SetCwdResults,
    ) -> Promise<(), capnp::Error> {
        let path = match params.get().and_then(|p| p.get_path()) {
            Ok(p) => match p.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(format!("invalid path: {}", e))),
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("missing path: {}", e))),
        };

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kaish = {
                let state_ref = state.borrow();
                let kernel = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;

                kernel.kaish.clone()
                    .ok_or_else(|| capnp::Error::failed("kaish not initialized".into()))?
            };

            // set_cwd doesn't return a Result in kaish
            kaish.set_cwd(std::path::PathBuf::from(&path)).await;
            results.get().set_success(true);
            results.get().set_error("");
            Ok(())
        })
    }

    fn get_last_result(
        self: Rc<Self>,
        _params: kernel::GetLastResultParams,
        mut results: kernel::GetLastResultResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kaish = {
                let state_ref = state.borrow();
                let kernel = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                kernel.kaish.clone()
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
                result_builder.set_stdout(exec_result.out.as_bytes());
                result_builder.set_stderr(&exec_result.err);

                // Serialize data if present (convert kaish Value to JSON)
                if let Some(ref data) = exec_result.data {
                    let json_value = crate::kaish_backend::kaish_value_to_json(data);
                    result_builder.set_data(&serde_json::to_string(&json_value).unwrap_or_default());
                }

                // Serialize display hint
                let hint = crate::embedded_kaish::convert_output_data(exec_result.output.as_ref());
                if let Some(hint_json) = crate::embedded_kaish::serialize_display_hint(&hint) {
                    result_builder.set_hint(&hint_json);
                }
            } else {
                // No last result - return empty/zero values
                result_builder.set_code(0);
                result_builder.set_ok(true);
                result_builder.set_stdout(&[]);
                result_builder.set_stderr("");
                result_builder.set_data("");
                result_builder.set_hint("");
            }

            Ok(())
        })
    }

    // =========================================================================
    // Blob Storage
    // =========================================================================

    fn write_blob(
        self: Rc<Self>,
        params: kernel::WriteBlobParams,
        mut results: kernel::WriteBlobResults,
    ) -> Promise<(), capnp::Error> {
        // Extract and copy data before async block
        let params_reader = pry!(params.get());
        let data = pry!(params_reader.get_data()).to_vec();
        let content_type = pry!(pry!(params_reader.get_content_type()).to_str()).to_owned();

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kaish = {
                let state_ref = state.borrow();
                let kernel = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                kernel.kaish.clone()
                    .ok_or_else(|| capnp::Error::failed("kaish not initialized".into()))?
            };

            let blob_info = kaish.write_blob(&data, &content_type).await
                .map_err(|e| capnp::Error::failed(format!("write_blob failed: {}", e)))?;

            let mut ref_builder = results.get().init_ref();
            ref_builder.set_id(&blob_info.id);
            ref_builder.set_size(blob_info.size);
            ref_builder.set_content_type(&blob_info.content_type);

            Ok(())
        })
    }

    fn read_blob(
        self: Rc<Self>,
        params: kernel::ReadBlobParams,
        mut results: kernel::ReadBlobResults,
    ) -> Promise<(), capnp::Error> {
        let id = match params.get().and_then(|p| p.get_id()) {
            Ok(id) => match id.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(format!("invalid id: {}", e))),
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("missing id: {}", e))),
        };

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kaish = {
                let state_ref = state.borrow();
                let kernel = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                kernel.kaish.clone()
                    .ok_or_else(|| capnp::Error::failed("kaish not initialized".into()))?
            };

            let data = kaish.read_blob(&id).await
                .map_err(|e| capnp::Error::failed(format!("read_blob failed: {}", e)))?;

            results.get().set_data(&data);
            Ok(())
        })
    }

    fn delete_blob(
        self: Rc<Self>,
        params: kernel::DeleteBlobParams,
        mut results: kernel::DeleteBlobResults,
    ) -> Promise<(), capnp::Error> {
        let id = match params.get().and_then(|p| p.get_id()) {
            Ok(id) => match id.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(format!("invalid id: {}", e))),
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("missing id: {}", e))),
        };

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kaish = {
                let state_ref = state.borrow();
                let kernel = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                kernel.kaish.clone()
                    .ok_or_else(|| capnp::Error::failed("kaish not initialized".into()))?
            };

            let success = kaish.delete_blob(&id).await
                .map_err(|e| capnp::Error::failed(format!("delete_blob failed: {}", e)))?;

            results.get().set_success(success);
            Ok(())
        })
    }

    fn list_blobs(
        self: Rc<Self>,
        _params: kernel::ListBlobsParams,
        mut results: kernel::ListBlobsResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kaish = {
                let state_ref = state.borrow();
                let kernel = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                kernel.kaish.clone()
                    .ok_or_else(|| capnp::Error::failed("kaish not initialized".into()))?
            };

            let blobs = kaish.list_blobs().await
                .map_err(|e| capnp::Error::failed(format!("list_blobs failed: {}", e)))?;

            let mut refs_builder = results.get().init_refs(blobs.len() as u32);
            for (i, blob) in blobs.iter().enumerate() {
                let mut ref_builder = refs_builder.reborrow().get(i as u32);
                ref_builder.set_id(&blob.id);
                ref_builder.set_size(blob.size);
                ref_builder.set_content_type(&blob.content_type);
            }

            Ok(())
        })
    }

    // =========================================================================
    // Git Repository Management
    // =========================================================================

    fn register_repo(
        self: Rc<Self>,
        params: kernel::RegisterRepoParams,
        mut results: kernel::RegisterRepoResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let name = pry!(pry!(params_reader.get_name()).to_str()).to_owned();
        let path = pry!(pry!(params_reader.get_path()).to_str()).to_owned();

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kaish = {
                let state_ref = state.borrow();
                let kernel = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                kernel.kaish.clone()
                    .ok_or_else(|| capnp::Error::failed("kaish not initialized".into()))?
            };

            match kaish.register_repo(&name, &path) {
                Ok(()) => {
                    results.get().set_success(true);
                    results.get().set_error("");
                }
                Err(e) => {
                    results.get().set_success(false);
                    results.get().set_error(&e.to_string());
                }
            }
            Ok(())
        })
    }

    fn unregister_repo(
        self: Rc<Self>,
        params: kernel::UnregisterRepoParams,
        mut results: kernel::UnregisterRepoResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let name = pry!(pry!(params_reader.get_name()).to_str()).to_owned();

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kaish = {
                let state_ref = state.borrow();
                let kernel = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                kernel.kaish.clone()
                    .ok_or_else(|| capnp::Error::failed("kaish not initialized".into()))?
            };

            match kaish.unregister_repo(&name) {
                Ok(()) => {
                    results.get().set_success(true);
                    results.get().set_error("");
                }
                Err(e) => {
                    results.get().set_success(false);
                    results.get().set_error(&e.to_string());
                }
            }
            Ok(())
        })
    }

    fn list_repos(
        self: Rc<Self>,
        _params: kernel::ListReposParams,
        mut results: kernel::ListReposResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kaish = {
                let state_ref = state.borrow();
                let kernel = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                kernel.kaish.clone()
                    .ok_or_else(|| capnp::Error::failed("kaish not initialized".into()))?
            };

            let repos = kaish.list_repos();
            let mut repos_builder = results.get().init_repos(repos.len() as u32);
            for (i, repo) in repos.iter().enumerate() {
                repos_builder.set(i as u32, repo);
            }

            Ok(())
        })
    }

    fn switch_branch(
        self: Rc<Self>,
        params: kernel::SwitchBranchParams,
        mut results: kernel::SwitchBranchResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let repo = pry!(pry!(params_reader.get_repo()).to_str()).to_owned();
        let branch = pry!(pry!(params_reader.get_branch()).to_str()).to_owned();

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kaish = {
                let state_ref = state.borrow();
                let kernel = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                kernel.kaish.clone()
                    .ok_or_else(|| capnp::Error::failed("kaish not initialized".into()))?
            };

            match kaish.switch_branch(&repo, &branch).await {
                Ok(()) => {
                    results.get().set_success(true);
                    results.get().set_error("");
                }
                Err(e) => {
                    results.get().set_success(false);
                    results.get().set_error(&e.to_string());
                }
            }
            Ok(())
        })
    }

    fn list_branches(
        self: Rc<Self>,
        params: kernel::ListBranchesParams,
        mut results: kernel::ListBranchesResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let repo = pry!(pry!(params_reader.get_repo()).to_str()).to_owned();

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kaish = {
                let state_ref = state.borrow();
                let kernel = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                kernel.kaish.clone()
                    .ok_or_else(|| capnp::Error::failed("kaish not initialized".into()))?
            };

            match kaish.list_branches(&repo) {
                Ok(branches) => {
                    let mut branches_builder = results.get().init_branches(branches.len() as u32);
                    for (i, branch) in branches.iter().enumerate() {
                        branches_builder.set(i as u32, branch);
                    }
                    results.get().set_error("");
                }
                Err(e) => {
                    results.get().init_branches(0);
                    results.get().set_error(&e.to_string());
                }
            }
            Ok(())
        })
    }

    fn get_current_branch(
        self: Rc<Self>,
        params: kernel::GetCurrentBranchParams,
        mut results: kernel::GetCurrentBranchResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let repo = pry!(pry!(params_reader.get_repo()).to_str()).to_owned();

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kaish = {
                let state_ref = state.borrow();
                let kernel = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                kernel.kaish.clone()
                    .ok_or_else(|| capnp::Error::failed("kaish not initialized".into()))?
            };

            let branch = kaish.get_current_branch(&repo).unwrap_or_default();
            results.get().set_branch(&branch);
            Ok(())
        })
    }

    fn flush_git(
        self: Rc<Self>,
        _params: kernel::FlushGitParams,
        mut results: kernel::FlushGitResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kaish = {
                let state_ref = state.borrow();
                let kernel = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                kernel.kaish.clone()
                    .ok_or_else(|| capnp::Error::failed("kaish not initialized".into()))?
            };

            match kaish.flush_git().await {
                Ok(()) => {
                    results.get().set_success(true);
                    results.get().set_error("");
                }
                Err(e) => {
                    results.get().set_success(false);
                    results.get().set_error(&e.to_string());
                }
            }
            Ok(())
        })
    }

    fn set_attribution(
        self: Rc<Self>,
        params: kernel::SetAttributionParams,
        _results: kernel::SetAttributionResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let source = pry!(pry!(params_reader.get_source()).to_str()).to_owned();
        let command = pry!(pry!(params_reader.get_command()).to_str()).to_owned();

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kaish = {
                let state_ref = state.borrow();
                let kernel = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                kernel.kaish.clone()
            };

            if let Some(kaish) = kaish {
                let cmd = if command.is_empty() { None } else { Some(command.as_str()) };
                kaish.set_pending_attribution(&source, cmd);
            }
            Ok(())
        })
    }

    fn push_ops(
        self: Rc<Self>,
        params: kernel::PushOpsParams,
        mut results: kernel::PushOpsResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let _trace_span = extract_rpc_trace(params_reader.get_trace(), "push_ops");
        let document_id = pry!(pry!(params_reader.get_document_id()).to_str()).to_owned();
        let ops_data = pry!(params_reader.get_ops()).to_vec();

        log::debug!("push_ops called for document {} with {} bytes", document_id, ops_data.len());

        let state = self.state.borrow();
        let kernel = match state.kernels.get(&self.kernel_id) {
            Some(k) => k,
            None => {
                return Promise::err(capnp::Error::failed(format!(
                    "kernel '{}' not found",
                    self.kernel_id
                )));
            }
        };

        // Deserialize the CRDT ops
        let serialized_ops: kaijutsu_crdt::SerializedOpsOwned = match postcard::from_bytes(&ops_data) {
            Ok(ops) => ops,
            Err(e) => {
                return Promise::err(capnp::Error::failed(format!(
                    "failed to deserialize ops: {}",
                    e
                )));
            }
        };

        // Merge the ops into the document
        let ack_version = match kernel.documents.merge_ops_owned(&document_id, serialized_ops) {
            Ok(version) => version,
            Err(e) => {
                return Promise::err(capnp::Error::failed(format!(
                    "failed to merge ops: {}",
                    e
                )));
            }
        };

        log::debug!("push_ops merged successfully, new version: {}", ack_version);
        results.get().set_ack_version(ack_version);
        Promise::ok(())
    }

    // =========================================================================
    // MCP Resource Operations
    // =========================================================================

    fn list_mcp_resources(
        self: Rc<Self>,
        params: kernel::ListMcpResourcesParams,
        mut results: kernel::ListMcpResourcesResults,
    ) -> Promise<(), capnp::Error> {
        let server = pry!(pry!(pry!(params.get()).get_server()).to_str()).to_owned();
        log::debug!("list_mcp_resources: server={}", server);

        let mcp_pool = self.state.borrow().mcp_pool.clone();

        Promise::from_future(async move {
            match mcp_pool.list_resources(&server).await {
                Ok(resources) => {
                    let mut list = results.get().init_resources(resources.len() as u32);
                    for (i, res) in resources.iter().enumerate() {
                        let mut builder = list.reborrow().get(i as u32);
                        builder.set_uri(&res.uri);
                        builder.set_name(res.name.as_deref().unwrap_or(""));
                        builder.set_has_description(res.description.is_some());
                        if let Some(desc) = &res.description {
                            builder.set_description(desc);
                        }
                        builder.set_has_mime_type(res.mime_type.is_some());
                        if let Some(mime) = &res.mime_type {
                            builder.set_mime_type(mime);
                        }
                    }
                    Ok(())
                }
                Err(e) => Err(capnp::Error::failed(format!(
                    "failed to list resources: {}",
                    e
                ))),
            }
        })
    }

    fn read_mcp_resource(
        self: Rc<Self>,
        params: kernel::ReadMcpResourceParams,
        mut results: kernel::ReadMcpResourceResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let server = pry!(pry!(params_reader.get_server()).to_str()).to_owned();
        let uri = pry!(pry!(params_reader.get_uri()).to_str()).to_owned();
        log::debug!("read_mcp_resource: server={}, uri={}", server, uri);

        let mcp_pool = self.state.borrow().mcp_pool.clone();

        Promise::from_future(async move {
            match mcp_pool.read_resource(&server, &uri).await {
                Ok(contents) => {
                    results.get().set_has_contents(true);
                    let mut contents_builder = results.get().init_contents();
                    contents_builder.set_uri(&uri);

                    match contents {
                        kaijutsu_kernel::McpResourceContents::TextResourceContents {
                            mime_type, text, ..
                        } => {
                            contents_builder.set_has_mime_type(mime_type.is_some());
                            if let Some(mime) = &mime_type {
                                contents_builder.set_mime_type(mime);
                            }
                            contents_builder.set_text(&text);
                        }
                        kaijutsu_kernel::McpResourceContents::BlobResourceContents {
                            mime_type, blob, ..
                        } => {
                            contents_builder.set_has_mime_type(mime_type.is_some());
                            if let Some(mime) = &mime_type {
                                contents_builder.set_mime_type(mime);
                            }
                            // blob is base64-encoded string in rmcp, store as-is
                            // (clients will need to decode)
                            contents_builder.set_blob(blob.as_bytes());
                        }
                    }
                    Ok(())
                }
                Err(e) => {
                    results.get().set_has_contents(false);
                    Err(capnp::Error::failed(format!(
                        "failed to read resource: {}",
                        e
                    )))
                }
            }
        })
    }

    fn subscribe_mcp_resources(
        self: Rc<Self>,
        params: kernel::SubscribeMcpResourcesParams,
        _results: kernel::SubscribeMcpResourcesResults,
    ) -> Promise<(), capnp::Error> {
        let callback = pry!(pry!(params.get()).get_callback());

        let state = self.state.borrow();
        let mcp_pool = state.mcp_pool.clone();
        let kernel_id = self.kernel_id.clone();
        drop(state);

        // Get the resource flow bus from the MCP pool
        let resource_flows = mcp_pool.resource_flows().clone();

        // Spawn a bridge task that forwards ResourceFlow events to the callback
        // Use spawn_local because Cap'n Proto callbacks are not Send
        tokio::task::spawn_local(async move {
            let mut sub = resource_flows.subscribe("resource.*");
            log::debug!("Started ResourceFlow subscription for kernel {}", kernel_id);

            while let Some(msg) = sub.recv().await {
                let success = match msg.payload {
                    kaijutsu_kernel::ResourceFlow::Updated {
                        ref server,
                        ref uri,
                        ..
                    } => {
                        // Notify-then-fetch pattern: we only send the notification.
                        // Content is not included; clients should call readMcpResource().
                        let mut req = callback.on_resource_updated_request();
                        {
                            let mut params = req.get();
                            params.set_server(server);
                            params.set_uri(uri);
                            params.set_has_contents(false);
                        }
                        req.send().promise.await.is_ok()
                    }
                    kaijutsu_kernel::ResourceFlow::ListChanged {
                        ref server,
                        ..
                    } => {
                        // Notify-then-fetch pattern: we only send the notification.
                        // Resource list is not included; clients should call listMcpResources().
                        let mut req = callback.on_resource_list_changed_request();
                        {
                            let mut params = req.get();
                            params.set_server(server);
                            params.set_has_resources(false);
                        }
                        req.send().promise.await.is_ok()
                    }
                    // Subscribed/Unsubscribed are internal events, no need to forward
                    kaijutsu_kernel::ResourceFlow::Subscribed { .. }
                    | kaijutsu_kernel::ResourceFlow::Unsubscribed { .. } => true,
                };

                // If callback fails (client disconnected), stop the bridge task
                if !success {
                    log::debug!(
                        "ResourceFlow bridge task for kernel {} stopping: callback failed",
                        kernel_id
                    );
                    break;
                }
            }

            log::debug!("ResourceFlow bridge task for kernel {} ended", kernel_id);
        });

        Promise::ok(())
    }

    fn subscribe_mcp_elicitations(
        self: Rc<Self>,
        params: kernel::SubscribeMcpElicitationsParams,
        _results: kernel::SubscribeMcpElicitationsResults,
    ) -> Promise<(), capnp::Error> {
        let callback = pry!(pry!(params.get()).get_callback());

        let state = self.state.borrow();
        let mcp_pool = state.mcp_pool.clone();
        let kernel_id = self.kernel_id.clone();
        drop(state);

        // Get the elicitation flow bus from the MCP pool
        let elicitation_flows = mcp_pool.elicitation_flows().clone();

        // Spawn a bridge task that handles elicitation request/response cycle
        // Use spawn_local because Cap'n Proto callbacks are not Send
        tokio::task::spawn_local(async move {
            let mut sub = elicitation_flows.subscribe("elicitation.*");
            log::debug!("Started ElicitationFlow subscription for kernel {}", kernel_id);

            while let Some(msg) = sub.recv().await {
                match msg.payload {
                    kaijutsu_kernel::ElicitationFlow::Request {
                        ref request_id,
                        ref server,
                        ref message,
                        ref schema,
                    } => {
                        // Forward the request to the client callback and await response
                        let mut req = callback.on_request_request();
                        {
                            let mut request_builder = req.get().init_request();
                            request_builder.set_request_id(request_id);
                            request_builder.set_server(server);
                            request_builder.set_message(message);
                            if let Some(s) = schema {
                                request_builder.set_schema(&s.to_string());
                                request_builder.set_has_schema(true);
                            } else {
                                request_builder.set_has_schema(false);
                            }
                        }

                        // Await the client's response
                        let response = match req.send().promise.await {
                            Ok(result) => {
                                match result.get() {
                                    Ok(reader) => {
                                        // Parse response, defaulting to decline on any error
                                        if let Ok(response_reader) = reader.get_response() {
                                            // Parse the action string
                                            let action_str = response_reader.get_action()
                                                .ok()
                                                .and_then(|r| r.to_str().ok())
                                                .unwrap_or("decline");
                                            let action = action_str.parse()
                                                .unwrap_or(kaijutsu_kernel::ElicitationAction::Decline);

                                            // Parse the content if present
                                            let content = if response_reader.get_has_content() {
                                                response_reader.get_content()
                                                    .ok()
                                                    .and_then(|s| s.to_str().ok())
                                                    .and_then(|s| serde_json::from_str(s).ok())
                                            } else {
                                                None
                                            };

                                            kaijutsu_kernel::ElicitationResponse { action, content }
                                        } else {
                                            // Parse error, decline
                                            kaijutsu_kernel::ElicitationResponse {
                                                action: kaijutsu_kernel::ElicitationAction::Decline,
                                                content: None,
                                            }
                                        }
                                    }
                                    Err(_) => {
                                        // Capnp error, decline
                                        kaijutsu_kernel::ElicitationResponse {
                                            action: kaijutsu_kernel::ElicitationAction::Decline,
                                            content: None,
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                // Network/callback error, decline
                                log::warn!(
                                    "Elicitation callback failed for kernel {}: {}",
                                    kernel_id, e
                                );
                                kaijutsu_kernel::ElicitationResponse {
                                    action: kaijutsu_kernel::ElicitationAction::Decline,
                                    content: None,
                                }
                            }
                        };

                        // Route the response back to the MCP server
                        mcp_pool.respond_to_elicitation(request_id, response);
                    }
                }
            }

            log::debug!("ElicitationFlow bridge task for kernel {} ended", kernel_id);
        });

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
        let config_reader = pry!(pry!(params.get()).get_config());

        // Extract config fields
        let nick = pry!(config_reader.get_nick()).to_str().unwrap_or("unknown").to_owned();
        let instance = pry!(config_reader.get_instance()).to_str().unwrap_or("default").to_owned();
        let provider = pry!(config_reader.get_provider()).to_str().unwrap_or("unknown").to_owned();
        let model_id = pry!(config_reader.get_model_id()).to_str().unwrap_or("unknown").to_owned();

        // Extract capabilities
        let caps_reader = pry!(config_reader.get_capabilities());
        let capabilities: Vec<AgentCapability> = (0..caps_reader.len())
            .filter_map(|i| {
                caps_reader.get(i).map(|c| capnp_to_agent_capability(c)).ok().flatten()
            })
            .collect();

        let config = AgentConfig {
            nick,
            instance,
            provider,
            model_id,
            capabilities,
        };

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kernel = {
                let state_ref = state.borrow();
                state_ref
                    .kernels
                    .get(&kernel_id)
                    .map(|k| k.kernel.clone())
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?
            };

            let agent_info = kernel
                .attach_agent(config)
                .await
                .map_err(|e| capnp::Error::failed(format!("failed to attach agent: {}", e)))?;

            // Build response
            let mut info = results.get().init_info();
            set_agent_info(&mut info, &agent_info);

            Ok(())
        })
    }

    fn list_agents(
        self: Rc<Self>,
        _params: kernel::ListAgentsParams,
        mut results: kernel::ListAgentsResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let agents = {
                let state_ref = state.borrow();
                let kernel = state_ref
                    .kernels
                    .get(&kernel_id)
                    .map(|k| k.kernel.clone())
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                kernel.list_agents().await
            };

            let mut list = results.get().init_agents(agents.len() as u32);
            for (i, agent) in agents.iter().enumerate() {
                let mut a = list.reborrow().get(i as u32);
                set_agent_info(&mut a, agent);
            }

            Ok(())
        })
    }

    fn detach_agent(
        self: Rc<Self>,
        params: kernel::DetachAgentParams,
        _results: kernel::DetachAgentResults,
    ) -> Promise<(), capnp::Error> {
        let nick = pry!(pry!(pry!(params.get()).get_nick()).to_str()).to_owned();

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kernel = {
                let state_ref = state.borrow();
                state_ref
                    .kernels
                    .get(&kernel_id)
                    .map(|k| k.kernel.clone())
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?
            };

            kernel.detach_agent(&nick).await;
            Ok(())
        })
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
                caps_reader.get(i).map(|c| capnp_to_agent_capability(c)).ok().flatten()
            })
            .collect();

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kernel = {
                let state_ref = state.borrow();
                state_ref
                    .kernels
                    .get(&kernel_id)
                    .map(|k| k.kernel.clone())
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?
            };

            kernel
                .set_agent_capabilities(&nick, capabilities)
                .await
                .map_err(|e| capnp::Error::failed(format!("failed to set capabilities: {}", e)))?;

            Ok(())
        })
    }

    fn invoke_agent(
        self: Rc<Self>,
        params: kernel::InvokeAgentParams,
        mut results: kernel::InvokeAgentResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let nick = pry!(pry!(params_reader.get_nick()).to_str()).to_owned();
        let block_id_reader = pry!(params_reader.get_block_id());
        let action = pry!(pry!(params_reader.get_action()).to_str()).to_owned();

        // Parse block ID
        let block_id = kaijutsu_crdt::BlockId {
            document_id: pry!(block_id_reader.get_document_id()).to_str().unwrap_or("").to_owned(),
            agent_id: pry!(block_id_reader.get_agent_id()).to_str().unwrap_or("").to_owned(),
            seq: block_id_reader.get_seq(),
        };

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kernel = {
                let state_ref = state.borrow();
                state_ref
                    .kernels
                    .get(&kernel_id)
                    .map(|k| k.kernel.clone())
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?
            };

            // Generate a request ID for tracking
            let request_id = uuid::Uuid::new_v4().to_string();

            // Emit a started event
            kernel
                .emit_agent_event(AgentActivityEvent::Started {
                    agent: nick.clone(),
                    block_id: block_id.to_string(),
                    action: action.clone(),
                })
                .await;

            // TODO: Actually invoke the agent's capability here
            // For now, just emit a completed event
            kernel
                .emit_agent_event(AgentActivityEvent::Completed {
                    agent: nick.clone(),
                    block_id: block_id.to_string(),
                    success: true,
                })
                .await;

            results.get().set_request_id(&request_id);
            Ok(())
        })
    }

    fn subscribe_agent_events(
        self: Rc<Self>,
        params: kernel::SubscribeAgentEventsParams,
        _results: kernel::SubscribeAgentEventsResults,
    ) -> Promise<(), capnp::Error> {
        let callback = pry!(pry!(params.get()).get_callback());

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        // Spawn a bridge task that forwards AgentActivityEvent to the callback
        tokio::task::spawn_local(async move {
            let mut receiver = {
                let state_ref = state.borrow();
                if let Some(kernel_state) = state_ref.kernels.get(&kernel_id) {
                    kernel_state.kernel.subscribe_agent_events().await
                } else {
                    log::warn!("Kernel {} not found for agent event subscription", kernel_id);
                    return;
                }
            };

            log::debug!("Started agent event subscription for kernel {}", kernel_id);

            while let Ok(event) = receiver.recv().await {
                let success = match &event {
                    AgentActivityEvent::Started { agent, block_id: _, action } => {
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
                    AgentActivityEvent::Progress { agent, block_id: _, message, percent } => {
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
                    AgentActivityEvent::Completed { agent, block_id: _, success: ok } => {
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
                    AgentActivityEvent::CursorMoved { agent, block_id: _, offset } => {
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
    // Timeline Navigation (Phase 3: Fork-First Temporal Model)
    // ========================================================================

    fn fork_from_version(
        self: Rc<Self>,
        params: kernel::ForkFromVersionParams,
        mut results: kernel::ForkFromVersionResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let document_id = pry!(pry!(params_reader.get_document_id()).to_str()).to_owned();
        let version = params_reader.get_version();
        let context_name = pry!(pry!(params_reader.get_context_name()).to_str()).to_owned();

        let kernel_id = self.kernel_id.clone();

        log::info!(
            "Fork request: document={}, version={}, new_context={}",
            document_id, version, context_name
        );

        // Get kernel state synchronously
        let state_ref = self.state.borrow();
        let kernel_state = match state_ref.kernels.get(&kernel_id) {
            Some(ks) => ks,
            None => return Promise::err(capnp::Error::failed("Kernel not found".into())),
        };

        // Get the document from the block store (DashMap access is sync)
        let doc_entry = kernel_state.documents.get(&document_id);
        let doc_entry = match doc_entry {
            Some(entry) => entry,
            None => return Promise::err(capnp::Error::failed("Document not found".into())),
        };

        // Check version is valid
        let current_version = doc_entry.version();
        if current_version < version {
            return Promise::err(capnp::Error::failed(
                format!("Requested version {} is in the future (current: {})", version, current_version)
            ));
        }

        // Release the doc_entry ref before forking
        drop(doc_entry);

        // Generate new document ID for the fork
        let new_doc_id = format!("{}@{}", kernel_id, context_name);

        // Fork the document at the specified version
        if let Err(e) = kernel_state.documents.fork_document_at_version(&document_id, new_doc_id.clone(), version) {
            return Promise::err(capnp::Error::failed(format!("Fork failed: {}", e)));
        }

        // Create a new context and attach the forked document
        let _seat_info = kernel_state.context_manager.join_context(&context_name);

        // Attach the forked document to the new context
        kernel_state.context_manager.attach_document(&context_name, &new_doc_id, "server");

        // Get the newly created context
        let context = kernel_state.context_manager.get_context(&context_name);
        let context = match context {
            Some(ctx) => ctx,
            None => return Promise::err(capnp::Error::failed("Failed to create context".into())),
        };

        // Build the result
        let mut ctx_builder = results.get().init_context();
        ctx_builder.set_name(&context.name);

        // Initialize and populate documents list
        let mut docs_builder = ctx_builder.reborrow().init_documents(context.documents.len() as u32);
        for (i, doc) in context.documents.iter().enumerate() {
            let mut doc_builder = docs_builder.reborrow().get(i as u32);
            doc_builder.set_id(&doc.id);
            doc_builder.set_attached_by(&doc.attached_by);
            doc_builder.set_attached_at(doc.attached_at);
        }


        log::info!(
            "Fork created: {} from document {} at version {}, new document {}",
            context_name, document_id, version, new_doc_id
        );
        Promise::ok(())
    }

    fn cherry_pick_block(
        self: Rc<Self>,
        params: kernel::CherryPickBlockParams,
        mut results: kernel::CherryPickBlockResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let source_block_id = pry!(params_reader.get_source_block_id());
        let source_doc_id = pry!(pry!(source_block_id.get_document_id()).to_str()).to_owned();
        let source_agent_id = pry!(pry!(source_block_id.get_agent_id()).to_str()).to_owned();
        let source_seq = source_block_id.get_seq();
        let target_context = pry!(pry!(params_reader.get_target_context()).to_str()).to_owned();

        let kernel_id = self.kernel_id.clone();

        log::info!(
            "Cherry-pick request: block={}/{}/{} to context={}",
            source_doc_id, source_agent_id, source_seq, target_context
        );

        // Get kernel state synchronously
        let state_ref = self.state.borrow();
        let kernel_state = match state_ref.kernels.get(&kernel_id) {
            Some(ks) => ks,
            None => return Promise::err(capnp::Error::failed("Kernel not found".into())),
        };

        // Get the source document (DashMap access is sync)
        let doc_entry = kernel_state.documents.get(&source_doc_id);
        let doc_entry = match doc_entry {
            Some(entry) => entry,
            None => return Promise::err(capnp::Error::failed("Source document not found".into())),
        };

        // Find the block to cherry-pick
        let block_id = kaijutsu_crdt::BlockId::new(&source_doc_id, &source_agent_id, source_seq);
        let block_snapshot = match doc_entry.doc.get_block_snapshot(&block_id) {
            Some(snapshot) => snapshot,
            None => return Promise::err(capnp::Error::failed("Block not found".into())),
        };

        // Release source doc ref
        drop(doc_entry);

        // Generate target document ID from context
        let target_doc_id = format!("{}@{}", kernel_id, target_context);

        // Target document must exist — join target context first
        if !kernel_state.documents.contains(&target_doc_id) {
            return Promise::err(capnp::Error::failed(
                format!("target document {} not found — join target context first", target_doc_id)
            ));
        }

        // Get the last block ID in target document for ordering
        let after_id = kernel_state.documents.last_block_id(&target_doc_id);

        // Insert the block into target document
        // Note: We don't preserve parent_id as it references the source document
        let new_block_result = kernel_state.documents.insert_block(
            &target_doc_id,
            None, // No parent in target (cherry-picked blocks are roots)
            after_id.as_ref(),
            block_snapshot.role,
            block_snapshot.kind,
            block_snapshot.content,
        );

        let new_block_id = match new_block_result {
            Ok(id) => id,
            Err(e) => return Promise::err(capnp::Error::failed(format!("Failed to insert block: {}", e))),
        };

        // Build result
        let mut new_block_builder = results.get().init_new_block_id();
        new_block_builder.set_document_id(&new_block_id.document_id);
        new_block_builder.set_agent_id(&new_block_id.agent_id);
        new_block_builder.set_seq(new_block_id.seq);

        log::info!(
            "Cherry-pick complete: {} -> {}",
            block_id.to_key(),
            new_block_id.to_key()
        );
        Promise::ok(())
    }

    fn get_document_history(
        self: Rc<Self>,
        params: kernel::GetDocumentHistoryParams,
        mut results: kernel::GetDocumentHistoryResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let document_id = pry!(pry!(params_reader.get_document_id()).to_str()).to_owned();
        let limit = params_reader.get_limit() as usize;

        let kernel_id = self.kernel_id.clone();

        // Get kernel state synchronously
        let state_ref = self.state.borrow();
        let kernel_state = match state_ref.kernels.get(&kernel_id) {
            Some(ks) => ks,
            None => return Promise::err(capnp::Error::failed("Kernel not found".into())),
        };

        // Get the document (DashMap access is sync)
        let doc_entry = kernel_state.documents.get(&document_id);
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
            snapshot.set_change_kind("block_added");

            let mut block_id = snapshot.init_changed_block_id();
            block_id.set_document_id(&block.id.document_id);
            block_id.set_agent_id(&block.id.agent_id);
            block_id.set_seq(block.id.seq);
        }

        log::debug!(
            "Document history: {} snapshots (current version: {})",
            snapshot_count, current_version
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
        let state_ref = self.state.borrow();
        let kernel_state = match state_ref.kernels.get(&self.kernel_id) {
            Some(ks) => ks,
            None => return Promise::err(capnp::Error::failed("Kernel not found".into())),
        };

        let configs = kernel_state.config_backend.list_configs();
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
        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let config_backend = {
                let state_ref = state.borrow();
                let kernel_state = match state_ref.kernels.get(&kernel_id) {
                    Some(ks) => ks,
                    None => return Err(capnp::Error::failed("Kernel not found".into())),
                };
                kernel_state.config_backend.clone()
            };

            match config_backend.reload_from_disk(&path).await {
                Ok(()) => {
                    results.get().set_success(true);
                    results.get().set_error("");
                }
                Err(e) => {
                    results.get().set_success(false);
                    results.get().set_error(&format!("{}", e));
                }
            }

            Ok(())
        })
    }

    fn reset_config(
        self: Rc<Self>,
        params: kernel::ResetConfigParams,
        mut results: kernel::ResetConfigResults,
    ) -> Promise<(), capnp::Error> {
        let path = pry!(pry!(pry!(params.get()).get_path()).to_str()).to_owned();
        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let config_backend = {
                let state_ref = state.borrow();
                let kernel_state = match state_ref.kernels.get(&kernel_id) {
                    Some(ks) => ks,
                    None => return Err(capnp::Error::failed("Kernel not found".into())),
                };
                kernel_state.config_backend.clone()
            };

            match config_backend.reset_to_default(&path).await {
                Ok(()) => {
                    results.get().set_success(true);
                    results.get().set_error("");
                }
                Err(e) => {
                    results.get().set_success(false);
                    results.get().set_error(&format!("{}", e));
                }
            }

            Ok(())
        })
    }

    fn get_config(
        self: Rc<Self>,
        params: kernel::GetConfigParams,
        mut results: kernel::GetConfigResults,
    ) -> Promise<(), capnp::Error> {
        let path = pry!(pry!(pry!(params.get()).get_path()).to_str()).to_owned();

        let state_ref = self.state.borrow();
        let kernel_state = match state_ref.kernels.get(&self.kernel_id) {
            Some(ks) => ks,
            None => return Promise::err(capnp::Error::failed("Kernel not found".into())),
        };

        match kernel_state.config_backend.get_content(&path) {
            Ok(content) => {
                results.get().set_content(&content);
                results.get().set_error("");
            }
            Err(e) => {
                results.get().set_content("");
                results.get().set_error(&format!("{}", e));
            }
        }

        Promise::ok(())
    }


    // ========================================================================
    // Drift: Cross-Context Communication
    // ========================================================================

    fn get_context_id(
        self: Rc<Self>,
        _params: kernel::GetContextIdParams,
        mut results: kernel::GetContextIdResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.borrow();
        let kernel_id = &self.kernel_id;

        let short_id = state.drift_router.short_id_for_context(kernel_id)
            .unwrap_or("");
        let name = state.drift_router.short_id_for_context(kernel_id)
            .and_then(|sid| state.drift_router.get(sid))
            .map(|h| h.context_name.as_str())
            .unwrap_or("");

        results.get().set_short_id(short_id);
        results.get().set_name(name);
        Promise::ok(())
    }

    fn configure_llm(
        self: Rc<Self>,
        params: kernel::ConfigureLlmParams,
        mut results: kernel::ConfigureLlmResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let provider_name = pry!(pry!(params_reader.get_provider()).to_str()).to_owned();
        let model = pry!(pry!(params_reader.get_model()).to_str()).to_owned();
        let kernel_id = self.kernel_id.clone();
        let state = self.state.clone();

        Promise::from_future(async move {
            // Update drift router metadata
            {
                let mut state_ref = state.borrow_mut();
                let short_id = state_ref.drift_router.short_id_for_context(&kernel_id)
                    .map(|s| s.to_string());
                if let Some(ref sid) = short_id {
                    let _ = state_ref.drift_router.configure_llm(sid, &provider_name, &model);
                }
            }

            // Get kernel
            let kernel_arc = {
                let state_ref = state.borrow();
                state_ref.kernels.get(&kernel_id)
                    .map(|k| k.kernel.clone())
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?
            };

            // Create new provider from config and register with kernel
            let config = kaijutsu_kernel::llm::ProviderConfig::new(&provider_name)
                .with_default_model(&model);
            match kaijutsu_kernel::llm::RigProvider::from_config(&config) {
                Ok(new_provider) => {
                    let mut registry = kernel_arc.llm().write().await;
                    registry.register(&provider_name, Arc::new(new_provider));
                    registry.set_default(&provider_name);
                    registry.set_default_model(&model);
                    results.get().set_success(true);
                    results.get().set_error("");
                    log::info!("LLM configured: provider={}, model={}", provider_name, model);
                }
                Err(e) => {
                    results.get().set_success(false);
                    results.get().set_error(&format!("{}", e));
                    log::warn!("Failed to configure LLM: {}", e);
                }
            }

            Ok(())
        })
    }

    fn drift_push(
        self: Rc<Self>,
        params: kernel::DriftPushParams,
        mut results: kernel::DriftPushResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let trace_span = extract_rpc_trace(params_reader.get_trace(), "drift_push");
        let target_ctx = pry!(pry!(params_reader.get_target_ctx()).to_str()).to_owned();
        let content = pry!(pry!(params_reader.get_content()).to_str()).to_owned();
        let summarize = params_reader.get_summarize();

        let kernel_id = self.kernel_id.clone();
        let state = self.state.clone();

        // Extract what we need from state before going async
        let (source_ctx, source_model, kernel_arc, documents, main_doc_id) = {
            let state_ref = state.borrow();
            let source_ctx = match state_ref.drift_router.short_id_for_context(&kernel_id) {
                Some(sid) => sid.to_string(),
                None => {
                    return Promise::err(capnp::Error::failed(
                        "current kernel not registered in drift router".into(),
                    ));
                }
            };
            let source_model = state_ref.drift_router.get(&source_ctx)
                .and_then(|h| h.model.clone());
            let (kernel_arc, documents, main_doc_id) = match state_ref.kernels.get(&kernel_id) {
                Some(ks) => (ks.kernel.clone(), ks.documents.clone(), ks.main_document_id.clone()),
                None => {
                    return Promise::err(capnp::Error::failed("kernel not found".into()));
                }
            };
            (source_ctx, source_model, kernel_arc, documents, main_doc_id)
        };

        if !summarize {
            // Direct push — no LLM needed, synchronous path
            let mut state_ref = state.borrow_mut();
            match state_ref.drift_router.stage(
                &source_ctx,
                &target_ctx,
                content,
                source_model,
                kaijutsu_crdt::DriftKind::Push,
            ) {
                Ok(staged_id) => {
                    results.get().set_staged_id(staged_id);
                    log::info!("Drift staged: {} → {} (id={})", source_ctx, target_ctx, staged_id);
                }
                Err(e) => {
                    return Promise::err(capnp::Error::failed(format!("drift push failed: {}", e)));
                }
            }
            return Promise::ok(());
        }

        // Summarize path — async LLM call

        Promise::from_future(async move {
            // Get LLM provider from kernel's registry
            let provider = {
                let registry = kernel_arc.llm().read().await;
                registry.default_provider().ok_or_else(|| {
                    capnp::Error::failed("LLM provider not configured — cannot summarize (check llm.rhai)".into())
                })?
            };

            // Build distillation prompt from source context's blocks
            let blocks = documents.block_snapshots(&main_doc_id)
                .map_err(|e| capnp::Error::failed(format!("failed to read blocks: {}", e)))?;

            let user_prompt = kaijutsu_kernel::build_distillation_prompt(&blocks, None);

            // Determine model to use
            let model = source_model.as_deref().unwrap_or_else(|| {
                provider.available_models().first().copied().unwrap_or("claude-sonnet-4-5-20250929")
            });

            log::info!("Distilling {} blocks from {} for push to {} (model={})",
                blocks.len(), source_ctx, target_ctx, model);

            let summary = provider
                .prompt_with_system(
                    model,
                    Some(kaijutsu_kernel::DISTILLATION_SYSTEM_PROMPT),
                    &user_prompt,
                )
                .await
                .map_err(|e| capnp::Error::failed(format!("distillation LLM call failed: {}", e)))?;

            // Stage the summarized content
            let mut state_ref = state.borrow_mut();
            match state_ref.drift_router.stage(
                &source_ctx,
                &target_ctx,
                summary,
                Some(model.to_string()),
                kaijutsu_crdt::DriftKind::Distill,
            ) {
                Ok(staged_id) => {
                    results.get().set_staged_id(staged_id);
                    log::info!("Drift staged (distilled): {} → {} (id={})", source_ctx, target_ctx, staged_id);
                }
                Err(e) => {
                    return Err(capnp::Error::failed(format!("drift push failed: {}", e)));
                }
            }

            Ok(())
        }.instrument(trace_span))
    }

    fn drift_flush(
        self: Rc<Self>,
        params: kernel::DriftFlushParams,
        mut results: kernel::DriftFlushResults,
    ) -> Promise<(), capnp::Error> {
        let _trace_span = extract_rpc_trace(pry!(params.get()).get_trace(), "drift_flush");
        let mut state = self.state.borrow_mut();

        // Scope flush to items involving the calling kernel's context
        let caller_ctx = state.drift_router.short_id_for_context(&self.kernel_id)
            .map(|s| s.to_string());
        let staged = state.drift_router.drain(caller_ctx.as_deref());
        let count = staged.len() as u32;
        let mut failed: Vec<kaijutsu_kernel::StagedDrift> = Vec::new();

        for drift in staged {
            // Look up target kernel's documents
            let target_kernel_id = match state.drift_router.get(&drift.target_ctx) {
                Some(h) => h.context_name.clone(),
                None => {
                    log::warn!("Drift flush: target context {} not found, re-queuing", drift.target_ctx);
                    failed.push(drift);
                    continue;
                }
            };

            let (documents, main_doc_id) = match state.kernels.get(&target_kernel_id) {
                Some(ks) => (ks.documents.clone(), ks.main_document_id.clone()),
                None => {
                    log::warn!("Drift flush: kernel {} not found, re-queuing", target_kernel_id);
                    failed.push(drift);
                    continue;
                }
            };

            // Build drift block snapshot
            let author = format!("drift:{}", drift.source_ctx);
            let snapshot = kaijutsu_kernel::DriftRouter::build_drift_block(&drift, &author);

            // Get last block in target document for ordering
            let after = documents.last_block_id(&main_doc_id);

            // Inject into target document
            match documents.insert_from_snapshot(&main_doc_id, snapshot, after.as_ref()) {
                Ok(block_id) => {
                    log::info!(
                        "Drift flushed: {} → {} (block={})",
                        drift.source_ctx, drift.target_ctx, block_id.to_key()
                    );
                }
                Err(e) => {
                    log::error!("Drift flush failed for {} → {}: {}, re-queuing", drift.source_ctx, drift.target_ctx, e);
                    failed.push(drift);
                }
            }
        }

        // Re-queue any failed items so they aren't lost
        if !failed.is_empty() {
            log::warn!("Re-queued {} failed drift items", failed.len());
            state.drift_router.requeue(failed);
        }

        results.get().set_count(count);
        Promise::ok(())
    }

    fn drift_queue(
        self: Rc<Self>,
        _params: kernel::DriftQueueParams,
        mut results: kernel::DriftQueueResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.borrow();
        let queue = state.drift_router.queue();

        let mut list = results.get().init_staged(queue.len() as u32);
        for (i, drift) in queue.iter().enumerate() {
            let mut entry = list.reborrow().get(i as u32);
            entry.set_id(drift.id);
            entry.set_source_ctx(&drift.source_ctx);
            entry.set_target_ctx(&drift.target_ctx);
            entry.set_content(&drift.content);
            entry.set_source_model(drift.source_model.as_deref().unwrap_or(""));
            entry.set_drift_kind(&drift.drift_kind.to_string());
            entry.set_created_at(drift.created_at);
        }

        Promise::ok(())
    }

    fn drift_cancel(
        self: Rc<Self>,
        params: kernel::DriftCancelParams,
        mut results: kernel::DriftCancelResults,
    ) -> Promise<(), capnp::Error> {
        let staged_id = pry!(params.get()).get_staged_id();
        let mut state = self.state.borrow_mut();
        let success = state.drift_router.cancel(staged_id);
        results.get().set_success(success);
        Promise::ok(())
    }

    fn drift_pull(
        self: Rc<Self>,
        params: kernel::DriftPullParams,
        mut results: kernel::DriftPullResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let trace_span = extract_rpc_trace(params_reader.get_trace(), "drift_pull");
        let source_ctx_id = pry!(pry!(params_reader.get_source_ctx()).to_str()).to_owned();
        let directed_prompt = params_reader.get_prompt().ok()
            .and_then(|p| p.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned());

        let kernel_id = self.kernel_id.clone();
        let state = self.state.clone();


        Promise::from_future(async move {
            // Extract everything from state (inside async block so ? works)
            let (kernel_arc, source_docs, source_main_doc, source_model,
                 target_docs, target_main_doc, target_ctx_id) = {
                let state_ref = state.borrow();

                let source_handle = state_ref.drift_router.get(&source_ctx_id)
                    .ok_or_else(|| capnp::Error::failed(format!("unknown source context: {}", source_ctx_id)))?;
                let source_kernel_id = source_handle.context_name.clone();
                let source_model = source_handle.model.clone();
                let source_ks = state_ref.kernels.get(&source_kernel_id)
                    .ok_or_else(|| capnp::Error::failed(format!("kernel {} not found", source_kernel_id)))?;
                let source_docs = source_ks.documents.clone();
                let source_main_doc = source_ks.main_document_id.clone();

                let target_ctx_id = state_ref.drift_router.short_id_for_context(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("current kernel not registered".into()))?
                    .to_string();
                let target_ks = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                let kernel_arc = target_ks.kernel.clone();
                let target_docs = target_ks.documents.clone();
                let target_main_doc = target_ks.main_document_id.clone();

                (kernel_arc, source_docs, source_main_doc, source_model,
                 target_docs, target_main_doc, target_ctx_id)
            };

            // Get LLM provider from kernel's registry
            let provider = {
                let registry = kernel_arc.llm().read().await;
                registry.default_provider().ok_or_else(|| {
                    capnp::Error::failed("LLM provider not configured — cannot pull (check llm.rhai)".into())
                })?
            };

            // Read source context's blocks
            let blocks = source_docs.block_snapshots(&source_main_doc)
                .map_err(|e| capnp::Error::failed(format!("failed to read source blocks: {}", e)))?;

            let user_prompt = kaijutsu_kernel::build_distillation_prompt(
                &blocks,
                directed_prompt.as_deref(),
            );

            let model = source_model.as_deref().unwrap_or_else(|| {
                provider.available_models().first().copied().unwrap_or("claude-sonnet-4-5-20250929")
            });

            log::info!("Pulling from {} ({} blocks, model={}) → {}",
                source_ctx_id, blocks.len(), model, target_ctx_id);

            let summary = provider
                .prompt_with_system(
                    model,
                    Some(kaijutsu_kernel::DISTILLATION_SYSTEM_PROMPT),
                    &user_prompt,
                )
                .await
                .map_err(|e| capnp::Error::failed(format!("distillation LLM call failed: {}", e)))?;

            // Build drift block and inject directly into target (caller's) document
            let staged = kaijutsu_kernel::StagedDrift {
                id: 0,
                source_ctx: source_ctx_id.clone(),
                target_ctx: target_ctx_id.clone(),
                content: summary,
                source_model: Some(model.to_string()),
                drift_kind: kaijutsu_crdt::DriftKind::Pull,
                created_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            };

            let author = format!("drift:{}", source_ctx_id);
            let snapshot = kaijutsu_kernel::DriftRouter::build_drift_block(&staged, &author);
            let after = target_docs.last_block_id(&target_main_doc);

            let block_id = target_docs.insert_from_snapshot(&target_main_doc, snapshot, after.as_ref())
                .map_err(|e| capnp::Error::failed(format!("failed to inject drift block: {}", e)))?;

            log::info!("Drift pulled: {} → {} (block={})", source_ctx_id, target_ctx_id, block_id.to_key());

            let mut result_block_id = results.get().init_block_id();
            result_block_id.set_document_id(&block_id.document_id);
            result_block_id.set_agent_id(&block_id.agent_id);
            result_block_id.set_seq(block_id.seq);

            Ok(())
        }.instrument(trace_span))
    }

    fn drift_merge(
        self: Rc<Self>,
        params: kernel::DriftMergeParams,
        mut results: kernel::DriftMergeResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let trace_span = extract_rpc_trace(params_reader.get_trace(), "drift_merge");
        let source_ctx_id = pry!(pry!(params_reader.get_source_ctx()).to_str()).to_owned();

        let state = self.state.clone();


        Promise::from_future(async move {
            // Extract everything from state (inside async block so ? works)
            let (kernel_arc, source_docs, source_main_doc, source_model,
                 parent_ctx_id, parent_docs, parent_main_doc) = {
                let state_ref = state.borrow();

                let source_handle = state_ref.drift_router.get(&source_ctx_id)
                    .ok_or_else(|| capnp::Error::failed(format!("unknown source context: {}", source_ctx_id)))?;

                let parent_ctx_id = source_handle.parent_short_id.clone()
                    .ok_or_else(|| capnp::Error::failed(
                        format!("context {} has no parent — cannot merge", source_ctx_id)
                    ))?;

                let source_kernel_id = source_handle.context_name.clone();
                let source_model = source_handle.model.clone();
                let source_ks = state_ref.kernels.get(&source_kernel_id)
                    .ok_or_else(|| capnp::Error::failed(format!("kernel {} not found", source_kernel_id)))?;
                let kernel_arc = source_ks.kernel.clone();
                let source_docs = source_ks.documents.clone();
                let source_main_doc = source_ks.main_document_id.clone();

                let parent_handle = state_ref.drift_router.get(&parent_ctx_id)
                    .ok_or_else(|| capnp::Error::failed(format!("parent context {} not found", parent_ctx_id)))?;
                let parent_kernel_id = parent_handle.context_name.clone();
                let parent_ks = state_ref.kernels.get(&parent_kernel_id)
                    .ok_or_else(|| capnp::Error::failed(format!("parent kernel {} not found", parent_kernel_id)))?;
                let parent_docs = parent_ks.documents.clone();
                let parent_main_doc = parent_ks.main_document_id.clone();

                (kernel_arc, source_docs, source_main_doc, source_model,
                 parent_ctx_id, parent_docs, parent_main_doc)
            };

            // Get LLM provider from kernel's registry
            let provider = {
                let registry = kernel_arc.llm().read().await;
                registry.default_provider().ok_or_else(|| {
                    capnp::Error::failed("LLM provider not configured — cannot merge (check llm.rhai)".into())
                })?
            };

            // Read source context's blocks
            let blocks = source_docs.block_snapshots(&source_main_doc)
                .map_err(|e| capnp::Error::failed(format!("failed to read source blocks: {}", e)))?;

            let user_prompt = kaijutsu_kernel::build_distillation_prompt(&blocks, None);

            let model = source_model.as_deref().unwrap_or_else(|| {
                provider.available_models().first().copied().unwrap_or("claude-sonnet-4-5-20250929")
            });

            log::info!("Merging {} ({} blocks, model={}) → parent {}",
                source_ctx_id, blocks.len(), model, parent_ctx_id);

            let summary = provider
                .prompt_with_system(
                    model,
                    Some(kaijutsu_kernel::DISTILLATION_SYSTEM_PROMPT),
                    &user_prompt,
                )
                .await
                .map_err(|e| capnp::Error::failed(format!("distillation LLM call failed: {}", e)))?;

            // Build drift block and inject into parent document
            let staged = kaijutsu_kernel::StagedDrift {
                id: 0,
                source_ctx: source_ctx_id.clone(),
                target_ctx: parent_ctx_id.clone(),
                content: summary,
                source_model: Some(model.to_string()),
                drift_kind: kaijutsu_crdt::DriftKind::Merge,
                created_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            };

            let author = format!("drift:{}", source_ctx_id);
            let snapshot = kaijutsu_kernel::DriftRouter::build_drift_block(&staged, &author);
            let after = parent_docs.last_block_id(&parent_main_doc);

            let block_id = parent_docs.insert_from_snapshot(&parent_main_doc, snapshot, after.as_ref())
                .map_err(|e| capnp::Error::failed(format!("failed to inject merge block: {}", e)))?;

            log::info!("Drift merged: {} → parent {} (block={})",
                source_ctx_id, parent_ctx_id, block_id.to_key());

            let mut result_block_id = results.get().init_block_id();
            result_block_id.set_document_id(&block_id.document_id);
            result_block_id.set_agent_id(&block_id.agent_id);
            result_block_id.set_seq(block_id.seq);

            Ok(())
        }.instrument(trace_span))
    }

    fn list_all_contexts(
        self: Rc<Self>,
        _params: kernel::ListAllContextsParams,
        mut results: kernel::ListAllContextsResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.borrow();
        let contexts = state.drift_router.list_contexts();

        let mut list = results.get().init_contexts(contexts.len() as u32);
        for (i, ctx) in contexts.iter().enumerate() {
            let mut entry = list.reborrow().get(i as u32);
            entry.set_short_id(&ctx.short_id);
            entry.set_name(&ctx.context_name);
            entry.set_kernel_id(&ctx.context_name);
            entry.set_provider(ctx.provider.as_deref().unwrap_or(""));
            entry.set_model(ctx.model.as_deref().unwrap_or(""));
            if let Some(ref parent) = ctx.parent_short_id {
                entry.set_parent_id(parent);
                entry.set_has_parent_id(true);
            } else {
                entry.set_parent_id("");
                entry.set_has_parent_id(false);
            }
            entry.set_created_at(ctx.created_at);
        }

        Promise::ok(())
    }

    // ========================================================================
    // LLM Configuration (Phase 5)
    // ========================================================================

    fn get_llm_config(
        self: Rc<Self>,
        _params: kernel::GetLlmConfigParams,
        mut results: kernel::GetLlmConfigResults,
    ) -> Promise<(), capnp::Error> {
        let kernel_arc = {
            let state = self.state.borrow();
            match state.kernels.get(&self.kernel_id) {
                Some(ks) => ks.kernel.clone(),
                None => return Promise::err(capnp::Error::failed("kernel not found".into())),
            }
        };

        Promise::from_future(async move {
            let registry = kernel_arc.llm().read().await;

            let mut config = results.get().init_config();
            config.set_default_provider(
                registry.default_provider_name().unwrap_or("")
            );
            config.set_default_model(
                registry.default_model().unwrap_or("")
            );

            let provider_names = registry.list();
            let mut providers = config.init_providers(provider_names.len() as u32);
            for (i, name) in provider_names.iter().enumerate() {
                let mut entry = providers.reborrow().get(i as u32);
                entry.set_name(name);
                // Get the provider's default model if available
                if let Some(p) = registry.get(name) {
                    let models = p.available_models();
                    entry.set_default_model(models.first().copied().unwrap_or(""));
                    entry.set_available(true);
                } else {
                    entry.set_default_model("");
                    entry.set_available(false);
                }
            }

            Ok(())
        })
    }

    fn set_default_provider(
        self: Rc<Self>,
        params: kernel::SetDefaultProviderParams,
        mut results: kernel::SetDefaultProviderResults,
    ) -> Promise<(), capnp::Error> {
        let provider_name = pry!(pry!(pry!(params.get()).get_provider()).to_str()).to_owned();
        let kernel_arc = {
            let state = self.state.borrow();
            match state.kernels.get(&self.kernel_id) {
                Some(ks) => ks.kernel.clone(),
                None => return Promise::err(capnp::Error::failed("kernel not found".into())),
            }
        };

        Promise::from_future(async move {
            let mut registry = kernel_arc.llm().write().await;
            if registry.set_default(&provider_name) {
                results.get().set_success(true);
                results.get().set_error("");
                log::info!("Default LLM provider set to: {}", provider_name);
            } else {
                results.get().set_success(false);
                results.get().set_error(&format!("provider '{}' not found", provider_name));
            }
            Ok(())
        })
    }

    fn set_default_model(
        self: Rc<Self>,
        params: kernel::SetDefaultModelParams,
        mut results: kernel::SetDefaultModelResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let provider_name = pry!(pry!(params_reader.get_provider()).to_str()).to_owned();
        let model = pry!(pry!(params_reader.get_model()).to_str()).to_owned();
        let kernel_arc = {
            let state = self.state.borrow();
            match state.kernels.get(&self.kernel_id) {
                Some(ks) => ks.kernel.clone(),
                None => return Promise::err(capnp::Error::failed("kernel not found".into())),
            }
        };

        Promise::from_future(async move {
            let mut registry = kernel_arc.llm().write().await;
            // Verify the provider exists
            if registry.get(&provider_name).is_none() {
                results.get().set_success(false);
                results.get().set_error(&format!("provider '{}' not found", provider_name));
                return Ok(());
            }
            registry.set_default_model(&model);
            results.get().set_success(true);
            results.get().set_error("");
            log::info!("Default model set to: {} (provider: {})", model, provider_name);
            Ok(())
        })
    }

    // ========================================================================
    // Tool Filter Configuration (Phase 5)
    // ========================================================================

    fn get_tool_filter(
        self: Rc<Self>,
        _params: kernel::GetToolFilterParams,
        mut results: kernel::GetToolFilterResults,
    ) -> Promise<(), capnp::Error> {
        let kernel_arc = {
            let state = self.state.borrow();
            match state.kernels.get(&self.kernel_id) {
                Some(ks) => ks.kernel.clone(),
                None => return Promise::err(capnp::Error::failed("kernel not found".into())),
            }
        };

        Promise::from_future(async move {
            let tool_config = kernel_arc.tool_config().await;
            let mut filter = results.get().init_filter();

            match &tool_config.filter {
                ToolFilter::All => {
                    filter.set_all(());
                }
                ToolFilter::AllowList(tools) => {
                    let tools_vec: Vec<&str> = tools.iter().map(|s| s.as_str()).collect();
                    let mut list = filter.init_allow_list(tools_vec.len() as u32);
                    for (i, tool) in tools_vec.iter().enumerate() {
                        list.set(i as u32, tool);
                    }
                }
                ToolFilter::DenyList(tools) => {
                    let tools_vec: Vec<&str> = tools.iter().map(|s| s.as_str()).collect();
                    let mut list = filter.init_deny_list(tools_vec.len() as u32);
                    for (i, tool) in tools_vec.iter().enumerate() {
                        list.set(i as u32, tool);
                    }
                }
            }

            Ok(())
        })
    }

    fn set_tool_filter(
        self: Rc<Self>,
        params: kernel::SetToolFilterParams,
        mut results: kernel::SetToolFilterResults,
    ) -> Promise<(), capnp::Error> {
        let filter_reader = pry!(pry!(params.get()).get_filter());
        let filter = match pry!(filter_reader.which()) {
            tool_filter_config::All(()) => ToolFilter::All,
            tool_filter_config::AllowList(list) => {
                let list = pry!(list);
                let mut tools = std::collections::HashSet::new();
                for i in 0..list.len() {
                    let name = pry!(pry!(list.get(i)).to_str());
                    tools.insert(name.to_string());
                }
                ToolFilter::AllowList(tools)
            }
            tool_filter_config::DenyList(list) => {
                let list = pry!(list);
                let mut tools = std::collections::HashSet::new();
                for i in 0..list.len() {
                    let name = pry!(pry!(list.get(i)).to_str());
                    tools.insert(name.to_string());
                }
                ToolFilter::DenyList(tools)
            }
        };

        let kernel_arc = {
            let state = self.state.borrow();
            match state.kernels.get(&self.kernel_id) {
                Some(ks) => ks.kernel.clone(),
                None => return Promise::err(capnp::Error::failed("kernel not found".into())),
            }
        };

        Promise::from_future(async move {
            kernel_arc.set_tool_filter(filter).await;
            results.get().set_success(true);
            results.get().set_error("");
            log::info!("Tool filter updated for kernel");
            Ok(())
        })
    }

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
                Err(e) => return Promise::err(capnp::Error::failed(format!("invalid name: {}", e))),
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("missing name: {}", e))),
        };

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kaish = {
                let state_ref = state.borrow();
                let kernel = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                kernel.kaish.clone()
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
        })
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
                return Promise::ok({
                    results.get().set_success(false);
                    results.get().set_error(&format!("invalid value: {}", e));
                });
            }
        };

        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kaish = {
                let state_ref = state.borrow();
                let kernel = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                kernel.kaish.clone()
                    .ok_or_else(|| capnp::Error::failed("kaish not initialized".into()))?
            };

            kaish.set_var(&name, value).await;
            results.get().set_success(true);
            results.get().set_error("");
            Ok(())
        })
    }

    fn list_shell_vars(
        self: Rc<Self>,
        _params: kernel::ListShellVarsParams,
        mut results: kernel::ListShellVarsResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.clone();
        let kernel_id = self.kernel_id.clone();

        Promise::from_future(async move {
            let kaish = {
                let state_ref = state.borrow();
                let kernel = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;
                kernel.kaish.clone()
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
        })
    }

    fn compact_document(
        self: Rc<Self>,
        params: kernel::CompactDocumentParams,
        mut results: kernel::CompactDocumentResults,
    ) -> Promise<(), capnp::Error> {
        let document_id = pry!(pry!(pry!(params.get()).get_document_id()).to_str()).to_string();

        let state = self.state.borrow();
        let kernel = match state.kernels.get(&self.kernel_id) {
            Some(k) => k,
            None => {
                return Promise::err(capnp::Error::failed(format!(
                    "kernel '{}' not found",
                    self.kernel_id
                )));
            }
        };

        match kernel.documents.compact_document(&document_id) {
            Ok(new_size) => {
                let generation = kernel.documents
                    .get(&document_id)
                    .map(|e| e.sync_generation())
                    .unwrap_or(0);
                let mut r = results.get();
                r.set_new_size(new_size as u64);
                r.set_generation(generation);
                Promise::ok(())
            }
            Err(e) => Promise::err(capnp::Error::failed(e)),
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
        Value::Json(j) => builder.set_json(&serde_json::to_string(j).unwrap_or_default()),
        Value::Blob(b) => builder.set_blob(&b.id),
    }
}

/// Convert Cap'n Proto `ShellValue` reader → kaish `ast::Value`.
fn shell_value_to_value(reader: shell_value::Reader<'_>) -> Result<kaish_kernel::ast::Value, capnp::Error> {
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
// Agent Helper Functions
// ============================================================================

use crate::kaijutsu_capnp::{
    AgentCapability as CapnpAgentCapability,
    AgentStatus as CapnpAgentStatus,
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
// LLM Stream Helpers
// ============================================================================

use kaijutsu_kernel::llm::{ToolDefinition, ContentBlock};

/// Build tool definitions from tools with engines, filtered by the kernel's ToolConfig.
async fn build_tool_definitions(kernel: &Arc<Kernel>) -> Vec<ToolDefinition> {
    let registry = kernel.tools().read().await;
    let tool_config = kernel.tool_config().await;

    // Get tools with engines, filtered by the kernel's tool filter
    let available = registry.list_with_engines();

    available
        .into_iter()
        .filter(|info| tool_config.allows(&info.name))
        .filter_map(|info| {
            // Only include tools that provide a schema — models can't use tools
            // without knowing the expected parameters
            let input_schema = registry
                .get_engine(&info.name)
                .and_then(|e| e.schema())?;
            Some(ToolDefinition {
                name: info.name.clone(),
                description: info.description.clone(),
                input_schema,
            })
        })
        .collect()
}

/// Process LLM streaming in a background task with agentic loop.
/// This function handles all stream events, executes tools, and loops until done.
/// Block events are broadcast via FlowBus (BlockStore emits BlockFlow events).
///
/// `after_block_id` is the starting point for block ordering - all streaming blocks
/// will be inserted after this block (typically the user's message).
async fn process_llm_stream(
    provider: Arc<RigProvider>,
    documents: SharedBlockStore,
    cell_id: String,
    content: String,
    model_name: String,
    kernel: Arc<Kernel>,
    tools: Vec<ToolDefinition>,
    after_block_id: kaijutsu_crdt::BlockId,
    system_prompt: String,
    max_output_tokens: u64,
) {
    // Build initial conversation messages
    let mut messages: Vec<LlmMessage> = vec![LlmMessage::user(&content)];

    // Track total iterations to prevent infinite loops
    let max_iterations = 20;
    let mut iteration = 0;

    // Track last inserted block for ordering - each new block goes after the previous
    let mut last_block_id = after_block_id;

    // Agentic loop - continue until model is done or max iterations
    loop {
        iteration += 1;
        if iteration > max_iterations {
            log::warn!("Agentic loop hit max iterations ({}), stopping", max_iterations);
            let _ = documents.insert_block(&cell_id, None, Some(&last_block_id), Role::Model, BlockKind::Text, "⚠️ Maximum tool iterations reached");
            break;
        }

        log::info!("Agentic loop iteration {} with {} messages, {} tools", iteration, messages.len(), tools.len());

        // Create streaming request with tools
        let stream_request = StreamRequest::new(&model_name, messages.clone())
            .with_system(&system_prompt)
            .with_max_tokens(max_output_tokens)
            .with_tools(tools.clone());

        // Start streaming
        let mut stream = match provider.stream(stream_request).await {
            Ok(s) => {
                log::info!("LLM stream started successfully");
                s
            }
            Err(e) => {
                log::error!("Failed to start LLM stream: {}", e);
                let _ = documents.insert_block(&cell_id, None, Some(&last_block_id), Role::Model, BlockKind::Text, format!("❌ Error: {}", e));
                return;
            }
        };

        // Process stream events
        let mut current_block_id: Option<kaijutsu_crdt::BlockId> = None;
        // Collect tool calls for this iteration
        let mut tool_calls: Vec<(String, String, serde_json::Value)> = vec![]; // (id, name, input)
        // Track tool_use_id → BlockId mapping for CRDT
        let mut tool_call_blocks: std::collections::HashMap<String, kaijutsu_crdt::BlockId> = std::collections::HashMap::new();
        // Collect text output for conversation history
        let mut assistant_text = String::new();

        log::debug!("Entering stream event loop");
        while let Some(event) = stream.next_event().await {
            log::debug!("Received stream event: {:?}", event);
            match event {
                StreamEvent::ThinkingStart => {
                    match documents.insert_block(&cell_id, None, Some(&last_block_id), Role::Model, BlockKind::Thinking, "") {
                        Ok(block_id) => {
                            last_block_id = block_id.clone();
                            current_block_id = Some(block_id);
                        }
                        Err(e) => log::error!("Failed to insert thinking block: {}", e),
                    }
                }

                StreamEvent::ThinkingDelta(text) => {
                    if let Some(ref block_id) = current_block_id {
                        if let Err(e) = documents.append_text(&cell_id, block_id, &text) {
                            log::error!("Failed to append thinking text: {}", e);
                        }
                    }
                }

                StreamEvent::ThinkingEnd => {
                    current_block_id = None;
                }

                StreamEvent::TextStart => {
                    match documents.insert_block(&cell_id, None, Some(&last_block_id), Role::Model, BlockKind::Text, "") {
                        Ok(block_id) => {
                            last_block_id = block_id.clone();
                            current_block_id = Some(block_id);
                        }
                        Err(e) => log::error!("Failed to insert text block: {}", e),
                    }
                }

                StreamEvent::TextDelta(text) => {
                    // Collect text for conversation history
                    assistant_text.push_str(&text);

                    if let Some(ref block_id) = current_block_id {
                        if let Err(e) = documents.append_text(&cell_id, block_id, &text) {
                            log::error!("Failed to append text: {}", e);
                        }
                    }
                }

                StreamEvent::TextEnd => {
                    current_block_id = None;
                }

                StreamEvent::ToolUse { id, name, input } => {
                    // Store for later execution
                    tool_calls.push((id.clone(), name.clone(), input.clone()));

                    // Insert block and track it
                    match documents.insert_tool_call(&cell_id, None, Some(&last_block_id), &name, input.clone()) {
                        Ok(block_id) => {
                            last_block_id = block_id.clone();
                            tool_call_blocks.insert(id.clone(), block_id);
                        }
                        Err(e) => log::error!("Failed to insert tool use block: {}", e),
                    }
                }

                StreamEvent::ToolResult { .. } => {
                    // This shouldn't happen during streaming - tool results are generated by us
                    log::warn!("Unexpected ToolResult event during streaming");
                }

                StreamEvent::Done { stop_reason, input_tokens, output_tokens } => {
                    log::info!(
                        "LLM stream completed: stop_reason={:?}, tokens_in={:?}, tokens_out={:?}",
                        stop_reason, input_tokens, output_tokens
                    );
                }

                StreamEvent::Error(err) => {
                    log::error!("LLM stream error: {}", err);
                    let _ = documents.insert_block(&cell_id, None, Some(&last_block_id), Role::Model, BlockKind::Text, format!("❌ Error: {}", err));
                    return;
                }
            }
        }

        // Check if we need to execute tools.
        // rig doesn't expose stop_reason through FinalCompletionResponse — its own
        // agent uses the presence of tool calls as the continuation signal (see
        // rig-core streaming.rs did_call_tool pattern). This is reliable because
        // the API only emits ToolCall content blocks when stop_reason is "tool_use".
        if tool_calls.is_empty() {
            log::info!("Agentic loop complete - no tool calls this iteration");
            break;
        }

        // Execute tools concurrently — CRDT handles concurrent block inserts
        log::info!("Executing {} tool calls concurrently", tool_calls.len());

        // Build assistant tool uses (for conversation history)
        let assistant_tool_uses: Vec<ContentBlock> = tool_calls.iter()
            .map(|(id, name, input)| ContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            })
            .collect();

        // Execute all tools concurrently
        let futures: Vec<_> = tool_calls.into_iter()
            .map(|(tool_use_id, tool_name, input)| {
                let kernel = kernel.clone();
                let documents = documents.clone();
                let cell_id = cell_id.clone();
                let tool_call_block_id = tool_call_blocks.get(&tool_use_id).cloned();
                async move {
                    let params = input.to_string();
                    log::info!("Executing tool: {} with params: {}", tool_name, params);

                    let result = kernel.execute_with(&tool_name, &params).await;

                    let (result_content, is_error) = match result {
                        Ok(r) if r.success => {
                            log::debug!("Tool {} succeeded: {}", tool_name, r.stdout);
                            (r.stdout, false)
                        }
                        Ok(r) => {
                            log::warn!("Tool {} failed: {}", tool_name, r.stderr);
                            (format!("Error: {}", r.stderr), true)
                        }
                        Err(e) => {
                            log::error!("Tool {} execution error: {}", tool_name, e);
                            (format!("Execution error: {}", e), true)
                        }
                    };

                    // Insert tool result block (CRDT DAG — each result parents its own tool_call)
                    let mut result_block_id = None;
                    if let Some(ref tcb_id) = tool_call_block_id {
                        match documents.insert_tool_result(
                            &cell_id, tcb_id, Some(tcb_id), &result_content, is_error, None,
                        ) {
                            Ok(id) => result_block_id = Some(id),
                            Err(e) => log::error!("Failed to insert tool result block: {}", e),
                        }
                    }

                    (
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content: result_content,
                            is_error,
                        },
                        result_block_id,
                    )
                }
            })
            .collect();

        let results_with_ids = futures::future::join_all(futures).await;

        // Unzip and update last_block_id so the next iteration's blocks
        // appear after tool results, not after tool calls.
        let mut tool_results = Vec::new();
        for (content_block, block_id_opt) in results_with_ids {
            tool_results.push(content_block);
            if let Some(id) = block_id_opt {
                last_block_id = id;
            }
        }

        // Add assistant message with tool uses to conversation
        messages.push(LlmMessage::with_tool_uses(
            if assistant_text.is_empty() { None } else { Some(assistant_text) },
            assistant_tool_uses,
        ));

        // Add user message with tool results
        messages.push(LlmMessage::tool_results(tool_results));

        // Loop continues - re-prompt with tool results
    }

    // Save final state after streaming completes
    if let Err(e) = documents.save_snapshot(&cell_id) {
        log::warn!("Failed to save snapshot for cell {}: {}", cell_id, e);
    }

    log::info!("LLM stream processing complete for cell {}", cell_id);
}

// ============================================================================
// Utility Functions
// ============================================================================

/// Set BlockSnapshot fields on a Cap'n Proto builder.
fn set_block_snapshot(
    builder: &mut crate::kaijutsu_capnp::block_snapshot::Builder,
    block: &kaijutsu_crdt::BlockSnapshot,
) {
    // Set ID
    {
        let mut id = builder.reborrow().init_id();
        id.set_document_id(&block.id.document_id);
        id.set_agent_id(&block.id.agent_id);
        id.set_seq(block.id.seq);
    }

    // Set parent_id if present
    if let Some(ref parent) = block.parent_id {
        builder.set_has_parent_id(true);
        let mut pid = builder.reborrow().init_parent_id();
        pid.set_document_id(&parent.document_id);
        pid.set_agent_id(&parent.agent_id);
        pid.set_seq(parent.seq);
    } else {
        builder.set_has_parent_id(false);
    }

    // Set role
    builder.set_role(match block.role {
        kaijutsu_crdt::Role::User => crate::kaijutsu_capnp::Role::User,
        kaijutsu_crdt::Role::Model => crate::kaijutsu_capnp::Role::Model,
        kaijutsu_crdt::Role::System => crate::kaijutsu_capnp::Role::System,
        kaijutsu_crdt::Role::Tool => crate::kaijutsu_capnp::Role::Tool,
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
        kaijutsu_crdt::BlockKind::ShellCommand => crate::kaijutsu_capnp::BlockKind::ShellCommand,
        kaijutsu_crdt::BlockKind::ShellOutput => crate::kaijutsu_capnp::BlockKind::ShellOutput,
        kaijutsu_crdt::BlockKind::Drift => crate::kaijutsu_capnp::BlockKind::Drift,
    });

    // Set basic fields
    builder.set_content(&block.content);
    builder.set_collapsed(block.collapsed);
    builder.set_author(&block.author);
    builder.set_created_at(block.created_at);

    // Set tool-specific fields
    if let Some(ref name) = block.tool_name {
        builder.set_tool_name(name);
    }
    if let Some(ref input) = block.tool_input {
        builder.set_tool_input(&input.to_string());
    }
    if let Some(ref tc_id) = block.tool_call_id {
        builder.set_has_tool_call_id(true);
        let mut tcid = builder.reborrow().init_tool_call_id();
        tcid.set_document_id(&tc_id.document_id);
        tcid.set_agent_id(&tc_id.agent_id);
        tcid.set_seq(tc_id.seq);
    } else {
        builder.set_has_tool_call_id(false);
    }
    if let Some(code) = block.exit_code {
        builder.set_has_exit_code(true);
        builder.set_exit_code(code);
    } else {
        builder.set_has_exit_code(false);
    }
    builder.set_is_error(block.is_error);

    // Set display hint if present
    if let Some(ref hint) = block.display_hint {
        builder.set_has_display_hint(true);
        builder.set_display_hint(hint);
    } else {
        builder.set_has_display_hint(false);
    }

    // Set drift-specific fields if present
    if let Some(ref ctx) = block.source_context {
        builder.set_has_source_context(true);
        builder.set_source_context(ctx);
    } else {
        builder.set_has_source_context(false);
    }
    if let Some(ref model) = block.source_model {
        builder.set_has_source_model(true);
        builder.set_source_model(model);
    } else {
        builder.set_has_source_model(false);
    }
    if let Some(ref dk) = block.drift_kind {
        builder.set_has_drift_kind(true);
        builder.set_drift_kind(dk.as_str());
    } else {
        builder.set_has_drift_kind(false);
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
fn parse_block_snapshot(
    reader: &crate::kaijutsu_capnp::block_snapshot::Reader<'_>,
) -> Result<kaijutsu_crdt::BlockSnapshot, capnp::Error> {
    let id_reader = reader.get_id()?;
    let id = kaijutsu_crdt::BlockId {
        document_id: id_reader.get_document_id()?.to_str()?.to_owned(),
        agent_id: id_reader.get_agent_id()?.to_str()?.to_owned(),
        seq: id_reader.get_seq(),
    };

    let parent_id = if reader.get_has_parent_id() {
        let pid_reader = reader.get_parent_id()?;
        Some(kaijutsu_crdt::BlockId {
            document_id: pid_reader.get_document_id()?.to_str()?.to_owned(),
            agent_id: pid_reader.get_agent_id()?.to_str()?.to_owned(),
            seq: pid_reader.get_seq(),
        })
    } else {
        None
    };

    let role = match reader.get_role()? {
        crate::kaijutsu_capnp::Role::User => kaijutsu_crdt::Role::User,
        crate::kaijutsu_capnp::Role::Model => kaijutsu_crdt::Role::Model,
        crate::kaijutsu_capnp::Role::System => kaijutsu_crdt::Role::System,
        crate::kaijutsu_capnp::Role::Tool => kaijutsu_crdt::Role::Tool,
    };

    let status = match reader.get_status()? {
        crate::kaijutsu_capnp::Status::Pending => kaijutsu_crdt::Status::Pending,
        crate::kaijutsu_capnp::Status::Running => kaijutsu_crdt::Status::Running,
        crate::kaijutsu_capnp::Status::Done => kaijutsu_crdt::Status::Done,
        crate::kaijutsu_capnp::Status::Error => kaijutsu_crdt::Status::Error,
    };

    let kind = match reader.get_kind()? {
        crate::kaijutsu_capnp::BlockKind::Text => kaijutsu_crdt::BlockKind::Text,
        crate::kaijutsu_capnp::BlockKind::Thinking => kaijutsu_crdt::BlockKind::Thinking,
        crate::kaijutsu_capnp::BlockKind::ToolCall => kaijutsu_crdt::BlockKind::ToolCall,
        crate::kaijutsu_capnp::BlockKind::ToolResult => kaijutsu_crdt::BlockKind::ToolResult,
        crate::kaijutsu_capnp::BlockKind::ShellCommand => kaijutsu_crdt::BlockKind::ShellCommand,
        crate::kaijutsu_capnp::BlockKind::ShellOutput => kaijutsu_crdt::BlockKind::ShellOutput,
        crate::kaijutsu_capnp::BlockKind::Drift => kaijutsu_crdt::BlockKind::Drift,
    };

    let tool_call_id = if reader.get_has_tool_call_id() {
        let tc_reader = reader.get_tool_call_id()?;
        Some(kaijutsu_crdt::BlockId {
            document_id: tc_reader.get_document_id()?.to_str()?.to_owned(),
            agent_id: tc_reader.get_agent_id()?.to_str()?.to_owned(),
            seq: tc_reader.get_seq(),
        })
    } else {
        None
    };

    let tool_input = reader.get_tool_input()
        .ok()
        .and_then(|s| s.to_str().ok())
        .filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str(s).ok());

    // Read display hint from wire protocol
    let display_hint = if reader.get_has_display_hint() {
        reader.get_display_hint()
            .ok()
            .and_then(|s| s.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned())
    } else {
        None
    };

    Ok(kaijutsu_crdt::BlockSnapshot {
        id,
        parent_id,
        role,
        status,
        kind,
        content: reader.get_content()?.to_str()?.to_owned(),
        collapsed: reader.get_collapsed(),
        author: reader.get_author()?.to_str()?.to_owned(),
        created_at: reader.get_created_at(),
        tool_name: reader.get_tool_name().ok().and_then(|s| s.to_str().ok()).filter(|s| !s.is_empty()).map(|s| s.to_owned()),
        tool_input,
        tool_call_id,
        exit_code: if reader.get_has_exit_code() { Some(reader.get_exit_code()) } else { None },
        is_error: reader.get_is_error(),
        display_hint,
        source_context: if reader.get_has_source_context() {
            reader.get_source_context().ok()
                .and_then(|s| s.to_str().ok())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_owned())
        } else { None },
        source_model: if reader.get_has_source_model() {
            reader.get_source_model().ok()
                .and_then(|s| s.to_str().ok())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_owned())
        } else { None },
        drift_kind: if reader.get_has_drift_kind() {
            reader.get_drift_kind().ok()
                .and_then(|s| s.to_str().ok())
                .and_then(|s| kaijutsu_crdt::DriftKind::from_str(s))
        } else { None },
    })
}

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
            let attr = kernel.getattr(Path::new(&path)).await.map_err(vfs_err_to_capnp)?;
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
            let entries = kernel.readdir(Path::new(&path)).await.map_err(vfs_err_to_capnp)?;
            let mut builder = results.get().init_entries(entries.len() as u32);
            for (i, entry) in entries.iter().enumerate() {
                let mut e = builder.reborrow().get(i as u32);
                e.set_name(&entry.name);
                e.set_kind(match entry.kind {
                    kaijutsu_kernel::FileType::File => crate::kaijutsu_capnp::FileType::File,
                    kaijutsu_kernel::FileType::Directory => crate::kaijutsu_capnp::FileType::Directory,
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
            let target = kernel.readlink(Path::new(&path)).await.map_err(vfs_err_to_capnp)?;
            results.get().set_target(&target.to_string_lossy());
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
            kernel.unlink(Path::new(&path)).await.map_err(vfs_err_to_capnp)?;
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
            kernel.rmdir(Path::new(&path)).await.map_err(vfs_err_to_capnp)?;
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
                    results.get().set_real_path(&real.to_string_lossy());
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
