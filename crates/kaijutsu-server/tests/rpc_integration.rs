//! Integration tests for kaijutsu RPC over SSH
//!
//! Uses ephemeral SSH keys generated in memory for testing.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::task::LocalSet;

use kaijutsu_client::{KeySource, RpcClient, SshConfig};
use kaijutsu_server::{SshServer, SshServerConfig};

/// Helper to run async test code that requires LocalSet (for capnp-rpc)
fn run_local<F: std::future::Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = LocalSet::new();
    rt.block_on(local.run_until(f));
}

/// Start an SSH server on an ephemeral port and return the address
async fn start_server() -> SocketAddr {
    // Bind to port 0 to get an ephemeral port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener); // Release so server can bind

    let config = SshServerConfig::ephemeral(addr.port());

    // Spawn server in background
    tokio::task::spawn_local(async move {
        let server = SshServer::new(config);
        if let Err(e) = server.run().await {
            log::error!("Server error: {}", e);
        }
    });

    // Give server time to start
    tokio::time::sleep(Duration::from_millis(50)).await;

    addr
}

/// Connect to server with ephemeral key
async fn connect_client(addr: SocketAddr) -> RpcClient {
    let config = SshConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        username: "test_user".to_string(),
        key_source: KeySource::ephemeral(),
    };

    let mut ssh_client = kaijutsu_client::SshClient::new(config);
    let channels = ssh_client.connect().await.expect("SSH connect failed");
    RpcClient::new(channels.rpc.into_stream())
        .await
        .expect("RPC client init failed")
}

#[test]
fn test_whoami() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let identity = client.whoami().await.unwrap();
        assert_eq!(identity.username, "test_user");
        assert_eq!(identity.display_name, "test_user");
    });
}

#[test]
fn test_list_kernels_empty() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        let kernels = client.list_kernels().await.unwrap();
        assert!(kernels.is_empty(), "Expected no kernels initially");
    });
}

#[test]
fn test_attach_kernel_creates_kernel() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        // Attach to a kernel (server auto-creates)
        let (kernel, kernel_id) = client.attach_kernel().await.unwrap();
        let info = kernel.get_info().await.unwrap();
        assert!(!kernel_id.is_nil());
        assert_eq!(info.id, kernel_id);
    });
}

#[test]
fn test_kernel_appears_in_list() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;

        // Attach to a kernel
        let (_kernel, _kernel_id) = client.attach_kernel().await.unwrap();

        // Check it appears in list
        let kernels = client.list_kernels().await.unwrap();
        assert_eq!(kernels.len(), 1);
    });
}
