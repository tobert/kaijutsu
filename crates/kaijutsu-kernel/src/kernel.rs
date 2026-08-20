//! The Kernel: core primitive of kaijutsu.
//!
//! A kernel owns:
//! - A VFS (MountTable)
//! - State (variables, history, checkpoints)
//! - Tools (execution engines)
//! - LLM providers (for model access)
//! - Control plane (consent mode)

use async_trait::async_trait;
use kaijutsu_types::PrincipalId;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::RwLock;
use uuid::Uuid;

use kaijutsu_cas::FileStore;

use crate::peers::{InvokeRequest, PeerConfig, PeerError, PeerInfo, PeerRegistry};
use crate::control::ConsentMode;
use crate::drift::{SharedDriftRouter, shared_drift_router};
use crate::execution::{ExecContext, ExecResult};
use crate::flows::{
    SharedBlockFlowBus, SharedEditorFlowBus, SharedLedgerFlowBus, SharedTurnFlowBus,
    shared_block_flow_bus, shared_editor_flow_bus, shared_ledger_flow_bus, shared_turn_flow_bus,
};
use crate::llm::{LlmRegistry, Provider};
use crate::mcp::Broker;
use crate::state::KernelState;
use crate::vfs::{DirEntry, FileAttr, MountTable, SetAttr, StatFs, VfsOps, VfsResult};

/// The Kernel: fundamental primitive of kaijutsu.
///
/// Everything is a kernel. A kernel:
/// - Owns `/` in its VFS
/// - Can mount worktrees, repos, other kernels
/// - Has a consent mode (collaborative vs autonomous)
/// - Can checkpoint, fork, and thread
pub struct Kernel {
    /// Stable kernel identity — set at construction from the KernelDb
    /// singleton row, immutable thereafter. Used by the wire layer for
    /// `ping`/`bind_kernel` so clients can detect kernel changes.
    id: kaijutsu_types::KernelId,
    /// VFS mount table.
    vfs: Arc<MountTable>,
    /// Kernel state (behind RwLock for interior mutability).
    state: RwLock<KernelState>,
    /// LLM provider registry (behind RwLock for interior mutability).
    llm: RwLock<LlmRegistry>,
    /// Peer registry (behind RwLock for interior mutability).
    peers: RwLock<PeerRegistry>,
    /// Consent mode (collaborative vs autonomous).
    consent_mode: RwLock<ConsentMode>,
    /// FlowBus for block events.
    block_flows: SharedBlockFlowBus,
    /// FlowBus for autonomous turn requests (headless drive). Kernel-side
    /// callers publish here; the server drains it and runs the LLM turn.
    turn_flows: SharedTurnFlowBus,
    /// DriftRouter for cross-context communication.
    drift: SharedDriftRouter,
    /// Content-addressed store for binary blobs (images, etc.).
    cas: Arc<FileStore>,
    /// `/r` client-shares registry (`docs/slash-r.md`) — session-scoped,
    /// in-memory bookkeeping of live reverse-SFTP sessions. Shared between
    /// the `ShareFs` `VfsOps` backend mounted at `/r` (routing/serving) and
    /// the SSH server's `kaijutsu-share` subsystem arm (registration on
    /// channel-up, unregistration on channel-down) — one `Arc`, two
    /// consumers, no other coupling between them.
    share_registry: Arc<crate::vfs::ShareRegistry>,
    /// Sink-fed MIDI presence (`docs/midi-next.md` "Presence is sink-fed").
    /// **Ephemeral on purpose**: in-memory only, so a restarted kernel with no
    /// sinks connected truthfully knows nothing about what is plugged in.
    /// Written solely by `reportMidiPresence` (the app matches, the kernel
    /// records); read by `kj midi list/show` and, through the read-only
    /// `MidiPresenceFs` mounted at `/run/midi`, by kaish/kai/file tools.
    midi_presence: Arc<crate::midi_presence::MidiPresenceStore>,
    /// Connection → MIDI exchange channel (`docs/midi-next.md` "SysEx: the
    /// exchange pattern"). The addressed counterpart to `midi_presence`'s
    /// records: presence says WHICH connection has a device, this says how to
    /// ask that connection a question. Ephemeral and connection-bound for the
    /// same reason — a sink that goes away can't answer.
    midi_exchange: Arc<crate::midi_exchange::MidiExchangeRegistry>,
    /// Image generation backend registry.
    image_backends: RwLock<crate::image::ImageBackendRegistry>,
    /// MCP-centric tool broker (Phase 1; sits alongside the old `tools`
    /// registry until M4 swaps call sites).
    broker: Arc<Broker>,
    /// Kernel-wide timeout policy: kaish-script bounds, LLM streaming,
    /// MCP connect/handshake. Per-instance MCP `call_timeout` overrides live
    /// on `InstancePolicy`.
    timeouts: kaijutsu_types::TimeoutPolicy,
    /// The kernel's block store. Owned at construction (`new`/`with_flows`
    /// take it as a parameter) rather than wired in after the fact — a
    /// `Kernel` that hands out `file_cache()` before anything else can reach
    /// it is structurally impossible, since both are built together in the
    /// constructor. `blocks()` exposes it so callers take the kernel's
    /// instance instead of building a second one.
    blocks: crate::block_store::SharedBlockStore,
    /// Shared file-document cache, built at construction over `blocks` +
    /// the kernel VFS + the `KernelDb` handle passed to `new`/`with_flows`.
    /// Both the MCP `builtin.file` tools and the kaish `MountBackend`
    /// resolve through this one instance so a single real file maps to a
    /// single kernel document regardless of surface — there is exactly one
    /// `FileDocumentCache` per kernel, never a lazily-built second one.
    file_cache: Arc<crate::file_tools::FileDocumentCache>,
    /// Direct handle to the same `KernelDb` that `blocks` and `file_cache`
    /// are built over — a clone of the one `Arc<Mutex<>>` passed to
    /// `new`/`with_flows`, not a second store. Exposed so a mount like
    /// `/v/swap` (docs/file-buffers.md) that needs a durable-table query
    /// neither `blocks()` nor `file_cache()` surface (`list_dirty_file_buffers`)
    /// can reach it without opening its own connection.
    db: Arc<parking_lot::Mutex<crate::kernel_db::KernelDb>>,
    /// Per-context hyoushigi timelines — the live open future for contexts that
    /// own a beat (musician, audio). A context is **armed** by inserting it here;
    /// a context with no entry (every coder) has no timeline and costs nothing.
    /// The beat scheduler in `kaijutsu-server` pumps these; the turn-completion
    /// handler schedules cells onto them. Sharded by `ContextId`, each behind a
    /// sync mutex (see [`SharedTimeline`]).
    timelines: dashmap::DashMap<kaijutsu_types::ContextId, crate::hyoushigi::SharedTimeline>,
    /// Per-**track** hyoushigi timelines — Stage 2 (`docs/tracks.md`): the clock's
    /// open future + committed score live on the TRACK now, not the producing
    /// context, so the timeline never leaves when a producer detaches/rotates
    /// (continuity is free) and N producers attached to one track share one open
    /// future serialized at this `SharedTimeline`'s lock (the per-track sequencer).
    /// Armed once on track creation; survives context detach; dropped only on track
    /// teardown. Coexists with `timelines` (per-context, for coders) during the
    /// Stage-2 cut. Keyed by `TrackId` like `timelines` is by `ContextId`.
    track_timelines: dashmap::DashMap<kaijutsu_types::TrackId, crate::hyoushigi::SharedTimeline>,
    /// Ingress to the beat scheduler. Installed by the server at startup (the
    /// scheduler lives there, since it needs the block store too). Kernel-side rc
    /// code arms/disarms musician contexts by sending here; absent in embedded /
    /// test setups with no scheduler, where sends are simply no-ops.
    beat_ingress: OnceLock<tokio::sync::mpsc::UnboundedSender<crate::hyoushigi::BeatRequest>>,
    /// RAII guard for a `new_ephemeral()` data dir: removes the throwaway dir
    /// when the kernel drops, so repeated test runs don't accumulate `kj-eph-*`
    /// dirs (each holding a full CAS + DB) in `/tmp`. `None` for kernels rooted
    /// at a caller-provided `data_dir` (production, embedded). `Arc` keeps the dir
    /// alive until the last clone of this guard drops.
    temp_cleanup: Option<std::sync::Arc<TempDirGuard>>,
    /// Open in-app editor sessions (`vi`/`kj editor`). The registry is
    /// kernel-owned so any peer can drive it and the app renders it. Behind a
    /// sync mutex because every editor op is synchronous — modalkit's `!Send`
    /// `EditorCore` never crosses an await (see [`crate::editor::SendSessions`]).
    editor_sessions: parking_lot::Mutex<crate::editor::SendSessions>,
    /// FlowBus for editor-session state changes — the push channel the app
    /// renders from. `editor_keys`/`editor_save` publish `StateChanged`,
    /// `editor_quit` publishes `Closed`; the server's `subscribe_editor` bridge
    /// serializes these onto the `EditorEvents` capnp callback.
    editor_flows: SharedEditorFlowBus,
    /// FlowBus for approval-ledger change notifications — the push channel
    /// that replaces polling `kj ledger list` and hoping. Published by
    /// [`Self::notify_ledger_changed`] after a ledger mutation commits;
    /// the server's `subscribe_ledger_events` bridge coalesces and forwards
    /// them onto the `LedgerEvents` capnp callback.
    ///
    /// Its own bus rather than a topic on `block_flows` because a ledger
    /// change belongs to no context — the whole reason `subscribeLedgerEvents`
    /// is kernel-wide — and the context feed's delivery filter drops anything
    /// that cannot name one.
    ledger_flows: SharedLedgerFlowBus,
    /// Background host-process registry (`background_exec.rs`,
    /// `docs/issues.md` "Background shell + process management"). Kernel-owned
    /// (not per-materialized-shell) so a process started by one `shell`
    /// tool call is still queryable/killable from the next — see the module
    /// docs for the full ownership/cleanup contract.
    background: Arc<crate::background_exec::BackgroundRegistry>,
    /// The bound Claude Code peer inbox (`cc_inbox.rs`, `docs/cc-peer.md`
    /// "Order from here: kernel wiring of the inbox"). `OnceLock` like
    /// `beat_ingress`/`file_cache`: the server binds the real socket and
    /// installs it once at startup, after the kernel exists, so no test
    /// kernel binds one unless it explicitly asks — a socket bind on every
    /// `new`/`new_ephemeral`/`with_flows` call would race dozens of
    /// concurrently-running test kernels over the same runtime directory.
    cc_inbox: OnceLock<Arc<crate::cc_inbox::CcInboxHandle>>,
}

/// Removes its directory on drop. A tiny owned guard so `new_ephemeral()` test
/// kernels self-clean their throwaway data dir instead of leaking it for the
/// process lifetime (the `/tmp` inode accumulation that bites repeated local
/// test runs).
struct TempDirGuard(std::path::PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        // Best-effort: a failed cleanup must never panic a dropping kernel.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::fmt::Debug for Kernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kernel")
            .field("vfs", &self.vfs)
            .field("state", &"<locked>")
            .field("tools", &"<locked>")
            .field("llm", &"<locked>")
            .field("consent_mode", &"<locked>")
            .field("drift", &"<shared>")
            .finish()
    }
}

/// Per-subscription lossless queue depth for the kernel's flow buses.
///
/// Resolved once per kernel from [`crate::flows::FLOW_QUEUE_DEPTH_ENV`], falling
/// back to [`crate::flows::DEFAULT_ORDERED_QUEUE_DEPTH`]. It is the one knob on
/// the backpressure story: how much memory a stalled subscriber may hold before
/// it is cut off (see `flows.rs` for the sizing rationale).
fn default_flow_capacity() -> usize {
    crate::flows::configured_queue_depth()
}

/// The vi status-line text for a failed editor flush. One source for all three
/// editor flush sites (`editor_keys`' `Updated` and `Closed` arms,
/// `editor_save`) — the strings are published text a model and a human both
/// read, and three hand-copied versions drift.
///
/// W12 is vim's "changed since reading it": disk moved past the buffer's load
/// generation, so a plain `:w` refuses and `:w!` overrides.
/// `UnacknowledgedSwap` and `NotCached` each get their own message — every
/// non-W12 case used to collapse into one "E212: Can't open file for
/// writing" that also interpolated the `FlushError`'s own text, which for
/// `UnacknowledgedSwap` and `NotCached` names internal call sites
/// (`docs/audits/2026-08-20-editor-fileio.md` "BUGS B2"). Only `Backend`
/// (a genuine write/store failure) still reads as E212. `kernel_id` names
/// the `/v/swap` mount this kernel's own recovered buffers live under. See
/// `docs/file-buffers.md`.
fn flush_error_message(
    path: &str,
    err: &crate::file_tools::cache::FlushError,
    kernel_id: kaijutsu_types::KernelId,
) -> String {
    use crate::file_tools::cache::FlushError;
    match err {
        FlushError::DiskChanged { .. } => {
            format!("W12: {path} changed on disk since it was read (add ! to override)")
        }
        FlushError::UnacknowledgedSwap { .. } => format!(
            "{path}: a recovered unsaved buffer is waiting at {}/{}{path} \
             (docs/file-buffers.md) — `kj swap ack {path}` keeps it and allows \
             the write, or `kj swap discard {path}` drops it",
            kaijutsu_types::paths::SWAP_ROOT,
            kernel_id.to_hex(),
        ),
        FlushError::NotCached { .. } => format!(
            "{path}: the buffer was evicted from the file cache before this write \
             reached it — the edit was NOT saved; reopen the file and redo the change"
        ),
        FlushError::Backend(_) => format!("E212: Can't open file for writing: {path}: {err}"),
    }
}
impl Kernel {
    /// Resolve the CAS base path from a data_dir.
    fn cas_for_data_dir(data_dir: &Path) -> Arc<FileStore> {
        Arc::new(FileStore::at_path(data_dir.join("cas")))
    }

    /// Create a new kernel with the given name.
    ///
    /// `data_dir` is the kernel's on-disk data directory; the frontend owns
    /// resolving it (XDG, config flag, etc.) — the kernel never defaults it,
    /// so a process can't accidentally write into the user's real store. CAS
    /// lives at `{data_dir}/cas/` and creates directories lazily on first write.
    ///
    /// `blocks` and `db` are the kernel's block store and `KernelDb` handle —
    /// the caller builds them (so `blocks` can share a `FlowBus` with other
    /// components; see [`Self::with_flows`]) and `Kernel` owns them from this
    /// point on, using them to build the one [`FileDocumentCache`](crate::file_tools::FileDocumentCache)
    /// every surface shares (`file_cache()`).
    pub async fn new(
        name: impl Into<String>,
        data_dir: &Path,
        blocks: crate::block_store::SharedBlockStore,
        db: Arc<parking_lot::Mutex<crate::kernel_db::KernelDb>>,
    ) -> Self {
        let name = name.into();
        let vfs = Arc::new(MountTable::new());
        let file_cache = Arc::new(crate::file_tools::FileDocumentCache::new(
            blocks.clone(),
            vfs.clone(),
            db.clone(),
        ));

        Self {
            id: kaijutsu_types::KernelId::new(),
            vfs,
            state: RwLock::new(KernelState::new(&name)),
            llm: RwLock::new(LlmRegistry::new()),
            peers: RwLock::new(PeerRegistry::new()),
            consent_mode: RwLock::new(ConsentMode::default()),
            block_flows: shared_block_flow_bus(default_flow_capacity()),
            turn_flows: shared_turn_flow_bus(default_flow_capacity()),
            drift: shared_drift_router(),
            cas: Self::cas_for_data_dir(data_dir),
            share_registry: Arc::new(crate::vfs::ShareRegistry::new()),
            midi_presence: Arc::new(crate::midi_presence::MidiPresenceStore::new()),
            midi_exchange: Arc::new(crate::midi_exchange::MidiExchangeRegistry::new()),
            image_backends: RwLock::new(crate::image::ImageBackendRegistry::new()),
            broker: Arc::new({
                let b = Broker::new();
                b.engage_unbound_deny();
                b
            }),
            timeouts: kaijutsu_types::TimeoutPolicy::default(),
            blocks,
            file_cache,
            db,
            timelines: dashmap::DashMap::new(),
            track_timelines: dashmap::DashMap::new(),
            beat_ingress: OnceLock::new(),
            temp_cleanup: None,
            editor_sessions: parking_lot::Mutex::new(crate::editor::SendSessions(
                crate::editor::EditorSessions::new(),
            )),
            editor_flows: shared_editor_flow_bus(default_flow_capacity()),
            ledger_flows: shared_ledger_flow_bus(default_flow_capacity()),
            // spawn_reaper: a lightweight periodic sweep so terminal
            // background-process entries are reaped even if nothing ever
            // polls the registry again (e.g. a context is removed —
            // cancelling its processes — and no other context ever calls
            // list_background_processes/read_background_output afterward).
            // See background_exec.rs's BackgroundRegistry::spawn_reaper docs.
            background: {
                let bg = Arc::new(crate::background_exec::BackgroundRegistry::new());
                bg.spawn_reaper();
                bg
            },
            cc_inbox: OnceLock::new(),
        }
    }

    /// Create a kernel rooted at a throwaway, per-call temp directory.
    ///
    /// For tests and short-lived tooling that need a real on-disk `data_dir`
    /// but must never touch the user's XDG store or share CAS state with any
    /// other kernel. Each call mints a unique `kj-eph-<id>/` under the system
    /// temp dir, isolating every kernel from every other. The dir is removed
    /// when the kernel drops (a `TempDirGuard` on `temp_cleanup`), so repeated
    /// test runs don't accumulate `kj-eph-*` dirs in `/tmp`.
    ///
    /// Opens a real file-backed `KernelDb` under that same temp dir via
    /// `KernelDb::open` directly (never `KernelDb::temporary()` — this
    /// function already mints and owns the temp dir, so calling `temporary()`
    /// too would only nest a second, redundant one inside it) and builds its
    /// own block store over it, so callers that just want a working kernel
    /// need no db plumbing of their own. `blocks()`/`file_cache()` expose
    /// them for callers that need to share the same instances.
    pub async fn new_ephemeral(name: impl Into<String>) -> Self {
        let dir = std::env::temp_dir()
            .join(format!("kj-eph-{}", kaijutsu_types::KernelId::new().to_hex()));
        std::fs::create_dir_all(&dir).expect("create ephemeral kernel data dir");
        let db = Arc::new(parking_lot::Mutex::new(
            crate::kernel_db::KernelDb::open(dir.join("kernel.db"))
                .expect("open ephemeral kernel db"),
        ));
        let principal = PrincipalId::system();
        let ws = db
            .lock()
            .get_or_create_default_workspace(principal)
            .expect("create ephemeral kernel default workspace");
        let blocks = crate::block_store::shared_block_store_with_db(db.clone(), ws, principal);
        let mut kernel = Self::new(name, &dir, blocks, db).await;
        kernel.temp_cleanup = Some(std::sync::Arc::new(TempDirGuard(dir)));
        kernel
    }

    /// Create a new kernel with a shared FlowBus.
    ///
    /// Use this when you need to share the flow bus with other components
    /// (like BlockStore) before creating the kernel. `blocks` and `db` are the
    /// same pair `new` takes — the caller builds the block store (sharing
    /// `block_flows` with it), then builds the kernel with it.
    pub async fn with_flows(
        id: kaijutsu_types::KernelId,
        name: impl Into<String>,
        block_flows: SharedBlockFlowBus,
        data_dir: &Path,
        blocks: crate::block_store::SharedBlockStore,
        db: Arc<parking_lot::Mutex<crate::kernel_db::KernelDb>>,
    ) -> Self {
        let name = name.into();
        let vfs = Arc::new(MountTable::new());
        let file_cache = Arc::new(crate::file_tools::FileDocumentCache::new(
            blocks.clone(),
            vfs.clone(),
            db.clone(),
        ));

        Self {
            id,
            vfs,
            state: RwLock::new(KernelState::new(&name)),
            llm: RwLock::new(LlmRegistry::new()),
            peers: RwLock::new(PeerRegistry::new()),
            consent_mode: RwLock::new(ConsentMode::default()),
            block_flows,
            turn_flows: shared_turn_flow_bus(default_flow_capacity()),
            drift: shared_drift_router(),
            cas: Self::cas_for_data_dir(data_dir),
            share_registry: Arc::new(crate::vfs::ShareRegistry::new()),
            midi_presence: Arc::new(crate::midi_presence::MidiPresenceStore::new()),
            midi_exchange: Arc::new(crate::midi_exchange::MidiExchangeRegistry::new()),
            image_backends: RwLock::new(crate::image::ImageBackendRegistry::new()),
            broker: Arc::new({
                let b = Broker::new();
                b.engage_unbound_deny();
                b
            }),
            timeouts: kaijutsu_types::TimeoutPolicy::default(),
            blocks,
            file_cache,
            db,
            timelines: dashmap::DashMap::new(),
            track_timelines: dashmap::DashMap::new(),
            beat_ingress: OnceLock::new(),
            temp_cleanup: None,
            editor_sessions: parking_lot::Mutex::new(crate::editor::SendSessions(
                crate::editor::EditorSessions::new(),
            )),
            editor_flows: shared_editor_flow_bus(default_flow_capacity()),
            ledger_flows: shared_ledger_flow_bus(default_flow_capacity()),
            // spawn_reaper: a lightweight periodic sweep so terminal
            // background-process entries are reaped even if nothing ever
            // polls the registry again (e.g. a context is removed —
            // cancelling its processes — and no other context ever calls
            // list_background_processes/read_background_output afterward).
            // See background_exec.rs's BackgroundRegistry::spawn_reaper docs.
            background: {
                let bg = Arc::new(crate::background_exec::BackgroundRegistry::new());
                bg.spawn_reaper();
                bg
            },
            cc_inbox: OnceLock::new(),
        }
    }

    /// Stable kernel identity.
    pub fn id(&self) -> kaijutsu_types::KernelId {
        self.id
    }

    /// Get the MCP tool broker (Phase 1).
    pub fn broker(&self) -> &Arc<Broker> {
        &self.broker
    }

    /// The host `PATH` this kernel process started with — the `$PATH` seed for
    /// exec-granted shells (kaish never reads OS env itself; see
    /// `ExternalExec`). Captured once on first use and frozen: the kernel
    /// process env is stable, and a snapshot keeps every materialized shell
    /// seeing the same search path for the kernel's lifetime.
    pub fn host_path(&self) -> Option<&str> {
        static HOST_PATH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        HOST_PATH
            .get_or_init(|| std::env::var("PATH").ok())
            .as_deref()
    }

    /// Kernel-wide timeout policy. Read-only today; future revisions will
    /// load this from config and expose RPC mutation via the kj CLI.
    pub fn timeouts(&self) -> &kaijutsu_types::TimeoutPolicy {
        &self.timeouts
    }

    /// Builder-style override for the kernel-wide timeout policy. **Must be
    /// called pre-`Arc::new`** — consumes `self` so the type system rejects
    /// post-wrap mutation (production code holds `Arc<Kernel>` and can't get
    /// `&mut`, so a setter method would be unreachable in practice and
    /// misleading to future maintainers).
    ///
    /// Used today by `KjDispatcher::test_dispatcher_with_timeouts`; once the
    /// config load lands, it will use the same construction shape.
    pub fn with_timeouts(mut self, policy: kaijutsu_types::TimeoutPolicy) -> Self {
        self.timeouts = policy;
        self
    }

    /// Builder-style attach of a throwaway-dir cleanup guard (test support).
    /// The given dir is removed when the kernel drops — use it to root a kernel
    /// (and any sibling test scaffolding, e.g. a mounted `/etc/rc` tree) under
    /// one temp dir that self-cleans, instead of leaking it for the process
    /// lifetime. Must be called pre-`Arc::new` (consumes `self`, like
    /// `with_timeouts`). `new_ephemeral` sets this for you; this is for tests
    /// that root a kernel via `new`/`with_flows` at their own temp dir.
    pub fn with_temp_cleanup(mut self, dir: std::path::PathBuf) -> Self {
        self.temp_cleanup = Some(std::sync::Arc::new(TempDirGuard(dir)));
        self
    }

    /// Dispatch a tool call through the broker using the internal
    /// `ExecContext` call-site shape.
    ///
    /// This is the shim kaijutsu-server / kaijutsu-mcp call from the legacy
    /// dispatch sites; it resolves the tool through the context's
    /// `ContextToolBinding`, executes via the broker, and flattens the
    /// `KernelToolResult` back into an `ExecResult` so the surrounding
    /// agentic-loop error handling keeps working without further rewriting.
    ///
    /// Resolves `tool_name` through the context's `ContextToolBinding`,
    /// auto-populating the binding on first call with all registered
    /// instances.
    pub async fn dispatch_tool_via_broker(
        &self,
        tool_name: &str,
        params_json: &str,
        tool_ctx: &ExecContext,
    ) -> Result<ExecResult, crate::mcp::McpError> {
        use tokio_util::sync::CancellationToken;
        // Default path: no propagated cancellation. Callers that need it
        // (LLM streaming) call `dispatch_tool_via_broker_with_cancel`.
        self.dispatch_tool_via_broker_with_cancel(
            tool_name,
            params_json,
            tool_ctx,
            CancellationToken::new(),
        )
        .await
    }

    /// Same as `dispatch_tool_via_broker` but threads an externally-managed
    /// `CancellationToken` into the broker call (M2-B5). Cancelling the token
    /// causes the in-flight broker call to abort within a bounded time.
    pub async fn dispatch_tool_via_broker_with_cancel(
        &self,
        tool_name: &str,
        params_json: &str,
        tool_ctx: &ExecContext,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ExecResult, crate::mcp::McpError> {
        use crate::mcp::{CallContext, KernelCallParams, McpError, ToolContent, TraceContext};

        // Deny-by-default: use whatever binding the context has (assigned by
        // its rc `create`/`fork` lifecycle). No first-touch permissive seeding
        // — an unbound context grants nothing.
        let broker = self.broker.clone();
        let seed_ctx = CallContext::new(
            tool_ctx.principal_id,
            tool_ctx.context_id,
            tool_ctx.session_id,
            tool_ctx.kernel_id,
        );

        // Surface a genuine binding-fetch failure (a real DB read error) as
        // this call's own error instead of letting it silently collapse to
        // an empty (deny-all) binding — that used to surface later as a
        // confusing `ToolNotFound` for a tool the loadout actually grants,
        // naming a typo instead of a storage fault. This also warms the same
        // in-memory binding cache `list_visible_tools` reads below, so the
        // happy path pays no extra DB round trip for the check.
        broker.binding_checked(&tool_ctx.context_id).await?;

        // The resolver needs the sticky `name_map` populated/refreshed
        // against the current binding — kick `list_visible_tools` and
        // resolve straight from its output (the same `name_map` entries
        // `ContextToolBinding::resolve` would read) instead of re-fetching
        // the binding a second time.
        let visible = broker
            .list_visible_tools(tool_ctx.context_id, &seed_ctx)
            .await?;
        // Names visible to this context, captured before `visible` is
        // consumed below — both error branches on the `None` arm need them
        // to tell the caller what it could call instead.
        let visible_names: Vec<String> = visible.iter().map(|(name, _)| name.clone()).collect();
        let (instance, tool) = match visible
            .into_iter()
            .find(|(visible_name, _)| visible_name == tool_name)
        {
            Some((_, kt)) => (kt.instance, kt.name),
            None => {
                // `tool_name` isn't visible to this context. That's either a
                // typo/hallucinated name (never existed anywhere) or a real,
                // registered tool this context's loadout/binding just
                // doesn't grant — deny-by-default filtering makes both look
                // identical here. Check the unfiltered registry (a handful
                // of servers, walked only on this error path) so the caller
                // learns which, instead of a bare "not found" that reads
                // like a typo when it was really a capability decision.
                let known = self
                    .list_all_registered_tools()
                    .await
                    .into_iter()
                    .any(|(name, _, _, _)| name == tool_name);
                let (available, total) = crate::mcp::error::tool_name_list(visible_names);
                return Err(if known {
                    McpError::LoadoutDenied {
                        context: tool_ctx.context_id,
                        tool: tool_name.to_string(),
                        available,
                        total,
                    }
                } else {
                    McpError::UnknownToolName {
                        tool: tool_name.to_string(),
                        available,
                        total,
                    }
                });
            }
        };

        let arguments: serde_json::Value = if params_json.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(params_json).map_err(McpError::InvalidParams)?
        };

        let mut call_ctx = CallContext::new(
            tool_ctx.principal_id,
            tool_ctx.context_id,
            tool_ctx.session_id,
            tool_ctx.kernel_id,
        )
        .with_trace(TraceContext::from_current_span());
        if let Some(cwd) = tool_ctx.cwd.clone() {
            call_ctx = call_ctx.with_cwd(cwd);
        }

        let result = broker
            .call_tool(
                KernelCallParams {
                    instance,
                    tool,
                    arguments,
                },
                &call_ctx,
                cancel,
            )
            .await?;

        // Flatten KernelToolResult → ExecResult. Preserve the is_error →
        // success=false convention so the existing llm_stream result arm
        // keeps working without modification.
        let mut text = String::new();
        for c in &result.content {
            match c {
                ToolContent::Text(s) => text.push_str(s),
                ToolContent::Json(v) => text.push_str(&v.to_string()),
            }
        }
        if let Some(s) = &result.structured
            && text.is_empty()
        {
            text = serde_json::to_string_pretty(s).unwrap_or_default();
        }
        if result.is_error {
            Ok(ExecResult::failure(1, text))
        } else {
            Ok(ExecResult::success(text))
        }
    }

    /// Enumerate every tool currently registered on the broker, without
    /// binding filtering. Returns `(tool_name, instance, schema,
    /// description)` quadruples. Used by admin/introspection paths (kaish
    /// CLI, capnp `get_tool_schemas`) that want the global surface.
    pub async fn list_all_registered_tools(
        &self,
    ) -> Vec<(String, crate::mcp::InstanceId, serde_json::Value, Option<String>)> {
        use crate::mcp::CallContext;
        let broker = self.broker.clone();
        let ctx = CallContext::new(
            PrincipalId::system(),
            kaijutsu_types::ContextId::new(),
            kaijutsu_types::SessionId::new(),
            kaijutsu_types::KernelId::new(),
        );
        let mut out = Vec::new();
        for instance in broker.list_instances().await {
            // Snapshot the server Arc to avoid holding the registry lock
            // across the list_tools await.
            let server = {
                let instances_guard = broker.instances_snapshot().await;
                instances_guard.get(&instance).cloned()
            };
            if let Some(server) = server
                && let Ok(tools) = server.list_tools(&ctx).await
            {
                for kt in tools {
                    out.push((
                        kt.name.clone(),
                        kt.instance.clone(),
                        kt.input_schema,
                        kt.description,
                    ));
                }
            }
        }
        out
    }

    /// List tool definitions visible to a context via the broker.
    /// Auto-populates the binding on first call. Returns `(name, schema,
    /// description)` triples suitable for LLM tool-definition construction.
    ///
    /// Propagates a broker failure rather than swallowing it into an empty
    /// `Vec` — a broken binding used to silently present the LLM a
    /// tool-less context with no log, indistinguishable from a context that
    /// legitimately has no grants. Both production callers
    /// (`get_tool_schemas` RPC, `llm_stream::build_tool_definitions`) are
    /// already set up to fail their request/turn loudly on a broker error,
    /// so propagating here was the contained fix — no new fallback path to
    /// invent at either call site.
    pub async fn list_tool_defs_via_broker(
        &self,
        context_id: kaijutsu_types::ContextId,
        principal_id: PrincipalId,
    ) -> crate::mcp::McpResult<Vec<(String, serde_json::Value, Option<String>)>> {
        use crate::mcp::CallContext;

        // Deny-by-default: list whatever the context's binding (assigned by its
        // rc lifecycle) allows. No first-touch permissive seeding.
        let broker = self.broker.clone();
        let ctx = CallContext::new(
            principal_id,
            context_id,
            kaijutsu_types::SessionId::new(),
            kaijutsu_types::KernelId::new(),
        );
        let visible = broker.list_visible_tools(context_id, &ctx).await.map_err(|e| {
            tracing::error!(
                context_id = %context_id,
                error = %e,
                "list_tool_defs_via_broker: broker.list_visible_tools failed — \
                 propagating instead of presenting a silently tool-less context",
            );
            e
        })?;
        Ok(visible
            .into_iter()
            .map(|(visible_name, kt)| (visible_name, kt.input_schema, kt.description))
            .collect())
    }

    /// Register the Phase 1 builtin virtual MCP servers
    /// (`BlockToolsServer`, `FileToolsServer`, `KernelInfoServer`) on the
    /// broker.
    ///
    /// Callers pass the `SharedBlockStore` + `FileDocumentCache` they already
    /// have (the kernel does not own a `BlockStore`). Safe to call multiple
    /// times — subsequent calls replace the previous registrations.
    ///
    /// Registered under: `builtin.block`, `builtin.file`, `builtin.kernel_info`.
    pub async fn register_builtin_mcp_servers(
        &self,
        documents: crate::block_store::SharedBlockStore,
        file_cache: Arc<crate::file_tools::FileDocumentCache>,
        workspace_guard: Option<crate::file_tools::WorkspaceGuard>,
        kernel_db: Arc<parking_lot::Mutex<crate::kernel_db::KernelDb>>,
    ) -> crate::mcp::McpResult<()> {
        use crate::mcp::servers::{
            BlockToolsServer, BuiltinBindingsServer, BuiltinHooksServer, BuiltinResourcesServer,
            BuiltinTasksServer, FileToolsServer, KernelInfoServer,
        };
        use crate::mcp::servers::bindings_builtin::KERNEL_TOOLS_URI;
        use crate::mcp::{InstancePolicy, KernelNotification};
        use crate::mcp::server_like::ServerNotification;

        // Wire the block store into the broker so Phase 2 notification
        // emission can reach bound contexts (D-37). Done before registering
        // so the initial tool snapshots are captured but `register_silently`
        // suppresses the bootstrap ToolAdded noise (D-38).
        self.broker.set_documents(documents.clone()).await;

        // Wire the kernel DB so `ContextToolBinding`s persist (and survive
        // restart) and so binding reads (e.g. fork inheritance via
        // `get_context_binding`) see what `set_binding` wrote.
        self.broker.set_db(kernel_db.clone()).await;

        self.broker
            .register_silently(
                Arc::new(BlockToolsServer::new(documents.clone(), self.cas.clone())),
                InstancePolicy::for_kernel(self),
            )
            .await?;

        // builtin.tasks — task/plan grooming (household-agent arc,
        // docs/tasks.md): task_create/update/complete/cancel/list. A
        // sibling of builtin.block, not an extension — see tasks.rs's
        // module doc for why it's a separate curated instance.
        self.broker
            .register_silently(
                Arc::new(BuiltinTasksServer::new(documents)),
                InstancePolicy::for_kernel(self),
            )
            .await?;

        self.broker
            .register_silently(
                Arc::new(FileToolsServer::new(
                    file_cache,
                    self.vfs.clone(),
                    workspace_guard,
                )),
                InstancePolicy::for_kernel(self),
            )
            .await?;

        self.broker
            .register_silently(
                Arc::new(KernelInfoServer::new(self.drift.clone(), kernel_db.clone())),
                InstancePolicy::for_kernel(self),
            )
            .await?;

        // Phase 3 (D-41): builtin.resources admin server. Weak<Broker> avoids
        // the Arc cycle (broker owns the instance arc, instance refers back).
        self.broker
            .register_silently(
                Arc::new(BuiltinResourcesServer::new(Arc::downgrade(&self.broker))),
                InstancePolicy::for_kernel(self),
            )
            .await?;

        // Phase 4: builtin.hooks admin server. Same Weak<Broker> pattern.
        // Exposes hook_add / hook_remove / hook_list / hook_inspect so
        // every admin path (LLM / kaish / kj CLI) speaks MCP (D-14).
        self.broker
            .register_silently(
                Arc::new(BuiltinHooksServer::new(Arc::downgrade(&self.broker))),
                InstancePolicy::for_kernel(self),
            )
            .await?;

        // Phase 5 (D-55): builtin.bindings admin server + kj://kernel/tools
        // resource. The bridge task subscribes to kernel-level ToolsChanged
        // events (fired by `register_inner`/`unregister`) and forwards them
        // to the bindings server's notif channel as `ResourceUpdated`, so
        // subscribers to `kj://kernel/tools` see a child Resource block via
        // the Phase 3 coalescer pipeline. Subscribe BEFORE the bindings
        // server registers so no ToolsChanged event from its own
        // registration is lost; the bridge task is spawned immediately so
        // the receiver drains the broadcast channel as events arrive.
        let bindings_server = Arc::new(BuiltinBindingsServer::new(Arc::downgrade(&self.broker)));
        let bridge_rx = self.broker.notifications();
        let bridge_tx = bindings_server.resource_update_sender();
        tokio::spawn(async move {
            let mut rx = bridge_rx;
            loop {
                match rx.recv().await {
                    Ok(KernelNotification::ToolsChanged { .. }) => {
                        // Drop-on-no-subscribers is fine: if nobody has read
                        // kj://kernel/tools, there's no resource parent to
                        // thread a child under.
                        let _ = bridge_tx.send(ServerNotification::ResourceUpdated {
                            uri: KERNEL_TOOLS_URI.to_string(),
                        });
                    }
                    Ok(_) => {} // other KernelNotification variants irrelevant
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });
        self.broker
            .register_silently(bindings_server, InstancePolicy::for_kernel(self))
            .await?;

        // M3-D2: builtin.tool_search — keyword search across the calling
        // context's visible tools. Holds Weak<Broker> to avoid cycles.
        let tool_search_server = Arc::new(
            crate::mcp::servers::BuiltinToolSearchServer::new(Arc::downgrade(&self.broker)),
        );
        self.broker
            .register_silently(tool_search_server, InstancePolicy::for_kernel(self))
            .await?;

        // M3-D5: builtin.policy — get/set per-instance InstancePolicy.
        let policy_server = Arc::new(
            crate::mcp::servers::BuiltinPolicyServer::new(Arc::downgrade(&self.broker)),
        );
        self.broker
            .register_silently(policy_server, InstancePolicy::for_kernel(self))
            .await?;

        // builtin.shell_write — the in-kernel projection of the `shell_write`
        // facade as a broker tool (`ShellServer::new`), so the native LLM
        // agent gets the HOT, mutating shell (the RPC seam alone never
        // reached its tool roster). Gated by `facade:shell_write` via the
        // binding's facade projection (FACADE_PROJECTED_INSTANCES), NOT a
        // separate instance grant — one capability covers both surfaces.
        // 2026-08-17 flag day (`docs/gate-and-shell-split.md`, "Slice 3"):
        // this is what `builtin.shell`/`facade:shell` used to be — a stale
        // `facade:shell` grant no longer reaches this instance. Holds
        // Weak<Broker> to reach the kj dispatcher (wired post-bootstrap by
        // `set_kj_dispatcher`) and materialize a per-context kaish on demand.
        //
        // `InstancePolicy::for_kernel_gated` (not the generic `for_kernel`):
        // this is the ONE builtin instance that can reach `kj::gate::run_gate`
        // through `Broker::call_tool` (its own `!self.read_only` check below
        // gates only the write path; `builtin.shell`'s read-only twin never
        // gates, and no other builtin server calls into the gate). Its broker
        // cap must be the gate ladder's `gate::BROKER_CALL`, not
        // `mcp_call_timeout_default` — see `mcp/policy.rs`'s doc on
        // `for_kernel_gated` for why the generic default would reopen the
        // defect the ladder closes.
        let shell_write_server = Arc::new(
            crate::mcp::servers::ShellServer::new(Arc::downgrade(&self.broker)),
        );
        self.broker
            .register_silently(shell_write_server, InstancePolicy::for_kernel_gated(self))
            .await?;

        // builtin.shell — the SAFE, unmarked twin (`ShellServer::new_read_only`,
        // tool name `shell`) for roles that must not write or shell out (the
        // `toolie`), and the name every caller reaches for by default post-flag-
        // day. Same facade-projection mechanism (FACADE_PROJECTED_INSTANCES),
        // gated by `facade:shell`. The constraint rides in the tool *name* so
        // the model never attempts a write it can't perform. `read_only_shell`
        // retires as a name entirely — this instance now answers to `shell`. A
        // safe-only role never grants `facade:shell_write`, so it gets this
        // shell or the hot one, not both (broad `*`/`facade:*` roles, and
        // `director`, may see both — a harmless strict subset for `*`/`facade:
        // *`, and an explicit operator's-console choice for `director`).
        let shell_server = Arc::new(
            crate::mcp::servers::ShellServer::new_read_only(Arc::downgrade(&self.broker)),
        );
        self.broker
            .register_silently(shell_server, InstancePolicy::for_kernel(self))
            .await?;

        // builtin.background — `list_background_processes` /
        // `read_background_output` / `kill_background_process`, the
        // companion tools for `shell_write`'s `background: true` jobs
        // (`background_exec.rs`). Sibling of `builtin.shell_write`, but STILL
        // riding the `facade:shell` projection string in
        // FACADE_PROJECTED_INSTANCES (unchanged by the flag day) — an open
        // item, not a decision: see the doc comment on
        // `FACADE_PROJECTED_INSTANCES` in `mcp/binding.rs` for why this
        // silently widened to the new safe facade and wasn't fixed here.
        let background_server = Arc::new(
            crate::mcp::servers::BackgroundServer::new(Arc::downgrade(&self.broker)),
        );
        self.broker
            .register_silently(background_server, InstancePolicy::for_kernel(self))
            .await?;

        Ok(())
    }

    /// Get the block flows bus.
    pub fn block_flows(&self) -> &SharedBlockFlowBus {
        &self.block_flows
    }

    /// Get the editor flows bus — the editor-state push channel. The server's
    /// `subscribe_editor` bridge subscribes here and forwards to a client.
    pub fn editor_flows(&self) -> &SharedEditorFlowBus {
        &self.editor_flows
    }

    /// Get the turn flows bus (autonomous turn requests).
    pub fn turn_flows(&self) -> &SharedTurnFlowBus {
        &self.turn_flows
    }

    /// Get the approval-ledger flows bus. The server's
    /// `subscribe_ledger_events` bridge subscribes here; the publishing side
    /// is [`crate::kj::gate::announce_ledger_change`], which lives with the
    /// `KernelDb` handle it needs to read the committed generation back.
    pub fn ledger_flows(&self) -> &SharedLedgerFlowBus {
        &self.ledger_flows
    }

    /// Get the drift router.
    pub fn drift(&self) -> &SharedDriftRouter {
        &self.drift
    }

    /// Get the content-addressed store.
    pub fn cas(&self) -> &Arc<FileStore> {
        &self.cas
    }

    /// The sink-fed MIDI presence store (`docs/midi-next.md` "Presence is
    /// sink-fed") — ephemeral, in-memory, `/run/midi`'s backing state.
    pub fn midi_presence(&self) -> &Arc<crate::midi_presence::MidiPresenceStore> {
        &self.midi_presence
    }

    /// The MIDI exchange registry (`docs/midi-next.md` "SysEx: the exchange
    /// pattern") — how the kernel asks ONE sink a bounded question.
    pub fn midi_exchange(&self) -> &Arc<crate::midi_exchange::MidiExchangeRegistry> {
        &self.midi_exchange
    }

    /// Get the `/r` client-shares registry (`docs/slash-r.md`).
    pub fn share_registry(&self) -> &Arc<crate::vfs::ShareRegistry> {
        &self.share_registry
    }

    /// Get the background host-process registry (`background_exec.rs`). See
    /// that module's docs for the ownership/cleanup/output-bounding contract.
    pub fn background_processes(&self) -> &Arc<crate::background_exec::BackgroundRegistry> {
        &self.background
    }

    /// Get the image backend registry.
    pub fn image_backends(&self) -> &RwLock<crate::image::ImageBackendRegistry> {
        &self.image_backends
    }

    // ========================================================================
    // Hyoushigi timelines (the beat substrate)
    // ========================================================================

    /// Arm a context with a hyoushigi timeline driven by `clock`, registering
    /// the production resolvers onto it and seeding its playhead to `seed`.
    /// Idempotent: a context already armed keeps its live timeline (we never
    /// clobber an open future), so re-arming is a no-op that returns the existing
    /// handle.
    ///
    /// `seed` positions the playhead so musical time stays globally monotone per
    /// context across restarts/rotations (design §4). It is applied **inside**
    /// `or_insert_with` — only on the freshly-constructed (virgin-by-construction)
    /// timeline — so an idempotent re-arm of a LIVE timeline never re-seeds or
    /// rewinds the playhead, preserving the "never clobber an open future"
    /// contract above. `seed_playhead` is virgin-only and `.expect()`s here
    /// because the timeline is brand new; a non-virgin seed would be a kernel bug
    /// (crash over corruption), not a recoverable condition.
    ///
    /// Arming is the *only* thing that gives a context a timeline — a coder is
    /// never armed, so it has no entry and the beat scheduler never wakes for
    /// it ("paused is no heap entry").
    pub fn arm_timeline(
        &self,
        context_id: kaijutsu_types::ContextId,
        clock: kaijutsu_hyoushigi::TickClock,
        seed: kaijutsu_types::Tick,
    ) -> crate::hyoushigi::SharedTimeline {
        self.timelines
            .entry(context_id)
            .or_insert_with(|| {
                let mut tl = kaijutsu_hyoushigi::Timeline::new(clock);
                crate::hyoushigi::register_resolvers(&mut tl, self.cas.clone());
                // Virgin by construction: this closure only fires when the entry
                // is absent, so the seed always lands on a fresh timeline. A
                // re-arm hits the existing entry and skips this block entirely —
                // that is exactly how re-arm avoids re-seeding a live playhead.
                tl.seed_playhead(seed)
                    .expect("freshly-constructed timeline must be virgin for seed_playhead");
                Arc::new(parking_lot::Mutex::new(tl))
            })
            .clone()
    }

    /// Look up a context's timeline, if it is armed. Returns `None` for every
    /// un-armed context (lookup never arms).
    pub fn timeline(
        &self,
        context_id: kaijutsu_types::ContextId,
    ) -> Option<crate::hyoushigi::SharedTimeline> {
        self.timelines.get(&context_id).map(|e| e.value().clone())
    }

    /// Disarm a context: drop its timeline and its open future. Used when a
    /// context is archived. The beat scheduler skips a context with no entry.
    pub fn disarm_timeline(&self, context_id: kaijutsu_types::ContextId) {
        self.timelines.remove(&context_id);
    }

    // --- Stage 2: per-track timeline registry (docs/tracks.md) ---------------
    //
    // The clock + open future + committed score live on the TRACK now. These are
    // the `TrackId`-keyed twins of the per-context arm/lookup/disarm above. The
    // beat scheduler arms a track once (on first attach / track creation), pumps
    // it each beat, and disarms it only on track teardown — NOT on context
    // detach, so the timeline (and continuity) never leaves when a producer
    // rotates out. `schedule_abc_cell` and the materialize bridge route here via
    // the cell's/attachment's `TrackId`.

    /// Arm a track's timeline: idempotent create-and-seed, keyed by `TrackId`.
    /// First call constructs a fresh, resolver-registered timeline and seeds its
    /// playhead (virgin by construction, like [`arm_timeline`]); a re-arm hits the
    /// existing entry and leaves the live playhead untouched. Returns the shared
    /// handle either way.
    pub fn arm_track_timeline(
        &self,
        track_id: kaijutsu_types::TrackId,
        clock: kaijutsu_hyoushigi::TickClock,
        seed: kaijutsu_types::Tick,
    ) -> crate::hyoushigi::SharedTimeline {
        self.track_timelines
            .entry(track_id)
            .or_insert_with(|| {
                let mut tl = kaijutsu_hyoushigi::Timeline::new(clock);
                crate::hyoushigi::register_resolvers(&mut tl, self.cas.clone());
                tl.seed_playhead(seed)
                    .expect("freshly-constructed track timeline must be virgin for seed_playhead");
                Arc::new(parking_lot::Mutex::new(tl))
            })
            .clone()
    }

    /// Look up a track's timeline, if it is armed. Returns `None` for an un-armed
    /// track (lookup never arms).
    pub fn track_timeline(
        &self,
        track_id: &kaijutsu_types::TrackId,
    ) -> Option<crate::hyoushigi::SharedTimeline> {
        self.track_timelines.get(track_id).map(|e| e.value().clone())
    }

    /// Disarm a track: drop its timeline and its open future. Used on **track
    /// teardown**, never on a context detach (the whole point of moving the clock
    /// onto the track is that it outlives any one producer).
    pub fn disarm_track_timeline(&self, track_id: &kaijutsu_types::TrackId) {
        self.track_timelines.remove(track_id);
    }

    /// Install the beat-scheduler ingress. Called once by the server at startup.
    /// Returns whether it was set (false if already installed).
    pub fn set_beat_ingress(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<crate::hyoushigi::BeatRequest>,
    ) -> bool {
        self.beat_ingress.set(tx).is_ok()
    }

    /// Install the bound Claude Code peer inbox. Called once by the server at
    /// startup, after a successful [`crate::cc_inbox::CcInboxHandle::bind`].
    /// Returns whether it was set (`false` if already installed) — same
    /// once-only shape as [`Self::set_beat_ingress`].
    pub fn set_cc_inbox(&self, handle: Arc<crate::cc_inbox::CcInboxHandle>) -> bool {
        self.cc_inbox.set(handle).is_ok()
    }

    /// The bound Claude Code peer inbox, if the server installed one.
    /// `None` in every test/embedded kernel and in a production kernel whose
    /// bind attempt failed (logged loudly at bind time, never silently) —
    /// callers must treat absence as "no inbox this run", not as an error.
    pub fn cc_inbox(&self) -> Option<&Arc<crate::cc_inbox::CcInboxHandle>> {
        self.cc_inbox.get()
    }

    /// Send a fire-and-forget command to the beat scheduler, if one is installed.
    /// Returns whether it was delivered — `false` when no scheduler is wired
    /// (embedded/test) or the scheduler has shut down. Callers decide whether
    /// that's fatal; arming a musician with no scheduler simply means it never
    /// beats (no silent corruption, just no beat). Use [`send_beat_request`] when
    /// you need to know whether the scheduler actually applied the command.
    ///
    /// [`send_beat_request`]: Self::send_beat_request
    pub fn send_beat_command(&self, cmd: crate::hyoushigi::BeatCommand) -> bool {
        match self.beat_ingress.get() {
            Some(tx) => tx.send(cmd.into()).is_ok(),
            None => false,
        }
    }

    /// Send a command and get a receiver for the scheduler's [`BeatAck`] — the
    /// truthful outcome (`Ok` applied, `Err(reason)` no-op, e.g. not armed). The
    /// scheduler owns the armed map, so this is how `kj transport` reports what
    /// really happened instead of blindly claiming success. `None` when no
    /// scheduler is wired or it has shut down (same meaning as `send_beat_command`
    /// returning `false`).
    ///
    /// [`BeatAck`]: crate::hyoushigi::BeatAck
    pub fn send_beat_request(
        &self,
        cmd: crate::hyoushigi::BeatCommand,
    ) -> Option<tokio::sync::oneshot::Receiver<crate::hyoushigi::BeatAck>> {
        let tx = self.beat_ingress.get()?;
        let (reply, reply_rx) = tokio::sync::oneshot::channel();
        let request = crate::hyoushigi::BeatRequest::Command { command: cmd, reply: Some(reply) };
        match tx.send(request) {
            Ok(()) => Some(reply_rx),
            Err(_) => None, // scheduler dropped its receiver
        }
    }

    /// Forward one clock reference to the beat scheduler (`docs/midi.md` M3
    /// — the `reportClockEstimate` wire verb). Fire-and-forget: returns
    /// whether it was delivered, same contract as [`send_beat_command`].
    ///
    /// [`send_beat_command`]: Self::send_beat_command
    pub fn send_clock_estimate(
        &self,
        context_id: kaijutsu_types::ContextId,
        beat: f64,
        tempo_bps: f64,
        epoch_ns: u64,
        source: String,
    ) -> bool {
        match self.beat_ingress.get() {
            Some(tx) => tx
                .send(crate::hyoushigi::BeatRequest::ClockEstimate {
                    context_id,
                    beat,
                    tempo_bps,
                    epoch_ns,
                    source,
                })
                .is_ok(),
            None => false,
        }
    }

    /// Ship a captured-MIDI batch to the beat scheduler for commit onto the
    /// capture context's track (`docs/midi.md` M2 — the `commitCapture` wire
    /// verb). The scheduler owns the quantization anchor, so the work happens
    /// there; the receiver yields the landed block id or a loud refusal.
    /// `None` when no scheduler is wired (embedded/test) or it has shut down.
    pub fn send_capture_commit(
        &self,
        context_id: kaijutsu_types::ContextId,
        payload: Vec<u8>,
        played_by: kaijutsu_types::PrincipalId,
    ) -> Option<tokio::sync::oneshot::Receiver<Result<kaijutsu_types::BlockId, String>>> {
        let tx = self.beat_ingress.get()?;
        let (reply, reply_rx) = tokio::sync::oneshot::channel();
        let request = crate::hyoushigi::BeatRequest::CommitCapture {
            context_id,
            payload,
            played_by,
            reply,
        };
        match tx.send(request) {
            Ok(()) => Some(reply_rx),
            Err(_) => None, // scheduler dropped its receiver
        }
    }

    /// Ask the beat scheduler for a live snapshot of every track (the
    /// `listTracks` wire surface reads this — the in-memory truth, not the
    /// lagging persisted row). `None` when no scheduler is wired
    /// (embedded/test) or it has shut down.
    pub fn request_track_snapshot(
        &self,
    ) -> Option<tokio::sync::oneshot::Receiver<Vec<crate::hyoushigi::TrackSnapshot>>> {
        let tx = self.beat_ingress.get()?;
        let (reply, reply_rx) = tokio::sync::oneshot::channel();
        match tx.send(crate::hyoushigi::BeatRequest::Snapshot { reply }) {
            Ok(()) => Some(reply_rx),
            Err(_) => None, // scheduler dropped its receiver
        }
    }

    // ========================================================================
    // Identity
    // ========================================================================

    /// Get the legacy KernelState UUID. Distinct from `Self::id()`, which is
    /// the stable wire-level `KernelId`. KernelState is kept around for
    /// checkpoint/fork bookkeeping; once that surface is retired this can
    /// collapse onto the singleton id.
    pub async fn state_id(&self) -> Uuid {
        self.state.read().await.id
    }

    /// Get the kernel name.
    pub async fn name(&self) -> String {
        self.state.read().await.name.clone()
    }

    /// Set the kernel name.
    pub async fn set_name(&self, name: impl Into<String>) {
        self.state.write().await.name = name.into();
    }

    // ========================================================================
    // VFS
    // ========================================================================

    /// Get the VFS mount table.
    pub fn vfs(&self) -> &Arc<MountTable> {
        &self.vfs
    }

    /// Recursive snapshot listing with generation stamps — FSN world
    /// stage-0/1 plumbing (`docs/scenes/vfs.md`). Thin delegate to
    /// [`MountTable::snapshot`], which documents caps, error policy,
    /// generation policy, and the `ignored` classification's known gaps.
    pub async fn snapshot(
        &self,
        path: &Path,
        depth: u32,
        max_entries: u32,
    ) -> VfsResult<crate::vfs::SnapshotResult> {
        self.vfs.snapshot(path, depth, max_entries).await
    }

    /// The kernel's block store, built at construction. Callers that need a
    /// `SharedBlockStore` paired with this kernel should take this one rather
    /// than building a second instance over the same documents.
    pub fn blocks(&self) -> &crate::block_store::SharedBlockStore {
        &self.blocks
    }

    /// The shared file-document cache, built at construction over `blocks()`
    /// + the kernel VFS + the `KernelDb` handle passed to `new`/`with_flows`.
    /// There is exactly one instance for the kernel's lifetime.
    pub fn file_cache(&self) -> &Arc<crate::file_tools::FileDocumentCache> {
        &self.file_cache
    }

    /// The kernel's `KernelDb` handle — the same instance `blocks()` and
    /// `file_cache()` are built over, not a second connection. Callers that
    /// need a durable-table query neither of those surfaces exposes (e.g.
    /// `list_dirty_file_buffers` for `/v/swap`, docs/file-buffers.md) take
    /// this rather than opening their own `KernelDb`.
    pub fn kernel_db(&self) -> &Arc<parking_lot::Mutex<crate::kernel_db::KernelDb>> {
        &self.db
    }

    // ── Editor sessions ───────────────────────────────────────────────────

    /// Open an in-app editor on `path`, binding to the kernel block that owns its
    /// text (config/rc → the ConfigDocFs block; ordinary file → its file-doc).
    /// Returns the session handle + initial state; fails loud if the path names
    /// no editable document.
    pub async fn editor_open(
        &self,
        path: &str,
    ) -> Result<(crate::editor::EditorSessionId, crate::editor::EditorState), String> {
        self.editor_open_as(path, None).await
    }

    /// Open an editor recording the [`EditorOpener`](crate::editor::EditorOpener)
    /// on the session, so `fg` can re-foreground it for that principal and
    /// `:r !cmd` can shell out in the opener's context. The signaled front doors
    /// (`vi`/`edit`, `kj editor`, `kj rc edit`) pass the caller here.
    ///
    /// For a **file-backed** target (not config/rc): pins the file-document
    /// cache entry for the session's lifetime (P1, `docs/issues.md` "Tech-debt
    /// audits, 2026-08-20"; `docs/file-buffers.md`). Before this, a session
    /// held no reference into the cache, so unrelated reads of *other* paths
    /// could evict its entry while the buffer sat clean — `mark_dirty`/
    /// `flush_one` then silently no-op'd on the now-uncached path, so `:w`
    /// reported success and wrote nothing. `editor_keys`' `Closed` arm and
    /// [`editor_quit`](Self::editor_quit) release the pin when the session
    /// closes.
    pub async fn editor_open_as(
        &self,
        path: &str,
        opener: Option<crate::editor::EditorOpener>,
    ) -> Result<(crate::editor::EditorSessionId, crate::editor::EditorState), String> {
        let blocks = self.blocks();
        let file_cache = self.file_cache().clone();
        // Resolve (the only async step) BEFORE taking the sync mutex, so the
        // `!Send` `EditorCore` never coexists with an await. The mount table is
        // the authority on what owns the path (config-doc backend vs. file).
        let target =
            crate::editor::resolve_editor_target(path, blocks, &file_cache, self.vfs()).await?;
        // Config/rc targets have no file-cache entry to pin — resolve_editor_target
        // bound straight to the ConfigDocFs block, never touching file_cache. Read
        // the mount-table answer it already carries rather than re-deriving it.
        let file_backed = !target.config_owned;
        if file_backed {
            file_cache.pin(path)?;
        }
        let opened = self.editor_sessions.lock().0.open(path, target, blocks, opener);
        if opened.is_err() && file_backed {
            // The session never came to exist — don't leak the pin.
            file_cache.unpin(path);
        }
        opened
    }

    /// Feed keys to an open session, mirroring the edits onto the kernel block.
    /// Publishes the new state on the editor push channel so every renderer of
    /// this session updates. A `ZZ`/`ZQ` in the batch saves/discards and closes
    /// the session (modalkit disambiguates it from an inserted `ZZ`), publishing
    /// `Closed` instead — so a key forwarder never needs to detect quit itself.
    ///
    /// For a **file-backed** session (`docs/file-buffers.md`): an edit that
    /// leaves the buffer dirty records the durable swap marker
    /// (`file_cache.mark_dirty`) so unsaved work survives a restart; a batch
    /// that executed a checkpoint (`:w`/`:wq`/`:x`/`ZZ`) flushes to disk
    /// (`file_cache.flush_one_guarded`) instead — refusing on the W12
    /// changed-under-us condition unless `:w!` set `update.forced`. A
    /// config/rc session has no host file and never touches the file cache
    /// (`EditorSessions::file_backed_path`).
    ///
    /// Unrestricted (`can_write: true`) — the shape every caller in this
    /// crate outside `kj/editor.rs` uses. See
    /// [`editor_keys_checked`](Self::editor_keys_checked) for the gated
    /// entry point.
    pub async fn editor_keys(
        &self,
        id: crate::editor::EditorSessionId,
        keys: &str,
    ) -> Result<crate::editor::EditorState, String> {
        self.editor_keys_checked(id, keys, true).await
    }

    /// [`editor_keys`](Self::editor_keys), gated: `can_write` is the caller's
    /// `Capability::Editor` check, computed once by the caller (`kj editor
    /// keys`) and carried through. A write intent in this batch refuses
    /// inside [`EditorSessions::keys_checked`] when `can_write` is false — the
    /// buffer stays dirty and open, and its message names the missing
    /// capability — so this function's flush/checkpoint logic below is
    /// unreachable for a refused batch: `update.saved` only comes back true
    /// when the pure layer actually let a write through. See `docs/vi.md`.
    pub(crate) async fn editor_keys_checked(
        &self,
        id: crate::editor::EditorSessionId,
        keys: &str,
        can_write: bool,
    ) -> Result<crate::editor::EditorState, String> {
        let blocks = self.blocks();
        // Capture the path and feed the keys under one lock — a ZZ/ZQ in the
        // batch drops the session, so the path must be read first. A `:r` read
        // intent is taken here too (only when the session is still open), then
        // fulfilled below: the async fetch happens *outside* the lock, so the
        // `!Send` `EditorCore` never crosses an await (the `SendSessions`
        // invariant); only the fetched `String` does.
        let (path, file_path, outcome, io, io_cursor, io_opener) = {
            let mut sessions = self.editor_sessions.lock();
            let path = sessions.0.session_path(id);
            // Captured now (session still exists) — `Closed` outcomes below
            // drop the session before the kernel ever sees it again.
            let file_path = sessions.0.file_backed_path(id);
            let outcome = sessions.0.keys_checked(id, keys, blocks, can_write)?;
            let io = if matches!(outcome, crate::editor::KeysOutcome::Updated(_)) {
                sessions.0.take_io(id)
            } else {
                None
            };
            // Capture the cursor NOW (at `:r` submit), so a keystroke that moves
            // it while the fetch awaits can't make the read land at the wrong
            // place (the "wandering cursor" race) — insert happens at this offset.
            let io_cursor = io.as_ref().and_then(|_| sessions.0.session_cursor(id));
            // The opener context, captured at submit too — `:r !cmd` shells out
            // in it (the caller's context/capabilities, not the edited block's).
            let io_opener = io.as_ref().and_then(|_| sessions.0.session_opener(id));
            (path, file_path, outcome, io, io_cursor, io_opener)
        };

        // Fulfill a `:r` read: fetch the content, then splice it at the cursor
        // captured above (not the live cursor, which may have moved). A failed
        // fetch (missing file, failed/denied command, no opener) is a
        // dialect-level failure: it reports on the `:` status line and keeps
        // the session open — same channel as an unknown command or a bad `:s`
        // regex. Hard errors out of here stay reserved for session and
        // infrastructure failures (no such session, a block mirror failure);
        // the app's session-lost detection keys on exactly that distinction.
        if let Some(io) = io {
            let state = match self.fetch_editor_io(io, io_opener).await {
                Ok(content) => {
                    let at = io_cursor.unwrap_or(0);
                    let state = {
                        let mut sessions = self.editor_sessions.lock();
                        sessions.0.insert_text(id, &content, at, blocks)?
                    };
                    // The block changed; drop the file-cache shadow of the
                    // *edited* path.
                    if let Some(path) = path.as_deref() {
                        self.invalidate_config_file_cache(path);
                    }
                    state
                }
                Err(msg) => {
                    let mut state = self.editor_sessions.lock().0.state(id)?;
                    state.message = Some(msg);
                    state
                }
            };
            // `:r` is an edit, never a save — mark dirty, don't flush.
            if let Some(fp) = file_path.as_deref()
                && state.dirty
            {
                self.file_cache
                    .mark_dirty(fp)
                    .map_err(|e| format!("editor :r: failed to mark {fp} dirty: {e}"))?;
            }
            self.publish_editor_state(id, &state);
            return Ok(state);
        }

        // The mirror (and any ZZ/ZQ rollback) wrote the block; drop the file
        // cache's now-stale shadow so a kaish `cat` re-reads fresh.
        if let Some(path) = path.as_deref() {
            self.invalidate_config_file_cache(path);
        }
        match outcome {
            crate::editor::KeysOutcome::Updated(update) => {
                let mut state = update.state;
                // Mark dirty BEFORE any same-batch flush is attempted below —
                // not just on the plain-edit path. `flush_one` treats a
                // cache entry it never saw `mark_dirty` on as "clean,
                // nothing to flush" and silently no-ops (P1: a single batch
                // like `iX<Esc>:w<CR>` mirrors the edit onto the block, then
                // went straight to `update.saved`'s flush branch below
                // without ever recording the entry as dirty first — the
                // checkpoint advanced, the caller saw success, and disk never
                // moved). Splitting the edit and the `:w` into two separate
                // `editor_keys` calls happened to route through the old
                // plain-edit branch and never tripped it; one batch always
                // must, so the mark now runs unconditionally here.
                if let Some(fp) = file_path.as_deref()
                    && state.dirty
                {
                    self.file_cache.mark_dirty(fp).map_err(|e| {
                        format!("editor keys: failed to mark {fp} dirty: {e}")
                    })?;
                }
                if update.saved {
                    match file_path.as_deref() {
                        Some(fp) => {
                            // File-backed: `state` still reads dirty here —
                            // `EditorSessions` deferred the checkpoint (see
                            // `KeysUpdate`). Flush to disk FIRST; the
                            // checkpoint must not advance unless the bytes
                            // actually landed. Guarded (docs/file-buffers.md
                            // rule 3, W12): a plain `:w` refuses when disk
                            // moved under the buffer, `:w!` overrides via
                            // `update.forced`.
                            if let Err(e) =
                                self.file_cache.flush_one_guarded(fp, update.forced).await
                            {
                                // Leave the swap row and the cache entry alone
                                // — the entry is legitimately dirty and the
                                // block IS the player's live buffer, not a
                                // speculative write with nothing else reading
                                // it. Invalidating (as `mount_backend.rs`'s
                                // *external*-write rollback correctly does)
                                // would turn a retry `:w` into a cold miss
                                // that recovers as an unacknowledged swap —
                                // the wrong error for a player retrying a
                                // write that just failed.
                                state.message = Some(flush_error_message(fp, &e, self.id()));
                            } else {
                                state = self.editor_sessions.lock().0.save(id)?;
                            }
                        }
                        None => {
                            // Config/rc: no file to flush — the block write
                            // already IS the durable persistence.
                            state = self.editor_sessions.lock().0.save(id)?;
                        }
                    }
                }
                self.publish_editor_state(id, &state);
                Ok(state)
            }
            crate::editor::KeysOutcome::Closed(update) => {
                // The session is already gone (ZZ/`:wq`/`:x`/ZQ dropped it
                // inside `EditorSessions::keys`, checkpointing before it did —
                // see `run_commands`) — there is no open session left to
                // report a flush failure on the status line, so it surfaces
                // as a hard error instead. This is the "infrastructure
                // failure" class `docs/vi.md` reserves for exactly this: the
                // app's session-lost detection keys on it, and here the
                // session genuinely IS lost — closed, with the write it
                // promised never landing. The swap row is left alone on
                // failure (same reasoning as the `Updated` arm above), so a
                // fresh `vi` on the path recovers the lost edit as a swap
                // instead of losing it.
                //
                // Same fix as the `Updated` arm above: a same-batch edit +
                // close (`ZZ`, `:wq`, `:x`) must mark the entry dirty before
                // this flush attempt runs, or `flush_one` finds a clean
                // entry and silently no-ops — the "gone either way" cleanup
                // below still must run, so a `mark_dirty` failure here folds
                // into the same reported-error path as a flush failure
                // rather than an early `?` return.
                let mark_err = if update.saved
                    && let Some(fp) = file_path.as_deref()
                    && let Err(e) = self.file_cache.mark_dirty(fp)
                {
                    Some(format!("editor keys: failed to mark {fp} dirty before close: {e}"))
                } else {
                    None
                };
                let flush_err = if mark_err.is_none()
                    && update.saved
                    && let Some(fp) = file_path.as_deref()
                    && let Err(e) =
                        self.file_cache.flush_one_guarded(fp, update.forced).await
                {
                    Some(flush_error_message(fp, &e, self.id()))
                } else {
                    None
                };
                // The session is gone either way — release the pin taken at
                // open (`editor_open_as`) so eviction can reclaim the entry.
                if let Some(fp) = file_path.as_deref() {
                    self.file_cache.unpin(fp);
                }
                self.editor_flows.publish(crate::flows::EditorFlow::Closed {
                    session_id: id.as_u64(),
                });
                match mark_err.or(flush_err) {
                    Some(e) => Err(e),
                    None => Ok(update.state),
                }
            }
        }
    }

    /// Fetch the content for a `:r` read intent. `:r <file>` reads through the
    /// shared `FileDocumentCache` (the same source the editor and file tools
    /// use). `:r !cmd` materializes a per-context kaish in the *opener's*
    /// `(principal, context_id, session_id)` — the same `materialize_context_kaish`
    /// the model shell and rc lifecycle use — and splices the command's stdout.
    /// Running in the opener's context means the command sees their cwd and
    /// capability allow-set, not the edited block's context. Fails loud (never a
    /// silent empty splice) when there's no opener, no dispatcher, or the command
    /// fails.
    async fn fetch_editor_io(
        &self,
        io: kaijutsu_editor::EditorIo,
        opener: Option<crate::editor::EditorOpener>,
    ) -> Result<String, String> {
        match io {
            kaijutsu_editor::EditorIo::ReadFile(path) => {
                self.file_cache().read_content(&path).await
            }
            kaijutsu_editor::EditorIo::ReadShell(cmd) => {
                // No opener (a headless driver / wire open) → no context to run
                // in. Fail loud pointing at the interactive shell, as before.
                let opener = opener.ok_or_else(|| {
                    format!(
                        "editor: ':r !{cmd}' needs an opener context — open via \
                         vi/edit, or use Ctrl+Z to the shell"
                    )
                })?;
                let dispatcher = self.broker.kj_dispatcher().await.ok_or_else(|| {
                    "editor: ':r !cmd' unavailable — kj dispatcher not wired".to_string()
                })?;
                let kaish = dispatcher
                    .materialize_context_kaish_internal(
                        "editor-read",
                        opener.principal,
                        opener.context_id,
                        opener.session_id,
                        dispatcher.semantic_index(),
                        dispatcher.block_source(),
                    )
                    .await
                    .map_err(|e| format!("editor: ':r !{cmd}' materialize shell: {e}"))?;
                let result = kaish
                    .execute_with_options(&cmd, kaish_kernel::ExecuteOptions::default())
                    .await
                    .map_err(|e| format!("editor: ':r !{cmd}' failed: {e}"))?;
                if result.code != 0 {
                    return Err(format!(
                        "editor: ':r !{cmd}' exited {}: {}",
                        result.code,
                        result.err.trim()
                    ));
                }
                Ok(result.text_out().into_owned())
            }
        }
    }

    /// Current state of an open session.
    pub fn editor_state(
        &self,
        id: crate::editor::EditorSessionId,
    ) -> Result<crate::editor::EditorState, String> {
        self.editor_sessions.lock().0.state(id)
    }

    /// A census of every open editor session — `kj editor list`'s data source.
    pub fn editor_list(&self) -> Vec<crate::editor::EditorSessionInfo> {
        self.editor_sessions.lock().0.list()
    }

    /// `ZZ` (direct call) — for a file-backed session (`docs/file-buffers.md`),
    /// flush to disk FIRST and only advance the checkpoint once the bytes
    /// land; a config/rc session has no flush step, so its checkpoint
    /// advances immediately. The flush is W12-guarded and unforced (this call
    /// carries no `:w!` bang), so it refuses if disk moved under the buffer.
    /// Publishes the resulting state so renderers reflect it — clean on
    /// success, still dirty on failure.
    ///
    /// The session stays open on a flush failure: this is the direct-call
    /// path (the wire `editorSave`, `kj editor save`), not a `ZZ`/`:wq` that
    /// already dropped the session — so, unlike `editor_keys`' `Closed` arm,
    /// there is somewhere to report it. Mirrors `run_commands`' `Quit{force}`
    /// dialect-level refusal: the status line gets the message and the call
    /// still returns `Ok`. The swap row and cache entry are left alone on
    /// failure — see `editor_keys`'s `Updated` arm for why this differs from
    /// `mount_backend.rs`'s *external*-write rollback.
    ///
    /// Unrestricted (`can_write: true`) — see
    /// [`editor_save_checked`](Self::editor_save_checked) for the gated
    /// entry point.
    pub async fn editor_save(
        &self,
        id: crate::editor::EditorSessionId,
    ) -> Result<crate::editor::EditorState, String> {
        self.editor_save_checked(id, true).await
    }

    /// [`editor_save`](Self::editor_save), gated: with `can_write: false` the
    /// checkpoint does not advance and nothing flushes — same status-line
    /// message and same "session stays open, call still returns `Ok`" shape
    /// as every other dialect-level refusal on this path.
    pub(crate) async fn editor_save_checked(
        &self,
        id: crate::editor::EditorSessionId,
        can_write: bool,
    ) -> Result<crate::editor::EditorState, String> {
        let (file_path, mut state) = {
            let mut sessions = self.editor_sessions.lock();
            let file_path = sessions.0.file_backed_path(id);
            let state = sessions.0.state(id)?;
            (file_path, state)
        };
        if !can_write {
            state.message = Some(crate::editor::WRITE_CAPABILITY_REFUSED.to_string());
            self.publish_editor_state(id, &state);
            return Ok(state);
        }
        match file_path.as_deref() {
            Some(fp) => {
                // Guarded, unforced: this direct-call path (the wire
                // `editorSave`, `kj editor save`) carries no bang, so it is a
                // plain `:w` and refuses on the W12 changed-under-us
                // condition (docs/file-buffers.md rule 3).
                if let Err(e) = self.file_cache.flush_one_guarded(fp, false).await {
                    state.message = Some(flush_error_message(fp, &e, self.id()));
                } else {
                    state = self.editor_sessions.lock().0.save(id)?;
                }
            }
            None => {
                state = self.editor_sessions.lock().0.save(id)?;
            }
        }
        self.publish_editor_state(id, &state);
        Ok(state)
    }

    /// `ZQ` — roll the block back to the session's checkpoint and close it.
    /// Publishes `Closed` so renderers drop the session.
    pub fn editor_quit(&self, id: crate::editor::EditorSessionId) -> Result<(), String> {
        let (path, file_path) = {
            let mut sessions = self.editor_sessions.lock();
            let path = sessions.0.session_path(id);
            let file_path = sessions.0.file_backed_path(id);
            sessions.0.quit(id, self.blocks())?;
            (path, file_path)
        };
        // The session is gone — release the pin taken at open
        // (`editor_open_as`, docs/file-buffers.md P1) so eviction can
        // reclaim the entry.
        if let Some(fp) = file_path.as_deref() {
            self.file_cache.unpin(fp);
        }
        // The rollback wrote the block; drop the file cache's stale shadow.
        if let Some(path) = path.as_deref() {
            self.invalidate_config_file_cache(path);
        }
        self.editor_flows.publish(crate::flows::EditorFlow::Closed {
            session_id: id.as_u64(),
        });
        Ok(())
    }

    /// Invalidate the shared [`FileDocumentCache`] shadow for a **config** path
    /// after a write that touched the `ConfigDocFs` block **directly** (the vi
    /// editor's block mirror, `kj rc edit/reset/add/rm`, `kj config set/reset`).
    ///
    /// Config paths get a separate `file_context_id` shadow doc that backs the
    /// kaish `cat`/file-tool read path; a direct config-block write leaves it
    /// stale (and the symlink-lstat mtime can't self-heal it). Every such writer
    /// calls this so the next read reloads. A no-op for non-config paths.
    pub fn invalidate_config_file_cache(&self, path: &str) {
        // Called from an open session (which already knows the mount-table
        // answer via `EditorTarget::config_owned`) and from callers with no
        // session in scope (`kj rc`/`kj config`, always on their own trees)
        // alike — the latter is why this stays a path predicate rather than
        // taking the fact as a parameter. `is_config_doc_root` is the one
        // place that predicate lives; see `docs/file-buffers.md`.
        if kaijutsu_types::paths::is_config_doc_root(path)
            && let Err(e) = self.file_cache.invalidate_document(path)
        {
            // The cache shadow is now inconsistent with the written config block;
            // a later kaish `cat` could serve stale text. Loud, not swallowed.
            tracing::warn!("failed to invalidate file cache for {path}: {e}");
        }
    }

    /// Publish a session's current state on the editor push channel.
    fn publish_editor_state(
        &self,
        id: crate::editor::EditorSessionId,
        state: &crate::editor::EditorState,
    ) {
        self.editor_flows
            .publish(crate::flows::EditorFlow::StateChanged {
                session_id: id.as_u64(),
                state: state.clone(),
            });
    }

    /// Reconcile open editor sessions after a block's text changed underneath
    /// them (a sibling session, MCP edit, or streaming turn wrote it), and push
    /// the new state for every session that actually moved. Driven by the
    /// server's editor-reconciler task off the block flow; a no-op when nothing
    /// is bound to this block (the common case). A session's *own* mirror write
    /// is skipped (its buffer already matches), so this never echoes a
    /// self-edit. This is the remote-merge half of the push channel — the reason
    /// the editor channel is push, not poll (docs/vi.md step 1b).
    pub fn editor_reconcile_block(
        &self,
        context_id: kaijutsu_types::ContextId,
        block_id: kaijutsu_types::BlockId,
    ) {
        let changed = self
            .editor_sessions
            .lock()
            .0
            .reconcile_block(context_id, block_id, self.blocks());
        for (id, state) in &changed {
            self.publish_editor_state(*id, state);
        }
    }

    /// Mount a filesystem at the given path.
    /// Returns false if the mount table is frozen.
    pub async fn mount(
        &self,
        path: impl Into<std::path::PathBuf>,
        fs: impl VfsOps + 'static,
    ) -> bool {
        self.vfs.mount(path, fs).await
    }

    /// Mount a filesystem (already wrapped in Arc) at the given path.
    /// Returns false if the mount table is frozen.
    pub async fn mount_arc(
        &self,
        path: impl Into<std::path::PathBuf>,
        fs: Arc<dyn VfsOps>,
    ) -> bool {
        self.vfs.mount_arc(path, fs).await
    }

    /// Unmount a filesystem.
    pub async fn unmount(&self, path: impl AsRef<Path>) -> bool {
        self.vfs.unmount(path).await
    }

    /// Freeze the mount table — no more mount/unmount after this.
    pub fn freeze_mounts(&self) {
        self.vfs.freeze();
    }

    /// List all mounts.
    pub async fn list_mounts(&self) -> Vec<crate::vfs::MountInfo> {
        self.vfs.list_mounts().await
    }

    // ========================================================================
    // State
    // ========================================================================

    /// Get a variable value.
    pub async fn get_var(&self, name: &str) -> Option<String> {
        self.state.read().await.get_var(name).map(|s| s.to_string())
    }

    /// Set a variable value.
    pub async fn set_var(&self, name: impl Into<String>, value: impl Into<String>) {
        self.state.write().await.set_var(name, value);
    }

    /// Unset a variable.
    pub async fn unset_var(&self, name: &str) -> Option<String> {
        self.state.write().await.unset_var(name)
    }

    /// Add a command to history.
    pub async fn add_history(&self, command: impl Into<String>) -> u64 {
        self.state.write().await.add_history(command)
    }

    /// Add a command with result to history.
    pub async fn add_history_with_result(
        &self,
        command: impl Into<String>,
        output: impl Into<String>,
        exit_code: i32,
    ) -> u64 {
        self.state
            .write()
            .await
            .add_history_with_result(command, output, exit_code)
    }

    /// Get recent history.
    pub async fn recent_history(&self, limit: usize) -> Vec<crate::state::HistoryEntry> {
        self.state.read().await.recent_history(limit).to_vec()
    }

    /// Create a checkpoint.
    pub async fn checkpoint(&self, name: impl Into<String>) -> Uuid {
        self.state.write().await.checkpoint(name)
    }

    /// Restore to a checkpoint.
    pub async fn restore_checkpoint(&self, id: Uuid) -> bool {
        self.state.write().await.restore_checkpoint(id)
    }

    // ========================================================================
    // LLM Providers
    // ========================================================================

    /// Register an LLM provider.
    pub async fn register_llm(&self, name: impl Into<String>, provider: Arc<Provider>) {
        self.llm.write().await.register(name, provider);
    }

    /// Set the default LLM provider.
    pub async fn set_default_llm(&self, name: &str) -> bool {
        self.llm.write().await.set_default(name)
    }

    /// Get the LLM registry (for direct access).
    pub fn llm(&self) -> &RwLock<LlmRegistry> {
        &self.llm
    }

    /// List registered LLM providers.
    pub async fn list_llm_providers(&self) -> Vec<String> {
        self.llm
            .read()
            .await
            .list()
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    }

    // ========================================================================
    // Consent Mode
    // ========================================================================

    /// Get the current consent mode.
    pub async fn consent_mode(&self) -> ConsentMode {
        *self.consent_mode.read().await
    }

    /// Set the consent mode.
    pub async fn set_consent_mode(&self, mode: ConsentMode) {
        *self.consent_mode.write().await = mode;
    }

    // ========================================================================
    // Peers (drift navigation transport)
    // ========================================================================

    /// Attach a peer to this kernel.
    ///
    /// The optional `invoke_sender` enables kernel → peer invocation.
    pub async fn attach_peer(
        &self,
        config: PeerConfig,
        invoke_sender: Option<tokio::sync::mpsc::Sender<InvokeRequest>>,
    ) -> Result<PeerInfo, PeerError> {
        self.peers.write().await.attach(config, invoke_sender)
    }

    /// Invoke a peer by nick.
    ///
    /// Dispatches the request to the peer's registered channel and awaits
    /// the response. The kernel-side timeout (30s) is a safety net — the
    /// client-side timeout (15s) should fire first, producing a clean
    /// `Disconnected` rather than `Timeout`.
    pub async fn invoke_peer(
        &self,
        nick: &str,
        action: &str,
        params: Vec<u8>,
    ) -> Result<Vec<u8>, PeerError> {
        let sender = {
            let registry = self.peers.read().await;
            registry
                .get_invoke_sender(nick)
                .ok_or_else(|| PeerError::NotFound(nick.to_string()))?
        };
        // RwLock released before the async send
        let result = Self::send_invoke(&sender, action, params, nick).await;
        if matches!(result, Err(PeerError::Disconnected(_))) {
            // The bridge task is gone — its self-detach on conn_cancel should
            // have removed it, but reap as a backstop so a dead window can't
            // linger in the registry (and out of fan-out).
            self.peers.write().await.reap_closed();
        }
        result
    }

    /// Send one invoke request to an already-resolved peer channel and await the
    /// reply. Shared by [`invoke_peer`](Self::invoke_peer) (single nick target)
    /// and [`signal_open_editor`](Self::signal_open_editor) (principal fan-out).
    /// `label` is only for error context.
    async fn send_invoke(
        sender: &tokio::sync::mpsc::Sender<InvokeRequest>,
        action: &str,
        params: Vec<u8>,
        label: &str,
    ) -> Result<Vec<u8>, PeerError> {
        // Outermost hop of the peer ladder — see
        // `kaijutsu_types::timeout::peer`. The client and server bounds fire
        // before this one, by tested contract.
        const PEER_INVOKE_TIMEOUT: Duration = kaijutsu_types::timeout::peer::KERNEL_WAIT;

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let request = InvokeRequest {
            action: action.to_string(),
            params,
            reply: reply_tx,
        };

        if sender.send(request).await.is_err() {
            return Err(PeerError::Disconnected(format!("{label}: channel closed")));
        }

        let response = tokio::time::timeout(PEER_INVOKE_TIMEOUT, reply_rx)
            .await
            .map_err(|_| {
                PeerError::Timeout(format!(
                    "{label}: no reply after {}s",
                    PEER_INVOKE_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|_| PeerError::Disconnected(format!("{label}: handler dropped reply")))?;

        response.result.map_err(PeerError::InvocationFailed)
    }

    /// Signal app renderers to open on `session`/`path` — the `open_editor` peer
    /// nudge that pops a `Screen::Editor`. **Submitter-aware:** fans out to the
    /// submitter principal's app windows (the server-stamped principal — the
    /// app-id addressing infra), falling back to the well-known
    /// [`APP_PEER_NICK`](crate::editor::APP_PEER_NICK) when that principal owns
    /// no window (e.g. a model running `vi` headless).
    ///
    /// Best-effort: the editor session is already open, so a missing or
    /// unreachable renderer is **logged, never fatal** — a headless driver
    /// (`kj editor keys …`) needs no app. Observable (a warn line), not silent.
    ///
    /// Exact-window targeting (by the submitter's `instance`) is a follow-up:
    /// the app's `instance` is not yet threaded onto the execute path
    /// (`ConnectionState`→`ExecContext`), so principal fan-out is the current
    /// precision. See `docs/vi.md`.
    pub async fn signal_open_editor(
        &self,
        session: crate::editor::EditorSessionId,
        path: &str,
        state: &crate::editor::EditorState,
        submitter: Option<kaijutsu_types::PrincipalId>,
    ) {
        // Carry the initial state in the signal so the renderer has text to draw
        // the instant it lands — no fetch, no race against the first push. Reuses
        // the shared `EditorState::to_json` shape (`{session,text,cursor,mode,dirty}`)
        // plus the path; subsequent `editor.state_changed` pushes carry updates.
        let mut params_json = state.to_json(session);
        params_json["path"] = serde_json::Value::String(path.to_string());
        let params = match serde_json::to_vec(&params_json) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("open_editor: failed to encode signal params: {e}");
                return;
            }
        };

        // Target the submitter's app windows; fall back to the well-known nick.
        let targets = {
            let reg = self.peers.read().await;
            let by_principal = submitter
                .map(|p| reg.senders_by_principal(p))
                .unwrap_or_default();
            if by_principal.is_empty() {
                reg.get_invoke_sender(crate::editor::APP_PEER_NICK)
                    .into_iter()
                    .collect()
            } else {
                by_principal
            }
        };

        if targets.is_empty() {
            tracing::warn!(
                "open_editor: no app peer to signal for session {} (headless?) — \
                 the session is open; drive it with `kj editor keys {0} …`",
                session.as_u64()
            );
            return;
        }

        for sender in &targets {
            if let Err(e) = Self::send_invoke(sender, "open_editor", params.clone(), "open_editor").await
            {
                tracing::warn!("open_editor: signal to an app window failed (non-fatal): {e}");
            }
        }
    }

    /// [`editor_open`](Self::editor_open) **plus** the `open_editor` peer signal
    /// to the submitter's app windows. The ergonomic front doors (`vi`/`edit`,
    /// `kj editor open`, `kj rc edit`) use this so a human's `vi foo` pops a
    /// renderer; the wire `editorOpen` handler and tests call the plain
    /// `editor_open` (they are the renderer / a driver and need no nudge). One
    /// signal site, threaded the submitter principal from each door's caller.
    pub async fn editor_open_signaled(
        &self,
        path: &str,
        opener: Option<crate::editor::EditorOpener>,
    ) -> Result<(crate::editor::EditorSessionId, crate::editor::EditorState), String> {
        // Record the full opener so `fg` and `:r !cmd` can find the caller's
        // session + context later; the open_editor signal fans only to their
        // app windows, so it needs just the principal.
        let submitter = opener.map(|o| o.principal);
        let (id, state) = self.editor_open_as(path, opener).await?;
        self.signal_open_editor(id, path, &state, submitter).await;
        Ok((id, state))
    }

    /// `fg` — re-foreground the submitter's most-recently-opened editor session
    /// (job-control resume after a Ctrl+Z suspend). Re-fires the existing
    /// `open_editor` signal with the session's *current* state, so the app pops
    /// back to `Screen::Editor` via the same landing handler. Fails loud with
    /// "no editor session" when the principal has nothing suspended (so the shell
    /// reports it like bash's `fg: no current job`).
    pub async fn resume_editor(
        &self,
        submitter: Option<kaijutsu_types::PrincipalId>,
    ) -> Result<(crate::editor::EditorSessionId, crate::editor::EditorState), String> {
        let (id, path, state) = {
            let mut sessions = self.editor_sessions.lock();
            // Prefer the caller's own most-recent session — now that the opener
            // is captured at construction on every materialized-shell front door,
            // this is the normal path. The most-recent-of-any fallback remains a
            // shared-trust safety net for a caller with no recorded session (a
            // headless / context-less open) — single-user "the editor" is
            // unambiguous; precise multi-user targeting is a later refinement.
            let found = submitter
                .and_then(|p| sessions.0.latest_session_for(p))
                .or_else(|| sessions.0.latest_session_any());
            let (id, path) = found.ok_or_else(|| "fg: no editor session".to_string())?;
            let state = sessions.0.state(id)?;
            (id, path, state)
        };
        self.signal_open_editor(id, &path, &state, submitter).await;
        Ok((id, state))
    }

    /// Detach a peer from this kernel.
    pub async fn detach_peer(&self, nick: &str) -> Option<PeerInfo> {
        self.peers.write().await.detach(nick)
    }

    /// Detach a peer by key only if `sender` is still its registered channel —
    /// the bridge task's self-detach, safe against a re-attach having replaced
    /// the entry. Returns whether it removed anything.
    pub async fn detach_peer_if_sender(
        &self,
        key: &str,
        sender: &tokio::sync::mpsc::Sender<InvokeRequest>,
    ) -> bool {
        self.peers.write().await.detach_if_sender(key, sender)
    }

    /// Get information about an attached peer.
    pub async fn get_peer(&self, nick: &str) -> Option<PeerInfo> {
        self.peers.read().await.get(nick).cloned()
    }

    /// List all attached peers.
    pub async fn list_peers(&self) -> Vec<PeerInfo> {
        self.peers
            .read()
            .await
            .list()
            .into_iter()
            .cloned()
            .collect()
    }

    /// Get the peer registry (for direct access).
    pub fn peers(&self) -> &RwLock<PeerRegistry> {
        &self.peers
    }

    /// Count of attached peers.
    pub async fn peer_count(&self) -> usize {
        self.peers.read().await.count()
    }
}

// Delegate VfsOps to the mount table
#[async_trait]
impl VfsOps for Kernel {
    async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
        self.vfs.getattr(path).await
    }

    async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
        self.vfs.readdir(path).await
    }

    async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        self.vfs.read(path, offset, size).await
    }

    async fn readlink(&self, path: &Path) -> VfsResult<std::path::PathBuf> {
        self.vfs.readlink(path).await
    }

    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32> {
        self.vfs.write(path, offset, data).await
    }

    async fn create(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        self.vfs.create(path, mode).await
    }

    async fn mkdir(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        self.vfs.mkdir(path, mode).await
    }

    async fn unlink(&self, path: &Path) -> VfsResult<()> {
        self.vfs.unlink(path).await
    }

    async fn rmdir(&self, path: &Path) -> VfsResult<()> {
        self.vfs.rmdir(path).await
    }

    async fn rename(&self, from: &Path, to: &Path) -> VfsResult<()> {
        self.vfs.rename(from, to).await
    }

    async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()> {
        self.vfs.truncate(path, size).await
    }

    async fn setattr(&self, path: &Path, attr: SetAttr) -> VfsResult<FileAttr> {
        self.vfs.setattr(path, attr).await
    }

    async fn symlink(&self, path: &Path, target: &Path) -> VfsResult<FileAttr> {
        self.vfs.symlink(path, target).await
    }

    async fn link(&self, oldpath: &Path, newpath: &Path) -> VfsResult<FileAttr> {
        self.vfs.link(oldpath, newpath).await
    }

    fn read_only(&self) -> bool {
        self.vfs.read_only()
    }

    async fn statfs(&self) -> VfsResult<StatFs> {
        self.vfs.statfs().await
    }

    async fn real_path(&self, path: &Path) -> VfsResult<Option<std::path::PathBuf>> {
        self.vfs.real_path(path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::paths::RC_ROOT;

    #[tokio::test]
    async fn test_kernel_creation() {
        let kernel = Kernel::new_ephemeral("test").await;
        assert_eq!(kernel.name().await, "test");
    }

    /// Drive the kernel-owned editor surface end to end: open an rc block, type,
    /// observe state, roll back. Proves the methods + the `!Send` registry
    /// integration work through the shared kernel.
    #[tokio::test]
    async fn editor_session_roundtrip_through_kernel() {
        use crate::runtime::config_doc_fs::ConfigDocFs;
        use crate::vfs::VfsOps as _;
        use std::path::Path;

        let kernel = Kernel::new_ephemeral("test").await;

        // Seed an rc script through its owning ConfigDocFs backend, over the
        // kernel's own block store — the same one its file_cache() is built
        // over, so editor_open below resolves through one coherent instance.
        let blocks = kernel.blocks().clone();
        ConfigDocFs::new(blocks.clone(), RC_ROOT)
            .write_all(Path::new("coder/create/S00.kai"), b"hello")
            .await
            .unwrap();
        // Mount it so the resolver's mount-table query routes the path to the
        // config backend (same blocks, so it finds the seeded block).
        kernel
            .mount(RC_ROOT, ConfigDocFs::new(blocks.clone(), RC_ROOT))
            .await;
        let path = "/etc/rc/coder/create/S00.kai";

        // Open → type → state reflects, all through the kernel surface.
        let (id, st) = kernel.editor_open(path).await.unwrap();
        assert_eq!(st.text, "hello");
        let st = kernel.editor_keys(id, "iX<Esc>").await.unwrap();
        assert_eq!(st.text, "Xhello");
        assert!(st.dirty);
        assert_eq!(kernel.editor_state(id).unwrap().text, "Xhello");

        // ZQ rolls the block back and closes the session.
        kernel.editor_quit(id).unwrap();
        let err = kernel.editor_keys(id, "x").await.unwrap_err();
        assert!(err.contains("no such session"), "got: {err}");
    }

    #[tokio::test]
    async fn editor_edit_invalidates_the_file_cache_shadow() {
        // A config path gets a *shadow* copy in the FileDocumentCache (keyed by
        // file_context_id) separate from the ConfigDocFs block the editor writes
        // (config_context_id). A direct editor block write would leave that shadow
        // stale — so a kaish `cat` after an in-app edit would serve old bytes.
        // Kernel::editor_keys must invalidate the shadow so the next read reloads.
        use crate::block_store::SharedBlockStore;
        use crate::runtime::config_doc_fs::ConfigDocFs;
        use crate::vfs::VfsOps as _;
        use kaijutsu_types::{BlockId, ContextId};
        use std::path::Path;

        fn block_content(blocks: &SharedBlockStore, ctx: ContextId, block: &BlockId) -> String {
            blocks
                .block_snapshots(ctx)
                .unwrap()
                .into_iter()
                .find(|s| s.id == *block)
                .expect("shadow block present")
                .content
        }

        let kernel = Kernel::new_ephemeral("test").await;
        let blocks = kernel.blocks().clone();

        // Mount the rc backend on the kernel VFS (so the cache reads through it),
        // then seed a config script over the same store.
        kernel
            .mount(RC_ROOT, ConfigDocFs::new(blocks.clone(), RC_ROOT))
            .await;
        ConfigDocFs::new(blocks.clone(), RC_ROOT)
            .write_all(Path::new("coder/create/S00.kai"), b"hello")
            .await
            .unwrap();
        let path = "/etc/rc/coder/create/S00.kai";

        // The kernel's own file cache, over the same store + kernel VFS —
        // the editor's invalidation and our reads must hit the same instance.
        let cache = kernel.file_cache().clone();

        // Populate the shadow from the source.
        let (sctx, sblock) = cache.get_or_load(path).await.unwrap();
        assert_eq!(
            block_content(&blocks, sctx, &sblock),
            "hello",
            "shadow loads the source content"
        );

        // Edit the config block through the editor (insert X at the front).
        let (id, _) = kernel.editor_open(path).await.unwrap();
        kernel.editor_keys(id, "iX<Esc>").await.unwrap();

        // The next read must reflect the edit — proving the shadow was dropped and
        // reloaded, not re-served stale. (With a plain cache-entry invalidate the
        // surviving shadow doc would re-serve "hello" and this fails.)
        let (sctx2, sblock2) = cache.get_or_load(path).await.unwrap();
        assert_eq!(
            block_content(&blocks, sctx2, &sblock2),
            "Xhello",
            "kaish read sees the editor's edit after invalidation"
        );
    }

    #[tokio::test]
    async fn editor_colon_r_reads_a_file_into_the_buffer() {
        // `:r <file>` slurps a file's contents at the cursor — the async fetch
        // (read_content via the FileDocumentCache) happens inside Kernel::editor_keys
        // *outside* the session lock; the result mirrors onto the editor's block.
        use crate::runtime::config_doc_fs::ConfigDocFs;
        use crate::vfs::VfsOps as _;
        use std::path::Path;

        let kernel = Kernel::new_ephemeral("test").await;
        let blocks = kernel.blocks().clone();

        kernel
            .mount(RC_ROOT, ConfigDocFs::new(blocks.clone(), RC_ROOT))
            .await;
        let rc = ConfigDocFs::new(blocks.clone(), RC_ROOT);
        // The block we'll edit, and a separate file we'll read into it.
        rc.write_all(Path::new("coder/create/S00.kai"), b"AB")
            .await
            .unwrap();
        rc.write_all(Path::new("coder/create/snippet.kai"), b"INSERTED")
            .await
            .unwrap();
        let edit_path = "/etc/rc/coder/create/S00.kai";
        let read_path = "/etc/rc/coder/create/snippet.kai";

        let (id, st) = kernel.editor_open(edit_path).await.unwrap();
        assert_eq!(st.text, "AB");

        // Move the cursor one char right (between A and B), then `:r` the file.
        kernel.editor_keys(id, "l").await.unwrap();
        let after = kernel
            .editor_keys(id, &format!(":r {read_path}<CR>"))
            .await
            .unwrap();
        assert_eq!(after.text, "AINSERTEDB", "file content spliced at the cursor");
        assert!(after.dirty, ":r dirties the buffer");
    }

    #[tokio::test]
    async fn resume_editor_finds_the_opener_session_or_fails_loud() {
        // `fg`: with nothing suspended → fail loud; after a signaled open for a
        // principal → resume_editor returns that session's current state.
        use crate::runtime::config_doc_fs::ConfigDocFs;
        use crate::vfs::VfsOps as _;
        use kaijutsu_types::{ContextId, PrincipalId};
        use std::path::Path;

        let kernel = Kernel::new_ephemeral("test").await;
        let blocks = kernel.blocks().clone();
        kernel
            .mount(RC_ROOT, ConfigDocFs::new(blocks.clone(), RC_ROOT))
            .await;
        ConfigDocFs::new(blocks.clone(), RC_ROOT)
            .write_all(Path::new("coder/create/S00.kai"), b"hello")
            .await
            .unwrap();
        let path = "/etc/rc/coder/create/S00.kai";

        // Nothing open at all → fail loud (no session to foreground).
        let me = PrincipalId::system();
        assert!(
            kernel.resume_editor(Some(me)).await.is_err(),
            "fg with nothing open fails loud"
        );
        assert!(kernel.resume_editor(None).await.is_err(), "...regardless of principal");

        // Open as `me` (records the opener), then resume finds it by principal.
        let me_opener = crate::editor::EditorOpener {
            principal: me,
            context_id: ContextId::new(),
            session_id: kaijutsu_types::SessionId::new(),
        };
        let (id, _) = kernel
            .editor_open_as(path, Some(me_opener))
            .await
            .unwrap();
        let (resumed_id, st) = kernel.resume_editor(Some(me)).await.unwrap();
        assert_eq!(resumed_id, id, "fg foregrounds the principal's session");
        assert_eq!(st.text, "hello");

        // Shared-trust fallback: even a caller with no recorded session (or no
        // principal at all — e.g. an opener-less open via the external MCP path)
        // resumes the most-recent editor.
        let (fallback_id, _) = kernel.resume_editor(None).await.unwrap();
        assert_eq!(fallback_id, id, "fg falls back to the most-recent editor");
    }

    #[tokio::test]
    async fn editor_colon_r_shell_runs_in_the_opener_context() {
        // `:r !cmd` materializes a kaish in the *opener's* context and splices the
        // command's stdout at the cursor. Wire the dispatcher into the broker (the
        // production shape — `set_self_arc` + `broker().set_kj_dispatcher`) so
        // `fetch_editor_io` can reach it.
        use crate::kj::test_helpers::{
            install_rc_script_file, register_context, test_dispatcher_rc,
        };
        use kaijutsu_types::PrincipalId;
        use kaijutsu_types::SessionId;

        let d = Arc::new(test_dispatcher_rc().await);
        d.set_self_arc();
        d.kernel().broker().set_kj_dispatcher(&d).await;
        let kernel = d.kernel();

        let path = "/etc/rc/vitest/create/S00-foo.kai";
        install_rc_script_file(&d, path, "hello").await;

        // A real registered context for `:r !cmd` to run in.
        let principal = PrincipalId::system();
        let context_id = register_context(&d, Some("vi-r"), None, principal);
        let opener = crate::editor::EditorOpener {
            principal,
            context_id,
            session_id: SessionId::new(),
        };

        let (id, _) = kernel
            .editor_open_as(path, Some(opener))
            .await
            .unwrap();
        // `:r !echo hi` splices the command's stdout at the cursor (buffer top).
        let state = kernel
            .editor_keys(id, ":r !echo hi<CR>")
            .await
            .unwrap();
        assert!(
            state.text.contains("hi"),
            "':r !echo' must splice command stdout: {:?}",
            state.text
        );
    }

    #[tokio::test]
    async fn editor_colon_r_shell_without_opener_reports_on_the_status_line() {
        // A headless open (no opener) has no context to shell out in — `:r !cmd`
        // must fail loud pointing at the interactive shell, never silently no-op.
        // "Loud" means the `:` status line (the dialect-level channel), NOT an
        // RPC error: the session stays open and the message rides the state.
        use crate::runtime::config_doc_fs::ConfigDocFs;
        use crate::vfs::VfsOps as _;
        use std::path::Path;

        let kernel = Kernel::new_ephemeral("test").await;
        let blocks = kernel.blocks().clone();
        kernel
            .mount(RC_ROOT, ConfigDocFs::new(blocks.clone(), RC_ROOT))
            .await;
        ConfigDocFs::new(blocks.clone(), RC_ROOT)
            .write_all(Path::new("coder/create/S00.kai"), b"hi")
            .await
            .unwrap();

        // `editor_open` records no opener.
        let (id, _) = kernel
            .editor_open("/etc/rc/coder/create/S00.kai")
            .await
            .unwrap();
        let state = kernel
            .editor_keys(id, ":r !date<CR>")
            .await
            .expect("a failed :r does not error the RPC");
        let msg = state.message.as_deref().expect("the failure reports on the status line");
        assert!(msg.contains("needs an opener context"), "got: {msg}");
        assert_eq!(state.text, "hi", "the buffer is untouched");
        assert!(
            kernel.editor_state(id).is_ok(),
            "the session survives a failed :r"
        );
    }

    #[tokio::test]
    async fn editor_colon_r_missing_file_reports_on_the_status_line() {
        // `:r <missing>` is a dialect-level failure: the fetch error rides the
        // `:` status line and the session stays open — never a silent no-op,
        // never an RPC error the GUI can't display.
        use crate::runtime::config_doc_fs::ConfigDocFs;
        use crate::vfs::VfsOps as _;
        use std::path::Path;

        let kernel = Kernel::new_ephemeral("test").await;
        let blocks = kernel.blocks().clone();
        kernel
            .mount(RC_ROOT, ConfigDocFs::new(blocks.clone(), RC_ROOT))
            .await;
        ConfigDocFs::new(blocks.clone(), RC_ROOT)
            .write_all(Path::new("coder/create/S00.kai"), b"hi")
            .await
            .unwrap();

        let (id, _) = kernel
            .editor_open("/etc/rc/coder/create/S00.kai")
            .await
            .unwrap();
        let state = kernel
            .editor_keys(id, ":r /nope/missing.txt<CR>")
            .await
            .expect("a missing :r file does not error the RPC");
        assert!(
            state.message.is_some(),
            "the fetch failure reports on the status line"
        );
        assert_eq!(state.text, "hi", "the buffer is untouched");
        // The session is alive and the message is transient.
        let state = kernel.editor_keys(id, "l").await.unwrap();
        assert!(state.message.is_none(), "message clears on the next batch");
    }

    // ── The gap: the editor never touched the file-document cache ───────────
    // (docs/file-buffers.md). These cover the fix: an edit to a file-backed
    // session records the durable swap marker, and `:w`/`:wq`/`ZZ` flush to
    // disk through the same cache every other writer uses.

    /// Mount a `MemoryBackend` (an ordinary, non-config VFS backend — no
    /// `owns_config_docs`) so `resolve_editor_target` routes through
    /// `FileDocumentCache::get_or_load`, the "ordinary file" branch, not the
    /// config-doc branch the other editor tests in this module exercise.
    async fn kernel_with_mem_fs() -> Kernel {
        use crate::vfs::backends::MemoryBackend;
        let kernel = Kernel::new_ephemeral("test").await;
        kernel.mount("/mem", MemoryBackend::new()).await;
        kernel
    }

    #[tokio::test]
    async fn editor_edit_marks_a_file_backed_session_dirty_in_the_cache() {
        use crate::vfs::VfsOps as _;
        use std::path::Path;

        let kernel = kernel_with_mem_fs().await;
        let path = Path::new("/mem/note.txt");
        kernel.vfs().write_all(path, b"hello").await.unwrap();

        let (id, st) = kernel.editor_open("/mem/note.txt").await.unwrap();
        assert_eq!(st.text, "hello");
        assert!(
            kernel
                .kernel_db()
                .lock()
                .list_dirty_file_buffers()
                .unwrap()
                .is_empty(),
            "opening clean must not mark anything dirty"
        );

        kernel.editor_keys(id, "iX<Esc>").await.unwrap();

        let rows = kernel.kernel_db().lock().list_dirty_file_buffers().unwrap();
        assert!(
            rows.iter().any(|r| r.path == "/mem/note.txt"),
            "an edit to a file-backed session must record a dirty_file_buffers \
             row — unsaved editor work must survive a restart, got rows: {rows:?}"
        );
    }

    #[tokio::test]
    async fn colon_w_flushes_a_file_backed_session_to_disk_and_clears_the_swap_row() {
        use crate::vfs::VfsOps as _;
        use std::path::Path;

        let kernel = kernel_with_mem_fs().await;
        let path = Path::new("/mem/note.txt");
        kernel.vfs().write_all(path, b"hello").await.unwrap();

        let (id, _) = kernel.editor_open("/mem/note.txt").await.unwrap();
        kernel.editor_keys(id, "iX<Esc>").await.unwrap();
        // Not on disk yet — only `:w` flushes.
        assert_eq!(
            kernel.vfs().read_all(path).await.unwrap(),
            b"hello",
            "an edit alone must not reach disk"
        );

        let st = kernel.editor_keys(id, ":w<CR>").await.unwrap();
        assert!(!st.dirty, ":w clears the editor's own dirty flag");
        assert_eq!(
            String::from_utf8(kernel.vfs().read_all(path).await.unwrap()).unwrap(),
            "Xhello",
            ":w must write the buffer to disk through the VFS"
        );
        assert!(
            !kernel
                .kernel_db()
                .lock()
                .list_dirty_file_buffers()
                .unwrap()
                .iter()
                .any(|r| r.path == "/mem/note.txt"),
            "a flushed buffer's swap row must be gone"
        );
    }

    #[tokio::test]
    async fn colon_wq_and_zz_flush_a_file_backed_session_before_closing() {
        use crate::vfs::VfsOps as _;
        use std::path::Path;

        let kernel = kernel_with_mem_fs().await;

        // `:wq`
        let path_a = Path::new("/mem/a.txt");
        kernel.vfs().write_all(path_a, b"hello").await.unwrap();
        let (id_a, _) = kernel.editor_open("/mem/a.txt").await.unwrap();
        kernel.editor_keys(id_a, "iX<Esc>").await.unwrap();
        kernel.editor_keys(id_a, ":wq<CR>").await.unwrap();
        assert_eq!(
            String::from_utf8(kernel.vfs().read_all(path_a).await.unwrap()).unwrap(),
            "Xhello",
            ":wq must flush before closing the session"
        );

        // `ZZ`
        let path_b = Path::new("/mem/b.txt");
        kernel.vfs().write_all(path_b, b"hello").await.unwrap();
        let (id_b, _) = kernel.editor_open("/mem/b.txt").await.unwrap();
        kernel.editor_keys(id_b, "iY<Esc>").await.unwrap();
        kernel.editor_keys(id_b, "ZZ").await.unwrap();
        assert_eq!(
            String::from_utf8(kernel.vfs().read_all(path_b).await.unwrap()).unwrap(),
            "Yhello",
            "ZZ must flush before closing the session"
        );

        assert!(
            kernel
                .kernel_db()
                .lock()
                .list_dirty_file_buffers()
                .unwrap()
                .is_empty(),
            "both sessions flushed cleanly — no swap rows left"
        );
    }

    #[tokio::test]
    async fn editor_edit_on_a_config_owned_session_never_touches_the_file_cache() {
        // Config/rc blocks have no host file — flushing the FileDocumentCache's
        // *shadow* entry for a config path would write the wrong content (a
        // stale read-through copy, not the block the editor is bound to) to
        // the wrong place. A config-owned session must never mark or flush it.
        use crate::runtime::config_doc_fs::ConfigDocFs;
        use crate::vfs::VfsOps as _;
        use std::path::Path;

        let kernel = Kernel::new_ephemeral("test").await;
        let blocks = kernel.blocks().clone();
        kernel
            .mount(RC_ROOT, ConfigDocFs::new(blocks.clone(), RC_ROOT))
            .await;
        ConfigDocFs::new(blocks.clone(), RC_ROOT)
            .write_all(Path::new("coder/create/S00.kai"), b"hello")
            .await
            .unwrap();
        let path = "/etc/rc/coder/create/S00.kai";

        let (id, _) = kernel.editor_open(path).await.unwrap();
        kernel.editor_keys(id, "iX<Esc>").await.unwrap();
        let st = kernel.editor_keys(id, ":w<CR>").await.unwrap();
        assert!(
            st.message.is_none(),
            "a config-owned :w must not report a (nonexistent) flush failure"
        );

        assert!(
            kernel
                .kernel_db()
                .lock()
                .list_dirty_file_buffers()
                .unwrap()
                .is_empty(),
            "a config-owned session has no host file and must never record a \
             file-cache swap row"
        );
    }

    #[tokio::test]
    async fn colon_w_on_a_cat_ed_client_path_does_not_revert_the_edit() {
        // `/etc/client` is a ConfigDocFs-owned tree exactly like `/etc/rc` —
        // an editor session on it must never be treated as file-backed, even
        // after an unrelated `cat` (any FileDocumentCache read) has minted a
        // shadow cache entry for the same path. See docs/file-buffers.md.
        use crate::runtime::config_doc_fs::ConfigDocFs;
        use crate::vfs::VfsOps as _;
        use kaijutsu_types::paths::CLIENT_ROOT;
        use std::path::Path;

        let kernel = Kernel::new_ephemeral("test").await;
        let blocks = kernel.blocks().clone();
        kernel
            .mount(CLIENT_ROOT, ConfigDocFs::new(blocks.clone(), CLIENT_ROOT))
            .await;
        ConfigDocFs::new(blocks.clone(), CLIENT_ROOT)
            .write_all(Path::new("theme.toml"), b"orig")
            .await
            .unwrap();
        let path = "/etc/client/theme.toml";

        // Mint the FileDocumentCache shadow the same way a kaish `cat` or an
        // MCP read would — this is the precondition B1 names as "one shell
        // read away".
        kernel.file_cache().get_or_load(path).await.unwrap();

        let (id, st) = kernel.editor_open(path).await.unwrap();
        assert_eq!(st.text, "orig");
        kernel.editor_keys(id, "iX<Esc>").await.unwrap();
        let st = kernel.editor_keys(id, ":w<CR>").await.unwrap();
        assert!(
            st.message.is_none(),
            "a config-owned :w must not report a (nonexistent) flush failure, got: {:?}",
            st.message
        );

        assert_eq!(
            String::from_utf8(kernel.vfs().read_all(Path::new(path)).await.unwrap()).unwrap(),
            "Xorig",
            "the config block must carry the edit, not the stale pre-edit shadow"
        );
        assert!(
            kernel
                .kernel_db()
                .lock()
                .list_dirty_file_buffers()
                .unwrap()
                .is_empty(),
            "a config-owned session has no host file and must never record a \
             file-cache swap row"
        );
    }

    #[tokio::test]
    async fn colon_w_round_trips_multibyte_content_to_disk() {
        // Regression coverage for the class of bug docs/vi.md already records
        // (`create_or_replace` once deleted BYTE count instead of CHAR count
        // and panicked on multi-byte content) — reusing the same characters
        // (改善, an em-dash) through the *new* flush path.
        use crate::vfs::VfsOps as _;
        use std::path::Path;

        let kernel = kernel_with_mem_fs().await;
        let path = Path::new("/mem/kanji.txt");
        kernel
            .vfs()
            .write_all(path, "改善—work".as_bytes())
            .await
            .unwrap();

        let (id, st) = kernel.editor_open("/mem/kanji.txt").await.unwrap();
        assert_eq!(st.text, "改善—work");

        kernel.editor_keys(id, "iX<Esc>").await.unwrap();
        let st = kernel.editor_keys(id, ":w<CR>").await.unwrap();
        assert_eq!(st.text, "X改善—work");

        let disk = String::from_utf8(kernel.vfs().read_all(path).await.unwrap()).unwrap();
        assert_eq!(
            disk, "X改善—work",
            "multi-byte content must round-trip through :w intact"
        );
    }

    /// W12 (`docs/file-buffers.md` rule 3, slice 3): a plain `:w` refuses
    /// once disk has moved under the buffer since it was loaded, keeps the
    /// session open and dirty, and never touches disk; `:w!` overrides.
    #[tokio::test]
    async fn colon_w_refuses_when_disk_moved_then_colon_w_bang_overrides() {
        use crate::vfs::VfsOps as _;
        use std::path::Path;

        let kernel = kernel_with_mem_fs().await;
        let path = Path::new("/mem/note.txt");
        kernel.vfs().write_all(path, b"hello").await.unwrap();

        let (id, _) = kernel.editor_open("/mem/note.txt").await.unwrap();
        kernel.editor_keys(id, "iX<Esc>").await.unwrap();

        // An external writer moves disk out from under the open buffer.
        kernel
            .vfs()
            .write_all(path, b"external-edit")
            .await
            .unwrap();

        let st = kernel.editor_keys(id, ":w<CR>").await.unwrap();
        assert!(
            st.dirty,
            "a refused :w must leave the buffer dirty — the checkpoint must not advance"
        );
        let msg = st.message.expect("a refused :w must report on the status line");
        assert!(
            msg.starts_with("W12:") && msg.contains("add ! to override"),
            "expected a W12 message naming the override, got: {msg}"
        );
        assert_eq!(
            String::from_utf8(kernel.vfs().read_all(path).await.unwrap()).unwrap(),
            "external-edit",
            "a refused :w must not touch disk"
        );
        assert!(
            kernel
                .kernel_db()
                .lock()
                .list_dirty_file_buffers()
                .unwrap()
                .iter()
                .any(|r| r.path == "/mem/note.txt"),
            "a refused :w must not clear the swap row"
        );

        // `:w!` overrides the refusal.
        let st = kernel.editor_keys(id, ":w!<CR>").await.unwrap();
        assert!(!st.dirty, ":w! must clear the editor's dirty flag once it lands");
        assert_eq!(
            String::from_utf8(kernel.vfs().read_all(path).await.unwrap()).unwrap(),
            "Xhello",
            ":w! must overwrite disk with the buffer's content"
        );
    }

    // ── A failed `:w` (docs/issues.md, "A failed `:w` reports clean, and
    // retrying it hits the wrong error") ────────────────────────────────────

    /// A file-backed session over a read-only host mount: `flush_one`'s VFS
    /// write genuinely fails (`VfsError::ReadOnly`), the honest way to
    /// provoke the failure without a test-only injection hook. The seed file
    /// is written directly to the host path before mounting, since the mount
    /// itself refuses writes.
    async fn kernel_with_readonly_fs(initial: &[u8]) -> (Kernel, tempfile::TempDir) {
        use crate::vfs::backends::LocalBackend;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.txt"), initial).unwrap();
        let kernel = Kernel::new_ephemeral("test").await;
        kernel.mount("/mem", LocalBackend::read_only(dir.path())).await;
        (kernel, dir)
    }

    #[tokio::test]
    async fn failed_w_leaves_the_buffer_dirty_and_q_still_refuses() {
        let (kernel, _dir) = kernel_with_readonly_fs(b"hello").await;

        let (id, st) = kernel.editor_open("/mem/note.txt").await.unwrap();
        assert_eq!(st.text, "hello");

        kernel.editor_keys(id, "iX<Esc>").await.unwrap();
        let st = kernel.editor_keys(id, ":w<CR>").await.unwrap();
        assert!(
            st.dirty,
            "a flush that failed must not let the buffer read clean"
        );
        assert!(
            st.message.as_deref().unwrap_or_default().starts_with("E212"),
            "got: {:?}",
            st.message
        );

        // The swap row must survive the failed flush — a following `:q`
        // still refuses with vim's E37, exactly as if `:w` had never run.
        let st = kernel.editor_keys(id, ":q<CR>").await.unwrap();
        assert!(
            st.message
                .as_deref()
                .unwrap_or_default()
                .contains("No write since last change"),
            "got: {:?}",
            st.message
        );
        assert!(kernel.editor_state(id).is_ok(), "the session must still be open");
        assert!(
            kernel
                .kernel_db()
                .lock()
                .list_dirty_file_buffers()
                .unwrap()
                .iter()
                .any(|r| r.path == "/mem/note.txt"),
            "the swap row must survive a failed flush"
        );
    }

    #[tokio::test]
    async fn failed_w_then_a_successful_retry_clears_the_row_without_a_swap_error() {
        use crate::vfs::backends::LocalBackend;
        use crate::vfs::VfsOps as _;
        use std::path::Path;

        let (kernel, dir) = kernel_with_readonly_fs(b"hello").await;

        let (id, _) = kernel.editor_open("/mem/note.txt").await.unwrap();
        kernel.editor_keys(id, "iX<Esc>").await.unwrap();
        let st = kernel.editor_keys(id, ":w<CR>").await.unwrap();
        assert!(st.dirty, "first :w must fail against the read-only mount");

        // Whatever caused the failure clears (permissions restored, disk
        // space freed) — remount the same host directory read-write.
        kernel.unmount("/mem").await;
        kernel.mount("/mem", LocalBackend::new(dir.path())).await;

        let st = kernel.editor_keys(id, ":w<CR>").await.unwrap();
        assert!(
            st.message.is_none(),
            "a retry must not report the swap-acknowledgement error, got: {:?}",
            st.message
        );
        assert!(!st.dirty, "the successful retry must clear dirty");
        assert_eq!(
            String::from_utf8(kernel.vfs().read_all(Path::new("/mem/note.txt")).await.unwrap())
                .unwrap(),
            "Xhello",
            "the retried write must land"
        );
        assert!(
            kernel
                .kernel_db()
                .lock()
                .list_dirty_file_buffers()
                .unwrap()
                .iter()
                .all(|r| r.path != "/mem/note.txt"),
            "the swap row must clear once the retry lands"
        );
    }

    // ── P1: an evicted editor buffer must not make `:w` report success and
    // write nothing (docs/issues.md "Tech-debt audits, 2026-08-20";
    // docs/audits/2026-08-20-editor-fileio.md "BUGS B2") ─────────────────────

    /// Before the fix, an editor session held no reference into the file
    /// cache, so ordinary reads of *other* paths (every MCP read/kaish `cat`
    /// inserts a cache entry) could evict the session's own clean entry
    /// while it sat idle between open and its first edit. `mark_dirty` and
    /// `flush_one` then silently no-op'd on the now-uncached path — `:w`
    /// reported success while nothing reached disk.
    ///
    /// Reproduces the real eviction pressure (not a hand-rolled removal):
    /// open a session, then read 80 unrelated files through the *same*
    /// shared `FileDocumentCache` — well past `DEFAULT_MAX_CACHED` (64) —
    /// exactly the "64 other files get read" scenario the bug report
    /// describes. The fix (`editor_open_as` pins the session's entry) must
    /// make it survive that pressure, so the eventual `:w` actually reaches
    /// disk.
    #[tokio::test]
    async fn editor_session_survives_cache_pressure_that_would_have_evicted_it() {
        use crate::vfs::VfsOps as _;
        use std::path::Path;

        let kernel = kernel_with_mem_fs().await;
        let path = Path::new("/mem/note.txt");
        kernel.vfs().write_all(path, b"hello").await.unwrap();

        let (id, _) = kernel.editor_open("/mem/note.txt").await.unwrap();

        for i in 0..80 {
            let other = format!("/mem/other{i}.txt");
            kernel
                .vfs()
                .write_all(Path::new(&other), b"x")
                .await
                .unwrap();
            kernel.file_cache().read_content(&other).await.unwrap();
        }

        kernel.editor_keys(id, "iX<Esc>").await.unwrap();
        let st = kernel.editor_keys(id, ":w<CR>").await.unwrap();

        // The pin (not a fallback reload) is the fix: `:w` must succeed
        // outright, never merely fail loud instead of silently no-op'ing.
        assert!(
            !st.dirty,
            "the session's cache entry must survive eviction pressure from \
             unrelated reads — :w reported a failure instead of succeeding: {:?}",
            st.message
        );
        assert_eq!(
            String::from_utf8(kernel.vfs().read_all(path).await.unwrap()).unwrap(),
            "Xhello",
            ":w reported clean but the edit never reached disk — the P1 \
             evicted-buffer bug"
        );
    }

    /// The pin releases when the session closes: after `:wq`, the same path
    /// is just an ordinary clean cache entry again and ordinary eviction
    /// pressure can reclaim it. Guards against the fix leaking a pin forever.
    #[tokio::test]
    async fn editor_quit_releases_the_pin_so_the_entry_can_be_evicted_again() {
        use crate::vfs::VfsOps as _;
        use std::path::Path;

        let kernel = kernel_with_mem_fs().await;
        let path = Path::new("/mem/note.txt");
        kernel.vfs().write_all(path, b"hello").await.unwrap();

        let (id, _) = kernel.editor_open("/mem/note.txt").await.unwrap();
        kernel.editor_keys(id, "iX<Esc>").await.unwrap();
        kernel.editor_keys(id, ":wq<CR>").await.unwrap();

        for i in 0..80 {
            let other = format!("/mem/other{i}.txt");
            kernel
                .vfs()
                .write_all(Path::new(&other), b"x")
                .await
                .unwrap();
            kernel.file_cache().read_content(&other).await.unwrap();
        }

        // A fresh mark_dirty on the (now unpinned, possibly evicted) path
        // must fail — proving the released entry is treated the same as any
        // other uncached path, not left permanently pinned.
        let err = kernel
            .file_cache()
            .mark_dirty("/mem/note.txt")
            .expect_err(
                "an uncached path (evicted after the pin released) must \
                 refuse mark_dirty, not silently succeed",
            );
        assert!(err.contains("note.txt"), "error must name the path: {err}");
    }

    #[tokio::test]
    async fn test_variables() {
        let kernel = Kernel::new_ephemeral("test").await;

        kernel.set_var("FOO", "bar").await;
        assert_eq!(kernel.get_var("FOO").await, Some("bar".to_string()));

        kernel.unset_var("FOO").await;
        assert_eq!(kernel.get_var("FOO").await, None);
    }

    #[tokio::test]
    async fn test_history() {
        let kernel = Kernel::new_ephemeral("test").await;

        kernel.add_history("echo hello").await;
        kernel.add_history("ls -la").await;

        let history = kernel.recent_history(10).await;
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].command, "echo hello");
    }

    #[tokio::test]
    async fn test_llm_provider() {
        let kernel = Kernel::new_ephemeral("test").await;

        // Register a provider (uses fake key, won't actually call API)
        let provider = Arc::new(Provider::Claude(crate::llm::claude::Client::new(
            "fake-key",
        )));
        kernel.register_llm("anthropic", provider).await;
        kernel.set_default_llm("anthropic").await;

        // Check provider is listed
        let providers = kernel.list_llm_providers().await;
        assert_eq!(providers, vec!["anthropic"]);
    }

    #[tokio::test]
    async fn test_llm_no_provider() {
        let kernel = Kernel::new_ephemeral("test").await;

        // Should fail gracefully without provider
        let result = kernel.llm().read().await.prompt("Hello").await;
        assert!(result.is_err());
    }
}
