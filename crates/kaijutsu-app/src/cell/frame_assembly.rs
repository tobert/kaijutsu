//! Frame assembly - spawning and layout for 9-slice frames.
//!
//! This module handles creating the multiple UI entities that make up a 9-slice
//! frame (4 corners + optional 4 edges) and keeping them positioned correctly
//! as cell bounds change.

use bevy::prelude::*;

use super::frame_style::{FrameStyle, FrameStyleMapping};
use super::{Cell, CurrentMode, EditorMode, FocusedCell};
use crate::shaders::nine_slice::{
    CornerMarker, CornerMaterial, CornerPosition, EdgeMarker, EdgeMaterial, EdgePosition,
    FramePiece,
};
use crate::text::TextAreaConfig;

/// Padding around cells for the frame (matches CELL_FRAME_PADDING in systems.rs)
pub const FRAME_PADDING: f32 = 24.0;

// ============================================================================
// FRAME ENTITY TRACKING
// ============================================================================

/// Tracks all entities that make up a cell's 9-slice frame.
#[derive(Component, Debug)]
pub struct NineSliceFrame {
    /// Corner entities (top-left, top-right, bottom-left, bottom-right)
    pub corners: [Entity; 4],
    /// Edge entities (top, bottom, left, right) - may be None
    pub edges: [Option<Entity>; 4],
    /// Handle to the frame style being used
    pub style: Handle<FrameStyle>,
}

/// Marker for cells using the 9-slice frame system
#[derive(Component, Debug)]
pub struct NineSliceMarker;

// ============================================================================
// SPAWN SYSTEM
// ============================================================================

/// Spawns 9-slice frame entities for new cells.
pub fn spawn_nine_slice_frames(
    mut commands: Commands,
    new_cells: Query<(Entity, &Cell, &TextAreaConfig), (Added<Cell>, Without<NineSliceFrame>)>,
    style_mapping: Option<Res<FrameStyleMapping>>,
    styles: Res<Assets<FrameStyle>>,
    mut corner_materials: ResMut<Assets<CornerMaterial>>,
    mut edge_materials: ResMut<Assets<EdgeMaterial>>,
) {
    let Some(mapping) = style_mapping else {
        return;
    };

    for (cell_entity, cell, text_config) in new_cells.iter() {
        let style_handle = mapping.style_for(cell.kind);
        let Some(style) = styles.get(&style_handle) else {
            warn!("Frame style not loaded yet for cell {:?}", cell.kind);
            continue;
        };

        // Calculate frame bounds (TextBounds uses i32, convert to f32)
        // Use saturating_sub to prevent overflow when bounds are invalid (right < left)
        let bounds = &text_config.bounds;
        let raw_width = bounds.right.saturating_sub(bounds.left);
        let raw_height = bounds.bottom.saturating_sub(bounds.top);

        // Skip cells with invalid or zero-size bounds (not yet laid out)
        if raw_width <= 0 || raw_height <= 0 {
            continue;
        }

        let frame_left = bounds.left as f32 - FRAME_PADDING;
        let frame_top = bounds.top as f32 - FRAME_PADDING;
        let frame_width = raw_width as f32 + FRAME_PADDING * 2.0;
        let frame_height = raw_height as f32 + FRAME_PADDING * 2.0;

        let corner_size = style.corner_size;
        let edge_thickness = style.edge_thickness;

        // Spawn corners
        let corners = spawn_corners(
            &mut commands,
            cell_entity,
            style,
            &mut corner_materials,
            frame_left,
            frame_top,
            frame_width,
            frame_height,
            corner_size,
        );

        // Spawn edges (if defined in style)
        let edges = spawn_edges(
            &mut commands,
            cell_entity,
            style,
            &mut edge_materials,
            frame_left,
            frame_top,
            frame_width,
            frame_height,
            corner_size,
            edge_thickness,
        );

        // Add frame tracking component to cell
        commands.entity(cell_entity).insert((
            NineSliceFrame {
                corners,
                edges,
                style: style_handle.clone(),
            },
            NineSliceMarker,
        ));
    }
}

/// Spawn the four corner entities.
fn spawn_corners(
    commands: &mut Commands,
    _cell_entity: Entity,
    style: &FrameStyle,
    corner_materials: &mut Assets<CornerMaterial>,
    frame_left: f32,
    frame_top: f32,
    frame_width: f32,
    frame_height: f32,
    corner_size: f32,
) -> [Entity; 4] {
    let corner_def = &style.corner;
    let color = corner_def.color_vec4();
    let params = corner_def.params_vec4();

    let positions = [
        (CornerPosition::TopLeft, false, false, frame_left, frame_top),
        (
            CornerPosition::TopRight,
            true,
            false,
            frame_left + frame_width - corner_size,
            frame_top,
        ),
        (
            CornerPosition::BottomLeft,
            false,
            true,
            frame_left,
            frame_top + frame_height - corner_size,
        ),
        (
            CornerPosition::BottomRight,
            true,
            true,
            frame_left + frame_width - corner_size,
            frame_top + frame_height - corner_size,
        ),
    ];

    let mut corners = [Entity::PLACEHOLDER; 4];

    for (i, (pos, flip_x, flip_y, x, y)) in positions.into_iter().enumerate() {
        let material = corner_materials.add(
            CornerMaterial {
                color,
                params,
                time: Vec4::ZERO,
                flip: Vec4::new(
                    if flip_x { 1.0 } else { 0.0 },
                    if flip_y { 1.0 } else { 0.0 },
                    0.0,
                    0.0,
                ),
                dimensions: Vec4::new(corner_size, corner_size, corner_size, 1.0),
            }
            .with_dimensions(corner_size, corner_size, corner_size),
        );

        let entity = commands
            .spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(x),
                    top: Val::Px(y),
                    width: Val::Px(corner_size),
                    height: Val::Px(corner_size),
                    ..default()
                },
                MaterialNode(material),
                ZIndex(-1),
                FramePiece,
                CornerMarker,
                pos,
                Name::new(format!("Corner_{:?}", pos)),
            ))
            .id();

        corners[i] = entity;
    }

    corners
}

/// Spawn edge entities (if defined in style).
fn spawn_edges(
    commands: &mut Commands,
    _cell_entity: Entity,
    style: &FrameStyle,
    edge_materials: &mut Assets<EdgeMaterial>,
    frame_left: f32,
    frame_top: f32,
    frame_width: f32,
    frame_height: f32,
    corner_size: f32,
    edge_thickness: f32,
) -> [Option<Entity>; 4] {
    let mut edges = [None; 4];

    // Horizontal edges (top, bottom)
    if let Some(edge_h) = &style.edge_h {
        let shader_def = &edge_h.shader;
        let color = shader_def.color_vec4();
        let params = shader_def.params_vec4();
        let (tile_size, tile_mode) = edge_h.mode.tile_info();
        let edge_length = frame_width - corner_size * 2.0;

        // Top edge
        {
            let material = edge_materials.add(EdgeMaterial {
                color,
                params,
                time: Vec4::ZERO,
                tile_info: Vec4::new(tile_size, tile_mode, edge_length, edge_thickness),
                orientation: Vec4::ZERO, // Horizontal
            });

            let entity = commands
                .spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: Val::Px(frame_left + corner_size),
                        top: Val::Px(frame_top),
                        width: Val::Px(edge_length),
                        height: Val::Px(edge_thickness),
                        ..default()
                    },
                    MaterialNode(material),
                    ZIndex(-1),
                    FramePiece,
                    EdgeMarker,
                    EdgePosition::Top,
                    Name::new("Edge_Top"),
                ))
                .id();

            edges[0] = Some(entity);
        }

        // Bottom edge
        {
            let material = edge_materials.add(EdgeMaterial {
                color,
                params,
                time: Vec4::ZERO,
                tile_info: Vec4::new(tile_size, tile_mode, edge_length, edge_thickness),
                orientation: Vec4::ZERO, // Horizontal
            });

            let entity = commands
                .spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: Val::Px(frame_left + corner_size),
                        top: Val::Px(frame_top + frame_height - edge_thickness),
                        width: Val::Px(edge_length),
                        height: Val::Px(edge_thickness),
                        ..default()
                    },
                    MaterialNode(material),
                    ZIndex(-1),
                    FramePiece,
                    EdgeMarker,
                    EdgePosition::Bottom,
                    Name::new("Edge_Bottom"),
                ))
                .id();

            edges[1] = Some(entity);
        }
    }

    // Vertical edges (left, right)
    if let Some(edge_v) = &style.edge_v {
        let shader_def = &edge_v.shader;
        let color = shader_def.color_vec4();
        let params = shader_def.params_vec4();
        let (tile_size, tile_mode) = edge_v.mode.tile_info();
        let edge_length = frame_height - corner_size * 2.0;

        // Left edge
        {
            let material = edge_materials.add(EdgeMaterial {
                color,
                params,
                time: Vec4::ZERO,
                tile_info: Vec4::new(tile_size, tile_mode, edge_length, edge_thickness),
                orientation: Vec4::new(1.0, 0.0, 0.0, 0.0), // Vertical
            });

            let entity = commands
                .spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: Val::Px(frame_left),
                        top: Val::Px(frame_top + corner_size),
                        width: Val::Px(edge_thickness),
                        height: Val::Px(edge_length),
                        ..default()
                    },
                    MaterialNode(material),
                    ZIndex(-1),
                    FramePiece,
                    EdgeMarker,
                    EdgePosition::Left,
                    Name::new("Edge_Left"),
                ))
                .id();

            edges[2] = Some(entity);
        }

        // Right edge
        {
            let material = edge_materials.add(EdgeMaterial {
                color,
                params,
                time: Vec4::ZERO,
                tile_info: Vec4::new(tile_size, tile_mode, edge_length, edge_thickness),
                orientation: Vec4::new(1.0, 0.0, 0.0, 0.0), // Vertical
            });

            let entity = commands
                .spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: Val::Px(frame_left + frame_width - edge_thickness),
                        top: Val::Px(frame_top + corner_size),
                        width: Val::Px(edge_thickness),
                        height: Val::Px(edge_length),
                        ..default()
                    },
                    MaterialNode(material),
                    ZIndex(-1),
                    FramePiece,
                    EdgeMarker,
                    EdgePosition::Right,
                    Name::new("Edge_Right"),
                ))
                .id();

            edges[3] = Some(entity);
        }
    }

    edges
}

// ============================================================================
// LAYOUT SYSTEM
// ============================================================================

/// Updates frame piece positions when cell bounds change.
///
/// Note: We use `Or<(Changed<TextAreaConfig>, Added<NineSliceFrame>)>` to handle both:
/// 1. Normal updates when bounds change
/// 2. First-frame updates after frame is spawned (commands are deferred, so
///    Changed<TextAreaConfig> from the same frame won't be visible yet)
pub fn layout_nine_slice_frames(
    cells: Query<(&NineSliceFrame, &TextAreaConfig), Or<(Changed<TextAreaConfig>, Added<NineSliceFrame>)>>,
    styles: Res<Assets<FrameStyle>>,
    mut corners: Query<(&CornerPosition, &mut Node), With<CornerMarker>>,
    mut edges: Query<(&EdgePosition, &mut Node), (With<EdgeMarker>, Without<CornerMarker>)>,
    mut corner_materials: ResMut<Assets<CornerMaterial>>,
    mut edge_materials: ResMut<Assets<EdgeMaterial>>,
    corner_entities: Query<&MaterialNode<CornerMaterial>>,
    edge_entities: Query<&MaterialNode<EdgeMaterial>>,
) {
    for (frame, text_config) in cells.iter() {
        let Some(style) = styles.get(&frame.style) else {
            continue;
        };

        let bounds = &text_config.bounds;
        let raw_width = bounds.right.saturating_sub(bounds.left);
        let raw_height = bounds.bottom.saturating_sub(bounds.top);

        // Skip cells with invalid or zero-size bounds (not yet laid out)
        if raw_width <= 0 || raw_height <= 0 {
            continue;
        }

        let frame_left = bounds.left as f32 - FRAME_PADDING;
        let frame_top = bounds.top as f32 - FRAME_PADDING;
        let frame_width = raw_width as f32 + FRAME_PADDING * 2.0;
        let frame_height = raw_height as f32 + FRAME_PADDING * 2.0;

        let corner_size = style.corner_size;
        let edge_thickness = style.edge_thickness;

        // Update corners
        for &corner_entity in &frame.corners {
            if let Ok((pos, mut node)) = corners.get_mut(corner_entity) {
                let (x, y) = match pos {
                    CornerPosition::TopLeft => (frame_left, frame_top),
                    CornerPosition::TopRight => {
                        (frame_left + frame_width - corner_size, frame_top)
                    }
                    CornerPosition::BottomLeft => {
                        (frame_left, frame_top + frame_height - corner_size)
                    }
                    CornerPosition::BottomRight => (
                        frame_left + frame_width - corner_size,
                        frame_top + frame_height - corner_size,
                    ),
                };

                node.left = Val::Px(x);
                node.top = Val::Px(y);
                node.width = Val::Px(corner_size);
                node.height = Val::Px(corner_size);

                // Update material dimensions
                if let Ok(mat_node) = corner_entities.get(corner_entity) {
                    if let Some(mat) = corner_materials.get_mut(&mat_node.0) {
                        mat.dimensions = Vec4::new(corner_size, corner_size, corner_size, 1.0);
                    }
                }
            }
        }

        // Update edges
        let edge_h_length = frame_width - corner_size * 2.0;
        let edge_v_length = frame_height - corner_size * 2.0;

        for edge_opt in &frame.edges {
            let Some(edge_entity) = edge_opt else {
                continue;
            };

            if let Ok((pos, mut node)) = edges.get_mut(*edge_entity) {
                let (x, y, w, h, length) = match pos {
                    EdgePosition::Top => (
                        frame_left + corner_size,
                        frame_top,
                        edge_h_length,
                        edge_thickness,
                        edge_h_length,
                    ),
                    EdgePosition::Bottom => (
                        frame_left + corner_size,
                        frame_top + frame_height - edge_thickness,
                        edge_h_length,
                        edge_thickness,
                        edge_h_length,
                    ),
                    EdgePosition::Left => (
                        frame_left,
                        frame_top + corner_size,
                        edge_thickness,
                        edge_v_length,
                        edge_v_length,
                    ),
                    EdgePosition::Right => (
                        frame_left + frame_width - edge_thickness,
                        frame_top + corner_size,
                        edge_thickness,
                        edge_v_length,
                        edge_v_length,
                    ),
                };

                node.left = Val::Px(x);
                node.top = Val::Px(y);
                node.width = Val::Px(w);
                node.height = Val::Px(h);

                // Update material tile_info with new length
                if let Ok(mat_node) = edge_entities.get(*edge_entity) {
                    if let Some(mat) = edge_materials.get_mut(&mat_node.0) {
                        mat.tile_info.z = length; // Update length
                    }
                }
            }
        }
    }
}

// ============================================================================
// STATE UPDATE SYSTEM
// ============================================================================

/// Updates frame colors/params based on focus and mode state.
pub fn update_nine_slice_state(
    focused: Res<FocusedCell>,
    mode: Res<CurrentMode>,
    cells: Query<(Entity, &NineSliceFrame, &Cell)>,
    styles: Res<Assets<FrameStyle>>,
    mut corner_materials: ResMut<Assets<CornerMaterial>>,
    mut edge_materials: ResMut<Assets<EdgeMaterial>>,
    corner_entities: Query<&MaterialNode<CornerMaterial>>,
    edge_entities: Query<&MaterialNode<EdgeMaterial>>,
) {
    // Only run when focus or mode changes
    if !focused.is_changed() && !mode.is_changed() {
        return;
    }

    for (entity, frame, _cell) in cells.iter() {
        let Some(style) = styles.get(&frame.style) else {
            continue;
        };

        let is_focused = focused.0 == Some(entity);

        // Determine which state override to apply
        let state_key = if is_focused {
            match mode.0 {
                EditorMode::Insert => "insert",
                EditorMode::Command => "command",
                EditorMode::Visual => "visual",
                EditorMode::Normal => "focused",
            }
        } else {
            "unfocused"
        };

        let override_opt = style.states.get(state_key);

        // Calculate final color and params
        let base_color = style.corner.color_vec4();
        let base_params = style.corner.params_vec4();

        let (final_color, final_params) = if let Some(ovr) = override_opt {
            (ovr.apply_color(base_color), ovr.apply_params(base_params))
        } else if is_focused {
            // Default focused: brighter
            (base_color * 1.2, base_params)
        } else {
            // Default unfocused: dimmer
            (base_color * 0.6, base_params * Vec4::new(1.0, 0.6, 0.5, 1.0))
        };

        // Update corner materials
        for &corner_entity in &frame.corners {
            if let Ok(mat_node) = corner_entities.get(corner_entity) {
                if let Some(mat) = corner_materials.get_mut(&mat_node.0) {
                    mat.color = final_color;
                    mat.params = final_params;
                }
            }
        }

        // Update edge materials (with edge-specific color from style)
        let edge_color = if let Some(edge_h) = &style.edge_h {
            edge_h.shader.color_vec4()
        } else {
            base_color * 0.8
        };

        let (final_edge_color, final_edge_params) = if let Some(ovr) = override_opt {
            (
                ovr.apply_color(edge_color),
                ovr.apply_params(base_params * 0.8),
            )
        } else if is_focused {
            (edge_color * 1.1, base_params * 0.8)
        } else {
            (edge_color * 0.5, base_params * Vec4::new(1.0, 0.5, 0.4, 1.0))
        };

        for edge_opt in &frame.edges {
            let Some(edge_entity) = edge_opt else {
                continue;
            };

            if let Ok(mat_node) = edge_entities.get(*edge_entity) {
                if let Some(mat) = edge_materials.get_mut(&mat_node.0) {
                    mat.color = final_edge_color;
                    mat.params = final_edge_params;
                }
            }
        }
    }
}

// ============================================================================
// CLEANUP SYSTEM
// ============================================================================

/// Despawns frame pieces when cells are removed.
pub fn cleanup_nine_slice_frames(
    mut commands: Commands,
    mut removed: RemovedComponents<Cell>,
    frames: Query<&NineSliceFrame>,
) {
    for entity in removed.read() {
        if let Ok(frame) = frames.get(entity) {
            // Despawn all corners
            for &corner in &frame.corners {
                commands.entity(corner).try_despawn();
            }

            // Despawn all edges
            for edge_opt in &frame.edges {
                if let Some(edge) = edge_opt {
                    commands.entity(*edge).try_despawn();
                }
            }
        }
    }
}

