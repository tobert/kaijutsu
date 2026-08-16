//! ActorPlugin — thin Bevy integration for ActorHandle.
//!
//! Replaces the monolithic ConnectionBridge with a minimal plugin that:
//! - Spawns a bootstrap thread (owns tokio + LocalSet for !Send capnp types)
//! - Polls broadcast channels from ActorHandle each frame
//! - Provides resources and messages for consumer systems
//!
//! All RPC goes through `ActorHandle` directly — consumer systems clone the
//! handle and spawn async tasks via `IoTaskPool`.

use std::sync::Mutex;

use bevy::prelude::*;
use bevy::winit::{EventLoopProxyWrapper, WinitUserEvent};
use kaijutsu_client::{
    ActorHandle, CallError, ContextMembership, Identity, KernelInfo, ServerEvent, SnapshotResult,
    SshConfig,
};
use kaijutsu_types::{ContextId, KernelId};
use tokio::sync::{broadcast, mpsc};

use super::bootstrap::{self, BootstrapChannel, BootstrapCommand};

/// This process's peer `instance` — minted once, stable for the window's life,
/// distinct from every other window's. Lets the kernel address THIS app among
/// several connected ones (and survives reconnect: same instance replaces its
/// own registry entry rather than spawning a duplicate).
fn app_peer_instance() -> &'static str {
    static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| format!("kaijutsu-app-{}", uuid::Uuid::new_v4()))
}

/// Subscribe to a context's change feed and hydrate a fresh `ContextMirror`
/// over it — docs/change-feed.md rules 21-26. Subscribe FIRST (so nothing
/// published between the subscribe and the fetch is lost), then fetch the
/// snapshot with `getBlocks`, then apply it.
///
/// No `select!` race against the receiver is needed here: `ContextMirror`
/// itself discards a delivery the fetch's own snapshot already covers
/// (`ContextMirror::receive`'s doc comment), so a caller may fetch first and
/// drain the receiver afterward without a spurious error.
///
/// Used both for a context's first join (`poll_bootstrap_results`) and for
/// the `FeedEvent::Terminated` recovery, which needs a brand-new receiver
/// exactly the way a first join does (`view::sync::drain_context_feeds`).
pub(crate) async fn hydrate_context(
    handle: &ActorHandle,
    context_id: ContextId,
) -> Result<
    (
        kaijutsu_client::ContextMirror,
        mpsc::Receiver<kaijutsu_client::FeedEvent>,
    ),
    String,
> {
    let rx = handle
        .subscribe_context(context_id)
        .await
        .map_err(|e| format!("subscribe_context: {e}"))?;
    let mut mirror = kaijutsu_client::ContextMirror::new(context_id);
    let (blocks, version) = handle
        .get_blocks_versioned(context_id, kaijutsu_types::BlockQuery::All)
        .await
        .map_err(|e| format!("get_blocks_versioned: {e}"))?;
    mirror
        .apply_snapshot(blocks, version)
        .map_err(|e| format!("apply_snapshot: {e}"))?;
    Ok((mirror, rx))
}

// ============================================================================
// Resources
// ============================================================================

/// The live RPC actor handle. Inserted when bootstrap reports ActorReady.
///
/// Consumer systems use `actor.handle.clone()` + `IoTaskPool::get().spawn()`
/// for async RPC calls.
#[derive(Resource)]
#[allow(dead_code)]
pub struct RpcActor {
    pub handle: ActorHandle,
    pub generation: u64,
}

/// Reactive connection state — updated by poll systems, read by UI.
#[derive(Resource, Default)]
pub struct RpcConnectionState {
    pub connected: bool,
    pub identity: Option<Identity>,
    pub current_kernel: Option<KernelInfo>,
    /// SSH config (for display and respawn)
    pub ssh_config: SshConfig,
    /// Reconnect attempt counter (0 = connected or idle)
    pub reconnect_attempt: u32,
    /// Server-authoritative kernel ID (set on connect)
    pub kernel_id: Option<KernelId>,
    /// Context ID from server's join_context (server-authoritative)
    pub context_id: Option<ContextId>,
    /// Last error message from the actor (cleared on successful connect).
    /// Survives across Reconnecting events so the dock can surface the
    /// underlying cause (e.g. SSH agent missing) instead of just spinning.
    pub last_error: Option<String>,
}

/// Channel for async tasks to send results back to Bevy systems.
///
/// `rx` is `Mutex<UnboundedReceiver>` because tokio's receiver is Send but
/// !Sync. The Mutex makes it Sync with zero real contention.
#[derive(Resource)]
pub struct RpcResultChannel {
    pub tx: mpsc::UnboundedSender<RpcResultMessage>,
    rx: Mutex<mpsc::UnboundedReceiver<RpcResultMessage>>,
}

impl RpcResultChannel {
    fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx: Mutex::new(rx),
        }
    }

    /// Convenience: clone the sender for passing to async tasks.
    pub fn sender(&self) -> mpsc::UnboundedSender<RpcResultMessage> {
        self.tx.clone()
    }
}

/// The result of a background change-feed subscribe/hydrate/re-hydrate,
/// docs/change-feed.md rules 21-28.
///
/// This does NOT ride [`RpcResultMessage`]: `Joined` carries a live
/// `mpsc::Receiver<FeedEvent>`, and `MessageReader` hands every consumer a
/// shared reference into Bevy's double-buffered message storage — a
/// receiver can't be moved out of a `&T`. [`ContextHydrationChannel`] is a
/// dedicated, drain-once channel instead (the same shape as
/// `peers::PeerInvocationChannel`), so ownership genuinely transfers to
/// whichever system drains it.
pub enum ContextHydration {
    /// A context's first join, or the `FeedEvent::Terminated` recovery: a
    /// brand-new mirror AND receiver, installed wholesale
    /// (`DocumentStore::install`).
    Joined {
        context_id: ContextId,
        mirror: kaijutsu_client::ContextMirror,
        feed: mpsc::Receiver<kaijutsu_client::FeedEvent>,
    },
    /// The `FeedEvent::Resubscribed`/`Desynced` recovery: a fresh snapshot
    /// for a mirror whose existing receiver is still good
    /// (`DocumentStore::apply_snapshot`).
    Snapshot {
        context_id: ContextId,
        blocks: Vec<kaijutsu_types::BlockSnapshot>,
        version: u64,
    },
    /// The initial join's subscribe+hydrate failed. The join still lands —
    /// against an empty, unfed mirror — rather than blocking on it forever.
    JoinFailed { context_id: ContextId },
}

/// Drain-once channel for [`ContextHydration`] results — see its doc comment
/// for why this can't be `RpcResultMessage`. `rx` uses `std::sync::mpsc`
/// (not tokio) because nothing here needs an async receiver: it is drained
/// synchronously, once a frame, exactly like `peers::PeerInvocationChannel`.
#[derive(Resource)]
pub struct ContextHydrationChannel {
    tx: std::sync::mpsc::Sender<ContextHydration>,
    rx: Mutex<std::sync::mpsc::Receiver<ContextHydration>>,
}

impl ContextHydrationChannel {
    fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            tx,
            rx: Mutex::new(rx),
        }
    }

    /// Convenience: clone the sender for passing to async tasks.
    pub fn sender(&self) -> std::sync::mpsc::Sender<ContextHydration> {
        self.tx.clone()
    }

    /// Drain whatever is queued right now. Locked internally so callers
    /// don't need `ResMut` just to read a channel.
    pub fn drain(&self) -> Vec<ContextHydration> {
        let Ok(rx) = self.rx.lock() else {
            return Vec::new();
        };
        rx.try_iter().collect()
    }
}

// ============================================================================
// Bevy Messages (written by poll systems, read by consumer systems)
// ============================================================================

/// Server-push events (block changes, resource updates).
#[derive(Message, Clone, Debug)]
pub struct ServerEventMessage(pub kaijutsu_client::ServerEvent);

/// Connection lifecycle events.
#[derive(Message, Clone, Debug)]
pub struct ConnectionStatusMessage(pub kaijutsu_client::ConnectionStatus);

/// Results from state-changing async operations.
///
/// Sent via `RpcResultChannel` from async tasks, polled and written as
/// Bevy messages by `poll_rpc_results`.
///
/// Not `Clone`: `ContextJoined`/`ContextRehydrated` carry a live
/// `mpsc::Receiver<FeedEvent>` (the change feed, docs/change-feed.md), and a
/// receiver cannot be duplicated.
#[derive(Message, Debug)]
#[allow(dead_code)]
pub enum RpcResultMessage {
    /// Kernel info received after attach/reconnect.
    KernelAttached(Result<KernelInfo, String>),
    /// Identity received.
    IdentityReceived(Identity),
    /// Context joined — membership info only. The change-feed hydration
    /// (docs/change-feed.md rules 21-26: subscribe, then `getBlocks`, then
    /// apply) travels separately on `ContextHydrationChannel` — a live
    /// `mpsc::Receiver<FeedEvent>` can't ride this message (see that
    /// channel's doc comment) — and is drained by
    /// `view::sync::drain_context_hydrations`.
    ContextJoined {
        membership: ContextMembership,
    },
    /// Context left.
    ContextLeft,
    /// Cherry-pick completed.
    CherryPicked {
        success: bool,
        new_block_id: Option<kaijutsu_crdt::BlockId>,
        error: Option<String>,
    },
    /// Drift contexts list received (from periodic polling).
    DriftContextsReceived {
        contexts: Vec<kaijutsu_client::ContextInfo>,
    },
    /// Drift staged queue received (from periodic polling).
    DriftQueueReceived {
        staged: Vec<kaijutsu_client::StagedDriftInfo>,
    },
    /// Attached-peer roster received (`connection::peers::poll_peer_roster`,
    /// the same periodic-poll pattern as the two arms above). Drained into
    /// `PeerRoster`, consumed by `view::room::seats` to reconcile each
    /// attached peer's wisp entity.
    PeersReceived {
        peers: Vec<kaijutsu_client::PeerInfo>,
    },
    /// Semantic clusters received (time-well band-2 poll). Drained into
    /// `TimeWellState.clusters` to drive the haystack's cluster-grouped angle.
    ClustersReceived {
        clusters: Vec<kaijutsu_client::ContextCluster>,
    },
    /// Track list received (time-well track-ray poll, `listTracks`). Drained
    /// into `WellTracks` to drive the rays + per-card track hue/beat lanes.
    TracksReceived {
        tracks: Vec<kaijutsu_client::TrackInfo>,
    },
    /// A `vfs_snapshot` reply landed (`view::fsn::sync`'s poll — the FSN
    /// world's enumeration-on-demand scheduler, `docs/scenes/vfs.md` claim
    /// 3). `path` is the query's own path (not necessarily the node's own
    /// path if the kernel normalizes it), so the drain site can match the
    /// reply back to whichever cell requested it.
    VfsSnapshotReceived {
        path: String,
        result: SnapshotResult,
    },
    /// A `vfs_snapshot` request failed (RPC error, disconnect). Drained by
    /// the same system as the success variant, whose only job on this arm is
    /// clearing the one-in-flight debounce slot — without a failure reply the
    /// FSN fetch queue would wedge forever on the first failed request.
    /// Deliberately carries no auto-requeue semantics (a permanently-failing
    /// path would hot-loop) — see `view::fsn::sync::apply_fsn_snapshot`.
    VfsSnapshotFailed { path: String },
    /// Context created on server — spawn an actor to join it.
    ContextCreated(ContextId),
    /// Restore the last-viewed context on (re)connect, read from the kernel KV
    /// (`<client-id>.current_context`). Drained into a `ContextSwitchRequested`
    /// so it travels the same join path as any switch. Closes the reattach bug
    /// (tech_debt_peer_reattach_on_reconnect).
    RestoreContext(ContextId),
    /// CRDT-owned `theme.toml` content, fetched over RPC on connect (the app no
    /// longer reads a host theme file — slice 2). Parsed and applied to the
    /// `Theme` resource by `apply_theme_from_rpc`.
    ThemeReceived(String),
    /// Generic RPC error (for toast/notification).
    RpcError { operation: String, error: String },
    /// A followed context's change feed ended and does not resume
    /// (`FeedEvent::Terminated`) — re-subscribed from scratch and
    /// re-hydrated (`view::sync::drain_context_feeds`). Installed wholesale
    /// via `DocumentStore::install`, replacing both the dead receiver and
    /// the mirror it fed.
    ContextRehydrated {
        context_id: ContextId,
        mirror: kaijutsu_client::ContextMirror,
        feed: mpsc::Receiver<kaijutsu_client::FeedEvent>,
    },
    /// A followed context's mirror was re-fetched from a fresh snapshot
    /// while KEEPING its existing feed receiver — the
    /// `FeedEvent::Resubscribed`/`Desynced` recovery
    /// (`view::sync::drain_context_feeds`). Installed via
    /// `DocumentStore::apply_snapshot`.
    ContextSnapshotReady {
        context_id: ContextId,
        blocks: Vec<kaijutsu_types::BlockSnapshot>,
        version: u64,
    },
    /// An open editor's kernel session is gone: a keystroke to `editor_keys`
    /// came back `no such session`. The session is in-memory kernel state and
    /// does not survive a kernel restart (the persisted `kernel_id` is unchanged,
    /// so a restart is invisible at the connection layer). Drained by
    /// `view::editor` to drop the stale session and pop back to the conversation.
    EditorSessionLost { session: u64 },
    /// An in-flight `editor_keys` batch resolved: `ok` on a normal return,
    /// `!ok` on a *transient* failure (a session-lost failure sends
    /// [`EditorSessionLost`](Self::EditorSessionLost) instead). Drained by
    /// `view::editor` to advance its ordered keystroke pipe — ship the next
    /// batch, or retry/drop the failed one.
    EditorKeysOutcome { session: u64, ok: bool },
    /// The per-client metronome config (`/etc/client/<id>/metronome.toml`,
    /// cascading to the shared `/etc/client/metronome.toml`), fetched over RPC on
    /// (re)connect. Drained by [`crate::metronome::apply_metronome_config`] into
    /// the `Metronome` resource. Carries the resolved TOML body.
    MetronomeConfigReceived(String),
    /// The per-client mouse-wheel scroll config (`/etc/client/<id>/scroll.toml`,
    /// cascading to the shared `/etc/client/scroll.toml`), fetched over RPC on
    /// (re)connect. Drained by
    /// [`crate::input::scroll_config::apply_scroll_config`] into the
    /// `ScrollConfig` resource. Carries the resolved TOML body.
    ScrollConfigReceived(String),
}

// ============================================================================
// Plugin
// ============================================================================

/// Replaces `ConnectionBridgePlugin` with ActorHandle-based architecture.
pub struct ActorPlugin {
    pub ssh_config: SshConfig,
}

impl Plugin for ActorPlugin {
    fn build(&self, app: &mut App) {
        // Spawn the bootstrap thread
        let bootstrap_channel = bootstrap::spawn_bootstrap_thread();

        let ssh_config = self.ssh_config.clone();

        // Initial connection — no context joined. The context strip populates
        // from list_contexts(); user picks or creates a context explicitly.
        // kernel_id is None: server is authoritative and reveals it during
        // bind_kernel (see actor.rs `try_connect_inner`).
        let _ = bootstrap_channel.tx.send(BootstrapCommand::SpawnActor {
            config: ssh_config.clone(),
            kernel_id: None,
            context_id: None,
            instance: app_peer_instance().to_string(),
        });

        // Register resources
        app.insert_resource(bootstrap_channel)
            .insert_resource(RpcResultChannel::new())
            .insert_resource(ContextHydrationChannel::new())
            .insert_resource(RpcConnectionState {
                ssh_config,
                ..Default::default()
            });

        // Register messages
        app.add_message::<ServerEventMessage>()
            .add_message::<ConnectionStatusMessage>()
            .add_message::<RpcResultMessage>();

        // Register systems
        app.add_systems(
            Update,
            (
                poll_bootstrap_results,
                poll_server_events,
                poll_connection_status,
                poll_rpc_results,
                update_connection_state,
                refetch_config_on_reconnect,
                restore_context_on_message,
                apply_theme_from_rpc,
            )
                .chain(),
        );
        // The current-context persistence observer runs independently — it only
        // needs to see the latest `DocumentCache::active_id`.
        app.add_systems(Update, persist_current_context);
    }
}

// `bump_sync_generation_on_reconnect` (the old CRDT-era generation-bump
// staleness sweep) is gone: the block document no longer has a coarse
// broadcast-level staleness signal to react to. Each followed context now
// gets its OWN precise recovery signal straight from its change feed
// (`FeedEvent::Resubscribed`/`Terminated`, docs/change-feed.md rules 21-28),
// drained by `view::sync::drain_context_feeds` — replacing both this system
// and `view::sync::check_cache_staleness`.

/// Re-fetch the theme/metronome/scroll config trio whenever the actor
/// reports a reconnect. Uses the SAME `ServerEvent::Reconnected` trigger the
/// now-deleted `bump_sync_generation_on_reconnect` used to (see the comment
/// just above) — the actor's one canonical "we came back from an outage"
/// signal — rather than adding a second reconnect-detection mechanism
/// alongside it.
///
/// Before this system existed, [`fetch_startup_configs`] only ran once, from
/// `poll_bootstrap_results`'s `ActorReady` arm. That is correct for cold
/// start but leaves the app silently running whatever theme/metronome/scroll
/// config it booted with even after a kernel bounce onto a fresh or edited
/// CRDT config (`kj rc reset`, a config wipe, a different kernel instance on
/// the same port during dev) — reconnected at the transport layer but not
/// re-initialized at the domain layer, the exact bug this task exists to
/// close. `ServerEvent::Reconnected` is never emitted for the first connect
/// (see `RpcActor::enter_connected`), so cold start and reconnect never
/// double-fetch.
fn refetch_config_on_reconnect(
    mut server_events: MessageReader<ServerEventMessage>,
    actor: Option<Res<RpcActor>>,
    result_channel: Res<RpcResultChannel>,
    client_id: Res<crate::connection::client_id::ClientId>,
) {
    let Some(actor) = actor else { return };
    let reconnected = server_events
        .read()
        .any(|ServerEventMessage(event)| matches!(event, ServerEvent::Reconnected));
    if !reconnected {
        return;
    }
    log::info!("reconnect signalled — refetching theme/metronome/scroll config");
    let h = actor.handle.clone();
    let tx = result_channel.sender();
    let client_id = client_id.0.to_string();
    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            // `visible_on_failure = true`: unlike cold start, a failure here
            // means we just had a live config and lost track of whether it's
            // still current — that must be visible, not a silent fallback to
            // stale state (project directive).
            fetch_startup_configs(h, client_id, tx, true).await;
        })
        .detach();
}

/// Fetch the CRDT-owned per-client config trio (theme, metronome, scroll)
/// and forward each into its existing `RpcResultMessage` sink
/// (`apply_theme_from_rpc` below; `dj::thread`'s `MetronomeConfigReceived`
/// handler; `input::scroll_config::apply_scroll_config`).
///
/// The ONE fetch path for this trio — called from both the initial
/// `ActorReady` bootstrap (cold start, `visible_on_failure = false`) and
/// [`refetch_config_on_reconnect`] (every actor-internal reconnect,
/// `visible_on_failure = true`). See that fn's doc comment for why a
/// reconnect must re-run this rather than trusting whatever was fetched at
/// boot. Thin wrapper over [`fetch_startup_configs_with`] that supplies the
/// live `ActorHandle::get_config` as the fetch — see that fn for why the
/// split exists.
async fn fetch_startup_configs(
    h: ActorHandle,
    client_id: String,
    tx: mpsc::UnboundedSender<RpcResultMessage>,
    visible_on_failure: bool,
) {
    let fetch = |path: String| {
        let h = h.clone();
        async move { h.get_config(path).await }
    };
    fetch_startup_configs_with(&fetch, &client_id, &tx, visible_on_failure).await;
}

/// Same fetch/fallback/visibility logic as [`fetch_startup_configs`], with
/// the RPC call itself injected as `fetch` rather than going through a live
/// `ActorHandle`. `ActorHandle` can't be constructed as a test double from
/// outside `kaijutsu-client` (its fields are private to that crate, and
/// building a real one means a real capnp connection) — injecting the fetch
/// dependency here does for this config-refresh logic what
/// `backoff_for_attempt_jittered` does for the RNG in `actor.rs`: tests drive
/// the two-layer-fallback / failure-visibility branches with a canned async
/// closure instead of asserting against live I/O.
async fn fetch_startup_configs_with<F, Fut>(
    fetch: &F,
    client_id: &str,
    tx: &mpsc::UnboundedSender<RpcResultMessage>,
    visible_on_failure: bool,
) where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<String, CallError>>,
{
    match fetch("theme.toml".to_string()).await {
        Ok(toml) => {
            let _ = tx.send(RpcResultMessage::ThemeReceived(toml));
        }
        Err(e) => {
            log::warn!("theme fetch over RPC failed: {e}; keeping current theme");
            if visible_on_failure {
                let _ = tx.send(RpcResultMessage::RpcError {
                    operation: "theme.toml refetch after reconnect".into(),
                    error: e.to_string(),
                });
            }
        }
    }

    fetch_layered_config(
        fetch,
        client_id,
        "metronome.toml",
        tx,
        visible_on_failure,
        RpcResultMessage::MetronomeConfigReceived,
    )
    .await;

    fetch_layered_config(
        fetch,
        client_id,
        "scroll.toml",
        tx,
        visible_on_failure,
        RpcResultMessage::ScrollConfigReceived,
    )
    .await;
}

/// Shared two-layer (`/etc/client/<id>/<name>` then `/etc/client/<name>`)
/// config fetch used by both the metronome and scroll-gain legs of
/// [`fetch_startup_configs_with`]. Only surfaces a failure (log + optional
/// toast) when at least one layer actually errored — both layers coming back
/// empty is the expected, silent common case (a bare kernel with no
/// per-client overrides at all), not a failure.
async fn fetch_layered_config<F, Fut>(
    fetch: &F,
    client_id: &str,
    name: &str,
    tx: &mpsc::UnboundedSender<RpcResultMessage>,
    visible_on_failure: bool,
    wrap: impl Fn(String) -> RpcResultMessage,
) where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<String, CallError>>,
{
    let mut last_err: Option<String> = None;
    for path in [
        kaijutsu_types::paths::client_config_path(Some(client_id), name),
        kaijutsu_types::paths::client_config_path(None, name),
    ] {
        match fetch(path.clone()).await {
            Ok(toml) if !toml.trim().is_empty() => {
                let _ = tx.send(wrap(toml));
                return;
            }
            // Empty body: try the next (shared) layer.
            Ok(_) => {}
            // Absent override (common) / read error: fall through.
            Err(e) => {
                log::debug!("{name} config {path} unavailable: {e}");
                last_err = Some(e.to_string());
            }
        }
    }
    if let Some(e) = last_err {
        log::warn!("{name} config unavailable on both layers: {e}");
        if visible_on_failure {
            let _ = tx.send(RpcResultMessage::RpcError {
                operation: format!("{name} refetch after reconnect"),
                error: e,
            });
        }
    }
}

#[cfg(test)]
mod reconnect_config_refetch_tests {
    use super::*;

    /// Drain every message currently buffered on an unbounded receiver
    /// without blocking — the tests below await the fetch future to
    /// completion first, so everything it sent is already queued.
    fn drain(rx: &mut mpsc::UnboundedReceiver<RpcResultMessage>) -> Vec<RpcResultMessage> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            out.push(msg);
        }
        out
    }

    #[tokio::test]
    async fn theme_fetch_success_sends_theme_received() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        // `fetch_startup_configs_with` calls this for theme, then metronome,
        // then scroll — only theme.toml gets real content here, so the
        // others fall through both layers to an empty/silent no-op.
        let fetch = |path: String| async move {
            if path == "theme.toml" {
                Ok("scheme = \"dark\"".to_string())
            } else {
                Ok(String::new())
            }
        };
        fetch_startup_configs_with(&fetch, "client-1", &tx, false).await;
        let msgs = drain(&mut rx);
        assert!(
            msgs.iter().any(
                |m| matches!(m, RpcResultMessage::ThemeReceived(t) if t == "scheme = \"dark\"")
            ),
            "expected a ThemeReceived among: {msgs:?}"
        );
    }

    #[tokio::test]
    async fn theme_fetch_failure_is_silent_when_not_visible() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let fetch = |_: String| async { Err(CallError::Shutdown) };
        fetch_startup_configs_with(&fetch, "client-1", &tx, false).await;
        let msgs = drain(&mut rx);
        assert!(
            !msgs.iter().any(|m| matches!(m, RpcResultMessage::RpcError { .. })),
            "cold start must not toast a theme-fetch failure: {msgs:?}"
        );
    }

    /// The project directive under test: a post-reconnect re-init failure
    /// must be visible, not a silent fallback. `visible_on_failure = true`
    /// (what `refetch_config_on_reconnect` always passes) must turn a fetch
    /// error into an app-visible `RpcError`, on top of the log line that
    /// always fires.
    #[tokio::test]
    async fn theme_fetch_failure_is_visible_on_reconnect() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let fetch = |_: String| async { Err(CallError::Shutdown) };
        fetch_startup_configs_with(&fetch, "client-1", &tx, true).await;
        let msgs = drain(&mut rx);
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                RpcResultMessage::RpcError { operation, .. }
                    if operation.contains("theme.toml")
            )),
            "reconnect must surface a theme-fetch failure, got: {msgs:?}"
        );
    }

    #[tokio::test]
    async fn layered_config_prefers_the_per_client_override() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let fetch = |path: String| async move {
            if path.contains("client-1") {
                Ok("line_gain = 9.0".to_string())
            } else {
                Ok("line_gain = 1.0".to_string())
            }
        };
        fetch_layered_config(
            &fetch,
            "client-1",
            "scroll.toml",
            &tx,
            true,
            RpcResultMessage::ScrollConfigReceived,
        )
        .await;
        let msgs = drain(&mut rx);
        assert_eq!(msgs.len(), 1, "must stop at the first non-empty layer: {msgs:?}");
        assert!(matches!(
            &msgs[0],
            RpcResultMessage::ScrollConfigReceived(t) if t == "line_gain = 9.0"
        ));
    }

    #[tokio::test]
    async fn layered_config_falls_back_to_the_shared_default_when_the_override_is_empty() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let fetch = |path: String| async move {
            if path.contains("client-1") {
                Ok(String::new()) // per-client override absent
            } else {
                Ok("line_gain = 1.0".to_string())
            }
        };
        fetch_layered_config(
            &fetch,
            "client-1",
            "scroll.toml",
            &tx,
            true,
            RpcResultMessage::ScrollConfigReceived,
        )
        .await;
        let msgs = drain(&mut rx);
        assert!(matches!(
            &msgs[0],
            RpcResultMessage::ScrollConfigReceived(t) if t == "line_gain = 1.0"
        ));
    }

    /// Both layers empty (no override anywhere, bare kernel) is the normal
    /// case — never a failure toast even when `visible_on_failure` is set.
    #[tokio::test]
    async fn layered_config_both_layers_empty_is_silent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let fetch = |_: String| async { Ok(String::new()) };
        fetch_layered_config(
            &fetch,
            "client-1",
            "scroll.toml",
            &tx,
            true,
            RpcResultMessage::ScrollConfigReceived,
        )
        .await;
        let msgs = drain(&mut rx);
        assert!(msgs.is_empty(), "both-empty must not toast: {msgs:?}");
    }

    /// Both layers erroring (not just empty) IS a failure — and per the
    /// project directive it must be visible on reconnect.
    #[tokio::test]
    async fn layered_config_both_layers_erroring_is_visible_on_reconnect() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let fetch = |_: String| async { Err(CallError::Shutdown) };
        fetch_layered_config(
            &fetch,
            "client-1",
            "scroll.toml",
            &tx,
            true,
            RpcResultMessage::ScrollConfigReceived,
        )
        .await;
        let msgs = drain(&mut rx);
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                RpcResultMessage::RpcError { operation, .. } if operation.contains("scroll.toml")
            )),
            "both layers erroring must surface on reconnect, got: {msgs:?}"
        );
    }

    /// Same both-erroring case at the cold-start callsite (`visible_on_failure
    /// = false`): must stay silent, matching the original bootstrap behavior
    /// this refactor must not regress.
    #[tokio::test]
    async fn layered_config_both_layers_erroring_is_silent_at_cold_start() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let fetch = |_: String| async { Err(CallError::Shutdown) };
        fetch_layered_config(
            &fetch,
            "client-1",
            "scroll.toml",
            &tx,
            false,
            RpcResultMessage::ScrollConfigReceived,
        )
        .await;
        let msgs = drain(&mut rx);
        assert!(msgs.is_empty(), "cold start must not toast: {msgs:?}");
    }

    /// The full trio in one call, exercising `fetch_startup_configs_with`
    /// end to end with a single fetch closure that answers all three names.
    #[tokio::test]
    async fn fetch_startup_configs_with_delivers_all_three_on_success() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let fetch = |path: String| async move {
            if path == "theme.toml" {
                Ok("scheme = \"dark\"".to_string())
            } else if path.contains("metronome") {
                Ok("enabled = true".to_string())
            } else if path.contains("scroll") {
                Ok("line_gain = 2.0".to_string())
            } else {
                Ok(String::new())
            }
        };
        fetch_startup_configs_with(&fetch, "client-1", &tx, false).await;
        let msgs = drain(&mut rx);
        assert!(msgs.iter().any(|m| matches!(m, RpcResultMessage::ThemeReceived(_))));
        assert!(msgs.iter().any(|m| matches!(m, RpcResultMessage::MetronomeConfigReceived(_))));
        assert!(msgs.iter().any(|m| matches!(m, RpcResultMessage::ScrollConfigReceived(_))));
    }
}

/// Apply a theme fetched over RPC. Slice 2: the app no longer reads a host
/// `theme.toml` — the kernel is the sole owner, so theme arrives as a
/// [`RpcResultMessage::ThemeReceived`] on connect and replaces BOTH color
/// resources: `Theme` (the UI lane) and `ScenePalette` (the 3D scene lane's
/// `[scene]` table — docs/color.md). Theme-reading systems pick the new values
/// up next frame; `[scene.post]` hot-applies to the camera via
/// `apply_scene_post_on_change`. A parse failure is surfaced as a toast and
/// leaves the current theme intact — never a silent revert to default.
fn apply_theme_from_rpc(
    mut results: MessageReader<RpcResultMessage>,
    mut theme: ResMut<crate::ui::theme::Theme>,
    mut scene_palette: ResMut<crate::view::scene_palette::ScenePalette>,
    mut error_queue: ResMut<crate::view::components::GlobalErrorQueue>,
    time: Res<Time>,
) {
    for result in results.read() {
        if let RpcResultMessage::ThemeReceived(toml) = result {
            match crate::ui::theme_loader::parse_theme_data(toml) {
                Ok(data) => {
                    *scene_palette =
                        crate::view::scene_palette::ScenePalette::from_scene_data(&data.scene);
                    *theme = crate::ui::theme::Theme::from(data);
                    log::info!("applied theme from kernel config (RPC): UI + scene palette");
                }
                Err(e) => {
                    log::error!("theme.toml from kernel is unparseable: {e}");
                    error_queue.push(
                        "config",
                        format!("theme.toml: {e}"),
                        time.elapsed_secs_f64(),
                    );
                }
            }
        }
    }
}

/// Drain [`RpcResultMessage::RestoreContext`] into a [`ContextSwitchRequested`]
/// so a reconnect rejoins the last-viewed context through the normal switch
/// path (which spawns the actor and fetches state).
fn restore_context_on_message(
    mut results: bevy::prelude::MessageReader<RpcResultMessage>,
    mut switch_writer: bevy::prelude::MessageWriter<crate::cell::ContextSwitchRequested>,
) {
    for msg in results.read() {
        if let RpcResultMessage::RestoreContext(context_id) = msg {
            switch_writer.write(crate::cell::ContextSwitchRequested {
                context_id: *context_id,
            });
        }
    }
}

/// Persist the active context to the kernel's per-client view row whenever it
/// changes, so the next (re)connect can restore it. A single observer over
/// `DocumentCache::active_id` captures every switch source — app UI, MCP-peer
/// `switch_context`, and the restore itself (a harmless re-write of the same
/// value). Fire-and-forget: a failed write is logged, never fatal (per-client
/// view state is a convenience).
fn persist_current_context(
    doc_cache: Res<crate::cell::DocumentCache>,
    actor: Option<Res<RpcActor>>,
    client_id: Res<crate::connection::client_id::ClientId>,
    mut last_written: Local<Option<ContextId>>,
) {
    let active = doc_cache.active_id();
    if active == *last_written {
        return;
    }
    // Only advance the high-water mark once we've actually dispatched a write
    // for a concrete id — a transient `None` (e.g. context left) shouldn't make
    // us forget the last persisted value.
    let (Some(actor), Some(id)) = (actor, active) else {
        return;
    };
    *last_written = Some(id);
    let handle = actor.handle.clone();
    let client_id = client_id.0.to_string();
    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            if let Err(e) = handle.set_last_context(&client_id, id).await {
                log::warn!("persist current_context failed: {e}");
            }
        })
        .detach();
}

// ============================================================================
// Poll Systems
// ============================================================================

/// Check for new actors from the bootstrap thread.
///
/// When a new actor arrives, replace the `RpcActor` resource. The change
/// detection on `RpcActor` triggers re-subscription in other poll systems.
fn poll_bootstrap_results(
    mut commands: Commands,
    channel: Res<BootstrapChannel>,
    result_channel: Res<RpcResultChannel>,
    hydration_channel: Res<ContextHydrationChannel>,
    invocation_channel: Res<crate::peers::PeerInvocationChannel>,
    event_loop_proxy: Res<EventLoopProxyWrapper>,
    client_id: Res<crate::connection::client_id::ClientId>,
) {
    let Ok(mut rx) = channel.rx.lock() else {
        return;
    };
    let mut received_any = false;
    while let Ok(result) = rx.try_recv() {
        received_any = true;
        match result {
            bootstrap::BootstrapResult::ActorReady {
                handle,
                generation,
                kernel_id,
                context_id,
            } => {
                log::info!(
                    "Actor ready (generation {}) kernel={:?} context={:?}",
                    generation,
                    kernel_id,
                    context_id
                );

                // The reconnect FSM rejects the first call to a fresh actor
                // with NotReady(Idle) and starts connecting in the background.
                // We kick it with a throwaway call, then wait for the FSM to
                // surface Connected (or Terminal) before issuing the real
                // bootstrap calls. The wait reads the *level* (watch_status),
                // not the one-shot transition broadcast: this task is spawned a
                // frame or more after the actor began its eager dial, so on a
                // fast local handshake the Connected edge can fire before we get
                // here — and `broadcast` never replays it, which used to hang
                // this loop forever (whoami/peer/theme/list_contexts never ran,
                // and the UI sat on "Disconnected" while drift-poll data flowed).
                let h = handle.clone();
                let tx = result_channel.sender();
                let hydration_tx = hydration_channel.sender();
                let inv_tx = invocation_channel.tx.clone();
                let ctx_id = context_id;
                let client_id = client_id.0.to_string();
                bevy::tasks::IoTaskPool::get()
                    .spawn(async move {
                        let mut status_rx = h.watch_status();
                        // Kick the FSM out of Idle. NotReady is expected.
                        let _ = h.whoami().await;

                        // Wait until the actor reaches Connected — or Terminal,
                        // in which case bootstrap is over. `wait_for` checks the
                        // current value before awaiting a change, so a Connected
                        // that already landed is observed immediately (no
                        // missed-edge hang).
                        match status_rx
                            .wait_for(|s| {
                                matches!(
                                    s,
                                    kaijutsu_client::ConnectionStatus::Connected { .. }
                                        | kaijutsu_client::ConnectionStatus::Terminal { .. }
                                )
                            })
                            .await
                        {
                            Ok(status) => {
                                if let kaijutsu_client::ConnectionStatus::Terminal { reason } =
                                    &*status
                                {
                                    log::warn!("Bootstrap aborted: actor terminal: {reason}");
                                    return;
                                }
                                // Drop the watch borrow before the awaits below.
                            }
                            Err(_) => {
                                log::warn!("Bootstrap aborted: status watch closed");
                                return;
                            }
                        }

                        // 0/0b/0c. Fetch the CRDT-owned theme + per-client
                        // metronome/scroll configs over RPC. Shared with
                        // `refetch_config_on_reconnect` — see that fn's doc
                        // comment for why cold start and reconnect must run
                        // the exact same fetch, not two copies of it.
                        fetch_startup_configs(h.clone(), client_id.clone(), tx.clone(), false)
                            .await;

                        // 1. whoami — now guaranteed not to be NotReady
                        let identity = match h.whoami().await {
                            Ok(id) => {
                                let _ = tx.send(RpcResultMessage::IdentityReceived(id.clone()));
                                Some(id)
                            }
                            Err(e) => {
                                log::warn!("Initial whoami failed: {e}");
                                return;
                            }
                        };

                        // 1b. Register as a peer so the kernel can invoke us.
                        // The invocation_tx sender goes into the capnp callback;
                        // invocations arrive directly in PeerInvocationChannel.
                        {
                            let h2 = h.clone();
                            let inv_tx2 = inv_tx;
                            bevy::tasks::IoTaskPool::get()
                                .spawn(async move {
                                    let config = kaijutsu_client::PeerConfig {
                                        nick: "kaijutsu-app".to_string(),
                                        // Stable for this process, fresh per window — so
                                        // two app windows coexist in the peer registry and
                                        // the kernel can address a specific one. Reused
                                        // across reconnects (same instance → replaces).
                                        instance: app_peer_instance().to_string(),
                                    };
                                    match h2.attach_peer(config, inv_tx2).await {
                                        Ok(info) => {
                                            log::info!(
                                                "App registered as peer: {}",
                                                info.nick
                                            );
                                        }
                                        Err(e) => {
                                            log::warn!("Failed to register as peer: {e}");
                                        }
                                    }
                                })
                                .detach();
                        }

                        // 1c. Subscribe to VFS activity digests (FSN slice 1
                        // ambient heat — view::fsn::heat ingests them off the
                        // shared event stream). interval 0 = server default
                        // (1000ms). Best-effort and decorative: a failure
                        // leaves the world cold, never blocks bootstrap. The
                        // actor remembers the subscription and best-effort
                        // re-issues it on every reconnect.
                        {
                            let h2 = h.clone();
                            bevy::tasks::IoTaskPool::get()
                                .spawn(async move {
                                    if let Err(e) = h2.subscribe_vfs_activity(0).await {
                                        log::warn!(
                                            "VFS activity subscribe failed (heat stays cold): {e}"
                                        );
                                    }
                                })
                                .detach();
                        }

                        // 2. If we joined a specific context, fetch its state.
                        // Invariant: SpawnActor with context_id=Some is only issued
                        // after the kernel is attached (see sync.rs / create_dialog.rs),
                        // so kernel_id must be Some here. Skip with a loud warning
                        // rather than letting a nil sentinel leak into membership.
                        if let Some(ctx_id) = ctx_id {
                            let Some(kernel_id) = kernel_id else {
                                log::warn!(
                                    "ContextJoined path reached without a known kernel_id for ctx={ctx_id}; skipping membership"
                                );
                                return;
                            };

                            // docs/change-feed.md rule 27: never `getContextSync`
                            // for a snapshot. Subscribe first, then `getBlocks`.
                            // Travels on `ContextHydrationChannel`, not
                            // `RpcResultMessage` — see that channel's doc
                            // comment for why.
                            match hydrate_context(&h, ctx_id).await {
                                Ok((mirror, feed)) => {
                                    let _ = hydration_tx.send(ContextHydration::Joined {
                                        context_id: ctx_id,
                                        mirror,
                                        feed,
                                    });
                                }
                                Err(e) => {
                                    log::warn!("Initial context hydrate failed: {e}");
                                    let _ = hydration_tx
                                        .send(ContextHydration::JoinFailed { context_id: ctx_id });
                                }
                            }

                            let nick = identity.map(|id| id.username).unwrap_or_default();
                            let membership = ContextMembership {
                                context_id: ctx_id,
                                kernel_id,
                                nick,
                                instance: app_peer_instance().to_string(),
                            };

                            let _ = tx.send(RpcResultMessage::ContextJoined { membership });
                            return;
                        }

                        // 3. No context specified — fetch the context list, then
                        //    restore the last-viewed context from the kernel's
                        //    per-client view row if it still exists (closes the
                        //    reattach bug). The read is best-effort: any hiccup
                        //    just falls through to the list and the normal
                        //    first-context selection.
                        let saved_ctx = h.get_client_view(&client_id).await.ok().flatten();
                        match h.list_contexts().await {
                            Ok(contexts) => {
                                log::info!(
                                    "Bootstrap: list_contexts returned {} contexts",
                                    contexts.len()
                                );
                                let restore = saved_ctx
                                    .filter(|id| contexts.iter().any(|c| c.id == *id));
                                let _ =
                                    tx.send(RpcResultMessage::DriftContextsReceived { contexts });
                                if let Some(id) = restore {
                                    log::info!("Restoring last-viewed context {id}");
                                    let _ = tx.send(RpcResultMessage::RestoreContext(id));
                                } else if let Some(id) = saved_ctx {
                                    log::info!(
                                        "Saved context {id} no longer exists; not restoring"
                                    );
                                }
                            }
                            Err(e) => {
                                log::warn!("Bootstrap: list_contexts failed: {e}");
                            }
                        }
                    })
                    .detach();

                commands.insert_resource(RpcActor { handle, generation });
            }
            bootstrap::BootstrapResult::Error(e) => {
                log::warn!("Bootstrap error: {}", e);
            }
        }
    }
    if received_any {
        let _ = event_loop_proxy.send_event(WinitUserEvent::WakeUp);
    }
}

/// Drain server events from ActorHandle's broadcast channel.
///
/// Uses `Local<Option<Receiver>>` to hold the subscription. Re-subscribes
/// when `RpcActor` changes (new actor after respawn/reconnect).
fn poll_server_events(
    actor: Option<Res<RpcActor>>,
    mut events: MessageWriter<ServerEventMessage>,
    mut receiver: Local<Option<broadcast::Receiver<kaijutsu_client::ServerEvent>>>,
    event_loop_proxy: Res<EventLoopProxyWrapper>,
) {
    let Some(actor) = actor else { return };

    // Re-subscribe when actor changes (new generation)
    if actor.is_changed() {
        log::debug!(
            "poll_server_events: subscribing to event broadcast (gen {})",
            actor.generation
        );
        *receiver = Some(actor.handle.subscribe_events());
    }

    let Some(rx) = receiver.as_mut() else { return };

    // Drain all available events
    let mut received_any = false;
    loop {
        match rx.try_recv() {
            Ok(event) => {
                received_any = true;
                events.write(ServerEventMessage(event));
            }
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                // The block document no longer listens on this broadcast at
                // all (docs/change-feed.md) — its own per-context feed has
                // its own gap detection (`FeedEvent::Terminated`). A lag
                // here only drops whatever OTHER stream events (turn
                // completion, VFS activity, …) were in flight.
                log::warn!("Server event broadcast lagged by {n} messages");
            }
            Err(broadcast::error::TryRecvError::Empty) => {
                break;
            }
            Err(broadcast::error::TryRecvError::Closed) => {
                *receiver = None;
                break;
            }
        }
    }

    // Wake the event loop so the next tick runs immediately (reactive mode).
    // Without this, incoming bursts (context join) stall for up to 100ms per batch.
    if received_any {
        let _ = event_loop_proxy.send_event(WinitUserEvent::WakeUp);
    }
}

/// Drain connection status events from ActorHandle's broadcast channel.
///
/// When the broadcast channel closes (actor exited), removes the `RpcActor`
/// resource so `periodic_reconnect` can spawn a fresh one.
fn poll_connection_status(
    mut commands: Commands,
    actor: Option<Res<RpcActor>>,
    mut events: MessageWriter<ConnectionStatusMessage>,
    mut receiver: Local<Option<broadcast::Receiver<kaijutsu_client::ConnectionStatus>>>,
    event_loop_proxy: Res<EventLoopProxyWrapper>,
) {
    let Some(actor) = actor else { return };

    let mut received_any = false;

    // Re-subscribe when actor changes, and seed the UI from the current
    // *level*. The broadcast below only delivers transitions that happen after
    // we subscribe, and the one-shot Connected may already have fired (the
    // RpcActor resource is inserted via a deferred command, so this poll
    // subscribes a frame late). Without the seed, a healthy-but-silent
    // Connected actor would leave the indicator stuck on its prior value.
    if actor.is_changed() {
        *receiver = Some(actor.handle.subscribe_status());
        events.write(ConnectionStatusMessage(actor.handle.current_status()));
        received_any = true;
    }

    let Some(rx) = receiver.as_mut() else {
        if received_any {
            let _ = event_loop_proxy.send_event(WinitUserEvent::WakeUp);
        }
        return;
    };

    loop {
        match rx.try_recv() {
            Ok(status) => {
                received_any = true;
                events.write(ConnectionStatusMessage(status));
            }
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                log::warn!("Connection status broadcast lagged by {n}");
            }
            Err(broadcast::error::TryRecvError::Empty) => {
                break;
            }
            Err(broadcast::error::TryRecvError::Closed) => {
                // Actor exited — remove resource so periodic_reconnect can spawn a new one
                log::debug!(
                    "Actor status channel closed, removing RpcActor resource (gen {})",
                    actor.generation
                );
                commands.remove_resource::<RpcActor>();
                *receiver = None;
                break;
            }
        }
    }

    if received_any {
        let _ = event_loop_proxy.send_event(WinitUserEvent::WakeUp);
    }
}

/// Drain results from async RPC tasks and write them as Bevy messages.
fn poll_rpc_results(
    channel: Res<RpcResultChannel>,
    mut events: MessageWriter<RpcResultMessage>,
    event_loop_proxy: Res<EventLoopProxyWrapper>,
) {
    let Ok(mut rx) = channel.rx.lock() else {
        return;
    };
    let mut received_any = false;
    while let Ok(result) = rx.try_recv() {
        received_any = true;
        events.write(result);
    }
    if received_any {
        let _ = event_loop_proxy.send_event(WinitUserEvent::WakeUp);
    }
}

/// Update `RpcConnectionState` from connection status and RPC result messages.
fn update_connection_state(
    mut state: ResMut<RpcConnectionState>,
    mut status_events: MessageReader<ConnectionStatusMessage>,
    mut result_events: MessageReader<RpcResultMessage>,
    mut error_queue: ResMut<crate::view::components::GlobalErrorQueue>,
    time: Res<Time>,
) {
    for ConnectionStatusMessage(status) in status_events.read() {
        match status {
            kaijutsu_client::ConnectionStatus::Idle => {
                state.connected = false;
                state.reconnect_attempt = 0;
                state.last_error = None;
            }
            kaijutsu_client::ConnectionStatus::Connected {
                kernel_id,
                context_id,
                since_ms: _,
            } => {
                state.connected = true;
                state.reconnect_attempt = 0;
                state.kernel_id = Some(*kernel_id);
                state.context_id = *context_id;
                state.last_error = None;
            }
            kaijutsu_client::ConnectionStatus::Connecting { attempt } => {
                state.connected = false;
                state.reconnect_attempt = *attempt;
                // Intentionally leave last_error in place — the cause from
                // the previous cycle is what drives this Connecting.
            }
            kaijutsu_client::ConnectionStatus::Closing { cause } => {
                state.connected = false;
                state.last_error = Some(cause.clone());
            }
            kaijutsu_client::ConnectionStatus::Cooldown {
                next_attempt,
                last_error,
                ..
            } => {
                state.connected = false;
                state.reconnect_attempt = *next_attempt;
                state.last_error = Some(last_error.clone());
            }
            kaijutsu_client::ConnectionStatus::Terminal { reason } => {
                state.connected = false;
                state.last_error = Some(reason.clone());
                state.identity = None;
                state.current_kernel = None;
            }
        }
    }

    for result in result_events.read() {
        match result {
            RpcResultMessage::KernelAttached(Ok(info)) => {
                state.current_kernel = Some(info.clone());
            }
            RpcResultMessage::IdentityReceived(identity) => {
                state.identity = Some(identity.clone());
                // If we got identity, the connection succeeded — mark connected.
                // This was the original workaround for the deferred-subscription
                // race (the one-shot ConnectionStatus::Connected fired before
                // poll_connection_status subscribed a frame late). That race is
                // now closed at the source: poll_connection_status seeds from
                // current_status() on (re)subscribe, so it sets `connected`
                // before this message arrives. Kept as a harmless belt-and-
                // suspenders backup — the `!state.connected` guard no-ops it
                // once the seed already worked.
                if !state.connected {
                    log::info!("Connection established (from IdentityReceived)");
                    state.connected = true;
                }
            }
            RpcResultMessage::RpcError { operation, error } => {
                log::warn!("RPC error ({operation}): {error}");
                error_queue.push(operation, error, time.elapsed_secs_f64());
            }
            _ => {}
        }
    }

    // GC old errors (auto-dismiss after 10s)
    error_queue.gc(time.elapsed_secs_f64(), 10.0);
}
