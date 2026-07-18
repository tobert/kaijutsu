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
    ActorHandle, ContextMembership, Identity, KernelInfo, ServerEvent, SnapshotResult, SshConfig,
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
#[derive(Message, Clone, Debug)]
#[allow(dead_code)]
pub enum RpcResultMessage {
    /// Kernel info received after attach/reconnect.
    KernelAttached(Result<KernelInfo, String>),
    /// Identity received.
    IdentityReceived(Identity),
    /// Context joined — includes membership info and initial sync state.
    ContextJoined {
        membership: ContextMembership,
        initial_sync: Option<kaijutsu_client::SyncState>,
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
    /// Input document state received (for initializing SyncedInput on context join).
    InputStateReceived {
        context_id: ContextId,
        state: kaijutsu_client::InputState,
    },
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
    /// A staleness re-fetch (`view::sync::check_cache_staleness`, fired by a
    /// `SyncGeneration` bump on reconnect or broadcast lag) pulled the full CRDT
    /// sync state for a context. Routed to `handle_block_events`, which merges it
    /// into the cached document — the idempotent re-sync that heals blocks lost
    /// during a transport outage. Distinct from `ContextJoined`'s `initial_sync`:
    /// this refreshes an already-cached doc without re-running the join bootstrap.
    ContextResynced {
        context_id: ContextId,
        sync: kaijutsu_client::SyncState,
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
            instance: "bevy-client".to_string(),
        });

        // Register resources
        app.insert_resource(bootstrap_channel)
            .insert_resource(RpcResultChannel::new())
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
                bump_sync_generation_on_reconnect,
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

/// Bump the document store's generation when the actor reports a reconnect, so
/// the active document re-syncs after a transport flake (laptop sleep, Wi-Fi
/// blip). The reconnect handshake re-subscribes the block *stream*, but blocks
/// the kernel published *during* the outage went to the dropped subscription and
/// are gone; without a re-fetch the view stays gap-stale until a manual context
/// switch respawns the actor. The bump drives
/// [`crate::view::sync::check_cache_staleness`] to re-fetch and merge the full
/// CRDT state (idempotent).
///
/// Detection lives in the actor ([`kaijutsu_client`]): it owns the reconnect FSM,
/// so it emits [`ServerEvent::Reconnected`] only on a real reconnect (never the
/// first connect, which hydrates via the `ActorReady` bootstrap). The app just
/// reacts — no re-derivation from the `ConnectionStatus` stream.
fn bump_sync_generation_on_reconnect(
    mut server_events: MessageReader<ServerEventMessage>,
    mut doc_cache: ResMut<crate::cell::DocumentCache>,
) {
    for ServerEventMessage(event) in server_events.read() {
        if matches!(event, ServerEvent::Reconnected) {
            let generation = doc_cache.bump_generation();
            log::info!(
                "reconnect signalled — bumped sync generation to {} for active-doc re-sync",
                generation
            );
        }
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

                        // 0. Fetch the CRDT-owned theme over RPC. The app no
                        // longer reads a host theme.toml (slice 2): the kernel
                        // is the sole owner. Best-effort — a failure keeps the
                        // default theme already in place. Done before the
                        // context branches below (both of which can `return`).
                        match h.get_config("theme.toml".to_string()).await {
                            Ok(toml) => {
                                let _ = tx.send(RpcResultMessage::ThemeReceived(toml));
                            }
                            Err(e) => {
                                log::warn!("theme fetch over RPC failed: {e}; keeping default theme")
                            }
                        }

                        // 0b. Fetch the per-client metronome config (per-client
                        // /etc/client/<id>/metronome.toml first, then the shared
                        // /etc/client/metronome.toml default). Best-effort — a
                        // miss keeps the compiled-in click already on the
                        // Metronome resource. Runs before the context branches
                        // (which can `return`) so it always fires.
                        for path in [
                            kaijutsu_types::paths::client_config_path(
                                Some(&client_id),
                                "metronome.toml",
                            ),
                            kaijutsu_types::paths::client_config_path(None, "metronome.toml"),
                        ] {
                            match h.get_config(path.clone()).await {
                                Ok(toml) if !toml.trim().is_empty() => {
                                    let _ = tx
                                        .send(RpcResultMessage::MetronomeConfigReceived(toml));
                                    break;
                                }
                                // Empty body: try the next (shared) layer.
                                Ok(_) => {}
                                // Absent override (common) / read error: fall through.
                                Err(e) => log::debug!("metronome config {path} unavailable: {e}"),
                            }
                        }

                        // 0c. Fetch the per-client scroll-gain config (per-client
                        // /etc/client/<id>/scroll.toml first, then the shared
                        // /etc/client/scroll.toml default). Best-effort — a miss
                        // keeps the compiled-in gains already on the ScrollConfig
                        // resource. Runs before the context branches (which can
                        // `return`) so it always fires.
                        for path in [
                            kaijutsu_types::paths::client_config_path(
                                Some(&client_id),
                                "scroll.toml",
                            ),
                            kaijutsu_types::paths::client_config_path(None, "scroll.toml"),
                        ] {
                            match h.get_config(path.clone()).await {
                                Ok(toml) if !toml.trim().is_empty() => {
                                    let _ = tx.send(RpcResultMessage::ScrollConfigReceived(toml));
                                    break;
                                }
                                // Empty body: try the next (shared) layer.
                                Ok(_) => {}
                                // Absent override (common) / read error: fall through.
                                Err(e) => log::debug!("scroll config {path} unavailable: {e}"),
                            }
                        }

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

                            let initial_sync = match h.get_context_sync(ctx_id).await {
                                Ok(state) => Some(state),
                                Err(e) => {
                                    log::warn!("Initial get_context_sync failed: {e}");
                                    None
                                }
                            };

                            let nick = identity.map(|id| id.username).unwrap_or_default();
                            let membership = ContextMembership {
                                context_id: ctx_id,
                                kernel_id,
                                nick,
                                instance: "bevy-client".to_string(),
                            };

                            let _ = tx.send(RpcResultMessage::ContextJoined {
                                membership,
                                initial_sync,
                            });
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
    mut doc_cache: ResMut<crate::cell::DocumentCache>,
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
                log::warn!("Server event broadcast lagged by {n} messages");
                doc_cache.bump_generation();
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
