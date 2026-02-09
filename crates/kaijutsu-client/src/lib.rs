//! Kaijutsu RPC client library
//!
//! Provides typed Cap'n Proto RPC client for connecting to kaijutsu servers.
//! Can connect via SSH (to remote servers) or Unix socket (for testing).

pub mod actor;
pub mod constants;
pub mod rpc;
pub mod ssh;
pub mod subscriptions;
pub mod sync;

// Generated Cap'n Proto code
pub mod kaijutsu_capnp {
    include!(concat!(env!("OUT_DIR"), "/kaijutsu_capnp.rs"));
}

pub use actor::{ActorError, ActorHandle, spawn_actor};
pub use rpc::{
    ClientToolFilter, Completion, CompletionKind, ConsentMode, Context, ContextDocument,
    ContextInfo, DocumentState, HistoryEntry, Identity, KernelConfig, KernelHandle, KernelInfo,
    LlmConfigInfo, LlmProviderInfo, McpResource, McpResourceContents, McpToolResult, MountSpec,
    RpcClient, RpcError, SeatHandle, SeatId, SeatInfo, SeatStatus, StagedDriftInfo, ToolResult,
    VersionSnapshot,
};
pub use ssh::{KeySource, SshChannels, SshClient, SshConfig, SshError};
pub use subscriptions::{ConnectionStatus, ServerEvent, SyncGeneration};
pub use sync::{SkipReason, SyncError, SyncManager, SyncResult};

/// Connect to a server via SSH and return an RPC client
///
/// This is the main entry point for connecting to a kaijutsu server.
/// Must be called within a `tokio::task::LocalSet` context.
pub async fn connect_ssh(config: SshConfig) -> Result<RpcClient, ConnectError> {
    let mut ssh = SshClient::new(config);
    let channels = ssh.connect().await?;
    let rpc_stream = channels.rpc.into_stream();
    let client = RpcClient::new(rpc_stream).await?;
    Ok(client)
}

/// Connect to a server via Unix socket (for testing)
///
/// Must be called within a `tokio::task::LocalSet` context.
#[cfg(unix)]
pub async fn connect_unix(path: impl AsRef<std::path::Path>) -> Result<RpcClient, ConnectError> {
    use tokio::net::UnixStream;
    use tokio_util::compat::TokioAsyncReadCompatExt;

    let stream = UnixStream::connect(path).await?;
    let client = RpcClient::from_stream(stream.compat()).await?;
    Ok(client)
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("SSH error: {0}")]
    Ssh(#[from] SshError),
    #[error("RPC error: {0}")]
    Rpc(#[from] RpcError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
