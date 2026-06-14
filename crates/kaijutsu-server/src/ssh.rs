//! SSH server for kaijutsu
//!
//! Accepts SSH connections and provides Cap'n Proto RPC over channels.
//! Public key authentication with user identity from SQLite.

use std::cell::{Cell, RefCell};
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use capnp_rpc::{RpcSystem, rpc_twoparty_capnp, twoparty};
use parking_lot::Mutex;
use russh::keys::PrivateKey;
use russh::keys::ssh_key::{self, HashAlg};
use russh::server::{self, Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId};
use tokio::net::TcpListener;
use tokio_util::compat::TokioAsyncReadCompatExt;

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
                PrivateKey::random(&mut rand_v10::rng(), russh::keys::Algorithm::Ed25519)
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
        PrivateKey::from_openssh(&key_data)
            .map_err(|e| std::io::Error::other(format!("Failed to parse host key: {}", e)))
    } else {
        log::info!("Generating new host key at {}", path.display());

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let key = PrivateKey::random(&mut rand_v10::rng(), russh::keys::Algorithm::Ed25519)
            .map_err(std::io::Error::other)?;

        // Save in OpenSSH format
        let key_pem = key
            .to_openssh(ssh_key::LineEnding::LF)
            .map_err(|e| std::io::Error::other(format!("Failed to serialize host key: {}", e)))?;
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
    /// Maximum number of concurrent SSH connections. Default: 100.
    pub max_connections: usize,
    /// RAII guard for an `ephemeral()` test dir: removes the dir when the config
    /// (and so the server task that owns it) is dropped, so repeated local test
    /// runs don't accumulate dirs in `/tmp`. `None` for production / explicit-dir
    /// configs. `Arc` so the config stays `Clone` (the dir lives until the last
    /// clone drops).
    _cleanup: Option<std::sync::Arc<TempDirGuard>>,
}

/// Removes its directory on drop. A tiny owned guard so `ephemeral()` test
/// configs self-clean (no leaked `/tmp` dirs across repeated local runs)
/// WITHOUT pulling `tempfile` — a dev-dependency — into the production
/// dependency tree just for a test-support constructor.
struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        // Best-effort: a failed cleanup must never panic a dropping server.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl SshServerConfig {
    /// Create config with an ephemeral key (for testing).
    ///
    /// Uses in-memory auth database and allows anonymous connections.
    pub fn ephemeral(port: u16) -> Self {
        // Use a fresh tempdir so no real configs (mcp.toml etc.) load. The name is
        // unique by construction: PID (cross-process) + timestamp (cross-run) + a
        // process-wide atomic counter so two `ephemeral()` calls that land in the
        // same SystemTime tick (parallel tests in one binary) NEVER share a dir — a
        // shared data_dir means two kernels open the same SQLite DB and
        // contend/cross-contaminate. The TempDirGuard removes the dir when this
        // config (owned by the server task) drops, so repeated local runs don't
        // pile dirs into /tmp (the inode leak that surfaced as the shell-var flake).
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kaijutsu-test-{}-{}-{}",
            std::process::id(),
            stamp,
            seq
        ));
        std::fs::create_dir_all(&path).ok();

        Self {
            bind_addr: SocketAddr::from(([127, 0, 0, 1], port)),
            key_source: KeySource::Ephemeral,
            auth_db_path: None,
            allow_anonymous: true, // Tests need to accept any key
            config_dir: Some(path.clone()),
            data_dir: Some(path.clone()),
            max_connections: 100,
            _cleanup: Some(std::sync::Arc::new(TempDirGuard(path))),
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
            max_connections: 100,
            _cleanup: None,
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
            // Server-side keepalive: emit SSH_MSG_GLOBAL_REQUEST every 30s and
            // tear down the session if 3 in a row go unanswered. Without this,
            // a silently-vanished client (TCP half-open after NAT timeout, app
            // crash without graceful shutdown) leaves the per-connection RPC
            // thread running and its FlowBus bridge holding subscriptions.
            // 30s × 3 = ~90s upper bound on dead-peer detection, matched to
            // the client-side keepalive in kaijutsu-client::constants.
            keepalive_interval: Some(std::time::Duration::from_secs(30)),
            keepalive_max: 3,
            ..Default::default()
        };

        let allow_anonymous = self.config.allow_anonymous;
        if allow_anonymous {
            log::warn!("Anonymous mode enabled - unknown keys will be auto-registered");
        }

        // External MCP pool pre-init removed in Phase 1 M5; a Phase 2
        // replacement will run ExternalMcpServer startup from mcp.toml
        // via the broker.

        // Create the shared kernel at server startup — 会の場所 (the meeting place).
        // All connections share this single kernel.
        let shared_kernel = crate::rpc::create_shared_kernel(
            self.config.config_dir.as_deref(),
            self.config.data_dir.as_deref(),
        )
        .await
        .map_err(|e| std::io::Error::other(format!("Failed to create shared kernel: {}", e)))?;

        let registry = Arc::new(ServerRegistry {
            kernel: shared_kernel,
        });

        log::info!("Shared kernel created: {}", registry.kernel.name);

        // Bring the turn driver online before accepting connections so an
        // autonomous turn requested by an early `kj fork --prompt` isn't
        // dropped. One driver for the whole server (see spawn_turn_driver).
        crate::rpc::spawn_turn_driver(registry.clone());

        // The single coalescing beat scheduler: drives per-context hyoushigi
        // timelines on their wall-clock beat (composer contexts). Installs its
        // ingress on the kernel so the rc lifecycle can arm/disarm composers.
        crate::beat::spawn_beat_scheduler(registry.clone());

        let active_connections = Arc::new(AtomicUsize::new(0));
        log::info!("Max connections: {}", self.config.max_connections);

        let mut server = Server {
            auth_db: Arc::new(Mutex::new(auth_db)),
            allow_anonymous,
            registry,
            active_connections,
            max_connections: self.config.max_connections,
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
    /// Number of currently active SSH connections.
    active_connections: Arc<AtomicUsize>,
    /// Maximum allowed concurrent connections.
    max_connections: usize,
}

impl server::Server for Server {
    type Handler = ConnectionHandler;

    fn new_client(&mut self, peer_addr: Option<SocketAddr>) -> Self::Handler {
        ConnectionHandler::new(
            self.auth_db.clone(),
            peer_addr,
            self.allow_anonymous,
            self.registry.clone(),
            self.active_connections.clone(),
            self.max_connections,
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
    /// Shared counter of active connections (decremented on drop).
    active_connections: Arc<AtomicUsize>,
    /// Maximum allowed concurrent connections.
    max_connections: usize,
    /// Whether this handler has been counted in active_connections.
    counted: bool,
}

impl ConnectionHandler {
    fn new(
        auth_db: Arc<Mutex<AuthDb>>,
        peer_addr: Option<SocketAddr>,
        allow_anonymous: bool,
        registry: Arc<ServerRegistry>,
        active_connections: Arc<AtomicUsize>,
        max_connections: usize,
    ) -> Self {
        Self {
            auth_db,
            peer_addr,
            allow_anonymous,
            identity: None,
            registry,
            channel_count: 0,
            active_connections,
            max_connections,
            counted: false,
        }
    }
}

impl Drop for ConnectionHandler {
    fn drop(&mut self) {
        if self.counted {
            let prev = self.active_connections.fetch_sub(1, Ordering::Relaxed);
            log::debug!(
                "Connection closed for {:?}, active connections: {}",
                self.peer_addr,
                prev - 1,
            );
        }
    }
}

/// Run Cap'n Proto RPC over an SSH channel stream.
///
/// Creates per-connection state and hands out a capability to the shared kernel.
///
/// Wedge defenses (the SSH/RPC connection from 2026-05-10):
///   * `ConnectionState::Drop` cancels a per-connection token, so any
///     `spawn_local` background task (FlowBus bridges, peer-invoke bridge)
///     observes shutdown via `tokio::select!` rather than pinning the LocalSet.
///   * `session_contexts` cleanup also lives in that Drop — used to live as
///     an explicit `remove(&session_id)` here, but that line never ran when
///     `rpc_system.await` got stuck.
///   * A liveness watchdog (`run_watchdog`) logs at warning level every
///     `RPC_WATCHDOG_INTERVAL` while the RPC system has not returned. Without
///     thread injection there is no safe way to force-kill a wedged
///     `current_thread` runtime from outside; the watchdog is for diagnosis.
async fn run_rpc(
    stream: russh::ChannelStream<Msg>,
    principal: Principal,
    registry: Arc<ServerRegistry>,
) {
    // Stamp a liveness timestamp on every byte that moves in either
    // direction, so the watchdog can tell a healthy long-lived session
    // (traffic flowing) from a genuinely stalled one (open but silent).
    let last_activity = Rc::new(Cell::new(Instant::now()));
    let stream = ActivityStream::new(stream.compat(), last_activity.clone());
    let (reader, writer) = futures::AsyncReadExt::split(stream);

    let session_contexts = registry.kernel.session_contexts.clone();
    let connection = Rc::new(RefCell::new(ConnectionState::new(
        principal.clone(),
        session_contexts.clone(),
    )));
    let session_id = connection.borrow().session_id;
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
        "RPC session started for {} ({}) session={}",
        principal.username,
        principal.display_name,
        session_id.short(),
    );

    // Concurrent watchdog: logs if rpc_system stops responding. When
    // rpc_system completes (Ok, Err, or returns from drop), we cancel the
    // watchdog. If it doesn't complete because the LocalSet wedged, the
    // watchdog still runs and surfaces the problem in logs.
    let watchdog_cancel = tokio_util::sync::CancellationToken::new();
    let watchdog_token = watchdog_cancel.clone();
    let watchdog_username = principal.username.clone();
    let watchdog_session = session_id;
    let watchdog_activity = last_activity.clone();
    let watchdog = tokio::task::spawn_local(async move {
        run_rpc_watchdog(
            watchdog_token,
            watchdog_username,
            watchdog_session,
            watchdog_activity,
        )
        .await
    });

    let rpc_result = rpc_system.await;
    watchdog_cancel.cancel();
    let _ = watchdog.await;

    match rpc_result {
        Ok(()) => log::info!(
            "RPC session ended cleanly for {} session={}",
            principal.username,
            session_id.short(),
        ),
        Err(e) => log::error!(
            "RPC system error for {} session={}: {}",
            principal.username,
            session_id.short(),
            e,
        ),
    }
    // session_contexts cleanup lives in ConnectionState::Drop (RAII) so it
    // can't be skipped when the future is dropped without completing.
}

/// How often the watchdog wakes to check connection liveness.
const RPC_WATCHDOG_INTERVAL: Duration = Duration::from_secs(60);

/// Only warn once a connection has been *open but silent* this long.
///
/// Sits comfortably above the SSH keepalive's dead-peer detection window
/// (`keepalive_interval` 30s × `keepalive_max` 3 ≈ 90s, see the server
/// `Config`): a peer that vanishes is reaped by keepalive — its transport
/// EOFs, `rpc_system` returns, and the watchdog is cancelled — before it
/// ever crosses this threshold. So a warn here means the connection is still
/// open and still passing keepalive yet moving no RPC bytes: a genuine stall
/// worth surfacing, not the routine long-lived session that the old
/// "still active" line warned about every minute.
const RPC_IDLE_WARN_THRESHOLD: Duration = Duration::from_secs(120);

/// Decide whether an idle duration warrants a stall warning. Pulled out so
/// the boundary is unit-testable without driving the whole watchdog loop.
fn should_warn_idle(idle: Duration) -> bool {
    idle >= RPC_IDLE_WARN_THRESHOLD
}

/// Watchdog companion to `run_rpc`. Warns only when the RPC connection is
/// *open but stalled* — no bytes moving in either direction for
/// [`RPC_IDLE_WARN_THRESHOLD`].
///
/// `last_activity` is stamped by [`ActivityStream`] on every read/write, so a
/// healthy long-lived session keeps it fresh and never warns; a wedged
/// `rpc_system` (the failure mode this codebase has hit before) stops moving
/// bytes and trips the warn. Cancelled by the parent when `rpc_system`
/// returns. If the LocalSet itself wedges the watchdog goes quiet too — that
/// silence is itself the signal that the wedge is at the executor level.
async fn run_rpc_watchdog(
    cancel: tokio_util::sync::CancellationToken,
    username: String,
    session_id: kaijutsu_types::SessionId,
    last_activity: Rc<Cell<Instant>>,
) {
    let mut tick = tokio::time::interval(RPC_WATCHDOG_INTERVAL);
    tick.tick().await; // first tick fires immediately; drop it
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tick.tick() => {
                let idle = last_activity.get().elapsed();
                if should_warn_idle(idle) {
                    log::warn!(
                        "RPC session for {} session={} open but idle for {:?} \
                         (no RPC traffic) — possible stall",
                        username,
                        session_id.short(),
                        idle,
                    );
                }
            }
        }
    }
}

/// Wraps an `AsyncRead + AsyncWrite` stream, stamping `last_activity` whenever
/// bytes actually move in either direction. This is the RPC watchdog's
/// liveness signal: it distinguishes a healthy long-lived connection (traffic
/// flowing) from one that is open but stalled. A zero-byte poll (EOF, or a
/// spurious wakeup) is deliberately *not* counted as activity — otherwise a
/// dead reader spinning on EOF would look alive.
struct ActivityStream<S> {
    inner: S,
    last_activity: Rc<Cell<Instant>>,
}

impl<S> ActivityStream<S> {
    fn new(inner: S, last_activity: Rc<Cell<Instant>>) -> Self {
        Self {
            inner,
            last_activity,
        }
    }
}

impl<S: futures::io::AsyncRead + Unpin> futures::io::AsyncRead for ActivityStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(n)) = &result
            && *n > 0
        {
            this.last_activity.set(Instant::now());
        }
        result
    }
}

impl<S: futures::io::AsyncWrite + Unpin> futures::io::AsyncWrite for ActivityStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &result
            && *n > 0
        {
            this.last_activity.set(Instant::now());
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
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

        // On the first channel open for this connection, claim a slot in the
        // active-connections counter. Reject if we're at capacity.
        if !self.counted {
            let current = self.active_connections.fetch_add(1, Ordering::Relaxed);
            if current >= self.max_connections {
                self.active_connections.fetch_sub(1, Ordering::Relaxed);
                log::warn!(
                    "Connection rejected for {} ({:?}): at capacity ({}/{})",
                    principal.username,
                    self.peer_addr,
                    current,
                    self.max_connections,
                );
                return Ok(false);
            }
            self.counted = true;
            log::debug!(
                "Connection accepted for {} ({:?}), active connections: {}",
                principal.username,
                self.peer_addr,
                current + 1,
            );
        }

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
        let username_for_thread = principal.username.clone();
        let session_label = format!(
            "kjutsu-rpc-{}-{:?}",
            principal.username,
            self.peer_addr.as_ref().map(|a| a.port()).unwrap_or(0),
        );

        // Spawn RPC handler in a separate thread (capnp-rpc requires LocalSet).
        //
        // Named so wedged threads show up identifiably in `ps -T` / `top -H`.
        // We don't keep the JoinHandle: a wedged current_thread runtime can't
        // be killed from outside safely. We rely on the per-task watchdog in
        // `run_rpc` for diagnosability and the conn_cancel + per-callback
        // timeouts inside `rpc.rs` to prevent the wedge in the first place.
        // A panic on the RPC thread is logged but does not take down the
        // server — the default panic hook plus this catch_unwind boundary
        // contain damage to that one connection.
        let builder = std::thread::Builder::new().name(session_label.clone());
        if let Err(e) = builder.spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    log::error!(
                        "Failed to build tokio runtime for {}: {}",
                        username_for_thread, e,
                    );
                    return;
                }
            };
            let local = tokio::task::LocalSet::new();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                local.block_on(&rt, async move {
                    run_rpc(stream, principal, registry).await;
                });
            }));
            if let Err(panic) = result {
                let msg = panic.downcast_ref::<&'static str>().copied()
                    .or_else(|| panic.downcast_ref::<String>().map(|s| s.as_str()))
                    .unwrap_or("<non-string panic payload>");
                log::error!(
                    "RPC thread for {} panicked: {}",
                    username_for_thread,
                    msg,
                );
            }
        }) {
            log::error!(
                "Failed to spawn RPC thread {}: {}",
                session_label, e,
            );
            return Ok(false);
        }

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

                    const RESERVED: &[&str] = &["root", "admin", "system", "nobody", "daemon"];
                    if RESERVED.contains(&safe_user.as_str())
                        || safe_user.chars().all(|c| c.is_ascii_digit())
                    {
                        log::warn!("Anonymous auth rejected: reserved username '{safe_user}'");
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
                        let key = ssh_key::PublicKey::from_openssh(&key_openssh).map_err(|e| {
                            rusqlite::Error::ToSqlConversionFailure(Box::new(
                                std::io::Error::other(format!("Failed to parse key: {}", e)),
                            ))
                        })?;
                        let mut db = db.lock();
                        db.add_key_auto_principal(
                            &key,
                            Some(&safe_user_clone),
                            Some(&safe_user_clone),
                        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{AsyncReadExt, AsyncWriteExt};

    /// An `Instant` far enough in the past that any real `Instant::now()`
    /// taken during the test is strictly newer — lets us assert "got stamped"
    /// without sleeping.
    fn stale_instant() -> Instant {
        Instant::now() - Duration::from_secs(3600)
    }

    #[tokio::test]
    async fn activity_stream_stamps_on_read() {
        let last = Rc::new(Cell::new(stale_instant()));
        let before = last.get();
        let mut stream = ActivityStream::new(futures::io::Cursor::new(vec![1u8, 2, 3, 4]), last.clone());

        let mut buf = [0u8; 4];
        let n = stream.read(&mut buf).await.unwrap();

        assert_eq!(n, 4);
        assert!(last.get() > before, "a non-empty read must refresh last_activity");
    }

    #[tokio::test]
    async fn activity_stream_stamps_on_write() {
        let last = Rc::new(Cell::new(stale_instant()));
        let before = last.get();
        let mut stream = ActivityStream::new(futures::io::Cursor::new(Vec::new()), last.clone());

        let n = stream.write(&[1u8, 2, 3]).await.unwrap();

        assert_eq!(n, 3);
        assert!(last.get() > before, "a non-empty write must refresh last_activity");
    }

    #[tokio::test]
    async fn activity_stream_does_not_stamp_on_eof() {
        let last = Rc::new(Cell::new(stale_instant()));
        let before = last.get();
        // Empty cursor: the first read is EOF (Ok(0)).
        let mut stream = ActivityStream::new(futures::io::Cursor::new(Vec::<u8>::new()), last.clone());

        let mut buf = [0u8; 4];
        let n = stream.read(&mut buf).await.unwrap();

        assert_eq!(n, 0);
        assert_eq!(
            last.get(),
            before,
            "an EOF (zero-byte) read must NOT count as activity",
        );
    }

    #[test]
    fn should_warn_idle_only_past_threshold() {
        assert!(!should_warn_idle(Duration::from_secs(0)));
        assert!(!should_warn_idle(RPC_IDLE_WARN_THRESHOLD - Duration::from_secs(1)));
        assert!(should_warn_idle(RPC_IDLE_WARN_THRESHOLD));
        assert!(should_warn_idle(RPC_IDLE_WARN_THRESHOLD + Duration::from_secs(1)));
    }
}
