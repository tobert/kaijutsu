//! Drift state management and polling.
//!
//! Provides the `DriftState` resource that powers drift-aware widgets
//! and constellation connections. Periodically polls the server for
//! context list and staged drift queue via ActorHandle.

use bevy::prelude::*;

use kaijutsu_client::{ContextInfo, StagedDriftInfo};
use kaijutsu_crdt::BlockKind;
use kaijutsu_types::ContextId;

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
    /// Our own context ID (for determining push direction).
    pub local_context_id: Option<ContextId>,
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
        self.contexts.iter().find(|c| c.id.short() == short_id)
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
    mut drift_state: ResMut<DriftState>,
    time: Res<Time>,
    result_channel: Res<RpcResultChannel>,
) {
    let Some(actor) = actor else { return };

    let elapsed = time.elapsed_secs_f64();

    // Throttle: only poll every DRIFT_POLL_INTERVAL seconds
    if elapsed - drift_state.last_poll < DRIFT_POLL_INTERVAL {
        return;
    }

    // Set last_poll immediately to prevent stacking concurrent requests
    drift_state.last_poll = elapsed;

    // Spawn async task to fetch both context list and drift queue
    let handle = actor.handle.clone();
    let tx = result_channel.sender();

    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            // Fetch contexts
            match handle.list_contexts().await {
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
) {
    for event in events.read() {
        match event {
            RpcResultMessage::DriftContextsReceived { contexts } => {
                if !drift_state.loaded {
                    log::info!("DriftState: initial load, {} contexts", contexts.len());
                } else {
                    log::debug!("DriftState: poll, {} contexts", contexts.len());
                }
                drift_state.contexts = contexts.clone();
                drift_state.loaded = true;
            }
            RpcResultMessage::DriftQueueReceived { staged } => {
                drift_state.staged = staged.clone();
            }
            RpcResultMessage::ContextJoined { membership, .. } => {
                drift_state.local_context_id = Some(membership.context_id);
                log::info!("DriftState: joined context = {}", membership.context_id);
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
            let source_ctx = block.source_context.map(|c| c.short()).unwrap_or_else(|| "?".to_string());
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

    // Remove nodes that have been archived since last poll
    constellation.nodes.retain(|node| {
        !drift_state.contexts.iter().any(|c| c.id == node.context_id && c.archived)
    });

    for ctx_info in &drift_state.contexts {
        // Skip archived contexts — they don't appear in the constellation
        if ctx_info.archived {
            continue;
        }

        // Find matching constellation node by context id
        if let Some(node) = constellation
            .nodes
            .iter_mut()
            .find(|n| n.context_id == ctx_info.id)
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

            // Update provider if it changed
            let new_provider = if ctx_info.provider.is_empty() {
                None
            } else {
                Some(ctx_info.provider.clone())
            };

            if node.provider != new_provider {
                node.provider = new_provider;
            }

            // Sync forked_from for radial tree layout
            if node.forked_from != ctx_info.forked_from {
                node.forked_from = ctx_info.forked_from;
            }

            // Sync label
            let new_label = if ctx_info.label.is_empty() {
                None
            } else {
                Some(ctx_info.label.clone())
            };
            if node.label != new_label {
                node.label = new_label;
            }

            // Sync fork_kind
            if node.fork_kind != ctx_info.fork_kind {
                node.fork_kind = ctx_info.fork_kind.clone();
            }
        } else {
            // Create placeholder node for server-known contexts not yet in constellation.
            // This makes all contexts visible in the constellation, not just joined ones.
            constellation.add_node_from_context_info(ctx_info);
        }
    }
}
