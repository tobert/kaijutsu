//! SSH client for kaijutsu server connection
//!
//! Supports multiple authentication methods:
//! - SSH agent (default, tries all keys in agent)
//! - Key file (loads from disk, optional passphrase)
//! - In-memory key (for testing with ephemeral keys)
//!
//! The connection provides channels for:
//! - Channel 0: control (version negotiation, keepalive)
//! - Channel 1: rpc (Cap'n Proto request/response)
//! - Channel 2: events (server-pushed subscription streams)

use std::path::PathBuf;
use std::sync::Arc;

use russh::client::{self, Config, Handle};
use russh::keys::agent::client::AgentClient;
use russh::keys::{Algorithm, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey};
use russh::{Channel, Disconnect};

use crate::constants::{
    DEFAULT_SSH_HOST, DEFAULT_SSH_PORT, SSH_INACTIVITY_TIMEOUT, SSH_KEEPALIVE_INTERVAL,
    SSH_KEEPALIVE_MAX,
};

/// Source for SSH authentication keys
#[derive(Debug, Clone)]
pub enum KeySource {
    /// Use SSH agent (default) - tries all keys in the agent
    Agent,
    /// Load key from file, with optional passphrase
    File {
        path: PathBuf,
        passphrase: Option<String>,
    },
    /// Use an in-memory private key (for testing)
    InMemory(Arc<PrivateKey>),
}

impl Default for KeySource {
    fn default() -> Self {
        Self::Agent
    }
}

impl KeySource {
    /// Generate an ephemeral Ed25519 key in memory (useful for tests)
    pub fn ephemeral() -> Self {
        let key = PrivateKey::random(&mut rand::thread_rng(), Algorithm::Ed25519)
            .expect("Failed to generate ephemeral key");
        Self::InMemory(Arc::new(key))
    }

    /// Load a key from a file path
    pub fn from_file(path: impl Into<PathBuf>) -> Self {
        Self::File {
            path: path.into(),
            passphrase: None,
        }
    }

    /// Load a key from a file with passphrase
    pub fn from_file_with_passphrase(path: impl Into<PathBuf>, passphrase: impl Into<String>) -> Self {
        Self::File {
            path: path.into(),
            passphrase: Some(passphrase.into()),
        }
    }
}

/// SSH connection configuration
#[derive(Debug, Clone)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub key_source: KeySource,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_SSH_HOST.into(),
            port: DEFAULT_SSH_PORT,
            username: whoami::username(),
            key_source: KeySource::Agent,
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
        // ╔═══════════════════════════════════════════════════════════════════════════╗
        // ║ SECURITY WARNING: SSH KEY VERIFICATION DISABLED                           ║
        // ║                                                                           ║
        // ║ This client accepts ANY server key without verification.                  ║
        // ║ This is acceptable for local development but NOT for production.          ║
        // ║                                                                           ║
        // ║ TODO: Implement proper known_hosts verification before production use.    ║
        // ║ - Parse ~/.ssh/known_hosts                                                ║
        // ║ - Verify server key fingerprint matches                                   ║
        // ║ - Prompt user for unknown keys                                            ║
        // ╚═══════════════════════════════════════════════════════════════════════════╝
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

    /// Connect to the server using the configured key source
    pub async fn connect(&mut self) -> Result<SshChannels, SshError> {
        let config = Config {
            inactivity_timeout: Some(SSH_INACTIVITY_TIMEOUT),
            keepalive_interval: Some(SSH_KEEPALIVE_INTERVAL),
            keepalive_max: SSH_KEEPALIVE_MAX,
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

        // Authenticate based on key source
        match &self.config.key_source {
            KeySource::Agent => {
                self.auth_with_agent(&mut session).await?;
            }
            KeySource::File { path, passphrase } => {
                self.auth_with_file(&mut session, path, passphrase.as_deref()).await?;
            }
            KeySource::InMemory(key) => {
                self.auth_with_key(&mut session, Arc::clone(key)).await?;
            }
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

    /// Authenticate using SSH agent
    async fn auth_with_agent(&self, session: &mut Handle<ClientHandler>) -> Result<(), SshError> {
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
                    return Ok(());
                }
                Ok(_) => {
                    log::debug!("Key rejected, trying next...");
                }
                Err(e) => {
                    log::warn!("Auth error with key: {}", e);
                }
            }
        }

        Err(SshError::AuthFailed("No keys accepted by server".into()))
    }

    /// Authenticate using a key file
    async fn auth_with_file(
        &self,
        session: &mut Handle<ClientHandler>,
        path: &PathBuf,
        passphrase: Option<&str>,
    ) -> Result<(), SshError> {
        let key = russh::keys::load_secret_key(path, passphrase)
            .map_err(|e| SshError::KeyLoadFailed(format!("{}: {}", path.display(), e)))?;

        self.auth_with_key(session, Arc::new(key)).await
    }

    /// Authenticate using an in-memory private key
    async fn auth_with_key(
        &self,
        session: &mut Handle<ClientHandler>,
        key: Arc<PrivateKey>,
    ) -> Result<(), SshError> {
        let fingerprint = key.public_key().fingerprint(HashAlg::Sha256);
        log::debug!("Authenticating with key: {}", fingerprint);

        let hash_alg = session
            .best_supported_rsa_hash()
            .await
            .ok()
            .flatten()
            .flatten();

        let key_with_hash = PrivateKeyWithHashAlg::new(key, hash_alg);

        let result = session
            .authenticate_publickey(&self.config.username, key_with_hash)
            .await
            .map_err(|e| SshError::AuthFailed(e.to_string()))?;

        if result.success() {
            log::info!("Authenticated as {} with key {}", self.config.username, fingerprint);
            Ok(())
        } else {
            Err(SshError::AuthFailed("Key rejected by server".into()))
        }
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
    #[error("Failed to load key: {0}")]
    KeyLoadFailed(String),
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
