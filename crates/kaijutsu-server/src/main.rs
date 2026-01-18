//! Kaijutsu server binary
//!
//! Runs the server in either SSH or TCP mode.
//!
//! TCP mode (default) is for local development and testing.
//! SSH mode is for production deployments.

use std::cell::RefCell;
use std::env;
use std::rc::Rc;

use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use tokio::net::TcpListener;
use tokio::task::LocalSet;
use tokio_util::compat::TokioAsyncReadCompatExt;

use kaijutsu_server::constants::{DEFAULT_BIND_ADDRESS, DEFAULT_SSH_PORT, DEFAULT_TCP_PORT};
use kaijutsu_server::rpc::{ServerState, WorldImpl};
use kaijutsu_server::{SshServer, SshServerConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = env::args().collect();

    if args.get(1).map(|s| s.as_str()) == Some("--ssh") {
        // SSH mode
        let port = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_SSH_PORT);
        log::info!("Starting kaijutsu server (SSH mode) on port {}...", port);

        let config = SshServerConfig::ephemeral(port);
        let server = SshServer::new(config);
        server.run().await?;
    } else {
        // TCP mode (default) - simple RPC server for testing
        let port = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_TCP_PORT);
        log::info!("Starting kaijutsu server (TCP mode) on port {}...", port);
        log::info!("Usage: kaijutsu-server [port] | kaijutsu-server --ssh [port]");

        run_tcp_server(port).await?;
    }

    Ok(())
}

/// Run a simple TCP server that exposes RPC directly (no SSH)
async fn run_tcp_server(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let addr = format!("{}:{}", DEFAULT_BIND_ADDRESS, port);
    let listener = TcpListener::bind(&addr).await?;
    log::info!("Listening on {}", addr);

    // Use LocalSet for capnp-rpc
    let local = LocalSet::new();

    local.run_until(async {
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    log::info!("New connection from {}", peer);
                    tokio::task::spawn_local(handle_connection(stream, peer));
                }
                Err(e) => {
                    log::error!("Accept failed: {}", e);
                }
            }
        }
    }).await;

    Ok(())
}

async fn handle_connection(stream: tokio::net::TcpStream, peer: std::net::SocketAddr) {
    let stream = stream.compat();
    let (reader, writer) = futures::AsyncReadExt::split(stream);

    // Create server state - use peer address as username for testing
    let username = format!("user_{}", peer.port());
    let state = Rc::new(RefCell::new(ServerState::new(username)));
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
        log::error!("RPC system error for {}: {}", peer, e);
    }
    log::info!("Connection closed: {}", peer);
}
