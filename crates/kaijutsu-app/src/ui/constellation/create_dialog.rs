//! New context tile for the constellation.
//!
//! The "New" tile creates contexts via the server (click or `n` key) with a
//! UUID-based label. Uses `create_context` RPC to let the server assign the ID,
//! then spawns an actor to join it.

use bevy::prelude::*;
use uuid::Uuid;

use crate::connection::{
    BootstrapChannel, BootstrapCommand, RpcActor, RpcConnectionState, RpcResultChannel,
    RpcResultMessage,
};

use super::NewContextConfig;

// ============================================================================
// COMPONENTS
// ============================================================================

/// Marker for the "New" create context tile in the constellation
#[derive(Component, Default, Reflect)]
#[reflect(Component)]
pub struct CreateContextNode;

// ============================================================================
// CONTEXT CREATION VIA RPC
// ============================================================================

/// Create a new empty context via the server.
///
/// Used by both the "New" tile click and the `n` key binding.
/// Sends `create_context` RPC via the existing actor, then on success
/// sends `ContextCreated` which triggers spawning a new actor to join it.
pub fn create_or_fork_context(
    _config: &NewContextConfig,
    actor: &RpcActor,
    result_channel: &RpcResultChannel,
    _conn_state: &RpcConnectionState,
) {
    let context_label = Uuid::new_v4().to_string()[..8].to_string();
    let handle = actor.handle.clone();
    let tx = result_channel.sender();

    info!("Creating context via server RPC (label: {})", context_label);

    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            match handle.create_context(&context_label).await {
                Ok(ctx_id) => {
                    info!(
                        "Server created context: {} (label: {})",
                        ctx_id, context_label
                    );
                    let _ = tx.send(RpcResultMessage::ContextCreated(ctx_id));
                }
                Err(e) => {
                    log::warn!("Failed to create context: {e}");
                    let _ = tx.send(RpcResultMessage::RpcError {
                        operation: "create_context".to_string(),
                        error: e.to_string(),
                    });
                }
            }
        })
        .detach();
}

// ============================================================================
// SYSTEMS
// ============================================================================

/// Setup the create context systems (New tile click handler + ContextCreated handler).
pub fn setup_create_dialog_systems(app: &mut App) {
    app.register_type::<CreateContextNode>()
        .add_systems(Update, (handle_create_node_click, handle_context_created));
}

/// Handle clicks on the "New" tile — creates context via server RPC.
fn handle_create_node_click(
    create_nodes: Query<&Interaction, (Changed<Interaction>, With<CreateContextNode>)>,
    new_ctx_config: Res<NewContextConfig>,
    actor: Option<Res<RpcActor>>,
    result_channel: Res<RpcResultChannel>,
    conn_state: Res<RpcConnectionState>,
) {
    let Some(actor) = actor else { return };
    for interaction in create_nodes.iter() {
        if *interaction == Interaction::Pressed {
            info!("New context tile clicked — creating via server RPC");
            create_or_fork_context(&new_ctx_config, &actor, &result_channel, &conn_state);
        }
    }
}

/// Handle `ContextCreated` messages — spawn an actor to join the new context.
fn handle_context_created(
    mut events: MessageReader<RpcResultMessage>,
    bootstrap: Res<BootstrapChannel>,
    conn_state: Res<RpcConnectionState>,
) {
    for event in events.read() {
        if let RpcResultMessage::ContextCreated(ctx_id) = event {
            let kernel_id = conn_state
                .kernel_id
                .unwrap_or_else(kaijutsu_types::KernelId::nil);
            let instance = Uuid::new_v4().to_string();
            info!("Spawning actor for server-created context: {}", ctx_id);
            let _ = bootstrap.tx.send(BootstrapCommand::SpawnActor {
                config: conn_state.ssh_config.clone(),
                kernel_id,
                context_id: Some(*ctx_id),
                instance,
            });
        }
    }
}
