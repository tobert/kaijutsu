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
use std::time::Instant;

use bevy::prelude::*;
use kaijutsu_client::{ActorHandle, ContextMembership, Identity, KernelInfo, SshConfig};
use tokio::sync::{broadcast, mpsc};

use super::bootstrap::{self, BootstrapChannel, BootstrapCommand};
use crate::constants::DEFAULT_KERNEL_ID;

/// How often to retry connection when disconnected (seconds).
const RECONNECT_INTERVAL_SECS: f64 = 5.0;

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

/// Monotonic generation — bumped on broadcast lag or reconnect.
#[derive(Resource, Default)]
pub struct SyncGeneration(pub u64);

/// Timer for periodic reconnect attempts when disconnected.
#[derive(Resource)]
struct ReconnectTimer {
    last_attempt: Instant,
}

impl Default for ReconnectTimer {
    fn default() -> Self {
        Self {
            last_attempt: Instant::now(),
        }
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
    /// Context joined — includes membership info and initial document state.
    ContextJoined {
        membership: ContextMembership,
        document_id: String,
        initial_state: Option<kaijutsu_client::DocumentState>,
    },
    /// Context left.
    ContextLeft,
    /// Fork completed.
    Forked {
        success: bool,
        context_name: Option<String>,
        document_id: Option<String>,
        error: Option<String>,
    },
    /// Cherry-pick completed.
    CherryPicked {
        success: bool,
        new_block_id: Option<kaijutsu_crdt::BlockId>,
        error: Option<String>,
    },
    /// Kernel list received (for dashboard). `generation` prevents stale results
    /// from a previous actor overwriting the current state.
    KernelList { kernels: Vec<KernelInfo>, generation: u64 },
    /// Context list received (for dashboard).
    ContextList { contexts: Vec<kaijutsu_client::ContextInfo>, generation: u64 },
    /// Context memberships received (for dashboard).
    MyContextsList { memberships: Vec<ContextMembership>, generation: u64 },
    /// Drift contexts list received (from periodic polling).
    DriftContextsReceived {
        contexts: Vec<kaijutsu_client::ContextInfo>,
    },
    /// Drift staged queue received (from periodic polling).
    DriftQueueReceived {
        staged: Vec<kaijutsu_client::StagedDriftInfo>,
    },
    /// Generic RPC error (for toast/notification).
    RpcError {
        operation: String,
        error: String,
    },
}

// ============================================================================
// Plugin
// ============================================================================

/// Replaces `ConnectionBridgePlugin` with ActorHandle-based architecture.
pub struct ActorPlugin;

impl Plugin for ActorPlugin {
    fn build(&self, app: &mut App) {
        // Spawn the bootstrap thread
        let bootstrap_channel = bootstrap::spawn_bootstrap_thread();

        // Send initial SpawnActor command
        let _ = bootstrap_channel.tx.send(BootstrapCommand::SpawnActor {
            config: SshConfig::default(),
            kernel_id: DEFAULT_KERNEL_ID.to_string(),
            context_id: None,
            instance: "bevy-client".to_string(),
        });

        // Register resources
        app.insert_resource(bootstrap_channel)
            .insert_resource(RpcResultChannel::new())
            .init_resource::<RpcConnectionState>()
            .init_resource::<SyncGeneration>()
            .init_resource::<ReconnectTimer>();

        // Register messages
        app.add_message::<ServerEventMessage>()
            .add_message::<ConnectionStatusMessage>()
            .add_message::<RpcResultMessage>();

        // Register systems
        app.add_systems(
            Update,
            (
                periodic_reconnect,
                poll_bootstrap_results,
                poll_server_events,
                poll_connection_status,
                poll_rpc_results,
                update_connection_state,
            )
                .chain(),
        );
    }
}

// ============================================================================
// Poll Systems
// ============================================================================

/// Periodically re-spawn the actor when disconnected.
///
/// Handles the case where the initial connection fails (e.g. no SSH keys in agent)
/// and nothing else triggers a retry. Every `RECONNECT_INTERVAL_SECS`, if we're
/// not connected, send a fresh `SpawnActor` command to the bootstrap thread.
fn periodic_reconnect(
    state: Res<RpcConnectionState>,
    mut timer: ResMut<ReconnectTimer>,
    bootstrap: Res<BootstrapChannel>,
    existing_actor: Option<Res<RpcActor>>,
) {
    if state.connected {
        return;
    }

    // Don't spawn a new actor if one already exists — it may still be in the
    // process of connecting (SSH handshake → attach_kernel → join_context →
    // subscriptions). Spawning a replacement would drop the existing handle,
    // killing the actor before it can report Connected.
    if existing_actor.is_some() {
        return;
    }

    let elapsed = timer.last_attempt.elapsed().as_secs_f64();
    if elapsed < RECONNECT_INTERVAL_SECS {
        return;
    }

    timer.last_attempt = Instant::now();
    log::debug!("Periodic reconnect: spawning fresh actor (disconnected for {elapsed:.1}s)");

    let _ = bootstrap.tx.send(BootstrapCommand::SpawnActor {
        config: state.ssh_config.clone(),
        kernel_id: DEFAULT_KERNEL_ID.to_string(),
        context_id: None,
        instance: "bevy-client".to_string(),
    });
}

/// Check for new actors from the bootstrap thread.
///
/// When a new actor arrives, replace the `RpcActor` resource. The change
/// detection on `RpcActor` triggers re-subscription in other poll systems.
fn poll_bootstrap_results(
    mut commands: Commands,
    channel: Res<BootstrapChannel>,
    result_channel: Res<RpcResultChannel>,
) {
    let Ok(mut rx) = channel.rx.lock() else { return };
    while let Ok(result) = rx.try_recv() {
        match result {
            bootstrap::BootstrapResult::ActorReady { handle, generation, kernel_id, context_id } => {
                log::info!("Actor ready (generation {}) kernel={} context={:?}", generation, kernel_id, context_id);

                // Eagerly connect: fire whoami to trigger ensure_connected
                // (SSH → attach_kernel → subscriptions → Connected).
                // If a context was specified, also fetch document state and
                // emit ContextJoined. Otherwise just get identity.
                let h = handle.clone();
                let tx = result_channel.sender();
                let kid = kernel_id.clone();
                let ctx_id = context_id;
                bevy::tasks::IoTaskPool::get()
                    .spawn(async move {
                        // 1. whoami triggers ensure_connected + gives us identity
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

                        // 2. If we joined a context, fetch its document state
                        let Some(ctx_id) = ctx_id else { return };

                        let document_id = format!("{}@{}", kid, ctx_id);
                        let initial_state = match h.get_document_state(&document_id).await {
                            Ok(state) => Some(state),
                            Err(e) => {
                                log::warn!("Initial get_document_state failed: {e}");
                                None
                            }
                        };

                        // 3. Construct ContextMembership from what we know
                        let nick = identity.map(|id| id.username).unwrap_or_default();
                        let membership = ContextMembership {
                            context_id: ctx_id,
                            kernel_id: kaijutsu_crdt::KernelId::parse(&kid)
                                .unwrap_or_else(|_| kaijutsu_crdt::KernelId::nil()),
                            nick,
                            instance: "bevy-client".to_string(),
                        };

                        let _ = tx.send(RpcResultMessage::ContextJoined {
                            membership,
                            document_id,
                            initial_state,
                        });
                    })
                    .detach();

                commands.insert_resource(RpcActor { handle, generation });
            }
            bootstrap::BootstrapResult::Error(e) => {
                log::warn!("Bootstrap error: {}", e);
            }
        }
    }
}

/// Drain server events from ActorHandle's broadcast channel.
///
/// Uses `Local<Option<Receiver>>` to hold the subscription. Re-subscribes
/// when `RpcActor` changes (new actor after respawn/reconnect).
fn poll_server_events(
    actor: Option<Res<RpcActor>>,
    mut events: MessageWriter<ServerEventMessage>,
    mut sync_gen: ResMut<SyncGeneration>,
    mut receiver: Local<Option<broadcast::Receiver<kaijutsu_client::ServerEvent>>>,
) {
    let Some(actor) = actor else { return };

    // Re-subscribe when actor changes (new generation)
    if actor.is_changed() {
        *receiver = Some(actor.handle.subscribe_events());
    }

    let Some(rx) = receiver.as_mut() else { return };

    // Drain all available events
    loop {
        match rx.try_recv() {
            Ok(event) => {
                events.write(ServerEventMessage(event));
            }
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                log::warn!("Server event broadcast lagged by {n} messages");
                sync_gen.0 += 1;
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
) {
    let Some(actor) = actor else { return };

    // Re-subscribe when actor changes
    if actor.is_changed() {
        *receiver = Some(actor.handle.subscribe_status());
    }

    let Some(rx) = receiver.as_mut() else { return };

    loop {
        match rx.try_recv() {
            Ok(status) => {
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
                log::debug!("Actor status channel closed, removing RpcActor resource (gen {})", actor.generation);
                commands.remove_resource::<RpcActor>();
                *receiver = None;
                break;
            }
        }
    }
}

/// Drain results from async RPC tasks and write them as Bevy messages.
fn poll_rpc_results(
    channel: Res<RpcResultChannel>,
    mut events: MessageWriter<RpcResultMessage>,
) {
    let Ok(mut rx) = channel.rx.lock() else { return };
    while let Ok(result) = rx.try_recv() {
        events.write(result);
    }
}

/// Update `RpcConnectionState` from connection status and RPC result messages.
fn update_connection_state(
    mut state: ResMut<RpcConnectionState>,
    mut status_events: MessageReader<ConnectionStatusMessage>,
    mut result_events: MessageReader<RpcResultMessage>,
) {
    for ConnectionStatusMessage(status) in status_events.read() {
        match status {
            kaijutsu_client::ConnectionStatus::Connected => {
                state.connected = true;
                state.reconnect_attempt = 0;
            }
            kaijutsu_client::ConnectionStatus::Disconnected => {
                state.connected = false;
                state.identity = None;
                state.current_kernel = None;
            }
            kaijutsu_client::ConnectionStatus::Reconnecting { attempt } => {
                state.connected = false;
                state.reconnect_attempt = *attempt;
            }
            kaijutsu_client::ConnectionStatus::Error(_) => {
                state.connected = false;
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
            }
            _ => {}
        }
    }
}
