//! Integration tests for kaijutsu RPC
//!
//! Tests the client-server interaction over TCP (bypassing SSH for simplicity).

use std::cell::RefCell;
use std::rc::Rc;

use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::LocalSet;
use tokio_util::compat::TokioAsyncReadCompatExt;

use kaijutsu_client::{RpcClient, RpcError};
use kaijutsu_server::rpc::{ServerState, WorldImpl};

/// Helper to run async test code that requires LocalSet (for capnp-rpc)
fn run_local<F: std::future::Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = LocalSet::new();
    rt.block_on(local.run_until(f));
}

/// Start a server on an ephemeral port and return the address
async fn start_server() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Spawn server accept loop
    tokio::task::spawn_local(async move {
        while let Ok((stream, _peer)) = listener.accept().await {
            tokio::task::spawn_local(handle_connection(stream));
        }
    });

    addr
}

async fn handle_connection(stream: TcpStream) {
    let stream = stream.compat();
    let (reader, writer) = futures::AsyncReadExt::split(stream);

    // Create server state with a test username
    let state = Rc::new(RefCell::new(ServerState::new("test_user".to_string())));
    let world = WorldImpl::new(state);
    let client: kaijutsu_server::kaijutsu_capnp::world::Client = capnp_rpc::new_client(world);

    let network = twoparty::VatNetwork::new(
        reader,
        writer,
        rpc_twoparty_capnp::Side::Server,
        Default::default(),
    );
    let rpc_system = RpcSystem::new(Box::new(network), Some(client.clone().client));

    if let Err(e) = rpc_system.await {
        log::error!("RPC system error: {}", e);
    }
}

async fn connect_client(addr: std::net::SocketAddr) -> Result<RpcClient, RpcError> {
    let stream = TcpStream::connect(addr).await.unwrap();
    RpcClient::from_stream(stream.compat()).await
}

#[test]
fn test_whoami() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await.unwrap();

        let identity = client.whoami().await.unwrap();
        assert_eq!(identity.username, "test_user");
        assert_eq!(identity.display_name, "test_user");
    });
}

#[test]
fn test_list_kernels_empty() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await.unwrap();

        let kernels = client.list_kernels().await.unwrap();
        assert!(kernels.is_empty(), "Expected no kernels initially");
    });
}

#[test]
fn test_attach_kernel_creates_kernel() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await.unwrap();

        // Attach to a kernel (should auto-create for now)
        let kernel = client.attach_kernel("test-kernel").await.unwrap();
        let info = kernel.get_info().await.unwrap();
        assert_eq!(info.name, "test-kernel");
        assert_eq!(info.id, "test-kernel");
    });
}

#[test]
fn test_kernel_appears_in_list() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await.unwrap();

        // Attach to a kernel
        let _kernel = client.attach_kernel("listed-kernel").await.unwrap();

        // Check it appears in list
        let kernels = client.list_kernels().await.unwrap();
        assert_eq!(kernels.len(), 1);
        assert_eq!(kernels[0].name, "listed-kernel");
    });
}

#[test]
fn test_send_message() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await.unwrap();

        let kernel = client.attach_kernel("chat-kernel").await.unwrap();
        let row = kernel.send("Hello, world!").await.unwrap();

        assert!(row.id > 0);
        assert_eq!(row.content, "Hello, world!");
        assert_eq!(row.sender, "test_user");
    });
}

#[test]
fn test_get_history() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await.unwrap();

        let kernel = client.attach_kernel("history-kernel").await.unwrap();

        // Send some messages
        kernel.send("Message 1").await.unwrap();
        kernel.send("Message 2").await.unwrap();
        kernel.send("Message 3").await.unwrap();

        // Get history
        let history = kernel.get_history(10, 0).await.unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].content, "Message 1");
        assert_eq!(history[1].content, "Message 2");
        assert_eq!(history[2].content, "Message 3");
    });
}

#[test]
fn test_create_kernel_with_config() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await.unwrap();

        let config = kaijutsu_client::KernelConfig {
            name: "feature/test".to_string(),
            consent_mode: kaijutsu_client::ConsentMode::Collaborative,
            mounts: vec![],
        };
        let kernel = client.create_kernel(config).await.unwrap();
        let info = kernel.get_info().await.unwrap();

        assert_eq!(info.name, "feature/test");
    });
}

#[test]
fn test_mention_agent() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await.unwrap();

        let kernel = client.attach_kernel("agent-kernel").await.unwrap();
        let row = kernel.mention("claude", "help me write tests").await.unwrap();

        assert!(row.content.contains("@claude"));
        assert!(row.content.contains("help me write tests"));
    });
}

#[test]
fn test_execute_command() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await.unwrap();

        let kernel = client.attach_kernel("exec-kernel").await.unwrap();
        let exec_id = kernel.execute("echo hello").await.unwrap();

        assert!(exec_id > 0);
    });
}

#[test]
fn test_command_history() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await.unwrap();

        let kernel = client.attach_kernel("cmd-history-kernel").await.unwrap();

        // Execute some commands
        kernel.execute("ls").await.unwrap();
        kernel.execute("pwd").await.unwrap();
        kernel.execute("echo test").await.unwrap();

        // Get command history
        let history = kernel.get_command_history(10).await.unwrap();
        assert_eq!(history.len(), 3);
        // History is in reverse order (most recent first)
        assert_eq!(history[0].code, "echo test");
        assert_eq!(history[1].code, "pwd");
        assert_eq!(history[2].code, "ls");
    });
}
