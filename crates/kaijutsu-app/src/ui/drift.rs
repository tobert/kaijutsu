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
    /// Last poll timestamp (from `Time::elapsed_secs_f64()`).
    pub last_poll: f64,
    /// Whether we've received at least one successful poll.
    pub loaded: bool,
    /// Transient notification for incoming drift (auto-dismisses).
    pub notification: Option<DriftNotification>,
}

impl DriftState {
    /// Get context info by short ID.
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
                    populate_local_context_id,
                    detect_drift_arrival,
                    dismiss_stale_notifications,
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
    time: Res<Time>,
    result_channel: Res<RpcResultChannel>,
) {
    let Some(actor) = actor else { return };
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

/// Drain `DriftContextsReceived` and `DriftQueueReceived` into `DriftState`.
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
            }
            RpcResultMessage::DriftQueueReceived { staged } => {
                drift_state.staged = staged.clone();
            }
            _ => {}
        }
    }
}

/// Populate `local_context_id` when we join a context.
fn populate_local_context_id(
    mut drift_state: ResMut<DriftState>,
    mut events: MessageReader<RpcResultMessage>,
) {
    for event in events.read() {
        if let RpcResultMessage::ContextJoined { seat, .. } = event {
            // The seat.id.context is our context name — we need the short ID.
            // For now, use the context name; the drift router registers with kernel_id
            // which maps to the context_name. We'll match against ContextInfo later.
            drift_state.local_context_id = Some(seat.id.context.clone());
            log::info!("DriftState: local context = {}", seat.id.context);
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
