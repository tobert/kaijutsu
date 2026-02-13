//! Drift state management and polling.
//!
//! Provides the `DriftState` resource that powers drift-aware widgets
//! and constellation connections. Periodically polls the server for
//! context list and staged drift queue via ActorHandle.

use bevy::prelude::*;

use kaijutsu_client::{ContextInfo, StagedDriftInfo};
use kaijutsu_crdt::BlockKind;

use crate::connection::{RpcActor, RpcResultChannel, RpcResultMessage, ServerEventMessage};

/// How often to poll drift state (seconds).
const DRIFT_POLL_INTERVAL: f64 = 5.0;

/// How long a drift notification stays visible (seconds).
const NOTIFICATION_DURATION: f64 = 5.0;

// ============================================================================
// Resource
// ============================================================================

/// Drift state resource — populated by polling, consumed by widgets and constellation.
#[derive(Resource, Default)]
pub struct DriftState {
    /// All contexts registered in the drift router.
    pub contexts: Vec<ContextInfo>,
    /// Staged (pending) drift operations.
    pub staged: Vec<StagedDriftInfo>,
    /// Our own context short ID (for determining push direction).
    pub local_context_id: Option<String>,
    /// The context name from our seat (used to resolve local_context_id).
    pub local_context_name: Option<String>,
    /// Last poll timestamp (from `Time::elapsed_secs_f64()`).
    pub last_poll: f64,
    /// Whether we've received at least one successful poll.
    pub loaded: bool,
    /// Transient notification for incoming drift (auto-dismisses).
    pub notification: Option<DriftNotification>,
}

impl DriftState {
    /// Get context info by short ID (used by Phase 4 constellation navigation).
    #[allow(dead_code)]
    pub fn context_by_id(&self, short_id: &str) -> Option<&ContextInfo> {
        self.contexts.iter().find(|c| c.short_id == short_id)
    }

    /// Count of staged drifts.
    pub fn staged_count(&self) -> usize {
        self.staged.len()
    }
}

/// Transient notification for drift arrival.
#[derive(Debug, Clone)]
pub struct DriftNotification {
    /// Source context short ID.
    pub source_ctx: String,
    /// Preview of content (first ~40 chars).
    pub preview: String,
    /// When the notification was created.
    pub created_at: f64,
}

// ============================================================================
// Plugin
// ============================================================================

/// Plugin for drift state management.
pub struct DriftPlugin;

impl Plugin for DriftPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DriftState>()
            .add_systems(
                Update,
                (
                    poll_drift_state,
                    update_drift_state,
                    detect_drift_arrival,
                    dismiss_stale_notifications,
                    sync_model_info_to_constellation,
                )
                    .chain(),
            );
    }
}

// ============================================================================
// Systems
// ============================================================================

/// Poll drift state from server at regular intervals.
///
/// Follows the `poll_server_events` pattern: clone handle, spawn async,
/// send results via `RpcResultChannel`.
fn poll_drift_state(
    actor: Option<Res<RpcActor>>,
    drift_state: Res<DriftState>,
    conn_state: Res<crate::connection::RpcConnectionState>,
    time: Res<Time>,
    result_channel: Res<RpcResultChannel>,
) {
    let Some(actor) = actor else { return };

    // Don't fire RPCs when disconnected — they'd just trigger reconnect attempts
    if !conn_state.connected {
        return;
    }

    let elapsed = time.elapsed_secs_f64();

    // Throttle: only poll every DRIFT_POLL_INTERVAL seconds
    if elapsed - drift_state.last_poll < DRIFT_POLL_INTERVAL {
        return;
    }

    // Spawn async task to fetch both context list and drift queue
    let handle = actor.handle.clone();
    let tx = result_channel.sender();

    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            // Fetch contexts
            match handle.list_all_contexts().await {
                Ok(contexts) => {
                    let _ = tx.send(RpcResultMessage::DriftContextsReceived { contexts });
                }
                Err(e) => {
                    log::debug!("drift poll: list_all_contexts failed: {e}");
                }
            }

            // Fetch staged queue
            match handle.drift_queue().await {
                Ok(staged) => {
                    let _ = tx.send(RpcResultMessage::DriftQueueReceived { staged });
                }
                Err(e) => {
                    log::debug!("drift poll: drift_queue failed: {e}");
                }
            }
        })
        .detach();
}

/// Drain `DriftContextsReceived`, `DriftQueueReceived`, and `ContextJoined` into `DriftState`.
///
/// All RPC result processing lives here to avoid multiple `MessageReader<RpcResultMessage>`
/// systems independently consuming the same event stream.
fn update_drift_state(
    mut drift_state: ResMut<DriftState>,
    mut events: MessageReader<RpcResultMessage>,
    time: Res<Time>,
) {
    for event in events.read() {
        match event {
            RpcResultMessage::DriftContextsReceived { contexts } => {
                drift_state.contexts = contexts.clone();
                drift_state.loaded = true;
                drift_state.last_poll = time.elapsed_secs_f64();

                // Resolve local_context_id to short_id now that we have context data.
                // local_context_name is the seat's context name (e.g. kernel_id);
                // we need the short_id that drift blocks use in source_context.
                if let Some(ref name) = drift_state.local_context_name {
                    if let Some(ctx) = contexts.iter().find(|c| c.name == *name || c.short_id == *name) {
                        if drift_state.local_context_id.as_ref() != Some(&ctx.short_id) {
                            log::info!("DriftState: resolved local context → @{}", ctx.short_id);
                            drift_state.local_context_id = Some(ctx.short_id.clone());
                        }
                    }
                }
            }
            RpcResultMessage::DriftQueueReceived { staged } => {
                drift_state.staged = staged.clone();
            }
            RpcResultMessage::ContextJoined { seat, .. } => {
                // Store context name from seat — will be resolved to short_id
                // when contexts arrive from the next poll.
                drift_state.local_context_name = Some(seat.id.context.clone());
                log::info!("DriftState: joined context = {}", seat.id.context);
            }
            _ => {}
        }
    }
}

/// Detect incoming drift blocks from `ServerEvent::BlockInserted` and create notifications.
fn detect_drift_arrival(
    mut drift_state: ResMut<DriftState>,
    mut events: MessageReader<ServerEventMessage>,
    time: Res<Time>,
) {
    for ServerEventMessage(event) in events.read() {
        if let kaijutsu_client::ServerEvent::BlockInserted { block, .. } = event
            && block.kind == BlockKind::Drift
        {
            let source_ctx = block.source_context.as_deref().unwrap_or("?").to_string();
            let preview: String = block.content.chars().take(40).collect();

            drift_state.notification = Some(DriftNotification {
                source_ctx: source_ctx.clone(),
                preview: preview.clone(),
                created_at: time.elapsed_secs_f64(),
            });

            log::info!("Drift arrived from @{}: {}...", source_ctx, preview);
        }
    }
}

/// Auto-dismiss stale notifications after NOTIFICATION_DURATION.
fn dismiss_stale_notifications(
    mut drift_state: ResMut<DriftState>,
    time: Res<Time>,
) {
    if let Some(ref notif) = drift_state.notification
        && time.elapsed_secs_f64() - notif.created_at > NOTIFICATION_DURATION
    {
        drift_state.notification = None;
    }
}

/// Sync model info from `DriftState.contexts` to `Constellation` nodes.
///
/// When the drift poll returns context info with model names, update the
/// matching constellation nodes so they can display model badges.
fn sync_model_info_to_constellation(
    drift_state: Res<DriftState>,
    mut constellation: ResMut<crate::ui::constellation::Constellation>,
) {
    if !drift_state.is_changed() {
        return;
    }

    for ctx_info in &drift_state.contexts {
        // Find matching constellation node by context name or short_id
        if let Some(node) = constellation
            .nodes
            .iter_mut()
            .find(|n| n.context_id == ctx_info.name || n.context_id == ctx_info.short_id)
        {
            // Update model if it changed
            let new_model = if ctx_info.model.is_empty() {
                None
            } else {
                Some(ctx_info.model.clone())
            };

            if node.model != new_model {
                node.model = new_model;
            }

            // Sync parent_id for radial tree layout
            if node.parent_id != ctx_info.parent_id {
                node.parent_id = ctx_info.parent_id.clone();
            }
        }
    }
}
