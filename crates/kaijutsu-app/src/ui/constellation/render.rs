//! Constellation rendering - visual representation of context nodes
//!
//! Renders the constellation as:
//! - Glowing orb nodes for each context (using PulseRingMaterial)
//! - Connection lines between related contexts
//! - Activity-based particle effects
//! - Mode-dependent layouts (Focused/Map/Orbital)

use bevy::prelude::*;

use super::{
    mini::MiniRenderRegistry,
    ActivityState, Constellation, ConstellationConnection, ConstellationContainer,
    ConstellationMode, ConstellationNode, OrbitalAnimation,
};
use crate::shaders::{ConnectionLineMaterial, PulseRingMaterial};
use crate::ui::theme::{color_to_vec4, Theme};

/// System set for constellation rendering
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConstellationRendering;

/// Marker component for nodes that have a mini-render attached
#[derive(Component)]
pub struct HasMiniRender;

/// Setup the constellation rendering systems
pub fn setup_constellation_rendering(app: &mut App) {
    app.add_systems(
        Update,
        (
            spawn_constellation_container,
            sync_constellation_visibility,
            spawn_context_nodes,
            spawn_connection_lines,
            attach_mini_renders,
            update_node_visuals,
            update_connection_visuals,
            despawn_removed_nodes,
            despawn_removed_connections,
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
        // Note: mini-render textures are attached by attach_mini_renders system
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
                // Enable interaction for click-to-focus
                Interaction::None,
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

/// Attach mini-render textures to nodes that don't have them yet
fn attach_mini_renders(
    mut commands: Commands,
    mini_registry: Res<MiniRenderRegistry>,
    nodes: Query<(Entity, &ConstellationNode), Without<HasMiniRender>>,
) {
    for (entity, node) in nodes.iter() {
        // Find mini-render for this context
        if let Some(entry) = mini_registry
            .renders
            .iter()
            .find(|r| r.context_id == node.context_id)
        {
            // Add mini-render image as child of the node
            let mini_child = commands
                .spawn((
                    ImageNode::new(entry.image.clone()),
                    Node {
                        position_type: PositionType::Absolute,
                        // Center the preview inside the orb
                        left: Val::Percent(10.0),
                        top: Val::Percent(10.0),
                        width: Val::Percent(80.0),
                        height: Val::Percent(80.0),
                        border_radius: BorderRadius::all(Val::Percent(50.0)),
                        overflow: Overflow::clip(),
                        ..default()
                    },
                ))
                .id();

            commands.entity(entity).add_child(mini_child);
            commands.entity(entity).insert(HasMiniRender);

            info!(
                "Attached mini-render to constellation node: {}",
                node.context_id
            );
        }
    }
}

/// Update visual properties of existing nodes based on state changes
fn update_node_visuals(
    constellation: Res<Constellation>,
    orbital: Res<OrbitalAnimation>,
    theme: Res<Theme>,
    mut pulse_materials: ResMut<Assets<PulseRingMaterial>>,
    mut nodes: Query<(
        &ConstellationNode,
        &mut Node,
        &MaterialNode<PulseRingMaterial>,
    )>,
) {
    // Update when constellation changes OR orbital animation is active and changed
    let needs_update = constellation.is_changed() || (orbital.active && orbital.is_changed());
    if !needs_update {
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

/// Spawn connection lines between constellation nodes
fn spawn_connection_lines(
    mut commands: Commands,
    constellation: Res<Constellation>,
    theme: Res<Theme>,
    mut connection_materials: ResMut<Assets<ConnectionLineMaterial>>,
    container: Query<Entity, With<ConstellationContainer>>,
    existing_connections: Query<&ConstellationConnection>,
) {
    let Ok(container_entity) = container.single() else {
        return;
    };

    // Only spawn connections when we have 2+ nodes
    if constellation.nodes.len() < 2 {
        return;
    }

    // Build list of existing connections
    let existing: Vec<(String, String)> = existing_connections
        .iter()
        .map(|c| (c.from.clone(), c.to.clone()))
        .collect();

    // Generate connections for adjacent nodes (circular layout)
    let node_count = constellation.nodes.len();
    for i in 0..node_count {
        let next_i = (i + 1) % node_count;
        let from_id = &constellation.nodes[i].context_id;
        let to_id = &constellation.nodes[next_i].context_id;

        // Skip if connection already exists
        if existing.iter().any(|(f, t)| f == from_id && t == to_id) {
            continue;
        }

        let from_node = &constellation.nodes[i];
        let to_node = &constellation.nodes[next_i];

        // Calculate bounding box for the connection line
        let min_x = from_node.position.x.min(to_node.position.x);
        let max_x = from_node.position.x.max(to_node.position.x);
        let min_y = from_node.position.y.min(to_node.position.y);
        let max_y = from_node.position.y.max(to_node.position.y);

        // Add padding so the line isn't clipped
        let padding = theme.constellation_node_size;
        let width = (max_x - min_x).max(padding);
        let height = (max_y - min_y).max(padding);

        // Container center offset
        let container_center =
            theme.constellation_layout_radius + theme.constellation_node_size_focused / 2.0;

        // Calculate endpoints relative to the connection's bounding box (0-1 UV space)
        let rel_from_x = (from_node.position.x - min_x + padding / 2.0) / (width + padding);
        let rel_from_y = (from_node.position.y - min_y + padding / 2.0) / (height + padding);
        let rel_to_x = (to_node.position.x - min_x + padding / 2.0) / (width + padding);
        let rel_to_y = (to_node.position.y - min_y + padding / 2.0) / (height + padding);

        // Calculate activity level based on both nodes
        let activity = (from_node.activity.glow_intensity() + to_node.activity.glow_intensity()) / 2.0;

        // Calculate aspect ratio for shader
        let mat_width = width + padding;
        let mat_height = height + padding;
        let aspect = mat_width / mat_height.max(1.0);

        // Create connection material with aspect ratio correction
        let material = connection_materials.add(ConnectionLineMaterial {
            color: color_to_vec4(theme.constellation_connection_color),
            params: Vec4::new(
                0.08,                              // glow_width
                theme.constellation_connection_glow, // intensity
                0.5,                               // flow_speed
                0.0,                               // unused
            ),
            time: Vec4::new(0.0, activity, 0.0, 0.0),
            endpoints: Vec4::new(rel_from_x, rel_from_y, rel_to_x, rel_to_y),
            dimensions: Vec4::new(mat_width, mat_height, aspect, 4.0), // width, height, aspect, falloff
        });

        // Spawn connection line entity
        let connection_entity = commands
            .spawn((
                ConstellationConnection {
                    from: from_id.clone(),
                    to: to_id.clone(),
                },
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(container_center + min_x - padding / 2.0),
                    top: Val::Px(container_center + min_y - padding / 2.0),
                    width: Val::Px(width + padding),
                    height: Val::Px(height + padding),
                    ..default()
                },
                MaterialNode(material),
                // Render behind nodes
                ZIndex(-1),
            ))
            .id();

        commands.entity(container_entity).add_child(connection_entity);

        info!(
            "Spawned connection line: {} -> {}",
            from_id, to_id
        );
    }
}

/// Update connection line visuals based on node activity and positions
fn update_connection_visuals(
    constellation: Res<Constellation>,
    orbital: Res<OrbitalAnimation>,
    theme: Res<Theme>,
    mut connection_materials: ResMut<Assets<ConnectionLineMaterial>>,
    mut connections: Query<(
        &ConstellationConnection,
        &mut Node,
        &MaterialNode<ConnectionLineMaterial>,
    )>,
) {
    // Update when constellation changes OR orbital animation is active and changed
    let needs_update = constellation.is_changed() || (orbital.active && orbital.is_changed());
    if !needs_update {
        return;
    }

    let padding = theme.constellation_node_size;
    let container_center =
        theme.constellation_layout_radius + theme.constellation_node_size_focused / 2.0;

    for (marker, mut node_style, material_node) in connections.iter_mut() {
        let from_node = constellation.node_by_id(&marker.from);
        let to_node = constellation.node_by_id(&marker.to);

        if let (Some(from), Some(to)) = (from_node, to_node) {
            // Recalculate bounding box (positions may have changed in orbital mode)
            let min_x = from.position.x.min(to.position.x);
            let max_x = from.position.x.max(to.position.x);
            let min_y = from.position.y.min(to.position.y);
            let max_y = from.position.y.max(to.position.y);

            let width = (max_x - min_x).max(padding);
            let height = (max_y - min_y).max(padding);

            // Update node position
            node_style.left = Val::Px(container_center + min_x - padding / 2.0);
            node_style.top = Val::Px(container_center + min_y - padding / 2.0);
            node_style.width = Val::Px(width + padding);
            node_style.height = Val::Px(height + padding);

            if let Some(mat) = connection_materials.get_mut(material_node.0.id()) {
                // Update activity level
                let activity =
                    (from.activity.glow_intensity() + to.activity.glow_intensity()) / 2.0;
                mat.time.y = activity;

                // Update color intensity based on activity
                mat.params.y = theme.constellation_connection_glow * (0.5 + activity * 0.5);

                // Update endpoint positions relative to bounding box
                let mat_width = width + padding;
                let mat_height = height + padding;
                let rel_from_x = (from.position.x - min_x + padding / 2.0) / mat_width;
                let rel_from_y = (from.position.y - min_y + padding / 2.0) / mat_height;
                let rel_to_x = (to.position.x - min_x + padding / 2.0) / mat_width;
                let rel_to_y = (to.position.y - min_y + padding / 2.0) / mat_height;
                mat.endpoints = Vec4::new(rel_from_x, rel_from_y, rel_to_x, rel_to_y);

                // Update dimensions for aspect ratio correction
                let aspect = mat_width / mat_height.max(1.0);
                mat.dimensions = Vec4::new(mat_width, mat_height, aspect, 4.0);
            }
        }
    }
}

/// Despawn connection lines for removed connections
fn despawn_removed_connections(
    mut commands: Commands,
    constellation: Res<Constellation>,
    connections: Query<(Entity, &ConstellationConnection)>,
) {
    for (entity, marker) in connections.iter() {
        let from_exists = constellation.node_by_id(&marker.from).is_some();
        let to_exists = constellation.node_by_id(&marker.to).is_some();

        if !from_exists || !to_exists {
            commands.entity(entity).despawn();
            info!(
                "Despawned connection line: {} -> {}",
                marker.from, marker.to
            );
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
