//! Kaijutsu RPC client library
//!
//! Provides typed Cap'n Proto RPC client for connecting to kaijutsu servers.
//! Can connect via SSH (to remote servers) or Unix socket (for testing).

pub mod rpc;
pub mod ssh;

// Generated Cap'n Proto code
pub mod kaijutsu_capnp {
    include!(concat!(env!("OUT_DIR"), "/kaijutsu_capnp.rs"));
}

pub use rpc::{
    CellInfo, CellKind, CellOp, CellPatch, CellState, CellVersion, Completion, CompletionKind,
    ConsentMode, CrdtOp, HistoryEntry, Identity, KernelConfig, KernelHandle, KernelInfo, MountSpec,
    Row, RowType, RpcClient, RpcError,
};
pub use ssh::{SshChannels, SshClient, SshConfig, SshError};

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
