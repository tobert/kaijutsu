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
use crate::kaish::KaishProcess;

use kaijutsu_kernel::{
    CellDb, CellEditEngine, CellKind, CellListEngine, CellReadEngine, CellStore, Kernel,
    LocalBackend, ToolInfo, VfsOps,
};
use std::sync::Mutex as StdMutex;

// ============================================================================
// Server State
// ============================================================================

/// Register cell editing tools with a kernel.
async fn register_cell_tools(kernel: &Arc<Kernel>, cells: Arc<StdMutex<CellStore>>) {
    // Register cell.edit tool
    kernel
        .register_tool_with_engine(
            ToolInfo::new("cell.edit", "Line-based cell editing", "cell"),
            Arc::new(CellEditEngine::new(cells.clone(), "server")),
        )
        .await;
    kernel.equip("cell.edit").await;

    // Register cell.read tool
    kernel
        .register_tool_with_engine(
            ToolInfo::new("cell.read", "Read cell content by ID", "cell"),
            Arc::new(CellReadEngine::new(cells.clone())),
        )
        .await;
    kernel.equip("cell.read").await;

    // Register cell.list tool
    kernel
        .register_tool_with_engine(
            ToolInfo::new("cell.list", "List all cells with metadata", "cell"),
            Arc::new(CellListEngine::new(cells)),
        )
        .await;
    kernel.equip("cell.list").await;
}

/// Server state shared across all capabilities
pub struct ServerState {
    pub identity: Identity,
    pub kernels: HashMap<String, KernelState>,
    next_kernel_id: AtomicU64,
    next_row_id: AtomicU64,
    next_exec_id: AtomicU64,
}

impl ServerState {
    pub fn new(username: String) -> Self {
        Self {
            identity: Identity {
                username: username.clone(),
                display_name: username,
            },
            kernels: HashMap::new(),
            next_kernel_id: AtomicU64::new(1),
            next_row_id: AtomicU64::new(1),
            next_exec_id: AtomicU64::new(1),
        }
    }

    fn next_kernel_id(&self) -> String {
        format!("kernel-{}", self.next_kernel_id.fetch_add(1, Ordering::SeqCst))
    }

    fn next_row_id(&self) -> u64 {
        self.next_row_id.fetch_add(1, Ordering::SeqCst)
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

/// Open or create a CellStore with database persistence for a kernel.
fn create_cell_store_with_db(kernel_id: &str) -> CellStore {
    let db_path = kernel_data_dir(kernel_id).join("data.db");
    match CellDb::open(&db_path) {
        Ok(db) => {
            log::info!("Opened cell database at {:?}", db_path);
            let mut store = CellStore::with_db(db);
            if let Err(e) = store.load_from_db() {
                log::warn!("Failed to load cells from DB: {}", e);
            } else {
                let count = store.iter().count();
                log::info!("Loaded {} cells from database", count);
            }
            store
        }
        Err(e) => {
            log::warn!("Failed to open cell database at {:?}: {}, using in-memory", db_path, e);
            CellStore::new()
        }
    }
}

pub struct KernelState {
    pub id: String,
    pub name: String,
    pub consent_mode: ConsentMode,
    pub rows: Vec<RowData>,
    pub command_history: Vec<CommandEntry>,
    /// Kaish subprocess for execution (spawned lazily)
    pub kaish: Option<Rc<KaishProcess>>,
    /// The kernel (VFS, state, tools, control plane)
    pub kernel: Arc<Kernel>,
    /// Cell CRDT store (wrapped for sharing with tools)
    pub cells: Arc<StdMutex<CellStore>>,
    /// Subscribers for cell update events
    pub cell_subscribers: Vec<crate::kaijutsu_capnp::cell_events::Client>,
}

#[derive(Clone, Copy)]
pub enum ConsentMode {
    Collaborative,
    Autonomous,
}

#[derive(Clone)]
pub struct RowData {
    pub id: u64,
    pub parent_id: u64,
    pub row_type: RowType,
    pub sender: String,
    pub content: String,
    pub timestamp: u64,
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
                // Create the kaijutsu kernel with default mounts
                let kernel = Kernel::new(&id).await;

                // Mount user's home directory at /home (read-only for now)
                if let Some(home) = dirs::home_dir() {
                    kernel
                        .mount("/home", LocalBackend::read_only(home))
                        .await;
                }

                // Create cell store with database persistence
                let mut cells = create_cell_store_with_db(&id);

                // Create default cell if none exist
                if cells.iter().next().is_none() {
                    let default_id = uuid::Uuid::new_v4().to_string();
                    log::info!("Creating default cell {} for kernel {}", default_id, id);
                    if let Err(e) = cells.create_cell(default_id.clone(), CellKind::Code, Some("rust".into())) {
                        log::warn!("Failed to create default cell: {}", e);
                    }
                }

                // Wrap cells in Arc<Mutex> for sharing with tools
                let cells = Arc::new(StdMutex::new(cells));

                // Register cell tools
                let kernel_arc = Arc::new(kernel);
                register_cell_tools(&kernel_arc, cells.clone()).await;

                let mut state_ref = state.borrow_mut();
                state_ref.kernels.insert(
                    id.clone(),
                    KernelState {
                        id: id.clone(),
                        name: id.clone(),
                        consent_mode: ConsentMode::Collaborative,
                        rows: Vec::new(),
                        command_history: Vec::new(),
                        kaish: None, // Spawned lazily
                        kernel: kernel_arc,
                        cells,
                        cell_subscribers: Vec::new(),
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

            // Create the kaijutsu kernel with default mounts
            let kernel = Kernel::new(&name).await;

            // Mount user's home directory at /home (read-only for now)
            if let Some(home) = dirs::home_dir() {
                kernel
                    .mount("/home", LocalBackend::read_only(home))
                    .await;
            }

            // Create cell store with database persistence
            let mut cells = create_cell_store_with_db(&id);

            // Create default cell if none exist
            if cells.iter().next().is_none() {
                let default_id = uuid::Uuid::new_v4().to_string();
                log::info!("Creating default cell {} for new kernel {}", default_id, id);
                if let Err(e) = cells.create_cell(default_id.clone(), CellKind::Code, Some("rust".into())) {
                    log::warn!("Failed to create default cell: {}", e);
                }
            }

            // Wrap cells in Arc<Mutex> for sharing with tools
            let cells = Arc::new(StdMutex::new(cells));

            // Register cell tools
            let kernel_arc = Arc::new(kernel);
            register_cell_tools(&kernel_arc, cells.clone()).await;

            {
                let mut state_ref = state.borrow_mut();
                state_ref.kernels.insert(
                    id.clone(),
                    KernelState {
                        id: id.clone(),
                        name,
                        consent_mode,
                        rows: Vec::new(),
                        command_history: Vec::new(),
                        kaish: None, // Spawned lazily
                        kernel: kernel_arc,
                        cells,
                        cell_subscribers: Vec::new(),
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

    fn get_history(
        self: Rc<Self>,
        params: kernel::GetHistoryParams,
        mut results: kernel::GetHistoryResults,
    ) -> Promise<(), capnp::Error> {
        let params = pry!(params.get());
        let limit = params.get_limit() as usize;
        let before_id = params.get_before_id();

        let state = self.state.borrow();
        if let Some(kernel) = state.kernels.get(&self.kernel_id) {
            let rows: Vec<_> = kernel.rows.iter()
                .filter(|r| before_id == 0 || r.id < before_id)
                .take(limit)
                .collect();

            let mut result_rows = results.get().init_rows(rows.len() as u32);
            for (i, row) in rows.iter().enumerate() {
                let mut r = result_rows.reborrow().get(i as u32);
                r.set_id(row.id);
                r.set_parent_id(row.parent_id);
                r.set_row_type(row.row_type);
                r.set_sender(&row.sender);
                r.set_content(&row.content);
                r.set_timestamp(row.timestamp);
            }
        }
        Promise::ok(())
    }

    fn send(
        self: Rc<Self>,
        params: kernel::SendParams,
        mut results: kernel::SendResults,
    ) -> Promise<(), capnp::Error> {
        let content = pry!(pry!(pry!(params.get()).get_content()).to_str()).to_owned();

        let row = {
            let mut state = self.state.borrow_mut();
            let id = state.next_row_id();
            let sender = state.identity.username.clone();
            let row = RowData {
                id,
                parent_id: 0,
                row_type: RowType::Chat,
                sender,
                content,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            };
            if let Some(kernel) = state.kernels.get_mut(&self.kernel_id) {
                kernel.rows.push(row.clone());
            }
            row
        };

        let mut r = results.get().init_row();
        r.set_id(row.id);
        r.set_parent_id(row.parent_id);
        r.set_row_type(row.row_type);
        r.set_sender(&row.sender);
        r.set_content(&row.content);
        r.set_timestamp(row.timestamp);
        Promise::ok(())
    }

    fn mention(
        self: Rc<Self>,
        params: kernel::MentionParams,
        mut results: kernel::MentionResults,
    ) -> Promise<(), capnp::Error> {
        let params = pry!(params.get());
        let agent = pry!(pry!(params.get_agent()).to_str()).to_owned();
        let content = pry!(pry!(params.get_content()).to_str()).to_owned();
        let full_content = format!("@{} {}", agent, content);

        let row = {
            let mut state = self.state.borrow_mut();
            let id = state.next_row_id();
            let sender = state.identity.username.clone();
            let row = RowData {
                id,
                parent_id: 0,
                row_type: RowType::Chat,
                sender,
                content: full_content,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            };
            if let Some(kernel) = state.kernels.get_mut(&self.kernel_id) {
                kernel.rows.push(row.clone());
            }
            row
        };

        let mut r = results.get().init_row();
        r.set_id(row.id);
        r.set_parent_id(row.parent_id);
        r.set_row_type(row.row_type);
        r.set_sender(&row.sender);
        r.set_content(&row.content);
        r.set_timestamp(row.timestamp);
        Promise::ok(())
    }

    fn subscribe(
        self: Rc<Self>,
        _params: kernel::SubscribeParams,
        _results: kernel::SubscribeResults,
    ) -> Promise<(), capnp::Error> {
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
            // Get or spawn kaish process
            let kaish = {
                let mut state_ref = state.borrow_mut();
                let kernel = state_ref.kernels.get_mut(&kernel_id)
                    .ok_or_else(|| capnp::Error::failed("kernel not found".into()))?;

                if kernel.kaish.is_none() {
                    log::info!("Spawning kaish subprocess for kernel {}", kernel_id);
                    match KaishProcess::spawn(&kernel_id).await {
                        Ok(process) => {
                            kernel.kaish = Some(Rc::new(process));
                        }
                        Err(e) => {
                            log::error!("Failed to spawn kaish: {}", e);
                            return Err(capnp::Error::failed(format!("kaish spawn failed: {}", e)));
                        }
                    }
                }

                kernel.kaish.as_ref().unwrap().clone()
            };

            // Execute code via kaish subprocess
            let exec_result = match kaish.execute(&code).await {
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
                let sender = state_ref.identity.username.clone();
                let row_id = state_ref.next_row_id();
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();

                if let Some(kernel) = state_ref.kernels.get_mut(&kernel_id) {
                    // Record command history
                    kernel.command_history.push(CommandEntry {
                        id: exec_id,
                        code: code.clone(),
                        timestamp,
                    });

                    // Always add a tool result row (even if empty, so history fetch works)
                    let content = if !exec_result.ok() {
                        format!("Error: {}", exec_result.err)
                    } else {
                        exec_result.out.clone() // May be empty string
                    };
                    kernel.rows.push(RowData {
                        id: row_id,
                        parent_id: 0,
                        row_type: RowType::ToolResult,
                        sender,
                        content,
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

    // Cell CRDT methods

    fn list_cells(
        self: Rc<Self>,
        _params: kernel::ListCellsParams,
        mut results: kernel::ListCellsResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.borrow();
        if let Some(kernel) = state.kernels.get(&self.kernel_id) {
            let cells_guard = kernel.cells.lock().unwrap();
            let cells: Vec<_> = cells_guard.iter().collect();
            let mut builder = results.get().init_cells(cells.len() as u32);
            for (i, doc) in cells.iter().enumerate() {
                let mut c = builder.reborrow().get(i as u32);
                c.set_id(&doc.id);
                c.set_kind(cell_kind_to_capnp(doc.kind));
                if let Some(ref lang) = doc.language {
                    c.set_language(lang);
                }
            }
        }
        Promise::ok(())
    }

    fn get_cell(
        self: Rc<Self>,
        params: kernel::GetCellParams,
        mut results: kernel::GetCellResults,
    ) -> Promise<(), capnp::Error> {
        let cell_id = pry!(pry!(pry!(params.get()).get_cell_id()).to_str()).to_owned();

        let state = self.state.borrow();
        if let Some(kernel) = state.kernels.get(&self.kernel_id) {
            let cells_guard = kernel.cells.lock().unwrap();
            if let Some(doc) = cells_guard.get(&cell_id) {
                let mut cell = results.get().init_cell();
                let mut info = cell.reborrow().init_info();
                info.set_id(&doc.id);
                info.set_kind(cell_kind_to_capnp(doc.kind));
                if let Some(ref lang) = doc.language {
                    info.set_language(lang);
                }
                cell.set_content(&doc.content());
                cell.set_version(doc.frontier_version());
                // Encode full doc for sync
                let encoded = doc.encode_full();
                cell.set_encoded_doc(&encoded);
            }
        }
        Promise::ok(())
    }

    fn create_cell(
        self: Rc<Self>,
        params: kernel::CreateCellParams,
        mut results: kernel::CreateCellResults,
    ) -> Promise<(), capnp::Error> {
        let params = pry!(params.get());
        let kind = cell_kind_from_capnp(params.get_kind().unwrap_or(crate::kaijutsu_capnp::CellKind::Code));
        let language = params.get_language().ok().and_then(|l| l.to_str().ok()).map(|s| s.to_owned());
        let _parent_id = params.get_parent_id().ok().and_then(|p| p.to_str().ok()).map(|s| s.to_owned());

        // Generate a new cell ID
        let cell_id = uuid::Uuid::new_v4().to_string();

        // Collect data for notification before releasing the borrow
        let cell_data: Option<(String, kaijutsu_kernel::CellKind, Option<String>, String, u64, Vec<u8>)>;
        let subscribers: Vec<crate::kaijutsu_capnp::cell_events::Client>;

        {
            let state = self.state.borrow();
            if let Some(kernel) = state.kernels.get(&self.kernel_id) {
                let mut cells_guard = kernel.cells.lock().unwrap();
                if let Ok(doc) = cells_guard.create_cell(cell_id, kind, language.clone()) {
                    // Build results
                    let mut cell = results.get().init_cell();
                    let mut info = cell.reborrow().init_info();
                    info.set_id(&doc.id);
                    info.set_kind(cell_kind_to_capnp(doc.kind));
                    if let Some(ref lang) = doc.language {
                        info.set_language(lang);
                    }
                    cell.set_content(&doc.content());
                    cell.set_version(doc.frontier_version());

                    // Collect data for notification
                    cell_data = Some((
                        doc.id.clone(),
                        doc.kind,
                        doc.language.clone(),
                        doc.content(),
                        doc.frontier_version(),
                        doc.encode_full(),
                    ));
                } else {
                    cell_data = None;
                }
                subscribers = kernel.cell_subscribers.clone();
            } else {
                cell_data = None;
                subscribers = Vec::new();
            }
        }

        // Notify subscribers (outside of borrow scope)
        if let Some((id, cell_kind, lang, content, version, encoded_doc)) = cell_data {
            for subscriber in subscribers {
                let mut req = subscriber.on_cell_created_request();
                {
                    let mut cell = req.get().init_cell();
                    let mut info = cell.reborrow().init_info();
                    info.set_id(&id);
                    info.set_kind(cell_kind_to_capnp(cell_kind));
                    if let Some(ref l) = lang {
                        info.set_language(l);
                    }
                    cell.set_content(&content);
                    cell.set_version(version);
                    cell.set_encoded_doc(&encoded_doc);
                }
                let _ = req.send(); // Fire and forget
            }
        }

        Promise::ok(())
    }

    fn delete_cell(
        self: Rc<Self>,
        params: kernel::DeleteCellParams,
        _results: kernel::DeleteCellResults,
    ) -> Promise<(), capnp::Error> {
        let cell_id = pry!(pry!(pry!(params.get()).get_cell_id()).to_str()).to_owned();

        let subscribers: Vec<crate::kaijutsu_capnp::cell_events::Client>;
        let deleted: bool;

        {
            let state = self.state.borrow();
            if let Some(kernel) = state.kernels.get(&self.kernel_id) {
                let mut cells_guard = kernel.cells.lock().unwrap();
                deleted = cells_guard.delete_cell(&cell_id).is_ok();
                subscribers = kernel.cell_subscribers.clone();
            } else {
                deleted = false;
                subscribers = Vec::new();
            }
        }

        // Notify subscribers
        if deleted {
            for subscriber in subscribers {
                let mut req = subscriber.on_cell_deleted_request();
                req.get().set_cell_id(&cell_id);
                let _ = req.send(); // Fire and forget
            }
        }

        Promise::ok(())
    }

    fn apply_op(
        self: Rc<Self>,
        params: kernel::ApplyOpParams,
        mut results: kernel::ApplyOpResults,
    ) -> Promise<(), capnp::Error> {
        let params = pry!(params.get());
        let op = pry!(params.get_op());
        let cell_id = pry!(pry!(op.get_cell_id()).to_str()).to_owned();
        let crdt_op = pry!(op.get_op());

        // Use a fixed agent name for server-side ops
        let agent_name = "server";

        // Data for persistence and notification
        let subscribers: Vec<crate::kaijutsu_capnp::cell_events::Client>;
        // (from_version, to_version, delta)
        let patch_data: Option<(u64, u64, Vec<u8>)>;

        {
            let state = self.state.borrow();
            if let Some(kernel) = state.kernels.get(&self.kernel_id) {
                let mut cells_guard = kernel.cells.lock().unwrap();
                subscribers = kernel.cell_subscribers.clone();

                // Apply operation and collect data for persistence (avoiding borrow conflict)
                patch_data = {
                    if let Some(doc) = cells_guard.get_mut(&cell_id) {
                        // Capture version before operation for delta encoding
                        let version_before = doc.frontier();
                        let version_before_u64 = doc.frontier_version();

                        // Apply the operation
                        match crdt_op.which() {
                            Ok(crate::kaijutsu_capnp::crdt_op::Insert(insert)) => {
                                let pos = insert.get_pos() as usize;
                                let text = pry!(pry!(insert.get_text()).to_str());
                                doc.insert(agent_name, pos, text);
                            }
                            Ok(crate::kaijutsu_capnp::crdt_op::Delete(delete)) => {
                                let pos = delete.get_pos() as usize;
                                let len = delete.get_len() as usize;
                                doc.delete(agent_name, pos, pos + len);
                            }
                            Ok(crate::kaijutsu_capnp::crdt_op::FullState(data)) => {
                                // Merge full state
                                let data = pry!(data);
                                let _ = doc.merge(&data);
                            }
                            Err(_) => {}
                        }

                        let new_version = doc.frontier_version();
                        results.get().set_new_version(new_version);

                        // Collect delta for persistence and notification
                        let delta = doc.encode_patch_from(&version_before);
                        if !delta.is_empty() {
                            Some((version_before_u64, new_version, delta))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };

                // Persist outside the doc borrow scope
                if let Some((_, _, ref delta)) = patch_data {
                    if let Err(e) = cells_guard.persist_op(&cell_id, agent_name, delta) {
                        log::warn!("Failed to persist cell op: {}", e);
                    }
                    // Auto-snapshot if enough ops have accumulated
                    let _ = cells_guard.maybe_snapshot(&cell_id);
                }
            } else {
                subscribers = Vec::new();
                patch_data = None;
            }
        }

        // Notify subscribers (outside of borrow scope)
        if let Some((from_version, to_version, delta)) = patch_data {
            for subscriber in subscribers {
                let mut req = subscriber.on_cell_updated_request();
                {
                    let mut patch = req.get().init_patch();
                    patch.set_cell_id(&cell_id);
                    patch.set_from_version(from_version);
                    patch.set_to_version(to_version);
                    patch.set_ops(&delta);
                }
                let _ = req.send(); // Fire and forget
            }
        }

        Promise::ok(())
    }

    fn subscribe_cells(
        self: Rc<Self>,
        params: kernel::SubscribeCellsParams,
        _results: kernel::SubscribeCellsResults,
    ) -> Promise<(), capnp::Error> {
        let callback = pry!(pry!(params.get()).get_callback());

        let mut state = self.state.borrow_mut();
        if let Some(kernel) = state.kernels.get_mut(&self.kernel_id) {
            kernel.cell_subscribers.push(callback);
            log::debug!(
                "Added cell subscriber for kernel {} (total: {})",
                self.kernel_id,
                kernel.cell_subscribers.len()
            );
        }
        Promise::ok(())
    }

    fn sync_cells(
        self: Rc<Self>,
        params: kernel::SyncCellsParams,
        mut results: kernel::SyncCellsResults,
    ) -> Promise<(), capnp::Error> {
        let params = pry!(params.get());
        let from_versions = pry!(params.get_from_versions());

        let state = self.state.borrow();
        if let Some(kernel) = state.kernels.get(&self.kernel_id) {
            // Build map of client versions
            let mut client_versions: HashMap<String, u64> = HashMap::new();
            for i in 0..from_versions.len() {
                let v = from_versions.get(i);
                if let Some(id) = v.get_cell_id().ok().and_then(|t| t.to_str().ok().map(|s| s.to_owned())) {
                    client_versions.insert(id, v.get_version());
                }
            }

            // Collect data from cells (must clone/copy since we hold a mutex guard)
            let cells_guard = kernel.cells.lock().unwrap();

            // Find cells that need patches and new cells
            let mut patches: Vec<(String, Vec<u8>, u64, u64)> = Vec::new();
            // Store owned data for new cells: (id, kind, language, content, version, encoded_doc)
            let mut new_cells: Vec<(String, kaijutsu_kernel::CellKind, Option<String>, String, u64, Vec<u8>)> = Vec::new();

            for doc in cells_guard.iter() {
                if let Some(&client_version) = client_versions.get(&doc.id) {
                    // Client has this cell - send patch if needed
                    let server_version = doc.frontier_version();
                    if client_version < server_version {
                        let patch = doc.encode_patch_from(&[client_version as usize]);
                        patches.push((doc.id.clone(), patch, client_version, server_version));
                    }
                } else {
                    // Client doesn't have this cell - send full state (clone all needed data)
                    new_cells.push((
                        doc.id.clone(),
                        doc.kind,
                        doc.language.clone(),
                        doc.content(),
                        doc.frontier_version(),
                        doc.encode_full(),
                    ));
                }
            }
            drop(cells_guard); // Explicitly release the lock

            // Build response
            let mut patches_builder = results.get().init_patches(patches.len() as u32);
            for (i, (cell_id, ops, from_v, to_v)) in patches.iter().enumerate() {
                let mut p = patches_builder.reborrow().get(i as u32);
                p.set_cell_id(cell_id);
                p.set_from_version(*from_v);
                p.set_to_version(*to_v);
                p.set_ops(ops);
            }

            let mut new_cells_builder = results.get().init_new_cells(new_cells.len() as u32);
            for (i, (id, kind, language, content, version, encoded_doc)) in new_cells.iter().enumerate() {
                let mut c = new_cells_builder.reborrow().get(i as u32);
                let mut info = c.reborrow().init_info();
                info.set_id(id);
                info.set_kind(cell_kind_to_capnp(*kind));
                if let Some(lang) = language {
                    info.set_language(lang);
                }
                c.set_content(content);
                c.set_version(*version);
                c.set_encoded_doc(encoded_doc);
            }
        }
        Promise::ok(())
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
}

/// Convert kernel CellKind to capnp CellKind
fn cell_kind_to_capnp(kind: kaijutsu_kernel::CellKind) -> crate::kaijutsu_capnp::CellKind {
    match kind {
        kaijutsu_kernel::CellKind::Code => crate::kaijutsu_capnp::CellKind::Code,
        kaijutsu_kernel::CellKind::Markdown => crate::kaijutsu_capnp::CellKind::Markdown,
        kaijutsu_kernel::CellKind::Output => crate::kaijutsu_capnp::CellKind::Output,
        kaijutsu_kernel::CellKind::System => crate::kaijutsu_capnp::CellKind::System,
        kaijutsu_kernel::CellKind::UserMessage => crate::kaijutsu_capnp::CellKind::UserMessage,
        kaijutsu_kernel::CellKind::AgentMessage => crate::kaijutsu_capnp::CellKind::AgentMessage,
    }
}

/// Convert capnp CellKind to kernel CellKind
fn cell_kind_from_capnp(kind: crate::kaijutsu_capnp::CellKind) -> kaijutsu_kernel::CellKind {
    match kind {
        crate::kaijutsu_capnp::CellKind::Code => kaijutsu_kernel::CellKind::Code,
        crate::kaijutsu_capnp::CellKind::Markdown => kaijutsu_kernel::CellKind::Markdown,
        crate::kaijutsu_capnp::CellKind::Output => kaijutsu_kernel::CellKind::Output,
        crate::kaijutsu_capnp::CellKind::System => kaijutsu_kernel::CellKind::System,
        crate::kaijutsu_capnp::CellKind::UserMessage => kaijutsu_kernel::CellKind::UserMessage,
        crate::kaijutsu_capnp::CellKind::AgentMessage => kaijutsu_kernel::CellKind::AgentMessage,
    }
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
}
