//! New context tile for the constellation.
//!
//! The "New" tile creates contexts instantly (click or `n` key) with a
//! UUID-based name. `NewContextConfig` controls whether "New" forks from
//! a parent context or creates empty.
//!
//! Fork configuration is handled by `fork_form.rs` (full-viewport form).

use bevy::prelude::*;
use kaijutsu_crdt::ContextId;
use uuid::Uuid;

use crate::connection::{BootstrapChannel, BootstrapCommand, RpcActor, RpcConnectionState};
use crate::text::{MsdfUiText, UiTextPositionCache};
use crate::ui::theme::Theme;

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
    let kernel_id = conn_state
        .current_kernel
        .as_ref()
        .map(|k| k.id.to_string())
        .unwrap_or_else(|| crate::constants::DEFAULT_KERNEL_ID.to_string());
    let context_label = Uuid::new_v4().to_string()[..8].to_string();

    if let Some(ref parent) = config.parent_context {
        if let Ok(parent_ctx_id) = ContextId::parse(parent) {
            let Some(actor) = actor else {
                error!("Cannot fork from parent: no active RPC actor");
                create_empty(bootstrap, conn_state, &kernel_id, &context_label);
                return;
            };
            let handle = actor.handle.clone();
            let fork_label = context_label.clone();
            let config = conn_state.ssh_config.clone();
            let kernel_id = kernel_id.clone();
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
            create_empty(bootstrap, conn_state, &kernel_id, &context_label);
        }
    } else {
        create_empty(bootstrap, conn_state, &kernel_id, &context_label);
    }
}

/// Create an empty context by spawning an actor.
///
/// `label` is a human-readable name hint. The server assigns the actual ContextId
/// when the actor joins; we pass `None` to let `spawn_actor` create a fresh context.
fn create_empty(
    bootstrap: &BootstrapChannel,
    conn_state: &RpcConnectionState,
    kernel_id: &str,
    label: &str,
) {
    let instance = Uuid::new_v4().to_string();
    // Create a new ContextId for the empty context
    let ctx_id = ContextId::new();
    info!("Creating empty context: {} (label: {}, instance: {})", ctx_id, label, instance);
    let _ = bootstrap.tx.send(BootstrapCommand::SpawnActor {
        config: conn_state.ssh_config.clone(),
        kernel_id: kernel_id.to_string(),
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

/// Spawn the "New" context tile (called from render.rs)
pub fn spawn_create_context_node(
    commands: &mut Commands,
    container_entity: Entity,
    theme: &Theme,
    card_materials: &mut Assets<crate::shaders::ConstellationCardMaterial>,
) {
    use crate::shaders::ConstellationCardMaterial;
    use crate::ui::theme::color_to_vec4;

    let card_w = theme.constellation_card_width;
    let card_h = theme.constellation_card_height;

    // Create a distinct material for the New tile (dimmer, subtle)
    let material = card_materials.add(ConstellationCardMaterial {
        color: color_to_vec4(theme.fg_dim.with_alpha(0.4)),
        params: Vec4::new(1.0, 6.0, 0.2, 0.3), // Thinner border, subtle glow
        time: Vec4::ZERO,
        mode: Vec4::ZERO, // No activity dot
        dimensions: Vec4::new(card_w, card_h, 0.6, 0.0), // Dimmer opacity
    });

    let node_entity = commands
        .spawn((
            CreateContextNode,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                top: Val::Px(0.0),
                width: Val::Px(card_w),
                height: Val::Px(card_h),
                ..default()
            },
            MaterialNode(material),
            Interaction::None,
        ))
        .with_children(|parent| {
            // "New" text in the center
            parent.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    ..default()
                },
            ))
            .with_children(|center| {
                // TODO: explicit size — MsdfUiText no intrinsic sizing
                center.spawn((
                    MsdfUiText::new("New")
                        .with_font_size(14.0)
                        .with_color(theme.fg_dim),
                    UiTextPositionCache::default(),
                    Node {
                        width: Val::Px(40.0),
                        height: Val::Px(16.0),
                        ..default()
                    },
                ));
            });

            // Key hint label below
            parent
                .spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        bottom: Val::Px(-20.0),
                        left: Val::Percent(50.0),
                        margin: UiRect::left(Val::Px(-40.0)),
                        width: Val::Px(80.0),
                        justify_content: JustifyContent::Center,
                        ..default()
                    },
                    BackgroundColor(theme.panel_bg.with_alpha(0.7)),
                ))
                .with_children(|label_bg| {
                    // TODO: explicit size — MsdfUiText no intrinsic sizing
                    label_bg.spawn((
                        MsdfUiText::new("n")
                            .with_font_size(10.0)
                            .with_color(theme.fg_dim),
                        UiTextPositionCache::default(),
                        Node {
                            width: Val::Percent(100.0),
                            height: Val::Px(12.0),
                            ..default()
                        },
                    ));
                });
        })
        .id();

    commands.entity(container_entity).add_child(node_entity);
    info!("Spawned create context (New) tile");
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

