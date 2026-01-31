//! SSH server for kaijutsu
//!
//! Accepts SSH connections and provides Cap'n Proto RPC over channels.
//! Public key authentication with user identity from SQLite.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use parking_lot::Mutex;
use russh::keys::ssh_key::{self, HashAlg};
use russh::keys::PrivateKey;
use russh::server::{self, Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId};
use tokio::net::TcpListener;
use tokio_util::compat::TokioAsyncReadCompatExt;

use crate::auth_db::AuthDb;
use crate::kaijutsu_capnp;
use crate::rpc::{ServerState, WorldImpl};

/// Source for the SSH host key.
#[derive(Clone)]
pub enum KeySource {
    /// Load from file, or generate and save if it doesn't exist.
    Persistent(PathBuf),
    /// Generate ephemeral key (for testing).
    Ephemeral,
}

impl KeySource {
    /// Default persistent key path: ~/.local/share/kaijutsu/host_key
    pub fn default_path() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("kaijutsu")
            .join("host_key")
    }

    /// Load or generate the host key.
    pub fn load_or_generate(&self) -> Result<PrivateKey, std::io::Error> {
        match self {
            KeySource::Persistent(path) => load_or_generate_host_key(path),
            KeySource::Ephemeral => {
                PrivateKey::random(&mut rand::thread_rng(), russh::keys::Algorithm::Ed25519)
                    .map_err(std::io::Error::other)
            }
        }
    }
}

/// Load a host key from file, or generate and save a new one.
///
/// Uses Ed25519 keys in OpenSSH format.
pub fn load_or_generate_host_key(path: &Path) -> Result<PrivateKey, std::io::Error> {
    if path.exists() {
        log::info!("Loading host key from {}", path.display());
        let key_data = fs::read_to_string(path)?;
        PrivateKey::from_openssh(&key_data).map_err(|e| {
            std::io::Error::other(format!("Failed to parse host key: {}", e))
        })
    } else {
        log::info!("Generating new host key at {}", path.display());

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let key = PrivateKey::random(&mut rand::thread_rng(), russh::keys::Algorithm::Ed25519)
            .map_err(std::io::Error::other)?;

        // Save in OpenSSH format
        let key_pem = key.to_openssh(ssh_key::LineEnding::LF).map_err(|e| {
            std::io::Error::other(format!("Failed to serialize host key: {}", e))
        })?;
        fs::write(path, key_pem.as_bytes())?;

        // Set restrictive permissions on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }

        log::info!(
            "Host key fingerprint: {}",
            key.public_key().fingerprint(HashAlg::Sha256)
        );

        Ok(key)
    }
}

/// SSH server configuration
#[derive(Clone)]
pub struct SshServerConfig {
    pub bind_addr: SocketAddr,
    /// Source for the host key (persistent file or ephemeral).
    pub key_source: KeySource,
    /// Path to auth database (None = in-memory for testing)
    pub auth_db_path: Option<PathBuf>,
    /// Allow anonymous connections (auto-register unknown keys).
    /// Only for testing - production should always be false.
    pub allow_anonymous: bool,
}

impl SshServerConfig {
    /// Create config with an ephemeral key (for testing).
    ///
    /// Uses in-memory auth database and allows anonymous connections.
    pub fn ephemeral(port: u16) -> Self {
        Self {
            bind_addr: SocketAddr::from(([127, 0, 0, 1], port)),
            key_source: KeySource::Ephemeral,
            auth_db_path: None,
            allow_anonymous: true, // Tests need to accept any key
        }
    }

    /// Create production config with persistent host key and auth database.
    pub fn production(port: u16) -> Self {
        Self {
            bind_addr: SocketAddr::from(([0, 0, 0, 0], port)),
            key_source: KeySource::Persistent(KeySource::default_path()),
            auth_db_path: Some(AuthDb::default_path()),
            allow_anonymous: false,
        }
    }

    /// Create config with default auth database path
    pub fn with_default_auth_db(mut self) -> Self {
        self.auth_db_path = Some(AuthDb::default_path());
        self.allow_anonymous = false; // Production: require known keys
        self
    }

    /// Create config with specific auth database path
    pub fn with_auth_db(mut self, path: PathBuf) -> Self {
        self.auth_db_path = Some(path);
        self.allow_anonymous = false;
        self
    }

    /// Use a persistent host key at the given path.
    pub fn with_host_key_path(mut self, path: PathBuf) -> Self {
        self.key_source = KeySource::Persistent(path);
        self
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
        // Load or generate the host key
        let host_key = self.config.key_source.load_or_generate()?;
        log::info!(
            "Host key fingerprint: {}",
            host_key.public_key().fingerprint(HashAlg::Sha256)
        );

        // Open or create the auth database
        let auth_db = match &self.config.auth_db_path {
            Some(path) => {
                log::info!("Using auth database: {}", path.display());
                AuthDb::open(path).map_err(std::io::Error::other)?
            }
            None => {
                log::warn!("Using in-memory auth database (all keys accepted)");
                AuthDb::in_memory().map_err(std::io::Error::other)?
            }
        };

        // Check if database is empty
        if auth_db.is_empty().map_err(std::io::Error::other)? {
            log::warn!("Auth database is empty! Add keys with: kaijutsu-server add-key <pubkey>");
            log::warn!("Or import existing keys: kaijutsu-server import ~/.ssh/authorized_keys");
        }

        let config = russh::server::Config {
            auth_rejection_time: std::time::Duration::from_secs(1),
            auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
            keys: vec![host_key],
            ..Default::default()
        };

        log::info!("Starting SSH server on {}", self.config.bind_addr);

        let allow_anonymous = self.config.allow_anonymous;
        if allow_anonymous {
            log::warn!("Anonymous mode enabled - unknown keys will be auto-registered");
        }

        let mut server = Server {
            auth_db: Arc::new(Mutex::new(auth_db)),
            allow_anonymous,
        };
        let socket = TcpListener::bind(self.config.bind_addr).await?;

        server
            .run_on_socket(Arc::new(config), &socket)
            .await
            .map_err(std::io::Error::other)
    }
}

/// Server factory - creates handlers for each connection
struct Server {
    auth_db: Arc<Mutex<AuthDb>>,
    allow_anonymous: bool,
}

impl server::Server for Server {
    type Handler = ConnectionHandler;

    fn new_client(&mut self, peer_addr: Option<SocketAddr>) -> Self::Handler {
        ConnectionHandler::new(self.auth_db.clone(), peer_addr, self.allow_anonymous)
    }

    fn handle_session_error(&mut self, error: <Self::Handler as server::Handler>::Error) {
        log::error!("Session error: {:?}", error);
    }
}

/// Authenticated user identity
#[derive(Debug, Clone)]
pub struct Identity {
    pub nick: String,
    pub display_name: String,
    pub is_admin: bool,
}

/// Handler for a single SSH connection
struct ConnectionHandler {
    auth_db: Arc<Mutex<AuthDb>>,
    peer_addr: Option<SocketAddr>,
    allow_anonymous: bool,
    identity: Option<Identity>,
    #[allow(dead_code)]
    channels: HashMap<ChannelId, ChannelState>,
}

#[derive(Default)]
struct ChannelState {
    // Will hold channel-specific state
}

impl ConnectionHandler {
    fn new(auth_db: Arc<Mutex<AuthDb>>, peer_addr: Option<SocketAddr>, allow_anonymous: bool) -> Self {
        Self {
            auth_db,
            peer_addr,
            allow_anonymous,
            identity: None,
            channels: HashMap::new(),
        }
    }
}

/// Run Cap'n Proto RPC over an SSH channel stream
async fn run_rpc(stream: russh::ChannelStream<Msg>, identity: Identity) {
    let stream = stream.compat();
    let (reader, writer) = futures::AsyncReadExt::split(stream);

    // Use nick as the username for RPC state
    let state = Rc::new(RefCell::new(ServerState::new(identity.nick.clone())));
    let world = WorldImpl::new(state);
    let client: kaijutsu_capnp::world::Client = capnp_rpc::new_client(world);

    let network = twoparty::VatNetwork::new(
        reader,
        writer,
        rpc_twoparty_capnp::Side::Server,
        Default::default(),
    );
    let rpc_system = RpcSystem::new(Box::new(network), Some(client.clone().client));

    log::info!(
        "RPC session started for {} ({})",
        identity.nick,
        identity.display_name
    );
    if let Err(e) = rpc_system.await {
        log::error!("RPC system error for {}: {}", identity.nick, e);
    }
    log::info!("RPC session ended for {}", identity.nick);
}

/// Sanitize a username for use in anonymous mode.
///
/// Filters to alphanumeric, underscore, and hyphen characters.
/// Truncates to 32 characters max.
fn sanitize_username(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .take(32)
        .collect()
}

impl server::Handler for ConnectionHandler {
    type Error = russh::Error;

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let identity = match &self.identity {
            Some(id) => id.clone(),
            None => {
                log::warn!("Channel open without authentication");
                return Ok(false);
            }
        };

        log::info!(
            "Channel {} opened for {} ({})",
            channel.id(),
            identity.nick,
            identity.display_name
        );
        self.channels.insert(channel.id(), ChannelState::default());

        let stream = channel.into_stream();

        // Spawn RPC handler in a separate thread (capnp-rpc requires LocalSet)
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime for RPC");
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                run_rpc(stream, identity).await;
            });
        });

        Ok(true)
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        let fingerprint = public_key.fingerprint(HashAlg::Sha256).to_string();
        let peer = self
            .peer_addr
            .map(|a| a.to_string())
            .unwrap_or_else(|| "unknown".into());

        log::debug!(
            "Auth attempt: user={}, fingerprint={}, peer={}",
            user,
            fingerprint,
            peer
        );

        // Clone what we need for spawn_blocking
        let db = self.auth_db.clone();
        let fp = fingerprint.clone();

        // Look up the key in the database (blocking I/O in spawn_blocking)
        let auth_result = tokio::task::spawn_blocking(move || {
            let db = db.lock();
            db.authenticate(&fp)
        })
        .await
        .map_err(|e| {
            log::error!("spawn_blocking panicked: {}", e);
            russh::Error::Disconnect
        })?;

        match auth_result {
            Ok(Some(db_user)) => {
                // Update last_used timestamp (fire and forget, non-blocking)
                let db = self.auth_db.clone();
                let fp = fingerprint.clone();
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = db.lock().update_last_used(&fp) {
                        log::warn!("Failed to update last_used for {}: {}", fp, e);
                    }
                });

                log::info!(
                    "Auth accepted: {} ({}) from {} [{}]",
                    db_user.nick,
                    db_user.display_name,
                    peer,
                    fingerprint
                );

                self.identity = Some(Identity {
                    nick: db_user.nick,
                    display_name: db_user.display_name,
                    is_admin: db_user.is_admin,
                });

                Ok(Auth::Accept)
            }
            Ok(None) => {
                // If anonymous mode, auto-register the key
                if self.allow_anonymous {
                    // Sanitize username to prevent injection
                    let safe_user = sanitize_username(user);
                    if safe_user.is_empty() {
                        log::warn!("Anonymous auth rejected: empty username after sanitization");
                        return Ok(Auth::Reject {
                            proceed_with_methods: None,
                            partial_success: false,
                        });
                    }

                    log::info!(
                        "Anonymous mode: auto-registering key {} for user {}",
                        fingerprint,
                        safe_user
                    );

                    // Clone for spawn_blocking - use OpenSSH format for serialization
                    let db = self.auth_db.clone();
                    let key_openssh = public_key.to_openssh().map_err(|e| {
                        log::error!("Failed to serialize public key: {}", e);
                        russh::Error::Disconnect
                    })?;
                    let safe_user_clone = safe_user.clone();

                    let result = tokio::task::spawn_blocking(move || {
                        // Reconstruct the key from OpenSSH format
                        let key = ssh_key::PublicKey::from_openssh(&key_openssh)
                            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(
                                std::io::Error::other(format!("Failed to parse key: {}", e))
                            )))?;
                        let mut db = db.lock();
                        db.add_key_auto_user(&key, Some(&safe_user_clone), Some(&safe_user_clone), false)
                    })
                    .await
                    .map_err(|e| {
                        log::error!("spawn_blocking panicked: {}", e);
                        russh::Error::Disconnect
                    })?;

                    match result {
                        Ok((user_id, _key_id)) => {
                            let db = self.auth_db.clone();
                            let user_result = tokio::task::spawn_blocking(move || {
                                db.lock().get_user(user_id)
                            })
                            .await
                            .map_err(|_| russh::Error::Disconnect)?;

                            if let Ok(Some(db_user)) = user_result {
                                self.identity = Some(Identity {
                                    nick: db_user.nick.clone(),
                                    display_name: db_user.display_name.clone(),
                                    is_admin: db_user.is_admin,
                                });
                                log::info!(
                                    "Auth accepted (anonymous): {} from {} [{}]",
                                    db_user.nick,
                                    peer,
                                    fingerprint
                                );
                                return Ok(Auth::Accept);
                            }
                        }
                        Err(e) => {
                            log::warn!("Failed to auto-register key: {}", e);
                        }
                    }
                }

                log::warn!(
                    "Auth rejected: unknown key {} (ssh user={}) from {}",
                    fingerprint,
                    user,
                    peer
                );
                Ok(Auth::Reject {
                    proceed_with_methods: None,
                    partial_success: false,
                })
            }
            Err(e) => {
                log::error!("Auth database error: {}", e);
                Ok(Auth::Reject {
                    proceed_with_methods: None,
                    partial_success: false,
                })
            }
        }
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
