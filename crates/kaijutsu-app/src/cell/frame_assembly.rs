//! Frame assembly - spawning and layout for 9-slice frames.
//!
//! This module handles creating the multiple UI entities that make up a 9-slice
//! frame (4 corners + optional 4 edges) and keeping them positioned correctly
//! as cell bounds change.
//!
//! Frame configuration (colors, sizes, shader params) comes from the Theme resource,
//! loaded from ~/.config/kaijutsu/theme.rhai.

use bevy::prelude::*;

use super::components::PromptCell;
use super::{Cell, CurrentMode, EditorMode, FocusedCell};
use crate::shaders::nine_slice::{
    CornerMarker, CornerMaterial, CornerPosition, EdgeMarker, EdgeMaterial, EdgePosition,
    FramePiece,
};
use crate::text::TextAreaConfig;
use crate::ui::state::AppScreen;
use crate::ui::theme::{color_to_vec4, Theme};

/// Padding around cells for the frame.
/// Reduced to 20.0 to match workspace_margin_left and prevent frames extending off-screen.
pub const FRAME_PADDING: f32 = 20.0;

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
}

/// Marker for cells using the 9-slice frame system
#[derive(Component, Debug)]
pub struct NineSliceMarker;

// ============================================================================
// SPAWN SYSTEM
// ============================================================================

/// Spawns 9-slice frame entities for new cells.
/// Currently only spawns for PromptCell - MainCell fills the screen and other cells
/// are rendered inline within the MainCell's content.
pub fn spawn_nine_slice_frames(
    mut commands: Commands,
    new_cells: Query<
        (Entity, &Cell, &TextAreaConfig),
        (Added<Cell>, Without<NineSliceFrame>, With<PromptCell>),
    >,
    theme: Res<Theme>,
    screen: Res<State<AppScreen>>,
    mut corner_materials: ResMut<Assets<CornerMaterial>>,
    mut edge_materials: ResMut<Assets<EdgeMaterial>>,
) {
    // Determine initial visibility based on current screen state
    let initial_visibility = match screen.get() {
        AppScreen::Dashboard => Visibility::Hidden,
        AppScreen::Conversation => Visibility::Inherited,
    };
    for (cell_entity, _cell, text_config) in new_cells.iter() {
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

        let corner_size = theme.frame_corner_size;
        let edge_thickness = theme.frame_edge_thickness;

        // Spawn corners using theme colors
        let corners = spawn_corners(
            &mut commands,
            &theme,
            &mut corner_materials,
            frame_left,
            frame_top,
            frame_width,
            frame_height,
            corner_size,
            initial_visibility,
        );

        // Spawn edges using theme colors
        let edges = spawn_edges(
            &mut commands,
            &theme,
            &mut edge_materials,
            frame_left,
            frame_top,
            frame_width,
            frame_height,
            corner_size,
            edge_thickness,
            initial_visibility,
        );

        // Add frame tracking component to cell
        commands.entity(cell_entity).insert((
            NineSliceFrame { corners, edges },
            NineSliceMarker,
        ));
    }
}

/// Spawn the four corner entities.
fn spawn_corners(
    commands: &mut Commands,
    theme: &Theme,
    corner_materials: &mut Assets<CornerMaterial>,
    frame_left: f32,
    frame_top: f32,
    frame_width: f32,
    frame_height: f32,
    corner_size: f32,
    initial_visibility: Visibility,
) -> [Entity; 4] {
    let color = color_to_vec4(theme.frame_base);
    let params = theme.frame_params_base;

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
                initial_visibility,
                Name::new(format!("Corner_{:?}", pos)),
            ))
            .id();

        corners[i] = entity;
    }

    corners
}

/// Spawn edge entities using theme colors.
fn spawn_edges(
    commands: &mut Commands,
    theme: &Theme,
    edge_materials: &mut Assets<EdgeMaterial>,
    frame_left: f32,
    frame_top: f32,
    frame_width: f32,
    frame_height: f32,
    corner_size: f32,
    edge_thickness: f32,
    initial_visibility: Visibility,
) -> [Option<Entity>; 4] {
    let mut edges = [None; 4];

    // Use theme edge color and base params (dimmed)
    let color = color_to_vec4(theme.frame_edge);
    let params = theme.frame_params_base * Vec4::new(1.0, 0.8, 1.0, 1.0); // Slightly dimmer
    let tile_size = 24.0; // Default tile size
    let tile_mode = 1.0; // Tiling mode

    // Horizontal edges (top, bottom)
    let edge_h_length = frame_width - corner_size * 2.0;

    // Top edge
    {
        let material = edge_materials.add(EdgeMaterial {
            color,
            params,
            time: Vec4::ZERO,
            tile_info: Vec4::new(tile_size, tile_mode, edge_h_length, edge_thickness),
            orientation: Vec4::ZERO, // Horizontal
        });

        let entity = commands
            .spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(frame_left + corner_size),
                    top: Val::Px(frame_top),
                    width: Val::Px(edge_h_length),
                    height: Val::Px(edge_thickness),
                    ..default()
                },
                MaterialNode(material),
                ZIndex(-1),
                FramePiece,
                EdgeMarker,
                EdgePosition::Top,
                initial_visibility,
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
            tile_info: Vec4::new(tile_size, tile_mode, edge_h_length, edge_thickness),
            orientation: Vec4::ZERO, // Horizontal
        });

        let entity = commands
            .spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(frame_left + corner_size),
                    top: Val::Px(frame_top + frame_height - edge_thickness),
                    width: Val::Px(edge_h_length),
                    height: Val::Px(edge_thickness),
                    ..default()
                },
                MaterialNode(material),
                ZIndex(-1),
                FramePiece,
                EdgeMarker,
                EdgePosition::Bottom,
                initial_visibility,
                Name::new("Edge_Bottom"),
            ))
            .id();

        edges[1] = Some(entity);
    }

    // Vertical edges (left, right)
    let edge_v_length = frame_height - corner_size * 2.0;

    // Left edge
    {
        let material = edge_materials.add(EdgeMaterial {
            color,
            params,
            time: Vec4::ZERO,
            tile_info: Vec4::new(tile_size, tile_mode, edge_v_length, edge_thickness),
            orientation: Vec4::new(1.0, 0.0, 0.0, 0.0), // Vertical
        });

        let entity = commands
            .spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(frame_left),
                    top: Val::Px(frame_top + corner_size),
                    width: Val::Px(edge_thickness),
                    height: Val::Px(edge_v_length),
                    ..default()
                },
                MaterialNode(material),
                ZIndex(-1),
                FramePiece,
                EdgeMarker,
                EdgePosition::Left,
                initial_visibility,
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
            tile_info: Vec4::new(tile_size, tile_mode, edge_v_length, edge_thickness),
            orientation: Vec4::new(1.0, 0.0, 0.0, 0.0), // Vertical
        });

        let entity = commands
            .spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(frame_left + frame_width - edge_thickness),
                    top: Val::Px(frame_top + corner_size),
                    width: Val::Px(edge_thickness),
                    height: Val::Px(edge_v_length),
                    ..default()
                },
                MaterialNode(material),
                ZIndex(-1),
                FramePiece,
                EdgeMarker,
                EdgePosition::Right,
                initial_visibility,
                Name::new("Edge_Right"),
            ))
            .id();

        edges[3] = Some(entity);
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
    theme: Res<Theme>,
    mut corners: Query<(&CornerPosition, &mut Node), With<CornerMarker>>,
    mut edges: Query<(&EdgePosition, &mut Node), (With<EdgeMarker>, Without<CornerMarker>)>,
    mut corner_materials: ResMut<Assets<CornerMaterial>>,
    mut edge_materials: ResMut<Assets<EdgeMaterial>>,
    corner_entities: Query<&MaterialNode<CornerMaterial>>,
    edge_entities: Query<&MaterialNode<EdgeMaterial>>,
) {
    let corner_size = theme.frame_corner_size;
    let edge_thickness = theme.frame_edge_thickness;

    for (frame, text_config) in cells.iter() {
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
                if let Ok(mat_node) = corner_entities.get(corner_entity)
                    && let Some(mat) = corner_materials.get_mut(&mat_node.0) {
                        mat.dimensions = Vec4::new(corner_size, corner_size, corner_size, 1.0);
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
                if let Ok(mat_node) = edge_entities.get(*edge_entity)
                    && let Some(mat) = edge_materials.get_mut(&mat_node.0) {
                        mat.tile_info.z = length; // Update length
                    }
            }
        }
    }
}

// ============================================================================
// STATE UPDATE SYSTEM
// ============================================================================

/// Updates frame colors/params based on focus and mode state.
///
/// Maps editor mode to theme colors:
/// - Unfocused → theme.frame_unfocused
/// - Normal (focused) → theme.frame_focused
/// - Insert → theme.frame_insert
/// - Command → theme.frame_command
/// - Visual → theme.frame_visual
pub fn update_nine_slice_state(
    focused: Res<FocusedCell>,
    mode: Res<CurrentMode>,
    cells: Query<(Entity, &NineSliceFrame, &Cell)>,
    theme: Res<Theme>,
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
        let is_focused = focused.0 == Some(entity);

        // Map focus + mode to theme color
        let frame_color = if !is_focused {
            theme.frame_unfocused
        } else {
            match mode.0 {
                EditorMode::Normal => theme.frame_focused,
                EditorMode::Insert => theme.frame_insert,
                EditorMode::Command => theme.frame_command,
                EditorMode::Visual => theme.frame_visual,
            }
        };

        // Map to shader params
        let frame_params = if !is_focused {
            theme.frame_params_unfocused
        } else {
            theme.frame_params_focused
        };

        let final_color = color_to_vec4(frame_color);
        let final_params = frame_params;

        // Update corner materials
        for &corner_entity in &frame.corners {
            if let Ok(mat_node) = corner_entities.get(corner_entity)
                && let Some(mat) = corner_materials.get_mut(&mat_node.0) {
                    mat.color = final_color;
                    mat.params = final_params;
                }
        }

        // Update edge materials (apply theme dimming multipliers)
        let edge_base_color = color_to_vec4(theme.frame_edge);
        let final_edge_color = if !is_focused {
            edge_base_color * theme.frame_edge_dim_unfocused
        } else {
            // Tint edges with the mode color for cohesion
            final_color * theme.frame_edge_dim_focused
        };
        let final_edge_params = final_params * Vec4::new(1.0, 0.8, 1.0, 1.0);

        for edge_opt in &frame.edges {
            let Some(edge_entity) = edge_opt else {
                continue;
            };

            if let Ok(mat_node) = edge_entities.get(*edge_entity)
                && let Some(mat) = edge_materials.get_mut(&mat_node.0) {
                    mat.color = final_edge_color;
                    mat.params = final_edge_params;
                }
        }
    }
}

// ============================================================================
// VISIBILITY SYNC SYSTEM
// ============================================================================

/// Syncs frame visibility with AppScreen state.
///
/// Frames are spawned at world level with absolute positioning, so they don't
/// inherit visibility from ConversationRoot. This system hides them when
/// we're in Dashboard state.
///
/// Runs when:
/// - AppScreen state changes
/// - New frames are spawned (Added<NineSliceFrame>)
pub fn sync_frame_visibility(
    screen: Res<State<AppScreen>>,
    new_frames: Query<&NineSliceFrame, Added<NineSliceFrame>>,
    all_frames: Query<&NineSliceFrame>,
    mut corners: Query<&mut Visibility, With<CornerMarker>>,
    mut edges: Query<&mut Visibility, (With<EdgeMarker>, Without<CornerMarker>)>,
) {
    let target_visibility = match screen.get() {
        AppScreen::Dashboard => Visibility::Hidden,
        AppScreen::Conversation => Visibility::Inherited,
    };

    // Determine which frames need updating
    let frames_to_update: Box<dyn Iterator<Item = &NineSliceFrame>> = if screen.is_changed() {
        // State changed - update all frames
        Box::new(all_frames.iter())
    } else if !new_frames.is_empty() {
        // New frames spawned - only update those
        Box::new(new_frames.iter())
    } else {
        // Nothing to do
        return;
    };

    for frame in frames_to_update {
        // Update corners
        for &corner_entity in &frame.corners {
            if let Ok(mut vis) = corners.get_mut(corner_entity) {
                *vis = target_visibility;
            }
        }

        // Update edges
        for edge_opt in &frame.edges {
            if let Some(edge_entity) = edge_opt {
                if let Ok(mut vis) = edges.get_mut(*edge_entity) {
                    *vis = target_visibility;
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
            for edge in frame.edges.iter().flatten() {
                commands.entity(*edge).try_despawn();
            }
        }
    }
}

