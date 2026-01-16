//! Cap'n Proto RPC server implementation
//!
//! Implements World, Room, and KaishKernel capabilities.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use capnp::capability::Promise;
use capnp_rpc::pry;

use crate::kaijutsu_capnp::*;

// ============================================================================
// Server State
// ============================================================================

/// Server state shared across all capabilities
pub struct ServerState {
    pub identity: Identity,
    pub rooms: HashMap<String, RoomState>,
    next_room_id: AtomicU64,
    next_row_id: AtomicU64,
}

impl ServerState {
    pub fn new(username: String) -> Self {
        Self {
            identity: Identity {
                username: username.clone(),
                display_name: username,
            },
            rooms: HashMap::new(),
            next_room_id: AtomicU64::new(1),
            next_row_id: AtomicU64::new(1),
        }
    }

    fn next_room_id(&self) -> u64 {
        self.next_room_id.fetch_add(1, Ordering::SeqCst)
    }

    fn next_row_id(&self) -> u64 {
        self.next_row_id.fetch_add(1, Ordering::SeqCst)
    }
}

#[derive(Clone)]
pub struct Identity {
    pub username: String,
    pub display_name: String,
}

pub struct RoomState {
    pub id: u64,
    pub name: String,
    pub branch: String,
    pub rows: Vec<RowData>,
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

    fn list_rooms(
        self: Rc<Self>,
        _params: world::ListRoomsParams,
        mut results: world::ListRoomsResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.borrow();
        let mut rooms = results.get().init_rooms(state.rooms.len() as u32);
        for (i, room) in state.rooms.values().enumerate() {
            let mut r = rooms.reborrow().get(i as u32);
            r.set_id(room.id);
            r.set_name(&room.name);
            r.set_branch(&room.branch);
            r.set_user_count(1);
            r.set_agent_count(0);
        }
        Promise::ok(())
    }

    fn join_room(
        self: Rc<Self>,
        params: world::JoinRoomParams,
        mut results: world::JoinRoomResults,
    ) -> Promise<(), capnp::Error> {
        let name = pry!(pry!(pry!(params.get()).get_name()).to_str()).to_owned();

        {
            let mut state = self.state.borrow_mut();
            if !state.rooms.contains_key(&name) {
                let id = state.next_room_id();
                state.rooms.insert(name.clone(), RoomState {
                    id,
                    name: name.clone(),
                    branch: "main".to_string(),
                    rows: Vec::new(),
                });
            }
        }

        let room_impl = RoomImpl::new(self.state.clone(), name);
        results.get().set_room(capnp_rpc::new_client(room_impl));
        Promise::ok(())
    }

    fn create_room(
        self: Rc<Self>,
        params: world::CreateRoomParams,
        mut results: world::CreateRoomResults,
    ) -> Promise<(), capnp::Error> {
        let config = pry!(pry!(params.get()).get_config());
        let name = pry!(pry!(config.get_name()).to_str()).to_owned();
        let branch = config.get_branch().ok()
            .and_then(|b| b.to_str().ok())
            .map(|s| s.to_owned())
            .unwrap_or_else(|| "main".to_string());

        {
            let mut state = self.state.borrow_mut();
            let id = state.next_room_id();
            state.rooms.insert(name.clone(), RoomState {
                id,
                name: name.clone(),
                branch,
                rows: Vec::new(),
            });
        }

        let room_impl = RoomImpl::new(self.state.clone(), name);
        results.get().set_room(capnp_rpc::new_client(room_impl));
        Promise::ok(())
    }
}

// ============================================================================
// Room Implementation
// ============================================================================

struct RoomImpl {
    state: Rc<RefCell<ServerState>>,
    room_name: String,
}

impl RoomImpl {
    fn new(state: Rc<RefCell<ServerState>>, room_name: String) -> Self {
        Self { state, room_name }
    }
}

impl room::Server for RoomImpl {
    fn get_info(
        self: Rc<Self>,
        _params: room::GetInfoParams,
        mut results: room::GetInfoResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.borrow();
        if let Some(room) = state.rooms.get(&self.room_name) {
            let mut info = results.get().init_info();
            info.set_id(room.id);
            info.set_name(&room.name);
            info.set_branch(&room.branch);
            info.set_user_count(1);
            info.set_agent_count(0);
        }
        Promise::ok(())
    }

    fn get_history(
        self: Rc<Self>,
        params: room::GetHistoryParams,
        mut results: room::GetHistoryResults,
    ) -> Promise<(), capnp::Error> {
        let params = pry!(params.get());
        let limit = params.get_limit() as usize;
        let before_id = params.get_before_id();

        let state = self.state.borrow();
        if let Some(room) = state.rooms.get(&self.room_name) {
            let rows: Vec<_> = room.rows.iter()
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
        params: room::SendParams,
        mut results: room::SendResults,
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
            if let Some(room) = state.rooms.get_mut(&self.room_name) {
                room.rows.push(row.clone());
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
        params: room::MentionParams,
        mut results: room::MentionResults,
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
            if let Some(room) = state.rooms.get_mut(&self.room_name) {
                room.rows.push(row.clone());
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
        _params: room::SubscribeParams,
        _results: room::SubscribeResults,
    ) -> Promise<(), capnp::Error> {
        Promise::ok(())
    }

    fn get_kernel(
        self: Rc<Self>,
        _params: room::GetKernelParams,
        mut results: room::GetKernelResults,
    ) -> Promise<(), capnp::Error> {
        let kernel_impl = KaishKernelImpl::new();
        results.get().set_kernel(capnp_rpc::new_client(kernel_impl));
        Promise::ok(())
    }

    fn leave(
        self: Rc<Self>,
        _params: room::LeaveParams,
        _results: room::LeaveResults,
    ) -> Promise<(), capnp::Error> {
        Promise::ok(())
    }

    fn fork(
        self: Rc<Self>,
        _params: room::ForkParams,
        _results: room::ForkResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("fork not yet implemented".into()))
    }

    fn list_equipment(
        self: Rc<Self>,
        _params: room::ListEquipmentParams,
        mut results: room::ListEquipmentResults,
    ) -> Promise<(), capnp::Error> {
        // Return empty tools list for now
        results.get().init_tools(0);
        Promise::ok(())
    }

    fn equip(
        self: Rc<Self>,
        _params: room::EquipParams,
        _results: room::EquipResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("equip not yet implemented".into()))
    }

    fn unequip(
        self: Rc<Self>,
        _params: room::UnequipParams,
        _results: room::UnequipResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("unequip not yet implemented".into()))
    }
}

// ============================================================================
// KaishKernel Implementation
// ============================================================================

struct KaishKernelImpl {
    next_exec_id: AtomicU64,
}

impl KaishKernelImpl {
    fn new() -> Self {
        Self {
            next_exec_id: AtomicU64::new(1),
        }
    }
}

impl kaish_kernel::Server for KaishKernelImpl {
    fn execute(
        self: Rc<Self>,
        params: kaish_kernel::ExecuteParams,
        mut results: kaish_kernel::ExecuteResults,
    ) -> Promise<(), capnp::Error> {
        let _code = pry!(pry!(pry!(params.get()).get_code()).to_str()).to_owned();
        let exec_id = self.next_exec_id.fetch_add(1, Ordering::SeqCst);
        results.get().set_exec_id(exec_id);
        Promise::ok(())
    }

    fn interrupt(
        self: Rc<Self>,
        _params: kaish_kernel::InterruptParams,
        _results: kaish_kernel::InterruptResults,
    ) -> Promise<(), capnp::Error> {
        Promise::ok(())
    }

    fn complete(
        self: Rc<Self>,
        _params: kaish_kernel::CompleteParams,
        mut results: kaish_kernel::CompleteResults,
    ) -> Promise<(), capnp::Error> {
        results.get().init_completions(0);
        Promise::ok(())
    }

    fn subscribe(
        self: Rc<Self>,
        _params: kaish_kernel::SubscribeParams,
        _results: kaish_kernel::SubscribeResults,
    ) -> Promise<(), capnp::Error> {
        Promise::ok(())
    }

    fn get_history(
        self: Rc<Self>,
        _params: kaish_kernel::GetHistoryParams,
        mut results: kaish_kernel::GetHistoryResults,
    ) -> Promise<(), capnp::Error> {
        results.get().init_entries(0);
        Promise::ok(())
    }
}
