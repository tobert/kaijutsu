//! Cap'n Proto RPC client for kaijutsu
//!
//! Provides typed interface to the World, Room, and KaishKernel capabilities.

use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use futures::AsyncReadExt;
use russh::client::Msg;
use russh::ChannelStream;
use tokio_util::compat::TokioAsyncReadCompatExt;

use crate::kaijutsu_capnp::world;

/// RPC client wrapper
///
/// Holds the World capability bootstrapped from the server.
///
/// IMPORTANT: Must be created and used within a `tokio::task::LocalSet` context
/// because capnp-rpc's RpcSystem is not Send.
pub struct RpcClient {
    world: world::Client,
}

impl RpcClient {
    /// Initialize RPC over an SSH channel stream
    ///
    /// MUST be called within a `tokio::task::LocalSet::run_until()` context.
    pub async fn new(channel_stream: ChannelStream<Msg>) -> Result<Self, RpcError> {
        let compat_stream = TokioAsyncReadCompatExt::compat(channel_stream);
        Self::from_stream(compat_stream).await
    }

    /// Initialize RPC from any AsyncRead+AsyncWrite stream
    ///
    /// Useful for testing with Unix sockets or in-memory streams.
    pub async fn from_stream<S>(stream: S) -> Result<Self, RpcError>
    where
        S: futures::AsyncRead + futures::AsyncWrite + Unpin + 'static,
    {
        let (reader, writer) = stream.split();

        let rpc_network = Box::new(twoparty::VatNetwork::new(
            futures::io::BufReader::new(reader),
            futures::io::BufWriter::new(writer),
            rpc_twoparty_capnp::Side::Client,
            Default::default(),
        ));

        let mut rpc_system = RpcSystem::new(rpc_network, None);
        let world: world::Client = rpc_system.bootstrap(rpc_twoparty_capnp::Side::Server);

        // Spawn the RPC system to run in the background (requires LocalSet)
        tokio::task::spawn_local(rpc_system);

        Ok(Self { world })
    }

    /// Get current identity from the server
    pub async fn whoami(&self) -> Result<Identity, RpcError> {
        let request = self.world.whoami_request();
        let response = request.send().promise.await?;
        let identity = response.get()?.get_identity()?;

        Ok(Identity {
            username: identity.get_username()?.to_string()?,
            display_name: identity.get_display_name()?.to_string()?,
        })
    }

    /// List available rooms
    pub async fn list_rooms(&self) -> Result<Vec<RoomInfo>, RpcError> {
        let request = self.world.list_rooms_request();
        let response = request.send().promise.await?;
        let rooms = response.get()?.get_rooms()?;

        let mut result = Vec::with_capacity(rooms.len() as usize);
        for room in rooms.iter() {
            result.push(RoomInfo {
                id: room.get_id(),
                name: room.get_name()?.to_string()?,
                branch: room.get_branch()?.to_string()?,
                user_count: room.get_user_count(),
                agent_count: room.get_agent_count(),
            });
        }
        Ok(result)
    }

    /// Join a room by name
    pub async fn join_room(&self, name: &str) -> Result<RoomHandle, RpcError> {
        let mut request = self.world.join_room_request();
        request.get().set_name(name);
        let response = request.send().promise.await?;
        let room = response.get()?.get_room()?;

        Ok(RoomHandle { room })
    }

    /// Create a new room
    pub async fn create_room(&self, config: RoomConfig) -> Result<RoomHandle, RpcError> {
        let mut request = self.world.create_room_request();
        {
            let mut cfg = request.get().init_config();
            cfg.set_name(&config.name);
            if let Some(branch) = &config.branch {
                cfg.set_branch(branch);
            }
            let mut repos = cfg.init_repos(config.repos.len() as u32);
            for (i, repo) in config.repos.iter().enumerate() {
                let mut r = repos.reborrow().get(i as u32);
                r.set_name(&repo.name);
                r.set_url(&repo.url);
                r.set_writable(repo.writable);
            }
        }
        let response = request.send().promise.await?;
        let room = response.get()?.get_room()?;

        Ok(RoomHandle { room })
    }
}

// ============================================================================
// Types
// ============================================================================

#[derive(Debug, Clone)]
pub struct Identity {
    pub username: String,
    pub display_name: String,
}

#[derive(Debug, Clone)]
pub struct RoomInfo {
    pub id: u64,
    pub name: String,
    pub branch: String,
    pub user_count: u32,
    pub agent_count: u32,
}

#[derive(Debug, Clone)]
pub struct RoomConfig {
    pub name: String,
    pub branch: Option<String>,
    pub repos: Vec<RepoMount>,
}

#[derive(Debug, Clone)]
pub struct RepoMount {
    pub name: String,
    pub url: String,
    pub writable: bool,
}

/// Handle to a joined room
pub struct RoomHandle {
    room: crate::kaijutsu_capnp::room::Client,
}

impl RoomHandle {
    /// Get room info
    pub async fn get_info(&self) -> Result<RoomInfo, RpcError> {
        let request = self.room.get_info_request();
        let response = request.send().promise.await?;
        let info = response.get()?.get_info()?;
        Ok(RoomInfo {
            id: info.get_id(),
            name: info.get_name()?.to_string()?,
            branch: info.get_branch()?.to_string()?,
            user_count: info.get_user_count(),
            agent_count: info.get_agent_count(),
        })
    }

    /// Send a message to the room
    pub async fn send(&self, content: &str) -> Result<Row, RpcError> {
        let mut request = self.room.send_request();
        request.get().set_content(content);
        let response = request.send().promise.await?;
        let row = response.get()?.get_row()?;
        Ok(Row::from_capnp(&row)?)
    }

    /// Mention an agent
    pub async fn mention(&self, agent: &str, content: &str) -> Result<Row, RpcError> {
        let mut request = self.room.mention_request();
        request.get().set_agent(agent);
        request.get().set_content(content);
        let response = request.send().promise.await?;
        let row = response.get()?.get_row()?;
        Ok(Row::from_capnp(&row)?)
    }

    /// Get room history
    pub async fn get_history(&self, limit: u32, before_id: u64) -> Result<Vec<Row>, RpcError> {
        let mut request = self.room.get_history_request();
        request.get().set_limit(limit);
        request.get().set_before_id(before_id);
        let response = request.send().promise.await?;
        let rows = response.get()?.get_rows()?;

        let mut result = Vec::with_capacity(rows.len() as usize);
        for row in rows.iter() {
            result.push(Row::from_capnp(&row)?);
        }
        Ok(result)
    }

    /// Get the kaish kernel for this room
    pub async fn get_kernel(&self) -> Result<KernelHandle, RpcError> {
        let request = self.room.get_kernel_request();
        let response = request.send().promise.await?;
        let kernel = response.get()?.get_kernel()?;
        Ok(KernelHandle { kernel })
    }

    /// Leave the room
    pub async fn leave(self) -> Result<(), RpcError> {
        let request = self.room.leave_request();
        request.send().promise.await?;
        Ok(())
    }
}

/// Handle to a kaish kernel
pub struct KernelHandle {
    kernel: crate::kaijutsu_capnp::kaish_kernel::Client,
}

impl KernelHandle {
    /// Execute code in the kernel
    pub async fn execute(&self, code: &str) -> Result<u64, RpcError> {
        let mut request = self.kernel.execute_request();
        request.get().set_code(code);
        let response = request.send().promise.await?;
        Ok(response.get()?.get_exec_id())
    }

    /// Interrupt an execution
    pub async fn interrupt(&self, exec_id: u64) -> Result<(), RpcError> {
        let mut request = self.kernel.interrupt_request();
        request.get().set_exec_id(exec_id);
        request.send().promise.await?;
        Ok(())
    }

    /// Get completions
    pub async fn complete(&self, partial: &str, cursor: u32) -> Result<Vec<Completion>, RpcError> {
        let mut request = self.kernel.complete_request();
        request.get().set_partial(partial);
        request.get().set_cursor(cursor);
        let response = request.send().promise.await?;
        let completions = response.get()?.get_completions()?;

        let mut result = Vec::with_capacity(completions.len() as usize);
        for c in completions.iter() {
            result.push(Completion {
                text: c.get_text()?.to_string()?,
                display_text: c.get_display_text()?.to_string()?,
                kind: CompletionKind::from_capnp(c.get_kind()?),
            });
        }
        Ok(result)
    }
}

#[derive(Debug, Clone)]
pub struct Row {
    pub id: u64,
    pub parent_id: u64,
    pub row_type: RowType,
    pub sender: String,
    pub content: String,
    pub timestamp: u64,
}

impl Row {
    fn from_capnp(row: &crate::kaijutsu_capnp::row::Reader<'_>) -> Result<Self, RpcError> {
        Ok(Self {
            id: row.get_id(),
            parent_id: row.get_parent_id(),
            row_type: RowType::from_capnp(row.get_row_type()?),
            sender: row.get_sender()?.to_string()?,
            content: row.get_content()?.to_string()?,
            timestamp: row.get_timestamp(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowType {
    Chat,
    AgentResponse,
    ToolCall,
    ToolResult,
    SystemMessage,
}

impl RowType {
    fn from_capnp(rt: crate::kaijutsu_capnp::RowType) -> Self {
        match rt {
            crate::kaijutsu_capnp::RowType::Chat => RowType::Chat,
            crate::kaijutsu_capnp::RowType::AgentResponse => RowType::AgentResponse,
            crate::kaijutsu_capnp::RowType::ToolCall => RowType::ToolCall,
            crate::kaijutsu_capnp::RowType::ToolResult => RowType::ToolResult,
            crate::kaijutsu_capnp::RowType::SystemMessage => RowType::SystemMessage,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    pub display_text: String,
    pub kind: CompletionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Command,
    Path,
    Variable,
    Keyword,
}

impl CompletionKind {
    fn from_capnp(kind: crate::kaijutsu_capnp::CompletionKind) -> Self {
        match kind {
            crate::kaijutsu_capnp::CompletionKind::Command => CompletionKind::Command,
            crate::kaijutsu_capnp::CompletionKind::Path => CompletionKind::Path,
            crate::kaijutsu_capnp::CompletionKind::Variable => CompletionKind::Variable,
            crate::kaijutsu_capnp::CompletionKind::Keyword => CompletionKind::Keyword,
        }
    }
}

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("Cap'n Proto error: {0}")]
    Capnp(#[from] capnp::Error),
    #[error("Not in schema: {0}")]
    NotInSchema(#[from] capnp::NotInSchema),
    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("Not connected to server")]
    NotConnected,
    #[error("Capability no longer valid")]
    CapabilityLost,
    #[error("Server error: {0}")]
    ServerError(String),
}
