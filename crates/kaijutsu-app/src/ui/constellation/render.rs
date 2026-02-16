//! Constellation rendering - visual representation of context nodes
//!
//! Renders the constellation as a full-takeover flex child of ContentArea:
//! - Glowing orb nodes for each context (using PulseRingMaterial)
//! - Connection lines between related contexts
//! - Camera-aware positioning with pan/zoom support
//! - Tab toggles Display + Visibility::Hidden (prevents MSDF text bleed-through)

use bevy::prelude::*;

use super::{
    create_dialog::{spawn_create_context_node, CreateContextNode},
    mini::MiniRenderRegistry,
    ActivityState, Constellation, ConstellationCamera, ConstellationConnection,
    ConstellationContainer, ConstellationNode, ConstellationVisible, DriftConnectionKind,
};
use crate::shaders::{ConnectionLineMaterial, PulseRingMaterial};
use crate::text::MsdfText;
use crate::ui::drift::DriftState;
use crate::ui::theme::{color_to_vec4, Theme};

/// System set for constellation rendering
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConstellationRendering;

/// Marker component for nodes that have a mini-render attached
#[derive(Component)]
pub struct HasMiniRender;

/// Marker for model label text on constellation nodes.
#[derive(Component)]
pub struct ModelLabel {
    pub context_id: String,
}

/// Setup the constellation rendering systems
pub fn setup_constellation_rendering(app: &mut App) {
    app.add_systems(
        Update,
        (
            spawn_constellation_container,
            sync_constellation_visibility,
            sync_cell_text_visibility,
            spawn_context_nodes,
            spawn_create_node,
            spawn_connection_lines,
            attach_mini_renders,
            update_node_visuals,
            update_create_node_visual,
            update_model_labels,
            update_connection_visuals,
            despawn_removed_nodes,
            despawn_removed_connections,
        )
            .chain()
            .in_set(ConstellationRendering),
    );
}

/// Spawn the constellation container as a full-size flex child of ContentArea.
///
/// Starts with `Display::None` — toggled by `sync_constellation_visibility`.
fn spawn_constellation_container(
    mut commands: Commands,
    existing: Query<Entity, With<ConstellationContainer>>,
    content_area: Query<Entity, With<crate::ui::state::ContentArea>>,
) {
    // Don't spawn if already exists
    if !existing.is_empty() {
        return;
    }

    // Need ContentArea to parent the constellation
    let Ok(content_entity) = content_area.single() else {
        return;
    };

    // Full-size flex child — takes over content area when visible
    let constellation_entity = commands
        .spawn((
            ConstellationContainer,
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_grow: 1.0,
                overflow: Overflow::clip(),
                display: Display::None, // Start hidden
                ..default()
            },
            Visibility::Hidden, // Hidden for rendering too — prevents MSDF bleed-through
            BackgroundColor(Color::NONE),
        ))
        .id();

    commands.entity(content_entity).add_child(constellation_entity);

    info!("Spawned constellation container (full-takeover tile)");
}

/// Sync constellation visibility: toggle Display and Visibility on constellation
/// and ConversationRoot.
///
/// `Display` controls layout (flex space allocation). `Visibility::Hidden` propagates
/// through `InheritedVisibility` to all descendants, which the MSDF extract phase
/// checks — preventing text bleed-through between views.
///
/// Targets `ConversationRoot` (the stable parent of all pane entities) rather than
/// individual `ConversationContainer`/`ComposeBlock` entities, which may not exist
/// if the tiling reconciler hasn't spawned them yet.
fn sync_constellation_visibility(
    visible: Res<ConstellationVisible>,
    mut constellation_containers: Query<
        (&mut Node, &mut Visibility),
        (With<ConstellationContainer>, Without<crate::ui::state::ConversationRoot>),
    >,
    mut conv_root: Query<
        (&mut Node, &mut Visibility),
        (With<crate::ui::state::ConversationRoot>, Without<ConstellationContainer>),
    >,
) {
    if !visible.is_changed() {
        return;
    }

    let constellation_display = if visible.0 { Display::Flex } else { Display::None };
    let conversation_display = if visible.0 { Display::None } else { Display::Flex };

    // Visibility::Hidden propagates to all descendants via InheritedVisibility,
    // which the MSDF extract phase checks before rendering text.
    let constellation_vis = if visible.0 { Visibility::Inherited } else { Visibility::Hidden };
    let conversation_vis = if visible.0 { Visibility::Hidden } else { Visibility::Inherited };

    for (mut node, mut vis) in constellation_containers.iter_mut() {
        node.display = constellation_display;
        *vis = constellation_vis;
    }

    for (mut node, mut vis) in conv_root.iter_mut() {
        node.display = conversation_display;
        *vis = conversation_vis;
    }
}

/// Hide orphaned cell-text entities when constellation is showing.
///
/// Block cells and role headers are spawned as root-level entities (no parent)
/// with screen-space coordinates via `MsdfTextAreaConfig`. Since they're not
/// descendants of `ConversationRoot`, `Visibility::Hidden` doesn't propagate
/// to them. This system directly sets their `Visibility` based on constellation
/// state. Targets `MsdfText` entities without `Node` (UI text has `Node` and
/// inherits visibility through the UI hierarchy).
fn sync_cell_text_visibility(
    visible: Res<ConstellationVisible>,
    mut cell_texts: Query<&mut Visibility, (With<MsdfText>, Without<Node>)>,
    new_texts: Query<(), (Added<MsdfText>, Without<Node>)>,
) {
    if !visible.is_changed() && new_texts.is_empty() {
        return;
    }

    let target = if visible.0 {
        Visibility::Hidden
    } else {
        Visibility::Inherited
    };

    for mut vis in cell_texts.iter_mut() {
        *vis = target;
    }
}

/// Get dynamic center point from the constellation container's computed size.
fn container_center(container: &ComputedNode) -> Vec2 {
    let size = container.size();
    Vec2::new(size.x / 2.0, size.y / 2.0)
}

/// Spawn entities for new constellation nodes
fn spawn_context_nodes(
    mut commands: Commands,
    mut constellation: ResMut<Constellation>,
    camera: Res<ConstellationCamera>,
    theme: Res<Theme>,
    mut pulse_materials: ResMut<Assets<PulseRingMaterial>>,
    container: Query<(Entity, &ComputedNode), With<ConstellationContainer>>,
    existing_nodes: Query<&ConstellationNode>,
) {
    let Ok((container_entity, computed)) = container.single() else {
        return;
    };

    // Skip first frame after visibility toggle when layout hasn't computed yet
    if computed.size() == Vec2::ZERO {
        return;
    }

    let center = container_center(computed);

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

        let node_size = theme.constellation_node_size;
        let half_size = node_size / 2.0;

        // Create pulse ring material based on activity state
        let material = pulse_materials.add(create_node_material(node.activity, &theme));

        // Camera-aware position
        let px = center.x + node.position.x * camera.zoom + camera.offset.x - half_size;
        let py = center.y + node.position.y * camera.zoom + camera.offset.y - half_size;

        let node_entity = commands
            .spawn((
                ConstellationNode {
                    context_id: node.context_id.clone(),
                },
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(px),
                    top: Val::Px(py),
                    width: Val::Px(node_size),
                    height: Val::Px(node_size),
                    ..default()
                },
                MaterialNode(material),
                Interaction::None,
            ))
            .with_children(|parent| {
                // Inner label showing context name (truncated)
                let label = truncate_context_name(&node.context_id, 12);
                parent
                    .spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            bottom: Val::Px(-24.0),
                            left: Val::Percent(50.0),
                            margin: UiRect::left(Val::Px(-60.0)),
                            width: Val::Px(120.0),
                            min_height: Val::Px(20.0),
                            justify_content: JustifyContent::Center,
                            align_items: AlignItems::Center,
                            padding: UiRect::axes(Val::Px(8.0), Val::Px(2.0)),
                            border_radius: BorderRadius::all(Val::Px(4.0)),
                            ..default()
                        },
                        BackgroundColor(theme.panel_bg.with_alpha(0.85)),
                    ))
                    .with_children(|label_bg| {
                        label_bg.spawn((
                            crate::text::MsdfUiText::new(&label)
                                .with_font_size(12.0)
                                .with_color(theme.fg),
                            crate::text::UiTextPositionCache::default(),
                            Node::default(),
                        ));
                    });

                // Model badge below context label
                let model_text = node.model.as_deref().map(truncate_model_name).unwrap_or_default();
                parent
                    .spawn((
                        ModelLabel { context_id: node.context_id.clone() },
                        Node {
                            position_type: PositionType::Absolute,
                            bottom: Val::Px(-40.0),
                            left: Val::Percent(50.0),
                            margin: UiRect::left(Val::Px(-50.0)),
                            width: Val::Px(100.0),
                            min_height: Val::Px(14.0),
                            justify_content: JustifyContent::Center,
                            align_items: AlignItems::Center,
                            ..default()
                        },
                    ))
                    .with_children(|model_bg| {
                        model_bg.spawn((
                            crate::text::MsdfUiText::new(&model_text)
                                .with_font_size(9.0)
                                .with_color(theme.fg_dim),
                            crate::text::UiTextPositionCache::default(),
                            Node::default(),
                        ));
                    });
            })
            .id();

        commands.entity(container_entity).add_child(node_entity);
        node.entity = Some(node_entity);

        info!(
            "Spawned constellation node for {} at {:?}",
            node.context_id, node.position
        );
    }
}

/// Spawn the "+" create context node (runs once per container)
fn spawn_create_node(
    mut commands: Commands,
    theme: Res<Theme>,
    mut pulse_materials: ResMut<Assets<PulseRingMaterial>>,
    container: Query<Entity, With<ConstellationContainer>>,
    existing_create_nodes: Query<Entity, With<CreateContextNode>>,
) {
    if !existing_create_nodes.is_empty() {
        return;
    }

    let Ok(container_entity) = container.single() else {
        return;
    };

    spawn_create_context_node(
        &mut commands,
        container_entity,
        &theme,
        &mut pulse_materials,
    );
}

/// Attach mini-render textures to nodes that don't have them yet
fn attach_mini_renders(
    mut commands: Commands,
    mini_registry: Res<MiniRenderRegistry>,
    nodes: Query<(Entity, &ConstellationNode), Without<HasMiniRender>>,
) {
    for (entity, node) in nodes.iter() {
        if let Some(entry) = mini_registry
            .renders
            .iter()
            .find(|r| r.context_id == node.context_id)
        {
            let mini_child = commands
                .spawn((
                    ImageNode::new(entry.image.clone()),
                    Node {
                        position_type: PositionType::Absolute,
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

/// Update visual properties of existing nodes — camera-aware positioning.
fn update_node_visuals(
    constellation: Res<Constellation>,
    camera: Res<ConstellationCamera>,
    doc_cache: Res<crate::cell::DocumentCache>,
    theme: Res<Theme>,
    mut pulse_materials: ResMut<Assets<PulseRingMaterial>>,
    container_q: Query<&ComputedNode, With<ConstellationContainer>>,
    mut nodes: Query<(
        &ConstellationNode,
        &mut Node,
        &MaterialNode<PulseRingMaterial>,
    )>,
) {
    let needs_update = constellation.is_changed()
        || camera.is_changed()
        || doc_cache.is_changed();
    if !needs_update {
        return;
    }

    let Ok(computed) = container_q.single() else {
        return;
    };
    if computed.size() == Vec2::ZERO {
        return;
    }
    let center = container_center(computed);

    let node_size = theme.constellation_node_size;
    let focused_size = theme.constellation_node_size_focused;

    for (marker, mut node_style, material_node) in nodes.iter_mut() {
        if let Some(ctx_node) = constellation.node_by_id(&marker.context_id) {
            let is_focused = constellation.focus_id.as_deref() == Some(&ctx_node.context_id);

            // Size based on focus state
            let base_size = if is_focused { focused_size } else { node_size };
            let half_size = base_size / 2.0;

            // Camera-aware position
            let px = center.x + ctx_node.position.x * camera.zoom + camera.offset.x - half_size;
            let py = center.y + ctx_node.position.y * camera.zoom + camera.offset.y - half_size;

            node_style.left = Val::Px(px);
            node_style.top = Val::Px(py);
            node_style.width = Val::Px(base_size);
            node_style.height = Val::Px(base_size);

            // Update material properties based on activity
            if let Some(mat) = pulse_materials.get_mut(material_node.0.id()) {
                let color = activity_to_color(ctx_node.activity, &theme);
                mat.color = color_to_vec4(color);

                mat.params.z = match ctx_node.activity {
                    ActivityState::Idle => 0.3,
                    ActivityState::Active => 0.6,
                    ActivityState::Streaming => 1.2,
                    ActivityState::Waiting => 0.8,
                    ActivityState::Error => 1.5,
                    ActivityState::Completed => 0.5,
                };

                // MRU boost for cached contexts
                let is_in_cache = doc_cache
                    .document_id_for_context(&ctx_node.context_id)
                    .is_some();
                let mru_boost = if is_in_cache { 0.1 } else { 0.0 };

                if is_focused {
                    mat.params.y = 0.08; // thicker rings for focus
                    mat.color.w = (0.9_f32 + mru_boost).min(1.0);
                } else {
                    mat.params.y = if is_in_cache { 0.06 } else { 0.05 };
                    mat.color.w = (0.7_f32 + mru_boost).min(1.0);
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
    for (entity, marker) in nodes.iter() {
        if constellation.node_by_id(&marker.context_id).is_none() {
            commands.entity(entity).despawn();
            info!("Despawned constellation node: {}", marker.context_id);
        }
    }
}

/// Spawn drift-aware connection lines between constellation nodes.
fn spawn_connection_lines(
    mut commands: Commands,
    constellation: Res<Constellation>,
    camera: Res<ConstellationCamera>,
    drift_state: Res<DriftState>,
    theme: Res<Theme>,
    mut connection_materials: ResMut<Assets<ConnectionLineMaterial>>,
    container: Query<(Entity, &ComputedNode), With<ConstellationContainer>>,
    existing_connections: Query<&ConstellationConnection>,
) {
    let Ok((container_entity, computed)) = container.single() else {
        return;
    };

    if computed.size() == Vec2::ZERO {
        return;
    }

    if constellation.nodes.len() < 2 {
        return;
    }

    let center = container_center(computed);
    let padding = theme.constellation_node_size;

    // Build set of existing connections
    let existing: Vec<(String, String, DriftConnectionKind)> = existing_connections
        .iter()
        .map(|c| (c.from.clone(), c.to.clone(), c.kind))
        .collect();

    let mut wanted: Vec<(String, String, DriftConnectionKind)> = Vec::new();

    // 1. Ancestry lines from DriftState.contexts (parent_id → child)
    for ctx in &drift_state.contexts {
        if let Some(ref parent_id) = ctx.parent_id {
            wanted.push((parent_id.clone(), ctx.short_id.clone(), DriftConnectionKind::Ancestry));
        }
    }

    // 2. Staged drift lines
    for staged in &drift_state.staged {
        wanted.push((
            staged.source_ctx.clone(),
            staged.target_ctx.clone(),
            DriftConnectionKind::StagedDrift,
        ));
    }

    for (from_id, to_id, kind) in &wanted {
        if existing.iter().any(|(f, t, k)| f == from_id && t == to_id && k == kind) {
            continue;
        }

        let Some(from_node) = constellation.node_by_id(from_id) else { continue };
        let Some(to_node) = constellation.node_by_id(to_id) else { continue };

        let (color, intensity, flow_speed) = match kind {
            DriftConnectionKind::Ancestry => (
                theme.constellation_connection_color,
                0.2,
                0.1,
            ),
            DriftConnectionKind::StagedDrift => (
                theme.ansi.cyan.with_alpha(0.8),
                0.6,
                0.5,
            ),
        };

        // Camera-aware positions
        let from_px = center.x + from_node.position.x * camera.zoom + camera.offset.x;
        let from_py = center.y + from_node.position.y * camera.zoom + camera.offset.y;
        let to_px = center.x + to_node.position.x * camera.zoom + camera.offset.x;
        let to_py = center.y + to_node.position.y * camera.zoom + camera.offset.y;

        let cb = compute_connection_box(from_px, from_py, to_px, to_py, padding);
        let activity = (from_node.activity.glow_intensity() + to_node.activity.glow_intensity()) / 2.0;
        let aspect = cb.width / cb.height.max(1.0);

        let material = connection_materials.add(ConnectionLineMaterial {
            color: color_to_vec4(color),
            params: Vec4::new(0.08, intensity, flow_speed, 0.0),
            time: Vec4::new(0.0, activity, 0.0, 0.0),
            endpoints: Vec4::new(cb.rel_from_x, cb.rel_from_y, cb.rel_to_x, cb.rel_to_y),
            dimensions: Vec4::new(cb.width, cb.height, aspect, 4.0),
        });

        let connection_entity = commands
            .spawn((
                ConstellationConnection {
                    from: from_id.clone(),
                    to: to_id.clone(),
                    kind: *kind,
                },
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(cb.left),
                    top: Val::Px(cb.top),
                    width: Val::Px(cb.width),
                    height: Val::Px(cb.height),
                    ..default()
                },
                MaterialNode(material),
                ZIndex(-1),
            ))
            .id();

        commands.entity(container_entity).add_child(connection_entity);

        info!("Spawned {:?} connection: {} -> {}", kind, from_id, to_id);
    }
}

/// Update connection line visuals based on node activity and camera
fn update_connection_visuals(
    constellation: Res<Constellation>,
    camera: Res<ConstellationCamera>,
    theme: Res<Theme>,
    mut connection_materials: ResMut<Assets<ConnectionLineMaterial>>,
    container_q: Query<&ComputedNode, With<ConstellationContainer>>,
    mut connections: Query<(
        &ConstellationConnection,
        &mut Node,
        &MaterialNode<ConnectionLineMaterial>,
    )>,
) {
    let needs_update = constellation.is_changed() || camera.is_changed();
    if !needs_update {
        return;
    }

    let Ok(computed) = container_q.single() else {
        return;
    };
    if computed.size() == Vec2::ZERO {
        return;
    }
    let center = container_center(computed);
    let padding = theme.constellation_node_size;

    for (marker, mut node_style, material_node) in connections.iter_mut() {
        let from_node = constellation.node_by_id(&marker.from);
        let to_node = constellation.node_by_id(&marker.to);

        if let (Some(from), Some(to)) = (from_node, to_node) {
            let from_px = center.x + from.position.x * camera.zoom + camera.offset.x;
            let from_py = center.y + from.position.y * camera.zoom + camera.offset.y;
            let to_px = center.x + to.position.x * camera.zoom + camera.offset.x;
            let to_py = center.y + to.position.y * camera.zoom + camera.offset.y;

            let cb = compute_connection_box(from_px, from_py, to_px, to_py, padding);

            node_style.left = Val::Px(cb.left);
            node_style.top = Val::Px(cb.top);
            node_style.width = Val::Px(cb.width);
            node_style.height = Val::Px(cb.height);

            if let Some(mat) = connection_materials.get_mut(material_node.0.id()) {
                let activity =
                    (from.activity.glow_intensity() + to.activity.glow_intensity()) / 2.0;
                mat.time.y = activity;
                mat.params.y = theme.constellation_connection_glow * (0.5 + activity * 0.5);

                mat.endpoints = Vec4::new(cb.rel_from_x, cb.rel_from_y, cb.rel_to_x, cb.rel_to_y);

                let aspect = cb.width / cb.height.max(1.0);
                mat.dimensions = Vec4::new(cb.width, cb.height, aspect, 4.0);
            }
        }
    }
}

/// Despawn connection lines whose nodes no longer exist or whose drift state is stale.
fn despawn_removed_connections(
    mut commands: Commands,
    constellation: Res<Constellation>,
    drift_state: Res<DriftState>,
    connections: Query<(Entity, &ConstellationConnection)>,
) {
    for (entity, marker) in connections.iter() {
        let from_exists = constellation.node_by_id(&marker.from).is_some();
        let to_exists = constellation.node_by_id(&marker.to).is_some();

        if !from_exists || !to_exists {
            commands.entity(entity).despawn();
            continue;
        }

        // Verify ancestry relationship is still valid (parent_id may change)
        if marker.kind == DriftConnectionKind::Ancestry {
            let still_valid = drift_state.contexts.iter().any(|ctx| {
                ctx.parent_id.as_deref() == Some(&marker.from) && ctx.short_id == marker.to
            });
            if !still_valid {
                commands.entity(entity).despawn();
                info!("Despawned stale ancestry line: {} -> {}", marker.from, marker.to);
                continue;
            }
        }

        if marker.kind == DriftConnectionKind::StagedDrift {
            let still_staged = drift_state.staged.iter().any(|s| {
                s.source_ctx == marker.from && s.target_ctx == marker.to
            });
            if !still_staged {
                commands.entity(entity).despawn();
                info!("Despawned flushed drift line: {} -> {}", marker.from, marker.to);
            }
        }
    }
}

/// Reposition the "+" create-context node with camera transforms.
///
/// Places it at `outer_radius * 1.1` at angle 0 (3 o'clock), outside the tree.
fn update_create_node_visual(
    constellation: Res<Constellation>,
    camera: Res<ConstellationCamera>,
    theme: Res<Theme>,
    container_q: Query<&ComputedNode, With<ConstellationContainer>>,
    mut create_nodes: Query<&mut Node, With<CreateContextNode>>,
) {
    let needs_update = constellation.is_changed() || camera.is_changed();
    if !needs_update {
        return;
    }

    let Ok(computed) = container_q.single() else {
        return;
    };
    if computed.size() == Vec2::ZERO {
        return;
    }
    let center = container_center(computed);

    // Compute outer radius from max tree depth
    let max_depth = compute_max_depth(&constellation);
    let outer_radius = theme.constellation_base_radius
        + max_depth as f32 * theme.constellation_ring_spacing;

    let node_size = theme.constellation_node_size * 0.8;
    let half_size = node_size / 2.0;

    // Position at angle 0 (right side), outside the outermost ring
    let x_pos = outer_radius * 1.1;
    let y_pos = 0.0;

    let px = center.x + x_pos * camera.zoom + camera.offset.x - half_size;
    let py = center.y + y_pos * camera.zoom + camera.offset.y - half_size;

    for mut node_style in create_nodes.iter_mut() {
        node_style.left = Val::Px(px);
        node_style.top = Val::Px(py);
    }
}

/// Compute the maximum tree depth from constellation node parent_id chains.
fn compute_max_depth(constellation: &Constellation) -> usize {
    let mut max_depth = 0_usize;
    for node in &constellation.nodes {
        let mut depth = 0;
        let mut current_id = node.parent_id.as_deref();
        while let Some(pid) = current_id {
            depth += 1;
            current_id = constellation
                .node_by_id(pid)
                .and_then(|n| n.parent_id.as_deref());
            // Safety: break if we've exceeded node count (cycle protection)
            if depth > constellation.nodes.len() {
                break;
            }
        }
        max_depth = max_depth.max(depth);
    }
    max_depth
}

// ============================================================================
// HELPERS
// ============================================================================

/// Bounding box and relative endpoint coordinates for a connection line.
struct ConnectionBox {
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    rel_from_x: f32,
    rel_from_y: f32,
    rel_to_x: f32,
    rel_to_y: f32,
}

/// Compute a centered connection box between two screen-space points.
///
/// Centers the box on the midpoint, with `padding` added to the span so
/// endpoints always land at `padding / (2 * total)` from the edge —
/// fixing the previous bug where axis-aligned lines had endpoints at 0.25
/// instead of 0.5.
fn compute_connection_box(
    from_px: f32, from_py: f32,
    to_px: f32, to_py: f32,
    padding: f32,
) -> ConnectionBox {
    let mid_x = (from_px + to_px) / 2.0;
    let mid_y = (from_py + to_py) / 2.0;
    let span_x = (from_px - to_px).abs();
    let span_y = (from_py - to_py).abs();

    let width = span_x + padding;
    let height = span_y + padding;
    let left = mid_x - width / 2.0;
    let top = mid_y - height / 2.0;

    ConnectionBox {
        left,
        top,
        width,
        height,
        rel_from_x: (from_px - left) / width,
        rel_from_y: (from_py - top) / height,
        rel_to_x: (to_px - left) / width,
        rel_to_y: (to_py - top) / height,
    }
}

/// Create a PulseRingMaterial for a constellation node based on activity state
fn create_node_material(activity: ActivityState, theme: &Theme) -> PulseRingMaterial {
    let color = activity_to_color(activity, theme);

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
        params: Vec4::new(3.0, 0.05, speed, 1.0),
        time: Vec4::ZERO,
    }
}

/// Get node color based on activity state
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
// TODO(dedup): identical to mini.rs truncate_name — extract shared helper
fn truncate_context_name(name: &str, max_len: usize) -> String {
    if name.len() <= max_len {
        name.to_string()
    } else {
        format!("{}...", &name[..max_len - 3])
    }
}

/// Strip provider prefix from model name for compact display.
fn truncate_model_name(model: &str) -> String {
    model.rsplit('/').next().unwrap_or(model).to_string()
}

/// Update model label text on constellation nodes when model info changes.
fn update_model_labels(
    constellation: Res<Constellation>,
    mut model_labels: Query<(&ModelLabel, &Children)>,
    mut msdf_texts: Query<&mut crate::text::MsdfUiText>,
) {
    if !constellation.is_changed() {
        return;
    }

    for (label, children) in model_labels.iter_mut() {
        let model_text = constellation
            .nodes
            .iter()
            .find(|n| n.context_id == label.context_id)
            .and_then(|n| n.model.as_deref())
            .map(truncate_model_name)
            .unwrap_or_default();

        for child in children.iter() {
            if let Ok(mut msdf_text) = msdf_texts.get_mut(child)
                && msdf_text.text != model_text
            {
                msdf_text.text = model_text.clone();
            }
        }
    }
}
