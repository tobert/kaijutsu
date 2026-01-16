//! SSH client for kaijutsu server connection
//!
//! Uses russh for SSH transport with SSH agent authentication.
//! The connection provides channels for:
//! - Channel 0: control (version negotiation, keepalive)
//! - Channel 1: rpc (Cap'n Proto request/response)
//! - Channel 2: events (server-pushed subscription streams)

use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Config, Handle};
use russh::keys::agent::client::AgentClient;
use russh::keys::{HashAlg, PublicKey};
use russh::{Channel, Disconnect};

/// SSH connection configuration
#[derive(Debug, Clone)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 2222,
            username: whoami::username(),
        }
    }
}

/// Client handler for russh - handles server key verification
struct ClientHandler {
    #[allow(dead_code)]
    server_key: Option<PublicKey>,
}

impl client::Handler for ClientHandler {
    type Error = SshError;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        // TODO: Implement proper known_hosts verification
        log::warn!(
            "Accepting server key without verification: {}",
            server_public_key.fingerprint(HashAlg::Sha256)
        );
        self.server_key = Some(server_public_key.clone());
        Ok(true)
    }
}

/// Channel handles for the three kaijutsu channels
pub struct SshChannels {
    pub control: Channel<client::Msg>,
    pub rpc: Channel<client::Msg>,
    pub events: Channel<client::Msg>,
}

/// SSH client wrapper
pub struct SshClient {
    config: SshConfig,
    session: Option<Handle<ClientHandler>>,
}

impl SshClient {
    pub fn new(config: SshConfig) -> Self {
        Self {
            config,
            session: None,
        }
    }

    /// Connect to the server using SSH agent authentication
    pub async fn connect(&mut self) -> Result<SshChannels, SshError> {
        // Connect to SSH agent
        let mut agent = AgentClient::connect_env()
            .await
            .map_err(|e| SshError::AgentFailed(e.to_string()))?;

        let keys = agent
            .request_identities()
            .await
            .map_err(|e| SshError::AgentFailed(e.to_string()))?;

        if keys.is_empty() {
            return Err(SshError::NoKeysAvailable);
        }

        log::info!("Found {} keys in SSH agent", keys.len());

        let config = Config {
            inactivity_timeout: Some(Duration::from_secs(300)),
            keepalive_interval: Some(Duration::from_secs(30)),
            keepalive_max: 3,
            ..<_>::default()
        };

        let handler = ClientHandler { server_key: None };
        let addr = (self.config.host.as_str(), self.config.port);
        let mut session = client::connect(Arc::new(config), addr, handler)
            .await
            .map_err(|e| SshError::ConnectionFailed(e.to_string()))?;

        log::info!(
            "Connected to {}:{}, attempting authentication",
            self.config.host,
            self.config.port
        );

        // Try each key until one works
        let mut authenticated = false;
        for key in &keys {
            log::debug!("Trying key: {}", key.fingerprint(HashAlg::Sha256));

            let hash_alg = session
                .best_supported_rsa_hash()
                .await
                .ok()
                .flatten()
                .flatten();

            let result = session
                .authenticate_publickey_with(&self.config.username, key.clone(), hash_alg, &mut agent)
                .await;

            match result {
                Ok(auth_result) if auth_result.success() => {
                    log::info!(
                        "Authenticated as {} with key {}",
                        self.config.username,
                        key.fingerprint(HashAlg::Sha256)
                    );
                    authenticated = true;
                    break;
                }
                Ok(_) => {
                    log::debug!("Key rejected, trying next...");
                }
                Err(e) => {
                    log::warn!("Auth error with key: {}", e);
                }
            }
        }

        if !authenticated {
            return Err(SshError::AuthFailed("No keys accepted by server".into()));
        }

        // Open three session channels
        let control = session
            .channel_open_session()
            .await
            .map_err(|e| SshError::ChannelFailed(format!("control: {}", e)))?;

        let rpc = session
            .channel_open_session()
            .await
            .map_err(|e| SshError::ChannelFailed(format!("rpc: {}", e)))?;

        let events = session
            .channel_open_session()
            .await
            .map_err(|e| SshError::ChannelFailed(format!("events: {}", e)))?;

        log::info!("Opened control, rpc, and events channels");
        self.session = Some(session);

        Ok(SshChannels { control, rpc, events })
    }

    /// Disconnect from the server
    pub async fn disconnect(&mut self) -> Result<(), SshError> {
        if let Some(session) = self.session.take() {
            session
                .disconnect(Disconnect::ByApplication, "Client disconnecting", "en")
                .await
                .map_err(|e| SshError::ConnectionFailed(e.to_string()))?;
        }
        Ok(())
    }

    /// Check if connected
    pub fn is_connected(&self) -> bool {
        self.session.as_ref().map(|s| !s.is_closed()).unwrap_or(false)
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum SshError {
    #[error("SSH not yet implemented")]
    NotImplemented,
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),
    #[error("Auth failed: {0}")]
    AuthFailed(String),
    #[error("Channel failed: {0}")]
    ChannelFailed(String),
    #[error("SSH agent error: {0}")]
    AgentFailed(String),
    #[error("No SSH keys available in agent")]
    NoKeysAvailable,
    #[error("Disconnected")]
    Disconnected,
}

impl From<russh::Error> for SshError {
    fn from(e: russh::Error) -> Self {
        SshError::ConnectionFailed(e.to_string())
    }
}
