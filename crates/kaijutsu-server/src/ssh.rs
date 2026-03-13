//! SSH server for kaijutsu
//!
//! Accepts SSH connections and provides Cap'n Proto RPC over channels.
//! Public key authentication with user identity from SQLite.

use std::cell::RefCell;
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

use kaijutsu_kernel::McpServerPool;
use kaijutsu_types::Principal;

use crate::auth_db::AuthDb;
use crate::kaijutsu_capnp;
use crate::rpc::{ConnectionState, ServerRegistry, WorldImpl};

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
        kaish_kernel::xdg_data_home()
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
    /// Config directory override. None = use XDG default (~/.config/kaijutsu).
    pub config_dir: Option<PathBuf>,
    /// Data directory override. None = use XDG default (~/.local/share/kaijutsu/kernel).
    pub data_dir: Option<PathBuf>,
}

impl SshServerConfig {
    /// Create config with an ephemeral key (for testing).
    ///
    /// Uses in-memory auth database and allows anonymous connections.
    pub fn ephemeral(port: u16) -> Self {
        // Use a fresh tempdir so no real configs (mcp.rhai etc.) are loaded.
        // Include PID + timestamp to avoid stale data from PID reuse.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let config_dir = std::env::temp_dir().join(format!("kaijutsu-test-{}-{}", std::process::id(), stamp));
        std::fs::create_dir_all(&config_dir).ok();

        Self {
            bind_addr: SocketAddr::from(([127, 0, 0, 1], port)),
            key_source: KeySource::Ephemeral,
            auth_db_path: None,
            allow_anonymous: true, // Tests need to accept any key
            config_dir: Some(config_dir.clone()),
            data_dir: Some(config_dir),
        }
    }

    /// Create production config with persistent host key and auth database.
    pub fn production(port: u16) -> Self {
        Self {
            bind_addr: SocketAddr::from(([0, 0, 0, 0], port)),
            key_source: KeySource::Persistent(KeySource::default_path()),
            auth_db_path: Some(AuthDb::default_path()),
            allow_anonymous: false,
            config_dir: None, // Use XDG default
            data_dir: None,   // Use XDG default
        }
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

    /// Spawn a background task to pre-initialize MCP servers from mcp.rhai.
    ///
    /// This runs concurrently with the SSH accept loop so server startup
    /// isn't blocked by slow MCP servers (e.g. streamable_http with no listener).
    fn start_mcp_initialization(
        pool: Arc<McpServerPool>,
        config_dir: Option<PathBuf>,
    ) {
        let config_path = config_dir
            .unwrap_or_else(|| {
                kaish_kernel::xdg_config_home().join("kaijutsu")
            })
            .join("mcp.rhai");

        tokio::spawn(async move {
            let script = match tokio::fs::read_to_string(&config_path).await {
                Ok(s) => s,
                Err(e) => {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        log::warn!("Failed to read {}: {}", config_path.display(), e);
                    }
                    return;
                }
            };

            let config = match kaijutsu_kernel::load_mcp_config(&script) {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("Failed to parse mcp.rhai: {}", e);
                    return;
                }
            };

            if config.servers.is_empty() {
                return;
            }

            log::info!(
                "Pre-initializing {} MCP servers from {}",
                config.servers.len(),
                config_path.display(),
            );

            let timeout = std::time::Duration::from_secs(5);
            let futs: Vec<_> = config.servers.into_iter().map(|server_config| {
                let pool = pool.clone();
                let name = server_config.name.clone();
                async move {
                    match tokio::time::timeout(timeout, pool.register(server_config)).await {
                        Ok(Ok(info)) => {
                            log::info!(
                                "MCP server '{}' pre-initialized ({} tools)",
                                name,
                                info.tools.len(),
                            );
                        }
                        Ok(Err(e)) => {
                            log::warn!("MCP server '{}' failed to pre-initialize: {}", name, e);
                        }
                        Err(_) => {
                            log::warn!(
                                "MCP server '{}' timed out during pre-initialization ({}s)",
                                name,
                                timeout.as_secs(),
                            );
                        }
                    }
                }
            }).collect();

            futures::future::join_all(futs).await;
            log::info!("MCP pre-initialization complete");
        });
    }

    /// Run the SSH server, binding to the configured address.
    pub async fn run(&self) -> Result<(), std::io::Error> {
        let socket = TcpListener::bind(self.config.bind_addr).await?;
        log::info!("Starting SSH server on {}", self.config.bind_addr);
        self.run_on_listener(socket).await
    }

    /// Run the SSH server on a pre-bound listener.
    ///
    /// Useful for tests: bind port 0 first to get the address, then pass the
    /// listener here. The listener stays bound during initialization, so
    /// incoming connections queue in the OS backlog instead of getting refused.
    pub async fn run_on_listener(&self, socket: TcpListener) -> Result<(), std::io::Error> {
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
            // 100ms delay on rejected keys (after the first, which is 0ms).
            // This is a local dev server bound to 127.0.0.1 — brute-force
            // timing attack defense is unnecessary, and 1s per rejected agent
            // key adds painful latency during SSH agent enumeration.
            auth_rejection_time: std::time::Duration::from_millis(100),
            auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
            keys: vec![host_key],
            ..Default::default()
        };

        let allow_anonymous = self.config.allow_anonymous;
        if allow_anonymous {
            log::warn!("Anonymous mode enabled - unknown keys will be auto-registered");
        }

        // Create shared MCP server pool and pre-initialize from config.
        // This avoids blocking kernel creation on slow MCP servers.
        let mcp_pool = Arc::new(McpServerPool::new());
        Self::start_mcp_initialization(mcp_pool.clone(), self.config.config_dir.clone());

        // Create the shared kernel at server startup — 会の場所 (the meeting place).
        // All connections share this single kernel.
        let shared_kernel = crate::rpc::create_shared_kernel(
            &mcp_pool,
            self.config.config_dir.as_deref(),
            self.config.data_dir.as_deref(),
        ).await.map_err(|e| std::io::Error::other(format!("Failed to create shared kernel: {}", e)))?;

        let registry = Arc::new(ServerRegistry {
            kernel: shared_kernel,
            mcp_pool,
        });

        log::info!("Shared kernel created: {}", registry.kernel.name);

        let mut server = Server {
            auth_db: Arc::new(Mutex::new(auth_db)),
            allow_anonymous,
            registry,
        };

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
    /// Shared kernel and MCP pool (created at server startup)
    registry: Arc<ServerRegistry>,
}

impl server::Server for Server {
    type Handler = ConnectionHandler;

    fn new_client(&mut self, peer_addr: Option<SocketAddr>) -> Self::Handler {
        ConnectionHandler::new(
            self.auth_db.clone(),
            peer_addr,
            self.allow_anonymous,
            self.registry.clone(),
        )
    }

    fn handle_session_error(&mut self, error: <Self::Handler as server::Handler>::Error) {
        log::error!("Session error: {:?}", error);
    }
}

/// Handler for a single SSH connection
struct ConnectionHandler {
    auth_db: Arc<Mutex<AuthDb>>,
    peer_addr: Option<SocketAddr>,
    allow_anonymous: bool,
    identity: Option<Principal>,
    /// Shared kernel and MCP pool (created at server startup)
    registry: Arc<ServerRegistry>,
    /// Channel index counter — only channel 1 (RPC) gets a handler thread
    channel_count: u32,
}

impl ConnectionHandler {
    fn new(
        auth_db: Arc<Mutex<AuthDb>>,
        peer_addr: Option<SocketAddr>,
        allow_anonymous: bool,
        registry: Arc<ServerRegistry>,
    ) -> Self {
        Self {
            auth_db,
            peer_addr,
            allow_anonymous,
            identity: None,
            registry,
            channel_count: 0,
        }
    }
}

/// Run Cap'n Proto RPC over an SSH channel stream.
///
/// Creates per-connection state and hands out a capability to the shared kernel.
async fn run_rpc(
    stream: russh::ChannelStream<Msg>,
    principal: Principal,
    registry: Arc<ServerRegistry>,
) {
    let stream = stream.compat();
    let (reader, writer) = futures::AsyncReadExt::split(stream);

    let connection = Rc::new(RefCell::new(ConnectionState::new(principal.clone())));
    let world = WorldImpl::new(registry, connection);
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
        principal.username,
        principal.display_name
    );
    if let Err(e) = rpc_system.await {
        log::error!("RPC system error for {}: {}", principal.username, e);
    }
    log::info!("RPC session ended for {}", principal.username);
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
        let principal = match &self.identity {
            Some(id) => id.clone(),
            None => {
                log::warn!("Channel open without authentication");
                return Ok(false);
            }
        };

        let channel_index = self.channel_count;
        self.channel_count += 1;

        log::info!(
            "Channel {} (index {}) opened for {} ({})",
            channel.id(),
            channel_index,
            principal.username,
            principal.display_name
        );

        // Only channel 1 (RPC) gets a handler thread. Channels 0 (control)
        // and 2 (events) are accepted but don't need their own RPC system —
        // they're retained by the client to keep the SSH connection alive.
        if channel_index != 1 {
            log::debug!(
                "Channel {} (index {}) accepted without RPC handler",
                channel.id(),
                channel_index,
            );
            // Channel is kept alive by the SSH session; no thread needed.
            return Ok(true);
        }

        let stream = channel.into_stream();
        let registry = self.registry.clone();

        // Spawn RPC handler in a separate thread (capnp-rpc requires LocalSet)
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime for RPC");
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                run_rpc(stream, principal, registry).await;
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
            Ok(Some(principal)) => {
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
                    principal.username,
                    principal.display_name,
                    peer,
                    fingerprint
                );

                self.identity = Some(principal);

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
                        db.add_key_auto_principal(&key, Some(&safe_user_clone), Some(&safe_user_clone))
                    })
                    .await
                    .map_err(|e| {
                        log::error!("spawn_blocking panicked: {}", e);
                        russh::Error::Disconnect
                    })?;

                    match result {
                        Ok((principal_id, _fingerprint)) => {
                            let db = self.auth_db.clone();
                            let principal_result = tokio::task::spawn_blocking(move || {
                                db.lock().get_principal(principal_id)
                            })
                            .await
                            .map_err(|_| russh::Error::Disconnect)?;

                            if let Ok(Some(principal)) = principal_result {
                                log::info!(
                                    "Auth accepted (anonymous): {} from {} [{}]",
                                    principal.username,
                                    peer,
                                    fingerprint
                                );
                                self.identity = Some(principal);
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
        Ok(())
    }
}
