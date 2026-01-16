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
fn test_list_rooms_empty() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await.unwrap();

        let rooms = client.list_rooms().await.unwrap();
        assert!(rooms.is_empty(), "Expected no rooms initially");
    });
}

#[test]
fn test_join_room_creates_room() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await.unwrap();

        // Join a room (should auto-create)
        let room = client.join_room("test-room").await.unwrap();
        let info = room.get_info().await.unwrap();
        assert_eq!(info.name, "test-room");
        assert_eq!(info.branch, "main");
    });
}

#[test]
fn test_room_appears_in_list() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await.unwrap();

        // Join a room
        let _room = client.join_room("listed-room").await.unwrap();

        // Check it appears in list
        let rooms = client.list_rooms().await.unwrap();
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0].name, "listed-room");
    });
}

#[test]
fn test_send_message() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await.unwrap();

        let room = client.join_room("chat-room").await.unwrap();
        let row = room.send("Hello, world!").await.unwrap();

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

        let room = client.join_room("history-room").await.unwrap();

        // Send some messages
        room.send("Message 1").await.unwrap();
        room.send("Message 2").await.unwrap();
        room.send("Message 3").await.unwrap();

        // Get history
        let history = room.get_history(10, 0).await.unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].content, "Message 1");
        assert_eq!(history[1].content, "Message 2");
        assert_eq!(history[2].content, "Message 3");
    });
}

#[test]
fn test_create_room_with_config() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await.unwrap();

        let config = kaijutsu_client::RoomConfig {
            name: "feature/test".to_string(),
            branch: Some("develop".to_string()),
            repos: vec![],
        };
        let room = client.create_room(config).await.unwrap();
        let info = room.get_info().await.unwrap();

        assert_eq!(info.name, "feature/test");
        assert_eq!(info.branch, "develop");
    });
}

#[test]
fn test_mention_agent() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await.unwrap();

        let room = client.join_room("agent-room").await.unwrap();
        let row = room.mention("claude", "help me write tests").await.unwrap();

        assert!(row.content.contains("@claude"));
        assert!(row.content.contains("help me write tests"));
    });
}
