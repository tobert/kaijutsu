//! SSH server for kaijutsu
//!
//! Accepts SSH connections and provides Cap'n Proto RPC over channels.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use russh::keys::ssh_key;
use russh::keys::PrivateKey;
use russh::server::{self, Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId, CryptoVec};
use tokio::net::TcpListener;

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
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
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

impl server::Handler for ConnectionHandler {
    type Error = russh::Error;

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        log::debug!("Channel {} opened", channel.id());
        self.channels.insert(channel.id(), ChannelState::default());
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

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        log::debug!("Received {} bytes on channel {}", data.len(), channel);

        // For now, echo back
        session.data(channel, CryptoVec::from_slice(data))?;
        Ok(())
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
