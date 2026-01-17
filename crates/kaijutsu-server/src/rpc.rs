//! Cap'n Proto RPC server implementation
//!
//! Implements World and Kernel capabilities.
//! Each kernel spawns a kaish subprocess for code execution.

#![allow(refining_impl_trait)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use capnp::capability::Promise;
use capnp_rpc::pry;

use crate::kaijutsu_capnp::*;
use crate::kaish::KaishProcess;

// ============================================================================
// Server State
// ============================================================================

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

pub struct KernelState {
    pub id: String,
    pub name: String,
    pub consent_mode: ConsentMode,
    pub rows: Vec<RowData>,
    pub command_history: Vec<CommandEntry>,
    /// Kaish subprocess for execution (spawned lazily)
    pub kaish: Option<Rc<KaishProcess>>,
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

        // Create kernel entry if it doesn't exist (kaish spawned lazily on first execute)
        {
            let mut state = self.state.borrow_mut();
            if !state.kernels.contains_key(&id) {
                state.kernels.insert(id.clone(), KernelState {
                    id: id.clone(),
                    name: id.clone(),
                    consent_mode: ConsentMode::Collaborative,
                    rows: Vec::new(),
                    command_history: Vec::new(),
                    kaish: None, // Spawned lazily
                });
            }
        }

        let kernel_impl = KernelImpl::new(self.state.clone(), id);
        results.get().set_kernel(capnp_rpc::new_client(kernel_impl));
        Promise::ok(())
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

        let id = {
            let mut state = self.state.borrow_mut();
            let id = state.next_kernel_id();
            state.kernels.insert(id.clone(), KernelState {
                id: id.clone(),
                name,
                consent_mode,
                rows: Vec::new(),
                command_history: Vec::new(),
                kaish: None, // Spawned lazily
            });
            id
        };

        let kernel_impl = KernelImpl::new(self.state.clone(), id);
        results.get().set_kernel(capnp_rpc::new_client(kernel_impl));
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

                    // Add output as a tool result row
                    if !exec_result.out.is_empty() || !exec_result.err.is_empty() {
                        let content = if exec_result.ok() {
                            exec_result.out.clone()
                        } else {
                            format!("Error: {}", exec_result.err)
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
        results.get().init_tools(0);
        Promise::ok(())
    }

    fn equip(
        self: Rc<Self>,
        _params: kernel::EquipParams,
        _results: kernel::EquipResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("equip not yet implemented".into()))
    }

    fn unequip(
        self: Rc<Self>,
        _params: kernel::UnequipParams,
        _results: kernel::UnequipResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("unequip not yet implemented".into()))
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
}
