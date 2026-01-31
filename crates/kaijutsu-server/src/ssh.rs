//! SSH server for kaijutsu
//!
//! Accepts SSH connections and provides Cap'n Proto RPC over channels.

use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;

use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use russh::keys::ssh_key;
use russh::keys::PrivateKey;
use russh::server::{self, Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId};
use tokio::net::TcpListener;
use tokio_util::compat::TokioAsyncReadCompatExt;

use crate::kaijutsu_capnp;
use crate::rpc::{ServerState, WorldImpl};

/// SSH server configuration
#[derive(Clone)]
pub struct SshServerConfig {
    pub bind_addr: SocketAddr,
    pub host_key: PrivateKey,
}

impl SshServerConfig {
    /// Create config with an ephemeral key (for testing)
    pub fn ephemeral(port: u16) -> Self {
        let host_key = PrivateKey::random(&mut rand::thread_rng(), russh::keys::Algorithm::Ed25519)
            .expect("Failed to generate host key");
        Self {
            bind_addr: SocketAddr::from(([127, 0, 0, 1], port)),
            host_key,
        }
    }
}

/// SSH server
pub struct SshServer {
    config: SshServerConfig,
}

impl SshServer {
    pub fn new(config: SshServerConfig) -> Self {
        Self { config }
    }

    /// Run the SSH server
    pub async fn run(&self) -> Result<(), std::io::Error> {
        let config = russh::server::Config {
            auth_rejection_time: std::time::Duration::from_secs(1),
            auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
            keys: vec![self.config.host_key.clone()],
            ..Default::default()
        };

        log::info!("Starting SSH server on {}", self.config.bind_addr);

        let mut server = Server;
        let socket = TcpListener::bind(self.config.bind_addr).await?;

        server
            .run_on_socket(Arc::new(config), &socket)
            .await
            .map_err(std::io::Error::other)
    }
}

/// Server factory - creates handlers for each connection
struct Server;

impl server::Server for Server {
    type Handler = ConnectionHandler;

    fn new_client(&mut self, _peer_addr: Option<SocketAddr>) -> Self::Handler {
        ConnectionHandler::new()
    }

    fn handle_session_error(&mut self, error: <Self::Handler as server::Handler>::Error) {
        log::error!("Session error: {:?}", error);
    }
}

/// Handler for a single SSH connection
struct ConnectionHandler {
    username: Option<String>,
    #[allow(dead_code)]
    channels: HashMap<ChannelId, ChannelState>,
}

#[derive(Default)]
struct ChannelState {
    // Will hold channel-specific state
}

impl ConnectionHandler {
    fn new() -> Self {
        Self {
            username: None,
            channels: HashMap::new(),
        }
    }
}

/// Run Cap'n Proto RPC over an SSH channel stream
async fn run_rpc(stream: russh::ChannelStream<Msg>, username: String) {
    let stream = stream.compat();
    let (reader, writer) = futures::AsyncReadExt::split(stream);

    let state = Rc::new(RefCell::new(ServerState::new(username.clone())));
    let world = WorldImpl::new(state);
    let client: kaijutsu_capnp::world::Client = capnp_rpc::new_client(world);

    let network = twoparty::VatNetwork::new(
        reader,
        writer,
        rpc_twoparty_capnp::Side::Server,
        Default::default(),
    );
    let rpc_system = RpcSystem::new(Box::new(network), Some(client.clone().client));

    log::info!("RPC session started for user: {}", username);
    if let Err(e) = rpc_system.await {
        log::error!("RPC system error for {}: {}", username, e);
    }
    log::info!("RPC session ended for user: {}", username);
}

impl server::Handler for ConnectionHandler {
    type Error = russh::Error;

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        log::info!("Channel {} opened, starting RPC handler", channel.id());
        self.channels.insert(channel.id(), ChannelState::default());

        let stream = channel.into_stream();
        let username = self.username.clone().unwrap_or_else(|| "anonymous".into());

        // Spawn RPC handler in a separate thread (capnp-rpc requires LocalSet)
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime for RPC");
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                run_rpc(stream, username).await;
            });
        });

        Ok(true)
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        _public_key: &ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        log::info!("Auth attempt from user: {}", user);
        self.username = Some(user.to_string());
        Ok(Auth::Accept)
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        log::debug!("Channel {} closed", channel);
        self.channels.remove(&channel);
        Ok(())
    }
}
