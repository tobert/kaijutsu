//! SSH client for sshwarma server connection
//!
//! Uses russh for SSH transport. The connection provides channels for:
//! - Channel 0: control (version negotiation, keepalive)
//! - Channel 1: rpc (Cap'n Proto request/response)
//! - Channel 2: events (server-pushed subscription streams)

/// SSH connection configuration
#[derive(Clone)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    // Key auth handled by russh-keys
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

/// SSH client wrapper
///
/// TODO: Implement actual connection logic
/// - Load SSH keys from ~/.ssh/
/// - Connect to sshwarma server
/// - Open channels for control, rpc, events
/// - Handle reconnection
pub struct SshClient {
    config: SshConfig,
    // session: Option<russh::client::Handle<Handler>>,
}

impl SshClient {
    pub fn new(config: SshConfig) -> Self {
        Self {
            config,
            // session: None,
        }
    }

    /// Connect to the server
    ///
    /// Returns channel handles for control, rpc, and events
    pub async fn connect(&mut self) -> Result<(), SshError> {
        // TODO: Implement
        // 1. Load SSH key
        // 2. Connect to server
        // 3. Authenticate
        // 4. Open channels
        Err(SshError::NotImplemented)
    }

    /// Disconnect from the server
    pub async fn disconnect(&mut self) -> Result<(), SshError> {
        // TODO: Implement
        Err(SshError::NotImplemented)
    }

    /// Check if connected
    pub fn is_connected(&self) -> bool {
        false // TODO
    }
}

#[derive(Debug, Clone)]
pub enum SshError {
    NotImplemented,
    ConnectionFailed(String),
    AuthFailed(String),
    ChannelFailed(String),
    Disconnected,
}

impl std::fmt::Display for SshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SshError::NotImplemented => write!(f, "SSH not yet implemented"),
            SshError::ConnectionFailed(s) => write!(f, "Connection failed: {}", s),
            SshError::AuthFailed(s) => write!(f, "Auth failed: {}", s),
            SshError::ChannelFailed(s) => write!(f, "Channel failed: {}", s),
            SshError::Disconnected => write!(f, "Disconnected"),
        }
    }
}

impl std::error::Error for SshError {}
