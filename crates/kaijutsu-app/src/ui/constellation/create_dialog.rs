//! New context tile for the constellation.
//!
//! The "New" tile creates contexts instantly (click or `n` key) with a
//! UUID-based name. `NewContextConfig` controls whether "New" forks from
//! a parent context or creates empty.
//!
//! Fork configuration is handled by `fork_form.rs` (full-viewport form).

use bevy::prelude::*;
use kaijutsu_crdt::ContextId;
use kaijutsu_types::KernelId;
use uuid::Uuid;

use crate::connection::{BootstrapChannel, BootstrapCommand, RpcActor, RpcConnectionState};

use super::NewContextConfig;

// ============================================================================
// COMPONENTS
// ============================================================================

/// Marker for the "New" create context tile in the constellation
#[derive(Component, Default, Reflect)]
#[reflect(Component)]
pub struct CreateContextNode;

// ============================================================================
// INSTANT CONTEXT CREATION
// ============================================================================

/// Create a new context immediately, optionally forking from a parent.
///
/// When `config.parent_context` is set and found in the document cache,
/// forks from that context. Otherwise creates an empty context.
/// Used by both the "New" tile click and the `n` key binding.
pub fn create_or_fork_context(
    config: &NewContextConfig,
    bootstrap: &BootstrapChannel,
    conn_state: &RpcConnectionState,
    actor: Option<&RpcActor>,
) {
    let kernel_id = conn_state.kernel_id.unwrap_or_else(KernelId::nil);
    let context_label = Uuid::new_v4().to_string()[..8].to_string();

    if let Some(ref parent) = config.parent_context {
        if let Ok(parent_ctx_id) = ContextId::parse(parent) {
            let Some(actor) = actor else {
                error!("Cannot fork from parent: no active RPC actor");
                create_empty(bootstrap, conn_state, kernel_id, &context_label);
                return;
            };
            let handle = actor.handle.clone();
            let fork_label = context_label.clone();
            let config = conn_state.ssh_config.clone();
            let bootstrap_tx = bootstrap.tx.clone();

            bevy::tasks::IoTaskPool::get()
                .spawn(async move {
                    match handle.fork_from_version(parent_ctx_id, 0, &fork_label).await {
                        Ok(ctx_id) => {
                            info!("Fork created: {}", ctx_id);
                            let instance = Uuid::new_v4().to_string();
                            let _ = bootstrap_tx.send(BootstrapCommand::SpawnActor {
                                config,
                                kernel_id,
                                context_id: Some(ctx_id),
                                instance,
                            });
                        }
                        Err(e) => error!("Fork from parent failed: {}", e),
                    }
                })
                .detach();
        } else {
            warn!("Parent context '{}' not in cache, creating empty", parent);
            create_empty(bootstrap, conn_state, kernel_id, &context_label);
        }
    } else {
        create_empty(bootstrap, conn_state, kernel_id, &context_label);
    }
}

/// Create an empty context by spawning an actor.
///
/// `label` is a human-readable name hint. The server assigns the actual ContextId
/// when the actor joins; we pass `None` to let `spawn_actor` create a fresh context.
fn create_empty(
    bootstrap: &BootstrapChannel,
    conn_state: &RpcConnectionState,
    kernel_id: KernelId,
    label: &str,
) {
    let instance = Uuid::new_v4().to_string();
    // Create a new ContextId for the empty context
    let ctx_id = ContextId::new();
    info!("Creating empty context: {} (label: {}, instance: {})", ctx_id, label, instance);
    let _ = bootstrap.tx.send(BootstrapCommand::SpawnActor {
        config: conn_state.ssh_config.clone(),
        kernel_id,
        context_id: Some(ctx_id),
        instance,
    });
}

// ============================================================================
// SYSTEMS
// ============================================================================

/// Setup the create context systems (New tile click handler).
pub fn setup_create_dialog_systems(app: &mut App) {
    app.register_type::<CreateContextNode>()
        .add_systems(Update, handle_create_node_click);
}

/// Handle clicks on the "New" tile — instant context creation, no dialog.
fn handle_create_node_click(
    create_nodes: Query<&Interaction, (Changed<Interaction>, With<CreateContextNode>)>,
    new_ctx_config: Res<NewContextConfig>,
    bootstrap: Res<BootstrapChannel>,
    conn_state: Res<RpcConnectionState>,
    actor: Option<Res<RpcActor>>,
) {
    for interaction in create_nodes.iter() {
        if *interaction == Interaction::Pressed {
            info!("New context tile clicked — creating instantly");
            create_or_fork_context(
                &new_ctx_config,
                &bootstrap,
                &conn_state,
                actor.as_deref(),
            );
        }
    }
}

