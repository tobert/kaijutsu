//! Cap'n Proto RPC client for kaijutsu
//!
//! Provides typed interface to the World and Kernel capabilities.

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

    /// List available kernels
    pub async fn list_kernels(&self) -> Result<Vec<KernelInfo>, RpcError> {
        let request = self.world.list_kernels_request();
        let response = request.send().promise.await?;
        let kernels = response.get()?.get_kernels()?;

        let mut result = Vec::with_capacity(kernels.len() as usize);
        for kernel in kernels.iter() {
            result.push(KernelInfo {
                id: kernel.get_id()?.to_string()?,
                name: kernel.get_name()?.to_string()?,
                user_count: kernel.get_user_count(),
                agent_count: kernel.get_agent_count(),
            });
        }
        Ok(result)
    }

    /// Attach to a kernel by ID
    pub async fn attach_kernel(&self, id: &str) -> Result<KernelHandle, RpcError> {
        let mut request = self.world.attach_kernel_request();
        request.get().set_id(id);
        let response = request.send().promise.await?;
        let kernel = response.get()?.get_kernel()?;

        Ok(KernelHandle { kernel })
    }

    /// Create a new kernel
    pub async fn create_kernel(&self, config: KernelConfig) -> Result<KernelHandle, RpcError> {
        let mut request = self.world.create_kernel_request();
        {
            let mut cfg = request.get().init_config();
            cfg.set_name(&config.name);
            cfg.set_consent_mode(match config.consent_mode {
                ConsentMode::Collaborative => crate::kaijutsu_capnp::ConsentMode::Collaborative,
                ConsentMode::Autonomous => crate::kaijutsu_capnp::ConsentMode::Autonomous,
            });
            let mut mounts = cfg.init_mounts(config.mounts.len() as u32);
            for (i, mount) in config.mounts.iter().enumerate() {
                let mut m = mounts.reborrow().get(i as u32);
                m.set_path(&mount.path);
                m.set_source(&mount.source);
                m.set_writable(mount.writable);
            }
        }
        let response = request.send().promise.await?;
        let kernel = response.get()?.get_kernel()?;

        Ok(KernelHandle { kernel })
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
pub struct KernelInfo {
    pub id: String,
    pub name: String,
    pub user_count: u32,
    pub agent_count: u32,
}

#[derive(Debug, Clone)]
pub struct KernelConfig {
    pub name: String,
    pub consent_mode: ConsentMode,
    pub mounts: Vec<MountSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConsentMode {
    #[default]
    Collaborative,
    Autonomous,
}

#[derive(Debug, Clone)]
pub struct MountSpec {
    pub path: String,
    pub source: String,
    pub writable: bool,
}

/// Handle to an attached kernel
pub struct KernelHandle {
    kernel: crate::kaijutsu_capnp::kernel::Client,
}

impl KernelHandle {
    /// Get kernel info
    pub async fn get_info(&self) -> Result<KernelInfo, RpcError> {
        let request = self.kernel.get_info_request();
        let response = request.send().promise.await?;
        let info = response.get()?.get_info()?;
        Ok(KernelInfo {
            id: info.get_id()?.to_string()?,
            name: info.get_name()?.to_string()?,
            user_count: info.get_user_count(),
            agent_count: info.get_agent_count(),
        })
    }

    /// Send a message to the kernel
    pub async fn send(&self, content: &str) -> Result<Row, RpcError> {
        let mut request = self.kernel.send_request();
        request.get().set_content(content);
        let response = request.send().promise.await?;
        let row = response.get()?.get_row()?;
        Row::from_capnp(&row)
    }

    /// Mention an agent
    pub async fn mention(&self, agent: &str, content: &str) -> Result<Row, RpcError> {
        let mut request = self.kernel.mention_request();
        request.get().set_agent(agent);
        request.get().set_content(content);
        let response = request.send().promise.await?;
        let row = response.get()?.get_row()?;
        Row::from_capnp(&row)
    }

    /// Get kernel history
    pub async fn get_history(&self, limit: u32, before_id: u64) -> Result<Vec<Row>, RpcError> {
        let mut request = self.kernel.get_history_request();
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

    // kaish execution methods (directly on Kernel)

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

    /// Get command history
    pub async fn get_command_history(&self, limit: u32) -> Result<Vec<HistoryEntry>, RpcError> {
        let mut request = self.kernel.get_command_history_request();
        request.get().set_limit(limit);
        let response = request.send().promise.await?;
        let entries = response.get()?.get_entries()?;

        let mut result = Vec::with_capacity(entries.len() as usize);
        for e in entries.iter() {
            result.push(HistoryEntry {
                id: e.get_id(),
                code: e.get_code()?.to_string()?,
                timestamp: e.get_timestamp(),
            });
        }
        Ok(result)
    }

    /// Detach from the kernel
    pub async fn detach(self) -> Result<(), RpcError> {
        let request = self.kernel.detach_request();
        request.send().promise.await?;
        Ok(())
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

#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub id: u64,
    pub code: String,
    pub timestamp: u64,
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
