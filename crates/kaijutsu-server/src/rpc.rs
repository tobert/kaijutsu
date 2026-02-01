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
    AnthropicProvider, LlmMessage, llm::stream::{LlmStream, StreamRequest, StreamEvent},
    // Block tools
    BlockAppendEngine, BlockCreateEngine, BlockEditEngine, BlockListEngine, BlockReadEngine,
    BlockSearchEngine, BlockSpliceEngine, BlockStatusEngine, KernelSearchEngine,
    // MCP
    McpServerPool, McpServerConfig, McpToolEngine,
    // FlowBus
    BlockFlow, SharedBlockFlowBus, shared_block_flow_bus,
    block_store::BlockStore,
};
use kaijutsu_crdt::{BlockKind, Role};
use serde_json;

// ============================================================================
// Server State
// ============================================================================

/// Register block tools with a kernel.
async fn register_block_tools(
    kernel: &Arc<Kernel>,
    documents: SharedBlockStore,
    context_manager: Arc<ContextManager>,
) {
    // context - Context management (new, switch, list)
    kernel
        .register_tool_with_engine(
            ToolInfo::new("context", "Manage conversation contexts (new, switch, list)", "kernel"),
            Arc::new(ContextEngine::new(context_manager)),
        )
        .await;
    kernel.equip("context").await;

    // block_create - Create a new block
    kernel
        .register_tool_with_engine(
            ToolInfo::new("block_create", "Create a new block with role, kind, content", "block"),
            Arc::new(BlockCreateEngine::new(documents.clone(), "server")),
        )
        .await;
    kernel.equip("block_create").await;

    // block_append - Append text to a block (streaming-optimized)
    kernel
        .register_tool_with_engine(
            ToolInfo::new("block_append", "Append text to a block", "block"),
            Arc::new(BlockAppendEngine::new(documents.clone())),
        )
        .await;
    kernel.equip("block_append").await;

    // block_edit - Line-based editing with atomic operations and CAS
    kernel
        .register_tool_with_engine(
            ToolInfo::new("block_edit", "Line-based editing with atomic ops and CAS validation", "block"),
            Arc::new(BlockEditEngine::new(documents.clone(), "server")),
        )
        .await;
    kernel.equip("block_edit").await;

    // block_splice - Character-based editing for programmatic tools
    kernel
        .register_tool_with_engine(
            ToolInfo::new("block_splice", "Character-based splice editing", "block"),
            Arc::new(BlockSpliceEngine::new(documents.clone())),
        )
        .await;
    kernel.equip("block_splice").await;

    // block_read - Read block content with line numbers and ranges
    kernel
        .register_tool_with_engine(
            ToolInfo::new("block_read", "Read block content with optional line numbers", "block"),
            Arc::new(BlockReadEngine::new(documents.clone())),
        )
        .await;
    kernel.equip("block_read").await;

    // block_search - Search within a block using regex
    kernel
        .register_tool_with_engine(
            ToolInfo::new("block_search", "Search within a block using regex", "block"),
            Arc::new(BlockSearchEngine::new(documents.clone())),
        )
        .await;
    kernel.equip("block_search").await;

    // block_list - List blocks with filters
    kernel
        .register_tool_with_engine(
            ToolInfo::new("block_list", "List blocks with optional filters", "block"),
            Arc::new(BlockListEngine::new(documents.clone())),
        )
        .await;
    kernel.equip("block_list").await;

    // block_status - Set block status
    kernel
        .register_tool_with_engine(
            ToolInfo::new("block_status", "Set block status", "block"),
            Arc::new(BlockStatusEngine::new(documents.clone())),
        )
        .await;
    kernel.equip("block_status").await;

    // kernel_search - Cross-block grep
    kernel
        .register_tool_with_engine(
            ToolInfo::new("kernel_search", "Search across blocks using regex", "kernel"),
            Arc::new(KernelSearchEngine::new(documents)),
        )
        .await;
    kernel.equip("kernel_search").await;
}

// ============================================================================
// Seat & Context Types (Rust-side mirrors of Cap'n Proto types)
// ============================================================================

/// Unique identifier for a seat
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SeatId {
    pub nick: String,
    pub instance: String,
    pub kernel: String,
    pub context: String,
}

impl SeatId {
    /// Create a string key for HashMap lookup
    pub fn key(&self) -> String {
        format!("@{}:{}@{}:{}", self.nick, self.instance, self.kernel, self.context)
    }
}

/// Seat status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SeatStatus {
    #[default]
    Active,
    Idle,
    Away,
}

/// Information about a seat
#[derive(Debug, Clone)]
pub struct SeatInfo {
    pub id: SeatId,
    pub owner: String,
    pub status: SeatStatus,
    pub last_activity: u64,
    pub cursor_block: Option<String>,
}

/// A context within a kernel
#[derive(Debug, Clone)]
pub struct ContextState {
    pub name: String,
    pub documents: Vec<String>,  // Document IDs attached to this context
    pub seats: Vec<SeatId>,      // Seats currently in this context
}

impl ContextState {
    pub fn new(name: String) -> Self {
        Self {
            name,
            documents: Vec::new(),
            seats: Vec::new(),
        }
    }
}

/// Server state shared across all capabilities
pub struct ServerState {
    pub identity: Identity,
    pub kernels: HashMap<String, KernelState>,
    next_kernel_id: AtomicU64,
    next_exec_id: AtomicU64,
    /// LLM provider (initialized from ANTHROPIC_API_KEY)
    pub llm_provider: Option<Arc<AnthropicProvider>>,
    /// User's active seats across all kernels
    pub my_seats: HashMap<String, SeatInfo>,  // key is SeatId::key()
    /// Currently active seat for this connection (if any)
    pub current_seat: Option<SeatId>,
    /// Shared MCP server pool
    pub mcp_pool: Arc<McpServerPool>,
}

impl ServerState {
    pub fn new(username: String) -> Self {
        // Initialize LLM provider from environment if API key is available
        let llm_provider = if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            log::info!("ANTHROPIC_API_KEY found, initializing LLM provider");
            Some(Arc::new(AnthropicProvider::from_env()))
        } else {
            log::warn!("ANTHROPIC_API_KEY not set, LLM features disabled");
            None
        };

        Self {
            identity: Identity {
                username: username.clone(),
                display_name: username,
            },
            kernels: HashMap::new(),
            next_kernel_id: AtomicU64::new(1),
            next_exec_id: AtomicU64::new(1),
            llm_provider,
            my_seats: HashMap::new(),
            current_seat: None,
            mcp_pool: Arc::new(McpServerPool::new()),
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
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    let dir = base.join("kaijutsu").join("kernels").join(kernel_id);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("Failed to create kernel data dir {:?}: {}", dir, e);
    }
    dir
}

/// Open or create a BlockStore with database persistence for a kernel.
fn create_block_store_with_db(kernel_id: &str, block_flows: SharedBlockFlowBus) -> SharedBlockStore {
    let db_path = kernel_data_dir(kernel_id).join("data.db");
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
    /// Contexts within this kernel (for seat management)
    pub contexts: HashMap<String, ContextState>,
    /// Thread-safe context manager for shell access
    pub context_manager: Arc<ContextManager>,
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
        let state = Rc::new(RefCell::new(ServerState::new(username)));
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
        let id = pry!(pry!(pry!(params.get()).get_id()).to_str()).to_owned();
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

                // Create the kaijutsu kernel with shared FlowBus
                let kernel = Kernel::with_flows(&id, block_flows.clone()).await;

                // Mount user's home directory at /home (read-only for now)
                if let Some(home) = dirs::home_dir() {
                    kernel
                        .mount("/home", LocalBackend::read_only(home))
                        .await;
                }

                // Create block store with database persistence and shared FlowBus
                let documents = create_block_store_with_db(&id, block_flows);

                // Ensure main document exists (convention ID)
                let main_document_id = ensure_main_document(&documents, &id)?;

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

                // Register block tools (including context engine)
                let kernel_arc = Arc::new(kernel);
                register_block_tools(&kernel_arc, documents.clone(), context_manager.clone()).await;

                // Create default context
                let mut contexts = HashMap::new();
                contexts.insert("default".to_string(), ContextState::new("default".to_string()));

                let mut state_ref = state.borrow_mut();
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

            // Create the kaijutsu kernel with shared FlowBus
            let kernel = Kernel::with_flows(&name, block_flows.clone()).await;

            // Mount user's home directory at /home (read-only for now)
            if let Some(home) = dirs::home_dir() {
                kernel
                    .mount("/home", LocalBackend::read_only(home))
                    .await;
            }

            // Create block store with database persistence and shared FlowBus
            let documents = create_block_store_with_db(&id, block_flows);

            // Ensure main document exists (convention ID)
            let main_document_id = ensure_main_document(&documents, &id)?;

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

            // Register block tools (including context engine)
            let kernel_arc = Arc::new(kernel);
            register_block_tools(&kernel_arc, documents.clone(), context_manager.clone()).await;

            // Create default context
            let mut contexts = HashMap::new();
            contexts.insert("default".to_string(), ContextState::new("default".to_string()));

            {
                let mut state_ref = state.borrow_mut();
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
                    },
                );
            }

            let kernel_impl = KernelImpl::new(state.clone(), id);
            results.get().set_kernel(capnp_rpc::new_client(kernel_impl));
            Ok(())
        })
    }

    fn list_my_seats(
        self: Rc<Self>,
        _params: world::ListMySeatsParams,
        mut results: world::ListMySeatsResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.borrow();
        let seats: Vec<_> = state.my_seats.values().collect();
        let mut seat_list = results.get().init_seats(seats.len() as u32);

        for (i, seat) in seats.iter().enumerate() {
            let mut s = seat_list.reborrow().get(i as u32);
            let mut id = s.reborrow().init_id();
            id.set_nick(&seat.id.nick);
            id.set_instance(&seat.id.instance);
            id.set_kernel(&seat.id.kernel);
            id.set_context(&seat.id.context);

            s.set_owner(&seat.owner);
            s.set_status(match seat.status {
                SeatStatus::Active => crate::kaijutsu_capnp::SeatStatus::Active,
                SeatStatus::Idle => crate::kaijutsu_capnp::SeatStatus::Idle,
                SeatStatus::Away => crate::kaijutsu_capnp::SeatStatus::Away,
            });
            s.set_last_activity(seat.last_activity);
            if let Some(ref cursor) = seat.cursor_block {
                s.set_cursor_block(cursor);
            }
        }

        Promise::ok(())
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
        let code = pry!(pry!(pry!(params.get()).get_code()).to_str()).to_owned();
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
        })
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

    // Equipment

    fn list_equipment(
        self: Rc<Self>,
        _params: kernel::ListEquipmentParams,
        mut results: kernel::ListEquipmentResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.borrow();
        let kernel = match state.kernels.get(&self.kernel_id) {
            Some(k) => k.kernel.clone(),
            None => return Promise::err(capnp::Error::failed("kernel not found".into())),
        };
        drop(state);

        Promise::from_future(async move {
            let tools = kernel.list_tools().await;
            let mut builder = results.get().init_tools(tools.len() as u32);
            for (i, tool) in tools.iter().enumerate() {
                let mut t = builder.reborrow().get(i as u32);
                t.set_name(&tool.name);
                t.set_description(&tool.description);
                t.set_equipped(tool.equipped);
            }
            Ok(())
        })
    }

    fn equip(
        self: Rc<Self>,
        params: kernel::EquipParams,
        _results: kernel::EquipResults,
    ) -> Promise<(), capnp::Error> {
        let tool_name = pry!(pry!(pry!(params.get()).get_tool()).to_str()).to_owned();

        let state = self.state.borrow();
        let kernel = match state.kernels.get(&self.kernel_id) {
            Some(k) => k.kernel.clone(),
            None => return Promise::err(capnp::Error::failed("kernel not found".into())),
        };
        drop(state);

        Promise::from_future(async move {
            // Tools must be pre-registered with engines
            // equip() just sets the equipped flag
            if kernel.equip(&tool_name).await {
                Ok(())
            } else {
                Err(capnp::Error::failed(format!(
                    "Unknown tool or no engine registered: {}",
                    tool_name
                )))
            }
        })
    }

    fn unequip(
        self: Rc<Self>,
        params: kernel::UnequipParams,
        _results: kernel::UnequipResults,
    ) -> Promise<(), capnp::Error> {
        let tool_name = pry!(pry!(pry!(params.get()).get_tool()).to_str()).to_owned();

        let state = self.state.borrow();
        let kernel = match state.kernels.get(&self.kernel_id) {
            Some(k) => k.kernel.clone(),
            None => return Promise::err(capnp::Error::failed("kernel not found".into())),
        };
        drop(state);

        Promise::from_future(async move {
            kernel.unequip(&tool_name).await;
            Ok(())
        })
    }

    // Lifecycle

    fn fork(
        self: Rc<Self>,
        _params: kernel::ForkParams,
        _results: kernel::ForkResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("fork not yet implemented".into()))
    }

    fn thread(
        self: Rc<Self>,
        _params: kernel::ThreadParams,
        _results: kernel::ThreadResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("thread not yet implemented".into()))
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
        let call = pry!(params.get());
        let call = pry!(call.get_call());
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
                    result.set_error(&format!("Tool not equipped: {}", tool_name));
                }
            }
            Ok(())
        })
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
            let tools = kernel.list_tools().await;
            let mut builder = results.get().init_schemas(tools.len() as u32);
            for (i, tool) in tools.iter().enumerate() {
                let mut s = builder.reborrow().get(i as u32);
                s.set_name(&tool.name);
                s.set_description(&tool.description);
                s.set_category(&tool.category);
                // TODO: Add input_schema when we add schema() method to ExecutionEngine
                s.set_input_schema("{}");
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
        let cell_id = pry!(pry!(pry!(params.get()).get_document_id()).to_str()).to_owned();

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
        let oplog_bytes = doc.doc.oplog_bytes();
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

            // Get LLM provider and kernel references
            let (provider, documents, kernel_arc) = {
                let state_ref = state.borrow();
                let provider = match &state_ref.llm_provider {
                    Some(p) => p.clone(),
                    None => {
                        log::error!("LLM provider not configured");
                        return Err(capnp::Error::failed("LLM provider not configured (missing ANTHROPIC_API_KEY)".into()));
                    }
                };
                let kernel_state = state_ref.kernels.get(&kernel_id)
                    .ok_or_else(|| {
                        log::error!("kernel {} not found", kernel_id);
                        capnp::Error::failed("kernel not found".into())
                    })?;
                log::debug!("Got provider and kernel state");

                (provider, kernel_state.documents.clone(), kernel_state.kernel.clone())
            };

            // Build tool definitions from equipped tools (async)
            let tools = build_tool_definitions(&kernel_arc).await;

            // Generate prompt ID
            let prompt_id = uuid::Uuid::new_v4().to_string();
            log::debug!("Generated prompt_id={}", prompt_id);

            // Auto-create document if it doesn't exist (client conversation ID)
            if documents.get(&cell_id).is_none() {
                log::info!("Auto-creating document {} for prompt", cell_id);
                documents.create_document(cell_id.clone(), DocumentKind::Conversation, None)
                    .map_err(|e| {
                        log::error!("Failed to create document: {}", e);
                        capnp::Error::failed(format!("failed to create document: {}", e))
                    })?;
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
            // User block broadcast happens via FlowBus (insert_block emits BlockFlow::Inserted)

            log::info!("User message block inserted, spawning LLM stream task");

            // Determine model name
            let default_model = provider.default_model();
            let model_name = model.as_deref().unwrap_or(default_model).to_string();
            log::info!("Using model: {} (requested: {:?}, default: {})",
                model_name, model, default_model);

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
            ));

            // Return immediately with prompt_id - streaming happens in background
            results.get().set_prompt_id(&prompt_id);
            log::debug!("prompt() returning immediately with prompt_id={}", prompt_id);
            Ok(())
        })
    }

    // =========================================================================
    // Context & Seat operations
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
                c.set_document_count(ctx.documents.len() as u32);
                c.set_seat_count(ctx.seats.len() as u32);
            }
        }
        Promise::ok(())
    }

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

        // Get username for the seat
        let nick = {
            let state_ref = state.borrow();
            state_ref.identity.username.clone()
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_millis() as u64;

        let seat_id = SeatId {
            nick: nick.clone(),
            instance: instance.clone(),
            kernel: kernel_id.clone(),
            context: context_name.clone(),
        };

        let seat_info = SeatInfo {
            id: seat_id.clone(),
            owner: nick.clone(),
            status: SeatStatus::Active,
            last_activity: now,
            cursor_block: None,
        };

        // Update state
        {
            let mut state_ref = state.borrow_mut();

            // Ensure context exists (create if not)
            if let Some(kernel) = state_ref.kernels.get_mut(&kernel_id) {
                kernel.contexts
                    .entry(context_name.clone())
                    .or_insert_with(|| ContextState::new(context_name.clone()))
                    .seats.push(seat_id.clone());
            }

            // Track seat in user's seats
            state_ref.my_seats.insert(seat_id.key(), seat_info.clone());
            state_ref.current_seat = Some(seat_id);
        }

        // Get main_document_id for this kernel
        let cell_id = {
            let state_ref = state.borrow();
            state_ref.kernels.get(&kernel_id)
                .map(|k| k.main_document_id.clone())
                .unwrap_or_else(|| format!("{}@main", kernel_id))
        };

        // Build result
        {
            let mut seat = results.get().init_seat();
            let mut id = seat.reborrow().init_id();
            id.set_nick(&seat_info.id.nick);
            id.set_instance(&seat_info.id.instance);
            id.set_kernel(&seat_info.id.kernel);
            id.set_context(&seat_info.id.context);

            seat.set_owner(&seat_info.owner);
            seat.set_status(crate::kaijutsu_capnp::SeatStatus::Active);
            seat.set_last_activity(seat_info.last_activity);
            seat.set_document_id(&cell_id);
        }

        Promise::ok(())
    }

    fn leave_seat(
        self: Rc<Self>,
        _params: kernel::LeaveSeatParams,
        _results: kernel::LeaveSeatResults,
    ) -> Promise<(), capnp::Error> {
        let mut state_ref = self.state.borrow_mut();
        let kernel_id = &self.kernel_id;

        if let Some(seat_id) = state_ref.current_seat.take() {
            // Remove from kernel's context
            if let Some(kernel) = state_ref.kernels.get_mut(kernel_id) {
                if let Some(context) = kernel.contexts.get_mut(&seat_id.context) {
                    context.seats.retain(|s| s != &seat_id);
                }
            }

            // Remove from user's seats
            state_ref.my_seats.remove(&seat_id.key());
        }

        Promise::ok(())
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

        let config = McpServerConfig {
            name: name.clone(),
            command,
            args,
            env,
            cwd,
        };

        let mcp_pool = self.state.borrow().mcp_pool.clone();
        let kernel_arc = self.state.borrow().kernels.get(&self.kernel_id)
            .map(|k| k.kernel.clone());

        Promise::from_future(async move {
            let info = mcp_pool.register(config).await
                .map_err(|e| capnp::Error::failed(format!("Failed to register MCP server: {}", e)))?;

            // Register MCP tools with the kernel if we have one
            if let Some(kernel) = kernel_arc {
                let tools = McpToolEngine::from_server_tools(mcp_pool.clone(), &name, &info.tools);
                for (qualified_name, engine) in tools {
                    let desc = engine.description().to_string();
                    kernel.register_tool_with_engine(
                        ToolInfo::new(&qualified_name, &desc, "mcp"),
                        engine,
                    ).await;
                    kernel.equip(&qualified_name).await;
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
            // Get tools before unregistering so we can unequip them
            if let Some(kernel) = kernel_arc {
                if let Ok(info) = mcp_pool.get_server_info(&name).await {
                    for tool in &info.tools {
                        let qualified_name = format!("{}:{}", name, tool.name);
                        kernel.unequip(&qualified_name).await;
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
        let call_reader = pry!(pry!(params.get()).get_call());
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

                    // Collect text content from the result
                    let content: String = r.content
                        .iter()
                        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                        .collect::<Vec<_>>()
                        .join("\n");
                    result_builder.set_content(&content);
                }
                Err(e) => {
                    result_builder.set_success(false);
                    result_builder.set_is_error(true);
                    result_builder.set_content(&e.to_string());
                }
            }

            Ok(())
        })
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

            // Auto-create document if it doesn't exist
            if documents.get(&cell_id).is_none() {
                log::info!("Auto-creating document {} for shell execute", cell_id);
                documents.create_document(cell_id.clone(), DocumentKind::Conversation, None)
                    .map_err(|e| capnp::Error::failed(format!("failed to create document: {}", e)))?;
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

            // Spawn execution in background
            let cell_id_clone = cell_id.clone();
            let output_block_id_clone = output_block_id.clone();
            let documents_clone = documents.clone();

            tokio::task::spawn_local(async move {
                // Execute via embedded kaish (routes through CRDT backend)
                log::info!("shell_execute: executing code via EmbeddedKaish: {:?}", code);
                match kaish.execute(&code).await {
                    Ok(result) => {
                        log::info!("shell_execute: kaish returned code={} out_len={} err_len={}",
                            result.code, result.out.len(), result.err.len());
                        log::debug!("shell_execute: out={:?} err={:?}", result.out, result.err);

                        // Convert kaish DisplayHint to kaijutsu format and serialize
                        let display_hint = crate::embedded_kaish::convert_display_hint(&result.hint);
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
                    }
                    Err(e) => {
                        let error_msg = format!("Error: {}", e);
                        log::error!("Shell execution failed: {}", e);
                        // Write error to output block
                        if let Err(e) = documents_clone.edit_text(&cell_id_clone, &output_block_id_clone, 0, &error_msg, 0) {
                            log::error!("Failed to update shell output with error: {}", e);
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
        })
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
                let hint = crate::embedded_kaish::convert_display_hint(&exec_result.hint);
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
        let serialized_ops: kaijutsu_crdt::SerializedOpsOwned = match serde_json::from_slice(&ops_data) {
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
}

// ============================================================================
// LLM Stream Helpers
// ============================================================================

use kaijutsu_kernel::llm::{ToolDefinition, ContentBlock};

/// Build tool definitions from equipped tools in the kernel.
async fn build_tool_definitions(kernel: &Arc<Kernel>) -> Vec<ToolDefinition> {
    let equipped = kernel.list_equipped().await;

    equipped
        .into_iter()
        .map(|info| {
            let input_schema = get_tool_schema(&info.name);
            ToolDefinition {
                name: info.name.clone(),
                description: info.description.clone(),
                input_schema,
            }
        })
        .collect()
}

/// Get JSON schema for a tool.
/// Schemas must be valid JSON Schema with `type: "object"` and `required` array.
fn get_tool_schema(tool_name: &str) -> serde_json::Value {
    match tool_name {
        "block_create" => serde_json::json!({
            "type": "object",
            "properties": {
                "role": {
                    "type": "string",
                    "enum": ["user", "model"],
                    "description": "Role of the block creator"
                },
                "kind": {
                    "type": "string",
                    "enum": ["text", "thinking", "tool_call", "tool_result"],
                    "description": "Type of block"
                },
                "content": {
                    "type": "string",
                    "description": "Initial content of the block"
                },
                "parent_id": {
                    "type": "string",
                    "description": "Parent block ID (optional)"
                }
            },
            "required": ["role", "kind", "content"]
        }),
        "block_append" => serde_json::json!({
            "type": "object",
            "properties": {
                "block_id": {
                    "type": "string",
                    "description": "Block ID to append to (format: cell_id:agent_id:seq)"
                },
                "text": {
                    "type": "string",
                    "description": "Text to append"
                }
            },
            "required": ["block_id", "text"]
        }),
        "block_edit" => serde_json::json!({
            "type": "object",
            "properties": {
                "block_id": {
                    "type": "string",
                    "description": "Block ID to edit (format: cell_id:agent_id:seq)"
                },
                "operations": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": { "type": "string", "enum": ["replace", "insert", "delete"], "description": "Operation type" },
                            "line": { "type": "integer", "description": "Line number (1-indexed)" },
                            "text": { "type": "string", "description": "Text for replace/insert" }
                        },
                        "required": ["op", "line"]
                    },
                    "description": "Edit operations to apply atomically"
                }
            },
            "required": ["block_id", "operations"]
        }),
        "block_read" => serde_json::json!({
            "type": "object",
            "properties": {
                "block_id": {
                    "type": "string",
                    "description": "Block ID to read (format: cell_id:agent_id:seq)"
                },
                "line_numbers": {
                    "type": "boolean",
                    "description": "Include line numbers in output (default: false)"
                }
            },
            "required": ["block_id"]
        }),
        "block_search" => serde_json::json!({
            "type": "object",
            "properties": {
                "block_id": {
                    "type": "string",
                    "description": "Block ID to search within"
                },
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for"
                }
            },
            "required": ["block_id", "pattern"]
        }),
        "block_list" => serde_json::json!({
            "type": "object",
            "properties": {
                "role": {
                    "type": "string",
                    "enum": ["user", "model"],
                    "description": "Filter by role"
                },
                "kind": {
                    "type": "string",
                    "enum": ["text", "thinking", "tool_call", "tool_result"],
                    "description": "Filter by kind"
                }
            },
            "required": []
        }),
        "block_status" => serde_json::json!({
            "type": "object",
            "properties": {
                "block_id": {
                    "type": "string",
                    "description": "Block ID"
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "streaming", "done", "error"],
                    "description": "New status"
                }
            },
            "required": ["block_id", "status"]
        }),
        "block_splice" => serde_json::json!({
            "type": "object",
            "properties": {
                "block_id": {
                    "type": "string",
                    "description": "Block ID to splice"
                },
                "pos": {
                    "type": "integer",
                    "description": "Character position (0-indexed)"
                },
                "delete": {
                    "type": "integer",
                    "description": "Number of characters to delete (default: 0)"
                },
                "insert": {
                    "type": "string",
                    "description": "Text to insert (default: empty)"
                }
            },
            "required": ["block_id", "pos"]
        }),
        "kernel_search" => serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Regex pattern to search across all blocks"
                },
                "cell_id": {
                    "type": "string",
                    "description": "Limit search to this cell"
                },
                "kind": {
                    "type": "string",
                    "enum": ["text", "thinking", "tool_call", "tool_result"],
                    "description": "Filter by block kind"
                },
                "role": {
                    "type": "string",
                    "enum": ["user", "model", "system", "tool"],
                    "description": "Filter by block role"
                },
                "context_lines": {
                    "type": "integer",
                    "description": "Lines of context around matches (default: 0)"
                },
                "max_matches": {
                    "type": "integer",
                    "description": "Maximum matches to return (default: 100)"
                }
            },
            "required": ["query"]
        }),
        // Default schema for unknown tools - minimal valid schema
        _ => serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
    }
}

/// Process LLM streaming in a background task with agentic loop.
/// This function handles all stream events, executes tools, and loops until done.
/// Block events are broadcast via FlowBus (BlockStore emits BlockFlow events).
///
/// `after_block_id` is the starting point for block ordering - all streaming blocks
/// will be inserted after this block (typically the user's message).
async fn process_llm_stream(
    provider: Arc<AnthropicProvider>,
    documents: SharedBlockStore,
    cell_id: String,
    content: String,
    model_name: String,
    kernel: Arc<Kernel>,
    tools: Vec<ToolDefinition>,
    after_block_id: kaijutsu_crdt::BlockId,
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
            .with_system("You are a helpful AI assistant in a collaborative coding environment called Kaijutsu. You have access to tools for manipulating blocks of content. Be concise and helpful.")
            .with_max_tokens(4096)
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
        // Track stop reason
        let mut stop_reason: Option<String> = None;

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

                StreamEvent::Done { stop_reason: sr, input_tokens, output_tokens } => {
                    log::info!(
                        "LLM stream completed: stop_reason={:?}, tokens_in={:?}, tokens_out={:?}",
                        sr, input_tokens, output_tokens
                    );
                    stop_reason = sr;
                }

                StreamEvent::Error(err) => {
                    log::error!("LLM stream error: {}", err);
                    let _ = documents.insert_block(&cell_id, None, Some(&last_block_id), Role::Model, BlockKind::Text, format!("❌ Error: {}", err));
                    return;
                }
            }
        }

        // Check if we need to execute tools
        if stop_reason.as_deref() != Some("tool_use") || tool_calls.is_empty() {
            log::info!("Agentic loop complete - no more tool calls (stop_reason={:?})", stop_reason);
            break;
        }

        // Execute tools and collect results
        log::info!("Executing {} tool calls", tool_calls.len());
        let mut tool_results: Vec<ContentBlock> = vec![];
        let mut assistant_tool_uses: Vec<ContentBlock> = vec![];

        for (tool_use_id, tool_name, input) in tool_calls {
            // Build tool uses for assistant message
            assistant_tool_uses.push(ContentBlock::ToolUse {
                id: tool_use_id.clone(),
                name: tool_name.clone(),
                input: input.clone(),
            });

            // Execute the tool
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

            // Insert tool result block after the tool call (FlowBus handles broadcasting)
            if let Some(tool_call_block_id) = tool_call_blocks.get(&tool_use_id) {
                match documents.insert_tool_result(&cell_id, tool_call_block_id, Some(tool_call_block_id), &result_content, is_error, None) {
                    Ok(block_id) => {
                        last_block_id = block_id;
                    }
                    Err(e) => log::error!("Failed to insert tool result block: {}", e),
                }
            }

            // Collect result for conversation
            tool_results.push(ContentBlock::ToolResult {
                tool_use_id,
                content: result_content,
                is_error,
            });
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
    })
}

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
