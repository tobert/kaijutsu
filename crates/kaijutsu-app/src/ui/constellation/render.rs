//! Constellation rendering - visual representation of context nodes
//!
//! Renders the constellation as a full-takeover flex child of ContentArea:
//! - Rectangular card nodes for each context (using ConstellationCardMaterial)
//! - Connection lines between related contexts
//! - Camera-aware positioning with pan/zoom support
//! - Tab toggles Display + Visibility::Hidden (prevents MSDF text bleed-through)

use bevy::prelude::*;

use super::{
    create_dialog::{spawn_create_context_node, CreateContextNode},
    ActivityState, Constellation, ConstellationCamera, ConstellationConnection,
    ConstellationContainer, ConstellationNode, ConstellationVisible, DriftConnectionKind,
};
use crate::input::focus::{FocusArea, FocusStack};
use crate::shaders::{DriftArcMaterial, ConstellationCardMaterial, HudPanelMaterial, RingGuideMaterial, StarFieldMaterial};
use crate::text::MsdfText;
use crate::ui::drift::DriftState;
use crate::ui::theme::{agent_color_for_provider, color_to_vec4, Theme};

/// System set for constellation rendering
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConstellationRendering;

/// Marker for model label text on constellation nodes.
#[derive(Component)]
pub struct ModelLabel {
    pub context_id: String,
}

/// Marker for the procedural star field background behind constellation content.
#[derive(Component)]
pub struct StarFieldBackground;

/// Marker for the concentric ring guide circles behind constellation content.
#[derive(Component)]
pub struct RingGuideBackground;

/// Marker for the legend panel container in the constellation view.
#[derive(Component)]
pub struct ConstellationLegend;

/// Marker for individual text rows inside the legend (rebuilt on data change).
#[derive(Component)]
pub struct LegendContent;

/// Setup the constellation rendering systems
pub fn setup_constellation_rendering(app: &mut App) {
    app.add_systems(
        Update,
        (
            enforce_constellation_focus_sync,
            spawn_constellation_container,
            spawn_star_field,
            spawn_ring_guide,
            sync_constellation_visibility,
            sync_cell_text_visibility,
            spawn_context_nodes,
            spawn_create_node,
            spawn_connection_lines,
            // attach_mini_renders disabled — card nodes don't use render-to-texture
            update_node_visuals,
            update_star_field,
            update_ring_guide,
            update_create_node_visual,
            update_model_labels,
            update_connection_visuals,
            spawn_legend_panel,
            update_legend_content,
            despawn_removed_nodes,
            despawn_removed_connections,
        )
            .chain()
            .in_set(ConstellationRendering),
    );
}

/// Enforce constellation↔focus sync: when `FocusArea` changes, update
/// `ConstellationVisible` to match. This is the single source of truth for
/// the invariant "constellation is visible iff focus is Constellation".
///
/// Skipped while a modal is active (dialog over constellation), so the
/// constellation stays visible behind the dialog overlay.
fn enforce_constellation_focus_sync(
    mut visible: ResMut<ConstellationVisible>,
    focus: Res<FocusArea>,
    focus_stack: Res<FocusStack>,
) {
    if !focus.is_changed() {
        return;
    }
    // Don't sync while modal is active — constellation stays as-is behind dialog
    if focus_stack.is_modal() {
        return;
    }

    let should_show = matches!(*focus, FocusArea::Constellation);
    if visible.0 != should_show {
        visible.0 = should_show;
    }
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

/// Spawn a full-size star field background as first child of ConstellationContainer.
///
/// Uses `ZIndex(-2)` to render behind connection lines (-1) and card nodes (0).
/// The shader draws procedural hash-based stars with subtle twinkle animation.
fn spawn_star_field(
    mut commands: Commands,
    mut star_materials: ResMut<Assets<StarFieldMaterial>>,
    container: Query<(Entity, &ComputedNode), With<ConstellationContainer>>,
    existing: Query<Entity, With<StarFieldBackground>>,
) {
    if !existing.is_empty() {
        return;
    }

    let Ok((container_entity, computed)) = container.single() else {
        return;
    };

    let size = computed.size();
    if size == Vec2::ZERO {
        return;
    }

    let material = star_materials.add(StarFieldMaterial {
        dimensions: Vec4::new(size.x, size.y, 0.0, 0.0),
        ..default()
    });

    let star_entity = commands
        .spawn((
            StarFieldBackground,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                top: Val::Px(0.0),
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                ..default()
            },
            MaterialNode(material),
            ZIndex(-2),
        ))
        .id();

    commands.entity(container_entity).add_child(star_entity);
    info!("Spawned star field background");
}

/// Update star field dimensions and camera offset for parallax.
fn update_star_field(
    camera: Res<ConstellationCamera>,
    mut star_materials: ResMut<Assets<StarFieldMaterial>>,
    container_q: Query<&ComputedNode, With<ConstellationContainer>>,
    star_nodes: Query<&MaterialNode<StarFieldMaterial>, With<StarFieldBackground>>,
) {
    if !camera.is_changed() {
        return;
    }

    let Ok(computed) = container_q.single() else {
        return;
    };
    let size = computed.size();
    if size == Vec2::ZERO {
        return;
    }

    for material_node in star_nodes.iter() {
        if let Some(mat) = star_materials.get_mut(material_node.0.id()) {
            mat.dimensions = Vec4::new(size.x, size.y, camera.offset.x, camera.offset.y);
        }
    }
}

/// Spawn a full-size ring guide background behind constellation cards.
///
/// Uses `ZIndex(-2)` like the star field — both are atmospheric backgrounds.
/// The shader draws faint dashed concentric circles at the layout algorithm's ring radii.
fn spawn_ring_guide(
    mut commands: Commands,
    theme: Res<Theme>,
    mut ring_materials: ResMut<Assets<RingGuideMaterial>>,
    container: Query<(Entity, &ComputedNode), With<ConstellationContainer>>,
    existing: Query<Entity, With<RingGuideBackground>>,
) {
    if !existing.is_empty() {
        return;
    }

    let Ok((container_entity, computed)) = container.single() else {
        return;
    };

    let size = computed.size();
    if size == Vec2::ZERO {
        return;
    }

    let material = ring_materials.add(RingGuideMaterial {
        params: Vec4::new(
            theme.constellation_base_radius,
            theme.constellation_ring_spacing,
            4.0, // max_rings — enough for typical tree depth
            24.0, // dash_count — dashes per ring circumference
        ),
        dimensions: Vec4::new(size.x, size.y, 0.0, 0.0),
        ..default()
    });

    let ring_entity = commands
        .spawn((
            RingGuideBackground,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                top: Val::Px(0.0),
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                ..default()
            },
            MaterialNode(material),
            ZIndex(-2),
        ))
        .id();

    commands.entity(container_entity).add_child(ring_entity);
    info!("Spawned ring guide background");
}

/// Update ring guide camera offset, zoom, and dimensions.
fn update_ring_guide(
    constellation: Res<Constellation>,
    camera: Res<ConstellationCamera>,
    theme: Res<Theme>,
    mut ring_materials: ResMut<Assets<RingGuideMaterial>>,
    container_q: Query<&ComputedNode, With<ConstellationContainer>>,
    ring_nodes: Query<&MaterialNode<RingGuideMaterial>, With<RingGuideBackground>>,
) {
    let needs_update = camera.is_changed() || constellation.is_changed() || theme.is_changed();
    if !needs_update {
        return;
    }

    let Ok(computed) = container_q.single() else {
        return;
    };
    let size = computed.size();
    if size == Vec2::ZERO {
        return;
    }

    let max_depth = compute_max_depth(&constellation);

    for material_node in ring_nodes.iter() {
        if let Some(mat) = ring_materials.get_mut(material_node.0.id()) {
            mat.params = Vec4::new(
                theme.constellation_base_radius,
                theme.constellation_ring_spacing,
                max_depth.max(1) as f32, // at least 1 ring
                24.0,
            );
            mat.camera = Vec4::new(
                camera.offset.x,
                camera.offset.y,
                camera.zoom,
                0.12, // line_opacity — subtle
            );
            mat.dimensions = Vec4::new(size.x, size.y, 0.0, 0.0);
        }
    }
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

/// Spawn entities for new constellation nodes as rectangular cards.
fn spawn_context_nodes(
    mut commands: Commands,
    mut constellation: ResMut<Constellation>,
    camera: Res<ConstellationCamera>,
    theme: Res<Theme>,
    mut card_materials: ResMut<Assets<ConstellationCardMaterial>>,
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

    let card_w = theme.constellation_card_width;
    let card_h = theme.constellation_card_height;

    // Spawn nodes that don't have entities yet
    for node in constellation.nodes.iter_mut() {
        if existing_ids.contains(&node.context_id) {
            continue;
        }

        // Agent color from provider
        let agent_color = agent_color_for_provider(&theme, node.provider.as_deref());
        let activity_dot_color = activity_to_color(node.activity, &theme);
        let dot_srgba = activity_dot_color.to_srgba();

        let material = card_materials.add(ConstellationCardMaterial {
            color: color_to_vec4(agent_color),
            params: Vec4::new(
                theme.constellation_card_border_thickness,
                theme.constellation_card_corner_radius,
                theme.constellation_card_glow_radius,
                theme.constellation_card_glow_intensity,
            ),
            time: Vec4::ZERO,
            mode: Vec4::new(dot_srgba.red, dot_srgba.green, dot_srgba.blue, 0.0),
            dimensions: Vec4::new(card_w, card_h, 1.0, 0.0),
        });

        // Camera-aware position
        let px = center.x + node.position.x * camera.zoom + camera.offset.x - card_w / 2.0;
        let py = center.y + node.position.y * camera.zoom + camera.offset.y - card_h / 2.0;

        let node_entity = commands
            .spawn((
                ConstellationNode {
                    context_id: node.context_id.clone(),
                },
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(px),
                    top: Val::Px(py),
                    width: Val::Px(card_w),
                    height: Val::Px(card_h),
                    ..default()
                },
                MaterialNode(material),
                Interaction::None,
            ))
            .with_children(|parent| {
                // Context name label below the card
                let label = truncate_context_name(&node.context_id, 16);
                parent
                    .spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            bottom: Val::Px(-24.0),
                            left: Val::Percent(50.0),
                            margin: UiRect::left(Val::Px(-70.0)),
                            width: Val::Px(140.0),
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
                            margin: UiRect::left(Val::Px(-60.0)),
                            width: Val::Px(120.0),
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
            "Spawned constellation card for {} at {:?}",
            node.context_id, node.position
        );
    }
}

/// Spawn the "+" create context node (runs once per container)
fn spawn_create_node(
    mut commands: Commands,
    theme: Res<Theme>,
    mut card_materials: ResMut<Assets<ConstellationCardMaterial>>,
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
        &mut card_materials,
    );
}

/// Update visual properties of existing card nodes — camera-aware positioning,
/// agent coloring, opacity based on depth/cache status, and focus state.
fn update_node_visuals(
    constellation: Res<Constellation>,
    camera: Res<ConstellationCamera>,
    doc_cache: Res<crate::cell::DocumentCache>,
    theme: Res<Theme>,
    mut card_materials: ResMut<Assets<ConstellationCardMaterial>>,
    container_q: Query<&ComputedNode, With<ConstellationContainer>>,
    mut nodes: Query<(
        &ConstellationNode,
        &mut Node,
        &MaterialNode<ConstellationCardMaterial>,
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

    let card_w = theme.constellation_card_width;
    let card_h = theme.constellation_card_height;
    let focused_scale = 1.15;

    for (marker, mut node_style, material_node) in nodes.iter_mut() {
        if let Some(ctx_node) = constellation.node_by_id(&marker.context_id) {
            let is_focused = constellation.focus_id.as_deref() == Some(&ctx_node.context_id);
            let is_in_cache = doc_cache
                .document_id_for_context(&ctx_node.context_id)
                .is_some();

            // Size: focused nodes are scaled up
            let scale = if is_focused {
                focused_scale
            } else if ctx_node.activity == ActivityState::Streaming || ctx_node.activity == ActivityState::Active {
                1.0
            } else if is_in_cache {
                0.9
            } else {
                0.85
            };
            let w = card_w * scale;
            let h = card_h * scale;

            // Camera-aware position
            let px = center.x + ctx_node.position.x * camera.zoom + camera.offset.x - w / 2.0;
            let py = center.y + ctx_node.position.y * camera.zoom + camera.offset.y - h / 2.0;

            node_style.left = Val::Px(px);
            node_style.top = Val::Px(py);
            node_style.width = Val::Px(w);
            node_style.height = Val::Px(h);

            // Update card material
            if let Some(mat) = card_materials.get_mut(material_node.0.id()) {
                // Agent color from provider
                let agent_color = agent_color_for_provider(&theme, ctx_node.provider.as_deref());
                mat.color = color_to_vec4(agent_color);

                // Activity dot color
                let dot_color = activity_to_color(ctx_node.activity, &theme);
                let dot_srgba = dot_color.to_srgba();
                mat.mode = Vec4::new(dot_srgba.red, dot_srgba.green, dot_srgba.blue, 0.0);

                // Opacity: depth + cache + activity
                let depth = tree_depth(ctx_node, &constellation);
                let depth_factor: f32 = if depth >= 3 { 0.7 } else { 1.0 };
                let base_opacity: f32 = if is_focused {
                    1.0
                } else if ctx_node.activity == ActivityState::Streaming || ctx_node.activity == ActivityState::Active {
                    0.9
                } else if is_in_cache {
                    0.7
                } else {
                    0.5
                };
                let opacity = (base_opacity * depth_factor).clamp(0.3, 1.0);

                mat.dimensions = Vec4::new(w, h, opacity, if is_focused { 1.0 } else { 0.0 });

                // Update border params (thickness scales slightly with focus)
                let thickness = theme.constellation_card_border_thickness * if is_focused { 1.5 } else { 1.0 };
                mat.params = Vec4::new(
                    thickness,
                    theme.constellation_card_corner_radius,
                    theme.constellation_card_glow_radius,
                    theme.constellation_card_glow_intensity,
                );
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
    mut arc_materials: ResMut<Assets<DriftArcMaterial>>,
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
    let padding = theme.constellation_card_width;

    // Build set of existing connections
    let existing: Vec<(String, String, DriftConnectionKind)> = existing_connections
        .iter()
        .map(|c| (c.from.clone(), c.to.clone(), c.kind))
        .collect();

    let mut wanted: Vec<(String, String, DriftConnectionKind)> = Vec::new();

    // 1. Ancestry lines from DriftState.contexts (parent_id → child)
    for ctx in &drift_state.contexts {
        if let Some(ref parent_id) = ctx.parent_id {
            wanted.push((parent_id.to_string(), ctx.id.to_string(), DriftConnectionKind::Ancestry));
        }
    }

    // 2. Staged drift lines
    for staged in &drift_state.staged {
        wanted.push((
            staged.source_ctx.to_string(),
            staged.target_ctx.to_string(),
            DriftConnectionKind::StagedDrift,
        ));
    }

    for (from_id, to_id, kind) in &wanted {
        if existing.iter().any(|(f, t, k)| f == from_id && t == to_id && k == kind) {
            continue;
        }

        let Some(from_node) = constellation.node_by_id(from_id) else { continue };
        let Some(to_node) = constellation.node_by_id(to_id) else { continue };

        let (color, intensity, flow_speed, curve_amount) = match kind {
            DriftConnectionKind::Ancestry => (
                theme.constellation_connection_color,
                0.2,
                0.1,
                0.25, // gentle curve for parent→child
            ),
            DriftConnectionKind::StagedDrift => (
                theme.ansi.cyan.with_alpha(0.8),
                0.6,
                0.5,
                0.35, // more pronounced curve for drift arcs
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

        let material = arc_materials.add(DriftArcMaterial {
            color: color_to_vec4(color),
            params: Vec4::new(0.08, intensity, flow_speed, curve_amount),
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

/// Update drift arc visuals based on node activity and camera
fn update_connection_visuals(
    constellation: Res<Constellation>,
    camera: Res<ConstellationCamera>,
    theme: Res<Theme>,
    mut arc_materials: ResMut<Assets<DriftArcMaterial>>,
    container_q: Query<&ComputedNode, With<ConstellationContainer>>,
    mut connections: Query<(
        &ConstellationConnection,
        &mut Node,
        &MaterialNode<DriftArcMaterial>,
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
    let padding = theme.constellation_card_width;

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

            if let Some(mat) = arc_materials.get_mut(material_node.0.id()) {
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
                ctx.parent_id.as_ref().map(|p| p.to_string()).as_deref() == Some(marker.from.as_str())
                    && ctx.id.to_string() == marker.to
            });
            if !still_valid {
                commands.entity(entity).despawn();
                info!("Despawned stale ancestry line: {} -> {}", marker.from, marker.to);
                continue;
            }
        }

        if marker.kind == DriftConnectionKind::StagedDrift {
            let still_staged = drift_state.staged.iter().any(|s| {
                s.source_ctx.to_string() == marker.from && s.target_ctx.to_string() == marker.to
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

    let card_w = theme.constellation_card_width;
    let card_h = theme.constellation_card_height;

    // Position at angle 0 (right side), clear of the widest card + padding
    let min_clearance = card_w + 24.0;
    let x_pos = outer_radius.max(min_clearance) + card_w * 0.3;
    let y_pos = 0.0;

    let px = center.x + x_pos * camera.zoom + camera.offset.x - card_w / 2.0;
    let py = center.y + y_pos * camera.zoom + camera.offset.y - card_h / 2.0;

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

/// Compute tree depth of a context node by walking parent_id chain.
fn tree_depth(node: &super::ContextNode, constellation: &Constellation) -> usize {
    let mut depth = 0;
    let mut current_id = node.parent_id.as_deref();
    while let Some(pid) = current_id {
        depth += 1;
        current_id = constellation
            .node_by_id(pid)
            .and_then(|n| n.parent_id.as_deref());
        if depth > constellation.nodes.len() {
            break; // Cycle protection
        }
    }
    depth
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

// ============================================================================
// LEGEND PANEL
// ============================================================================

/// Spawn the legend panel container as a child of ConstellationContainer.
///
/// Positioned top-left with `HudPanelMaterial` background. Content children
/// are spawned/rebuilt by `update_legend_content`.
fn spawn_legend_panel(
    mut commands: Commands,
    theme: Res<Theme>,
    mut hud_materials: ResMut<Assets<HudPanelMaterial>>,
    container: Query<Entity, With<ConstellationContainer>>,
    existing: Query<Entity, With<ConstellationLegend>>,
) {
    if !existing.is_empty() {
        return;
    }

    let Ok(container_entity) = container.single() else {
        return;
    };

    let panel_color = color_to_vec4(theme.panel_bg.with_alpha(0.85));
    let glow_color = color_to_vec4(theme.accent.with_alpha(0.3));

    let material = hud_materials.add(HudPanelMaterial {
        color: panel_color,
        glow_color,
        params: Vec4::new(0.3, 0.0, 1.0, 0.0),
        time: Vec4::ZERO,
    });

    let legend_entity = commands
        .spawn((
            ConstellationLegend,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(16.0),
                top: Val::Px(16.0),
                width: Val::Px(220.0),
                min_height: Val::Px(80.0),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(Val::Px(12.0)),
                row_gap: Val::Px(4.0),
                ..default()
            },
            MaterialNode(material),
            ZIndex(1), // Above cards and connections
        ))
        .id();

    commands.entity(container_entity).add_child(legend_entity);
    info!("Spawned constellation legend panel");
}

/// Rebuild legend content when DriftState or Constellation changes.
///
/// Despawns all `LegendContent` children and rebuilds from current data.
/// Shows: kernel name, context/agent summary, per-provider rows with colored
/// dots and context counts, and staged drift count.
fn update_legend_content(
    mut commands: Commands,
    visible: Res<ConstellationVisible>,
    drift_state: Res<DriftState>,
    constellation: Res<Constellation>,
    theme: Res<Theme>,
    legend_q: Query<Entity, With<ConstellationLegend>>,
    content_q: Query<Entity, With<LegendContent>>,
    mut last_fingerprint: Local<u64>,
) {
    if !visible.0 {
        return;
    }

    // Build a cheap fingerprint of the data that drives legend content.
    // Only rebuild when this fingerprint changes — avoids despawning MSDF
    // text entities every frame (they need 2+ frames to initialize rendering).
    let fingerprint = {
        let mut h: u64 = constellation.nodes.len() as u64;
        h = h.wrapping_mul(31).wrapping_add(drift_state.staged.len() as u64);
        h = h.wrapping_mul(31).wrapping_add(drift_state.contexts.len() as u64);
        for ctx in &drift_state.contexts {
            h = h.wrapping_mul(31).wrapping_add(ctx.provider.len() as u64);
        }
        h
    };

    if fingerprint == *last_fingerprint && !content_q.is_empty() {
        return;
    }

    let Ok(legend_entity) = legend_q.single() else {
        return;
    };

    // Despawn old content
    for entity in content_q.iter() {
        commands.entity(entity).despawn();
    }

    // Gather data from DriftState contexts
    let contexts = &drift_state.contexts;
    let total_contexts = constellation.nodes.len();
    let staged_count = drift_state.staged_count();

    // Kernel name from connection state (ContextInfo no longer carries kernel_id)
    let kernel_name = "(kernel)";

    // Group contexts by provider
    let mut provider_counts: Vec<(&str, Color, usize)> = Vec::new();
    let provider_groups = [
        ("human", theme.agent_color_human),
        ("anthropic", theme.agent_color_claude),
        ("google", theme.agent_color_gemini),
        ("deepseek", theme.agent_color_deepseek),
        ("local", theme.agent_color_local),
    ];

    for (provider_key, color) in &provider_groups {
        let count = contexts
            .iter()
            .filter(|c| {
                let p = c.provider.to_ascii_lowercase();
                match *provider_key {
                    "anthropic" => p.contains("anthropic") || p.contains("claude"),
                    "google" => p.contains("google") || p.contains("gemini"),
                    "deepseek" => p.contains("deepseek"),
                    "local" => p.contains("ollama") || p.contains("local") || p.contains("llama"),
                    "human" => p.is_empty(), // No provider = human
                    _ => false,
                }
            })
            .count();
        if count > 0 {
            provider_counts.push((provider_key, *color, count));
        }
    }

    let unique_providers = provider_counts.len();

    // === Spawn content rows ===

    // Header: kernel name
    let header = spawn_legend_text(
        &mut commands,
        &truncate_context_name(kernel_name, 22),
        theme.fg,
        11.0,
    );
    commands.entity(legend_entity).add_child(header);

    // Summary line
    let summary = format!("{} contexts \u{00b7} {} agents", total_contexts, unique_providers);
    let summary_entity = spawn_legend_text(&mut commands, &summary, theme.fg_dim, 9.0);
    commands.entity(legend_entity).add_child(summary_entity);

    // Separator
    let sep = commands
        .spawn((
            LegendContent,
            Node {
                width: Val::Percent(100.0),
                height: Val::Px(1.0),
                margin: UiRect::vertical(Val::Px(3.0)),
                ..default()
            },
            BackgroundColor(theme.border.with_alpha(0.4)),
        ))
        .id();
    commands.entity(legend_entity).add_child(sep);

    // Per-provider rows
    for (label, color, count) in &provider_counts {
        let display_name = match *label {
            "anthropic" => "claude",
            "google" => "gemini",
            "human" => "amy", // TODO: get from Identity when available
            l => l,
        };
        let row = spawn_legend_agent_row(
            &mut commands,
            display_name,
            *color,
            *count,
            &theme,
        );
        commands.entity(legend_entity).add_child(row);
    }

    // Bottom separator + drift count (only if there are staged drifts)
    if staged_count > 0 {
        let sep2 = commands
            .spawn((
                LegendContent,
                Node {
                    width: Val::Percent(100.0),
                    height: Val::Px(1.0),
                    margin: UiRect::vertical(Val::Px(3.0)),
                    ..default()
                },
                BackgroundColor(theme.border.with_alpha(0.4)),
            ))
            .id();
        commands.entity(legend_entity).add_child(sep2);

        let drift_text = format!("{} staged drifts", staged_count);
        let drift_entity = spawn_legend_text(&mut commands, &drift_text, theme.ansi.cyan, 9.0);
        commands.entity(legend_entity).add_child(drift_entity);
    }

    *last_fingerprint = fingerprint;
}

/// Spawn a simple text row for the legend panel.
fn spawn_legend_text(commands: &mut Commands, text: &str, color: Color, font_size: f32) -> Entity {
    commands
        .spawn((
            LegendContent,
            Node {
                min_height: Val::Px(font_size + 4.0),
                ..default()
            },
        ))
        .with_children(|parent| {
            parent.spawn((
                crate::text::MsdfUiText::new(text)
                    .with_font_size(font_size)
                    .with_color(color),
                crate::text::UiTextPositionCache::default(),
                // Explicit size needed — MsdfUiText doesn't participate in Bevy's
                // layout intrinsic sizing, so the node would compute as 0-width.
                Node {
                    width: Val::Percent(100.0),
                    height: Val::Px(font_size + 2.0),
                    ..default()
                },
            ));
        })
        .id()
}

/// Spawn an agent row with colored dot + name + count.
fn spawn_legend_agent_row(
    commands: &mut Commands,
    name: &str,
    color: Color,
    count: usize,
    theme: &Theme,
) -> Entity {
    commands
        .spawn((
            LegendContent,
            Node {
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                column_gap: Val::Px(6.0),
                min_height: Val::Px(16.0),
                ..default()
            },
        ))
        .with_children(|parent| {
            // Colored dot
            parent.spawn((
                Node {
                    width: Val::Px(8.0),
                    height: Val::Px(8.0),
                    border_radius: BorderRadius::all(Val::Px(4.0)),
                    ..default()
                },
                BackgroundColor(color),
            ));

            // Agent name
            parent.spawn((
                crate::text::MsdfUiText::new(name)
                    .with_font_size(10.0)
                    .with_color(color),
                crate::text::UiTextPositionCache::default(),
                Node {
                    width: Val::Px(70.0),
                    height: Val::Px(12.0),
                    ..default()
                },
            ));

            // Count (right-aligned)
            let count_text = format!("{} ctx", count);
            parent.spawn((
                crate::text::MsdfUiText::new(&count_text)
                    .with_font_size(9.0)
                    .with_color(theme.fg_dim),
                crate::text::UiTextPositionCache::default(),
                Node {
                    width: Val::Px(50.0),
                    height: Val::Px(11.0),
                    margin: UiRect::left(Val::Auto),
                    ..default()
                },
            ));
        })
        .id()
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
