//! SSH server for kaijutsu
//!
//! Accepts SSH connections and provides Cap'n Proto RPC over channels.
//! Public key authentication with user identity from SQLite.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
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

use kaijutsu_types::{PrincipalId, SSH_RPC_SUBSYSTEM, SSH_SFTP_SUBSYSTEM, SSH_SHARE_SUBSYSTEM};

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
    /// Where every `/config` tree comes from (`crate::config_mounts`,
    /// `docs/config-namespace.md`). Required, and never `Option`: a missing
    /// value must not be able to resolve to the user's real config trees,
    /// which is what a test harness would silently write into.
    pub config_mounts: crate::config_mounts::ConfigMounts,
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

/// `~/.config/kaijutsu/config/rc` — the rc tree's default host directory.
///
/// Kept as a named function because callers ask for exactly this one, but it
/// is now derived rather than decided here: the config root is the one place a
/// default location is chosen (`ConfigMounts::default_root`), and every tree
/// falls out of it.
pub fn default_rc_dir() -> PathBuf {
    crate::config_mounts::ConfigMounts::new(
        crate::config_mounts::ConfigMounts::default_root(),
    )
    .host_dir(kaijutsu_types::paths::RC_ROOT)
}

/// Pre-seed `rc_root` from the embedded defaults and leave every
/// `create/S50-lfm2d.kai` present but empty.
///
/// A test kernel must never reach a network classifier, and the seed
/// installs that hook on every seat that holds a shell — `default` included,
/// which is what every test context is. The file stays present so the
/// server's install-if-absent seed at startup leaves it alone (it names the
/// file as diverged rather than restoring it); it is empty so the create
/// lifecycle runs nothing for it. A test that wants a hook pushes one onto
/// the broker's table directly.
///
/// Fails loudly: a half-seeded test tree is a test that lies.
fn blank_network_scorer(rc_root: &Path) {
    kaijutsu_kernel::seed_scripts::ensure_rc_seed_files(rc_root)
        .expect("seed the ephemeral rc tree");
    let prefix = format!("{}/", kaijutsu_types::paths::RC_ROOT);
    for (canonical, _) in kaijutsu_kernel::seed_scripts::seed_files() {
        if !canonical.ends_with("/create/S50-lfm2d.kai") {
            continue;
        }
        let rel = canonical
            .strip_prefix(&prefix)
            .expect("every seed path lives under the rc root");
        let path = rc_root.join(rel);
        // A composed seed is a symlink to the shared body; replace the link
        // itself, never write through it into `lib`.
        if fs::symlink_metadata(&path).is_ok() {
            fs::remove_file(&path).expect("remove the seeded scorer link");
        }
        fs::write(&path, "").expect("blank the seeded scorer");
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
        let config_mounts = crate::config_mounts::ConfigMounts::new(path.join("config"));
        blank_network_scorer(&config_mounts.host_dir(kaijutsu_types::paths::RC_ROOT));

        Self {
            bind_addr: SocketAddr::from(([127, 0, 0, 1], port)),
            key_source: KeySource::Ephemeral,
            auth_db_path: None,
            allow_anonymous: true, // Tests need to accept any key
            config_dir: Some(path.clone()),
            config_mounts,
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
            config_mounts: crate::config_mounts::ConfigMounts::new(
                crate::config_mounts::ConfigMounts::default_root(),
            ),
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

/// On SIGTERM or SIGINT, run a WAL checkpoint on the kernel database and
/// exit 0. Holds only the database handle, never the `SharedKernel`, so the
/// clean-exit `Drop` path stays reachable. A failed handler install is
/// logged and the process keeps the default signal disposition.
fn spawn_signal_checkpoint(kernel_db: Arc<Mutex<kaijutsu_kernel::kernel_db::KernelDb>>) {
    use tokio::signal::unix::{SignalKind, signal};
    let (mut term, mut int) = match (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) {
        (Ok(t), Ok(i)) => (t, i),
        (Err(e), _) | (_, Err(e)) => {
            log::warn!("signal handler not installed; WAL is not checkpointed on stop: {e}");
            return;
        }
    };
    tokio::spawn(async move {
        let name = tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = int.recv() => "SIGINT",
        };
        log::info!("{name} received; checkpointing the kernel database before exit");
        match kernel_db.lock().checkpoint() {
            Ok((busy, _, _)) if busy != 0 => {
                log::warn!("{name} wal_checkpoint(TRUNCATE) busy; WAL left for next open");
            }
            Ok(_) => {}
            Err(e) => log::warn!("{name} wal_checkpoint failed: {e}"),
        }
        std::process::exit(0);
    });
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
        self.run_on_listener_inner(socket, None).await
    }

    /// Like [`Self::run_on_listener`], but hands the constructed
    /// `SharedKernel` back through `kernel_tx` right after it is built —
    /// before the server starts accepting connections.
    ///
    /// Test-only hook. Most wire behaviors have a real RPC surface a test
    /// can drive to produce a genuine event (e.g. a test using `hook_add` +
    /// `call_mcp_tool`). `LedgerFlow` has no such surface
    /// reachable without exercising the approval-ledger's rule/escalation
    /// machinery, which is out of this crate's territory — so
    /// `ledger_events_wire.rs` uses this to publish `LedgerFlow::Changed`
    /// directly onto the live kernel's own bus, proving the
    /// `subscribeLedgerEvents` bridge on the exact instance a connected
    /// client is talking to.
    #[doc(hidden)]
    pub async fn run_on_listener_with_kernel_sink(
        &self,
        socket: TcpListener,
        kernel_tx: tokio::sync::oneshot::Sender<crate::rpc::SharedKernel>,
    ) -> Result<(), std::io::Error> {
        self.run_on_listener_inner(socket, Some(kernel_tx)).await
    }

    async fn run_on_listener_inner(
        &self,
        socket: TcpListener,
        kernel_tx: Option<tokio::sync::oneshot::Sender<crate::rpc::SharedKernel>>,
    ) -> Result<(), std::io::Error> {
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
                log::warn!("Using ephemeral auth database (all keys accepted)");
                AuthDb::temporary().map_err(std::io::Error::other)?
            }
        };

        // A pre-melt `auth.db` still carries `principals.username NOT NULL
        // UNIQUE`. Opening it succeeds and existing fingerprints keep
        // authenticating, but every path that mints a principal — the seed
        // character, anonymous auto-register — fails on that constraint,
        // long after boot. Refuse here instead of running half a kernel.
        if auth_db.has_legacy_names().map_err(std::io::Error::other)? {
            return Err(std::io::Error::other(
                "auth database still carries the pre-melt 'username' column, so binding a \
                 new principal would fail. Stop the server, back up auth.db and kernel.db, \
                 then run: kaijutsu-server migrate-keyring",
            ));
        }

        // Check if database is empty
        if auth_db.is_empty().map_err(std::io::Error::other)? {
            log::warn!(
                "Auth database is empty! Bind a key to the seeded '{}' character: \
                 kaijutsu-server add-key <pubkey> --as {}",
                kaijutsu_kernel::seed_character::HAJIME,
                kaijutsu_kernel::seed_character::HAJIME,
            );
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

        // Create the shared kernel at server startup — 会の場所 (the meeting place).
        // All connections share this single kernel.
        let shared_kernel = crate::rpc::create_shared_kernel(
            self.config.config_dir.as_deref(),
            &self.config.config_mounts,
            self.config.data_dir.as_deref(),
        )
        .await
        .map_err(|e| std::io::Error::other(format!("Failed to create shared kernel: {}", e)))?;

        // Best-effort: a test that asked for the kernel handle but dropped
        // its receiver (or never awaited it) must not abort server startup.
        if let Some(kernel_tx) = kernel_tx {
            let _ = kernel_tx.send(shared_kernel.clone());
        }

        // systemd `stop` and a terminal Ctrl-C arrive as signals, and the
        // process dies without unwinding, so `SharedKernelState::drop` never
        // runs there. Checkpoint the WAL on the signal, then exit.
        spawn_signal_checkpoint(shared_kernel.kernel_db.clone());

        // External MCP servers (mcp.toml — kaibo, bevy_brp, …) start HERE,
        // not inside `create_shared_kernel`: they need the kernel's VFS
        // (mounted + frozen) and broker to exist first, which is why this
        // follows rather than precedes the call above. Keeping it on the
        // serving path, out of kernel construction, is deliberate — building
        // a kernel must never spawn host subprocesses as a side effect
        // (see `rpc::start_external_mcp_servers`).
        crate::rpc::start_external_mcp_servers(&shared_kernel.kernel).await;

        let auth_db = Arc::new(Mutex::new(auth_db));
        let registry = Arc::new(ServerRegistry {
            kernel: shared_kernel,
        });

        log::info!("Shared kernel created: {}", registry.kernel.name);

        // Bring the turn driver online before accepting connections so an
        // autonomous turn requested by an early `kj fork --prompt` isn't
        // dropped. One driver for the whole server (see spawn_turn_driver).
        crate::rpc::spawn_turn_driver(registry.clone());

        // The gate-resume driver, after the turn driver: it wakes a context
        // by publishing a turn request, and a request published with no
        // driver subscribed is dropped.
        crate::rpc::spawn_gate_resume_driver(registry.clone());

        // The single coalescing beat scheduler: drives per-context hyoushigi
        // timelines on their wall-clock beat (musician contexts). Installs its
        // ingress on the kernel so the rc lifecycle can arm/disarm musicians.
        crate::beat::spawn_beat_scheduler(registry.clone());

        // The single editor reconciler: pushes merged state to open editor
        // sessions when a *peer* writes a block one is bound to (see vi.md 1b).
        crate::rpc::spawn_editor_reconciler(registry.clone());

        let active_connections = Arc::new(AtomicUsize::new(0));
        log::info!("Max connections: {}", self.config.max_connections);

        let mut server = Server {
            auth_db,
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
    identity: Option<PrincipalId>,
    /// Shared kernel and MCP pool (created at server startup)
    registry: Arc<ServerRegistry>,
    /// Channels opened but not yet bound to a subsystem. `channel_open_session`
    /// stashes each one here; `subsystem_request` drains it and dispatches by
    /// name (`kaijutsu-rpc` today; SFTP and a debug shell later).
    pending_channels: HashMap<ChannelId, Channel<Msg>>,
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
            pending_channels: HashMap::new(),
            active_connections,
            max_connections,
            counted: false,
        }
    }

    /// Spawn the Cap'n Proto RPC handler for a channel on its own OS thread.
    ///
    /// capnp-rpc needs a current-thread runtime + `LocalSet`, so each RPC
    /// channel gets a dedicated thread. Returns `true` if the thread was
    /// spawned; the caller turns that into the SSH subsystem ack.
    ///
    /// Named so wedged threads show up identifiably in `ps -T` / `top -H`. We
    /// don't keep the `JoinHandle`: a wedged current_thread runtime can't be
    /// killed from outside safely. We rely on the per-task watchdog in
    /// `run_rpc` for diagnosability and the `conn_cancel` + per-callback
    /// timeouts inside `rpc.rs` to prevent the wedge in the first place. A
    /// panic on the RPC thread is logged but does not take down the server —
    /// the default panic hook plus this `catch_unwind` boundary contain damage
    /// to that one connection.
    fn spawn_rpc_thread(&self, channel: Channel<Msg>, principal: PrincipalId) -> bool {
        let stream = channel.into_stream();
        let registry = self.registry.clone();
        let short_id_for_thread = principal.short();
        let session_label = format!(
            "kjutsu-rpc-{}-{:?}",
            principal.short(),
            self.peer_addr.as_ref().map(|a| a.port()).unwrap_or(0),
        );

        // rc lifecycles (context create/fork, etc.) run on this session thread
        // and re-enter kaish deeply; the default 2 MiB stack is too small for
        // that nesting. Reserve a generous (virtual, commit-on-use) stack — see
        // `KAISH_RC_THREAD_STACK` and the beat-scheduler thread.
        let builder = std::thread::Builder::new()
            .name(session_label.clone())
            .stack_size(kaijutsu_kernel::KAISH_RC_THREAD_STACK);
        if let Err(e) = builder.spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    log::error!(
                        "Failed to build tokio runtime for {}: {}",
                        short_id_for_thread, e,
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
                    short_id_for_thread,
                    msg,
                );
            }
        }) {
            log::error!(
                "Failed to spawn RPC thread {}: {}",
                session_label, e,
            );
            return false;
        }

        true
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
    principal: PrincipalId,
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
    // Fired by a background task that decides this connection must go — today
    // only the block bridge, when the client's per-subscriber event queue
    // overflows. Taking the whole RPC session down (rather than quietly ending
    // one subscription) is what makes the client's existing
    // reconnect-with-full-resync path engage, and it works for every client
    // version, including binaries that predate `onSubscriptionTerminated`.
    let disconnect = connection.borrow().disconnect_token();
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
        "RPC session started for {} session={}",
        principal.short(),
        session_id.short(),
    );

    // Concurrent watchdog: logs if rpc_system stops responding. When
    // rpc_system completes (Ok, Err, or returns from drop), we cancel the
    // watchdog. If it doesn't complete because the LocalSet wedged, the
    // watchdog still runs and surfaces the problem in logs.
    let watchdog_cancel = tokio_util::sync::CancellationToken::new();
    let watchdog_token = watchdog_cancel.clone();
    let watchdog_id = principal.short();
    let watchdog_session = session_id;
    let watchdog_activity = last_activity.clone();
    let watchdog = tokio::task::spawn_local(async move {
        run_rpc_watchdog(
            watchdog_token,
            watchdog_id,
            watchdog_session,
            watchdog_activity,
        )
        .await
    });

    let rpc_result = tokio::select! {
        r = rpc_system => r,
        _ = disconnect.cancelled() => {
            log::warn!(
                "Dropping RPC session for {} session={}: a push subscription \
                 fell too far behind (see the FlowBus termination above). The \
                 client reconnects and resyncs — we do not serve a partial view.",
                principal.short(),
                session_id.short(),
            );
            Ok(())
        }
    };
    watchdog_cancel.cancel();
    let _ = watchdog.await;

    match rpc_result {
        Ok(()) => log::info!(
            "RPC session ended cleanly for {} session={}",
            principal.short(),
            session_id.short(),
        ),
        Err(e) => log::error!(
            "RPC system error for {} session={}: {}",
            principal.short(),
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
    principal_short_id: String,
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
                        principal_short_id,
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
                    principal.short(),
                    self.peer_addr,
                    current,
                    self.max_connections,
                );
                return Ok(false);
            }
            self.counted = true;
            log::debug!(
                "Connection accepted for {} ({:?}), active connections: {}",
                principal.short(),
                self.peer_addr,
                current + 1,
            );
        }

        // Stash the channel inert. It carries no traffic until the client
        // names a subsystem via `subsystem_request`, which drains the map and
        // dispatches by name. This retention-and-dispatch scaffold is shared by
        // every named tenant of the session-channel surface (RPC today; SFTP
        // and a debug shell later).
        let channel_id = channel.id();
        log::info!(
            "Channel {} opened for {}, awaiting subsystem request",
            channel_id,
            principal.short(),
        );
        self.pending_channels.insert(channel_id, channel);

        Ok(true)
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Every exit path must ack with channel_success or channel_failure:
        // the client requests with want_reply, so a silent return hangs it.
        let principal = match &self.identity {
            Some(id) => id.clone(),
            None => {
                log::warn!(
                    "Subsystem request {:?} on unauthenticated channel {}",
                    name,
                    channel,
                );
                // Unreachable in practice (auth precedes channel_open_session),
                // but drop any stashed channel so this path can't leak one.
                self.pending_channels.remove(&channel);
                session.channel_failure(channel)?;
                return Ok(());
            }
        };

        let chan = match self.pending_channels.remove(&channel) {
            Some(chan) => chan,
            None => {
                log::warn!(
                    "Subsystem request {:?} for unknown channel {} from {}",
                    name,
                    channel,
                    principal.short(),
                );
                session.channel_failure(channel)?;
                return Ok(());
            }
        };

        match name {
            SSH_RPC_SUBSYSTEM => {
                log::info!(
                    "Binding channel {} to {} for {}",
                    channel,
                    SSH_RPC_SUBSYSTEM,
                    principal.short(),
                );
                if self.spawn_rpc_thread(chan, principal) {
                    session.channel_success(channel)?;
                } else {
                    // Spawn failed; `chan` was consumed, drop closes it.
                    session.channel_failure(channel)?;
                }
                Ok(())
            }
            SSH_SFTP_SUBSYSTEM => {
                log::info!(
                    "Binding channel {} to {} for {}",
                    channel,
                    SSH_SFTP_SUBSYSTEM,
                    principal.short(),
                );
                // SFTP handler futures are `Send`, so unlike the capnp RPC path
                // this runs on the server's ambient runtime — no dedicated
                // thread. `run` spawns the per-connection processing loop and
                // returns; the loop owns the channel stream until EOF.
                let vfs = self.registry.kernel.kernel.vfs().clone();
                let session_handler = crate::sftp::SftpSession::new(principal, vfs);
                russh_sftp::server::run(chan.into_stream(), session_handler).await;
                session.channel_success(channel)?;
                Ok(())
            }
            SSH_SHARE_SUBSYSTEM => {
                log::info!(
                    "Binding channel {} to {} for {} — role swap: kernel plays SFTP client",
                    channel,
                    SSH_SHARE_SUBSYSTEM,
                    principal.short(),
                );
                // The role swap (`docs/slash-r.md`): the client just opened
                // this channel and is now serving its own SFTP `Handler` on
                // it, so the kernel plays the SFTP *client* role
                // (`RawSftpSession`) instead. Those futures are `Send` too —
                // same ambient-runtime placement as the forward adapter
                // above, no dedicated thread. Registration (reading /index,
                // validating shares, the live-duplicate-client-id check) runs
                // inside the spawned task, not here — `channel_success` must
                // fire immediately regardless of whether registration
                // eventually succeeds, or the client's `request_subsystem`
                // hangs waiting for an ack that a slow validation would
                // delay.
                let share_registry = self.registry.kernel.kernel.share_registry().clone();
                tokio::spawn(crate::share::run_share_session(
                    chan.into_stream(),
                    principal,
                    share_registry,
                ));
                session.channel_success(channel)?;
                Ok(())
            }
            other => {
                log::warn!(
                    "Unknown subsystem {:?} requested by {} on channel {}",
                    other,
                    principal.short(),
                    channel,
                );
                // Refuse the request, then close the channel so the client sees
                // prompt closure rather than an idle channel lingering to the
                // inactivity timeout. (channel_failure alone leaves it open per
                // the SSH spec.) `chan` drops here too.
                session.channel_failure(channel)?;
                session.close(channel)?;
                Ok(())
            }
        }
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
            Ok(Some(principal_id)) => {
                // Update last_used timestamp (fire and forget, non-blocking)
                let db = self.auth_db.clone();
                let fp = fingerprint.clone();
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = db.lock().update_last_used(&fp) {
                        log::warn!("Failed to update last_used for {}: {}", fp, e);
                    }
                });

                log::info!(
                    "Auth accepted: {} from {} [{}]",
                    principal_id.short(),
                    peer,
                    fingerprint
                );

                self.identity = Some(principal_id);

                Ok(Auth::Accept)
            }
            Ok(None) => {
                // Anonymous mode binds an unknown key to the seeded `hajime`
                // character rather than minting a principal for it
                // (`docs/character.md`, "Anonymous auto-register binds to
                // `hajime` instead of minting"). Every kernel seeds one, so
                // there is always somewhere to bind to.
                if self.allow_anonymous {
                    let kernel_db = self.registry.kernel.kernel_db.clone();
                    let hajime = tokio::task::spawn_blocking(move || {
                        kernel_db
                            .lock()
                            .get_character_by_name(kaijutsu_kernel::seed_character::HAJIME)
                    })
                    .await
                    .map_err(|e| {
                        log::error!("spawn_blocking panicked: {}", e);
                        russh::Error::Disconnect
                    })?;

                    let hajime_principal = match hajime {
                        Ok(Some(row)) => row.principal_id,
                        Ok(None) => {
                            log::error!(
                                "Anonymous auth rejected: no '{}' character seeded — every \
                                 kernel seeds one at startup",
                                kaijutsu_kernel::seed_character::HAJIME
                            );
                            return Ok(Auth::Reject {
                                proceed_with_methods: None,
                                partial_success: false,
                            });
                        }
                        Err(e) => {
                            log::error!("Failed to resolve '{}': {e}", kaijutsu_kernel::seed_character::HAJIME);
                            return Ok(Auth::Reject {
                                proceed_with_methods: None,
                                partial_success: false,
                            });
                        }
                    };

                    log::info!(
                        "Anonymous mode: binding key {} (ssh user={}) to {}",
                        fingerprint,
                        user,
                        kaijutsu_kernel::seed_character::HAJIME,
                    );

                    // Clone for spawn_blocking - use OpenSSH format for serialization
                    let db = self.auth_db.clone();
                    let key_openssh = public_key.to_openssh().map_err(|e| {
                        log::error!("Failed to serialize public key: {}", e);
                        russh::Error::Disconnect
                    })?;
                    let comment = format!("ssh user {user}");

                    let result = tokio::task::spawn_blocking(move || {
                        // Reconstruct the key from OpenSSH format
                        let key = ssh_key::PublicKey::from_openssh(&key_openssh).map_err(|e| {
                            rusqlite::Error::ToSqlConversionFailure(Box::new(
                                std::io::Error::other(format!("Failed to parse key: {}", e)),
                            ))
                        })?;
                        db.lock().add_key(hajime_principal, &key, Some(&comment))
                    })
                    .await
                    .map_err(|e| {
                        log::error!("spawn_blocking panicked: {}", e);
                        russh::Error::Disconnect
                    })?;

                    match result {
                        Ok(bound_fingerprint) => {
                            log::info!(
                                "Auth accepted (anonymous): {} bound to {} from {}",
                                kaijutsu_kernel::seed_character::HAJIME,
                                bound_fingerprint,
                                peer,
                            );
                            self.identity = Some(hajime_principal);
                            return Ok(Auth::Accept);
                        }
                        Err(e) => {
                            log::warn!("Failed to bind anonymous key to {}: {e}", kaijutsu_kernel::seed_character::HAJIME);
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
        // Drop any still-unbound channel so a client that opens then closes
        // without naming a subsystem doesn't leak an entry.
        self.pending_channels.remove(&channel);
        log::debug!("Channel {} closed", channel);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{AsyncReadExt, AsyncWriteExt};

    /// A test kernel must never reach a network classifier. The embedded
    /// seed installs the lfm2d advisory hook on every seat that holds a
    /// shell, so an ephemeral config pre-seeds its rc tree and leaves every
    /// `S50-lfm2d.kai` present but EMPTY: present, so the server's own
    /// install-if-absent seed leaves it alone (naming it as diverged);
    /// empty, so the create lifecycle runs nothing for it.
    ///
    /// Falsified by dropping the blanking: the file holds the seed body and
    /// the dry-run wire tests see `lfm2d-advisory` fire on a `default`
    /// context.
    #[test]
    fn an_ephemeral_config_carries_no_network_scorer() {
        let config = SshServerConfig::ephemeral(0);
        let rc_root = config
            .config_mounts
            .host_dir(kaijutsu_types::paths::RC_ROOT);
        let blanked: Vec<PathBuf> = kaijutsu_kernel::seed_scripts::seed_files()
            .into_iter()
            .map(|(canonical, _)| canonical)
            .filter(|p| p.ends_with("/create/S50-lfm2d.kai"))
            .map(|p| rc_root.join(p.trim_start_matches(&format!("{}/", kaijutsu_types::paths::RC_ROOT))))
            .collect();
        assert!(
            blanked.len() >= 3,
            "the seed installs the scorer on at least coder, mcp and default: {blanked:?}"
        );
        for path in &blanked {
            let body = fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("{} must exist in an ephemeral tree: {e}", path.display()));
            assert!(body.is_empty(), "{} must be blank, got {body:?}", path.display());
        }
        // The server's startup seed must not bring the hook back.
        let report = kaijutsu_kernel::seed_scripts::reseed_rc_files(&rc_root, false).unwrap();
        assert_eq!(report.written, 0, "nothing is absent after the pre-seed: {report:?}");
        for path in &blanked {
            let rel = path.strip_prefix(&rc_root).unwrap().to_string_lossy().to_string();
            assert!(
                report.diverged.iter().any(|d| d == &rel),
                "the blanked {rel} must be named as diverged, not silently kept: {report:?}"
            );
            assert!(fs::read_to_string(path).unwrap().is_empty(), "{rel} was restored");
        }
    }

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
