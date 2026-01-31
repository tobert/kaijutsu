//! Constellation rendering - visual representation of context nodes
//!
//! Renders the constellation as:
//! - Glowing orb nodes for each context (using PulseRingMaterial)
//! - Connection lines between related contexts
//! - Activity-based particle effects
//! - Mode-dependent layouts (Focused/Map/Orbital)

use bevy::prelude::*;

use super::{
    ActivityState, Constellation, ConstellationContainer, ConstellationMode, ConstellationNode,
};
use crate::shaders::PulseRingMaterial;
use crate::ui::theme::{color_to_vec4, Theme};

/// System set for constellation rendering
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConstellationRendering;

/// Setup the constellation rendering systems
pub fn setup_constellation_rendering(app: &mut App) {
    app.add_systems(
        Update,
        (
            spawn_constellation_container,
            sync_constellation_visibility,
            spawn_context_nodes,
            update_node_visuals,
            despawn_removed_nodes,
        )
            .chain()
            .in_set(ConstellationRendering),
    );
}

/// Spawn the constellation container (runs once when entering Conversation)
fn spawn_constellation_container(
    mut commands: Commands,
    constellation: Res<Constellation>,
    theme: Res<Theme>,
    existing: Query<Entity, With<ConstellationContainer>>,
    content_area: Query<Entity, With<crate::ui::state::ContentArea>>,
    screen: Res<State<crate::ui::state::AppScreen>>,
) {
    // Only spawn in Conversation state
    if *screen.get() != crate::ui::state::AppScreen::Conversation {
        return;
    }

    // Don't spawn if already exists
    if !existing.is_empty() {
        return;
    }

    // Need ContentArea to parent the constellation
    let Ok(content_entity) = content_area.single() else {
        return;
    };

    // Calculate container size to encompass the constellation
    // (layout radius * 2 + node size for padding)
    let container_size = theme.constellation_layout_radius * 2.0 + theme.constellation_node_size_focused;

    // Spawn the container - positioned at center of ContentArea
    // The container holds all constellation nodes and connections
    let constellation_entity = commands
        .spawn((
            ConstellationContainer,
            Node {
                position_type: PositionType::Absolute,
                // Center the container (offset by half its size)
                left: Val::Percent(50.0),
                top: Val::Percent(50.0),
                margin: UiRect {
                    left: Val::Px(-container_size / 2.0),
                    top: Val::Px(-container_size / 2.0),
                    ..default()
                },
                width: Val::Px(container_size),
                height: Val::Px(container_size),
                // Allow children to render - don't clip
                overflow: Overflow::visible(),
                ..default()
            },
            // Transparent background (don't block content behind)
            BackgroundColor(Color::NONE),
            // Start hidden in Focused mode
            Visibility::Hidden,
            ZIndex(crate::constants::ZLayer::CONSTELLATION),
        ))
        .id();

    // Add as child of ContentArea (required for UI visibility)
    commands.entity(content_entity).add_child(constellation_entity);

    info!(
        "Spawned constellation container (mode: {:?}, {} nodes)",
        constellation.mode,
        constellation.nodes.len()
    );
}

/// Sync constellation container visibility based on mode
fn sync_constellation_visibility(
    constellation: Res<Constellation>,
    mut containers: Query<&mut Visibility, With<ConstellationContainer>>,
) {
    if !constellation.is_changed() {
        return;
    }

    let should_show = !matches!(constellation.mode, ConstellationMode::Focused);

    for mut vis in containers.iter_mut() {
        *vis = if should_show {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
    }
}

/// Spawn entities for new constellation nodes
fn spawn_context_nodes(
    mut commands: Commands,
    mut constellation: ResMut<Constellation>,
    theme: Res<Theme>,
    mut pulse_materials: ResMut<Assets<PulseRingMaterial>>,
    container: Query<Entity, With<ConstellationContainer>>,
    existing_nodes: Query<&ConstellationNode>,
) {
    let Ok(container_entity) = container.single() else {
        return;
    };

    // Collect existing node IDs
    let existing_ids: Vec<String> = existing_nodes
        .iter()
        .map(|n| n.context_id.clone())
        .collect();

    // Spawn nodes that don't have entities yet
    for node in constellation.nodes.iter_mut() {
        if existing_ids.contains(&node.context_id) {
            continue;
        }

        // Use theme values for node sizing
        let node_size = theme.constellation_node_size;
        let half_size = node_size / 2.0;

        // Container center offset (container is centered, so nodes position from its center)
        let container_center = theme.constellation_layout_radius + theme.constellation_node_size_focused / 2.0;

        // Create pulse ring material based on activity state
        let material = pulse_materials.add(create_node_material(node.activity, &theme));

        // Spawn the node entity as a child of the container
        let node_entity = commands
            .spawn((
                ConstellationNode {
                    context_id: node.context_id.clone(),
                },
                Node {
                    position_type: PositionType::Absolute,
                    // Position relative to container's top-left, offset to center
                    left: Val::Px(container_center + node.position.x - half_size),
                    top: Val::Px(container_center + node.position.y - half_size),
                    width: Val::Px(node_size),
                    height: Val::Px(node_size),
                    ..default()
                },
                // Use PulseRingMaterial for glowing orb effect
                MaterialNode(material),
            ))
            .with_children(|parent| {
                // Inner label showing context name (truncated)
                let label = truncate_context_name(&node.seat_info.id.context, 8);
                parent.spawn((
                    crate::text::MsdfUiText::new(&label)
                        .with_font_size(10.0)
                        .with_color(theme.fg),
                    crate::text::UiTextPositionCache::default(),
                    Node {
                        position_type: PositionType::Absolute,
                        bottom: Val::Px(-16.0), // Below the orb
                        left: Val::Px(0.0),
                        width: Val::Px(node_size),
                        min_height: Val::Px(12.0),
                        justify_content: JustifyContent::Center,
                        ..default()
                    },
                ));
            })
            .id();

        // Parent to container
        commands.entity(container_entity).add_child(node_entity);

        // Store entity reference in constellation node
        node.entity = Some(node_entity);

        info!(
            "Spawned constellation node for {} at {:?}",
            node.context_id, node.position
        );
    }
}

/// Update visual properties of existing nodes based on state changes
fn update_node_visuals(
    constellation: Res<Constellation>,
    theme: Res<Theme>,
    mut pulse_materials: ResMut<Assets<PulseRingMaterial>>,
    mut nodes: Query<(
        &ConstellationNode,
        &mut Node,
        &MaterialNode<PulseRingMaterial>,
    )>,
) {
    if !constellation.is_changed() {
        return;
    }

    let node_size = theme.constellation_node_size;
    let focused_size = theme.constellation_node_size_focused;
    let container_center = theme.constellation_layout_radius + focused_size / 2.0;

    for (marker, mut node_style, material_node) in nodes.iter_mut() {
        if let Some(ctx_node) = constellation.node_by_id(&marker.context_id) {
            let is_focused = constellation.focus_id.as_deref() == Some(&ctx_node.context_id);
            let size = if is_focused { focused_size } else { node_size };
            let half_size = size / 2.0;

            // Update position and size (relative to container center)
            node_style.left = Val::Px(container_center + ctx_node.position.x - half_size);
            node_style.top = Val::Px(container_center + ctx_node.position.y - half_size);
            node_style.width = Val::Px(size);
            node_style.height = Val::Px(size);

            // Update material properties based on activity
            if let Some(mat) = pulse_materials.get_mut(material_node.0.id()) {
                let color = activity_to_color(ctx_node.activity, &theme);
                mat.color = color_to_vec4(color);

                // Adjust animation speed based on activity
                mat.params.z = match ctx_node.activity {
                    ActivityState::Idle => 0.3,
                    ActivityState::Active => 0.6,
                    ActivityState::Streaming => 1.2,
                    ActivityState::Waiting => 0.8,
                    ActivityState::Error => 1.5,
                    ActivityState::Completed => 0.5,
                };

                // Increase intensity for focused node
                if is_focused {
                    mat.params.y = 0.08; // thicker rings
                    mat.color.w = 0.9;   // more opaque
                } else {
                    mat.params.y = 0.05;
                    mat.color.w = 0.7;
                }
            }
        }
    }
}

/// Despawn entities for removed constellation nodes
fn despawn_removed_nodes(
    mut commands: Commands,
    constellation: Res<Constellation>,
    nodes: Query<(Entity, &ConstellationNode)>,
) {
    // Find nodes that exist as entities but not in constellation
    for (entity, marker) in nodes.iter() {
        if constellation.node_by_id(&marker.context_id).is_none() {
            commands.entity(entity).despawn();
            info!("Despawned constellation node: {}", marker.context_id);
        }
    }
}

// ============================================================================
// HELPERS
// ============================================================================

/// Create a PulseRingMaterial for a constellation node based on activity state
fn create_node_material(activity: ActivityState, theme: &Theme) -> PulseRingMaterial {
    let color = activity_to_color(activity, theme);

    // Animation speed based on activity
    let speed = match activity {
        ActivityState::Idle => 0.3,
        ActivityState::Active => 0.6,
        ActivityState::Streaming => 1.2,
        ActivityState::Waiting => 0.8,
        ActivityState::Error => 1.5,
        ActivityState::Completed => 0.5,
    };

    PulseRingMaterial {
        color: color_to_vec4(color),
        // params: x=ring_count, y=ring_width, z=speed, w=max_radius
        params: Vec4::new(3.0, 0.05, speed, 1.0),
        time: Vec4::ZERO,
    }
}

/// Get node color based on activity state (uses theme constellation colors)
fn activity_to_color(activity: ActivityState, theme: &Theme) -> Color {
    match activity {
        ActivityState::Idle => theme.constellation_node_glow_idle,
        ActivityState::Active => theme.constellation_node_glow_active,
        ActivityState::Streaming => theme.constellation_node_glow_streaming,
        ActivityState::Waiting => theme.warning,
        ActivityState::Error => theme.constellation_node_glow_error,
        ActivityState::Completed => theme.success,
    }
}

/// Truncate context name for display in node
fn truncate_context_name(name: &str, max_len: usize) -> String {
    if name.len() <= max_len {
        name.to_string()
    } else {
        format!("{}...", &name[..max_len - 3])
    }
}
