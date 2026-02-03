//! Frame assembly - spawning and layout for 9-slice frames.
//!
//! NOTE: Frame spawning is currently disabled. ComposeBlock uses native Bevy UI
//! borders. The layout/update/visibility/cleanup systems remain for future use
//! if we re-enable custom frames.

use bevy::prelude::*;

use super::{Cell, CurrentMode, EditorMode, FocusTarget};
use crate::shaders::nine_slice::{
    CornerMarker, CornerMaterial, CornerPosition, EdgeMarker, EdgeMaterial, EdgePosition,
};
use crate::text::MsdfTextAreaConfig;
use crate::ui::state::AppScreen;
use crate::ui::theme::{color_to_vec4, Theme};

/// Padding around cells for the frame.
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

// ============================================================================
// SPAWN SYSTEM (DISABLED)
// ============================================================================

/// Spawns 9-slice frame entities for new cells.
/// Currently disabled - ComposeBlock uses native Bevy UI borders.
pub fn spawn_nine_slice_frames(
    _commands: Commands,
    _new_cells: Query<(Entity, &Cell, &MsdfTextAreaConfig), (Added<Cell>, Without<NineSliceFrame>)>,
    _theme: Res<Theme>,
    _screen: Res<State<AppScreen>>,
    _corner_materials: ResMut<Assets<CornerMaterial>>,
    _edge_materials: ResMut<Assets<EdgeMaterial>>,
) {
    // Frame spawning disabled - ComposeBlock uses Bevy UI borders
}

// ============================================================================
// LAYOUT SYSTEM
// ============================================================================

/// Updates frame piece positions when cell bounds change.
pub fn layout_nine_slice_frames(
    cells: Query<
        (&NineSliceFrame, &MsdfTextAreaConfig),
        Or<(Changed<MsdfTextAreaConfig>, Added<NineSliceFrame>)>,
    >,
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
                    CornerPosition::TopRight => (frame_left + frame_width - corner_size, frame_top),
                    CornerPosition::BottomLeft => (frame_left, frame_top + frame_height - corner_size),
                    CornerPosition::BottomRight => {
                        (frame_left + frame_width - corner_size, frame_top + frame_height - corner_size)
                    }
                };

                node.left = Val::Px(x);
                node.top = Val::Px(y);
                node.width = Val::Px(corner_size);
                node.height = Val::Px(corner_size);

                if let Ok(mat_node) = corner_entities.get(corner_entity)
                    && let Some(mat) = corner_materials.get_mut(&mat_node.0)
                {
                    mat.dimensions = Vec4::new(corner_size, corner_size, corner_size, 1.0);
                }
            }
        }

        // Update edges
        let edge_h_length = frame_width - corner_size * 2.0;
        let edge_v_length = frame_height - corner_size * 2.0;

        for edge_opt in &frame.edges {
            let Some(edge_entity) = edge_opt else { continue };

            if let Ok((pos, mut node)) = edges.get_mut(*edge_entity) {
                let (x, y, w, h, length) = match pos {
                    EdgePosition::Top => {
                        (frame_left + corner_size, frame_top, edge_h_length, edge_thickness, edge_h_length)
                    }
                    EdgePosition::Bottom => (
                        frame_left + corner_size,
                        frame_top + frame_height - edge_thickness,
                        edge_h_length,
                        edge_thickness,
                        edge_h_length,
                    ),
                    EdgePosition::Left => {
                        (frame_left, frame_top + corner_size, edge_thickness, edge_v_length, edge_v_length)
                    }
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

                if let Ok(mat_node) = edge_entities.get(*edge_entity)
                    && let Some(mat) = edge_materials.get_mut(&mat_node.0)
                {
                    mat.tile_info.z = length;
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
    focus: Res<FocusTarget>,
    mode: Res<CurrentMode>,
    cells: Query<(Entity, &NineSliceFrame, &Cell)>,
    theme: Res<Theme>,
    mut corner_materials: ResMut<Assets<CornerMaterial>>,
    mut edge_materials: ResMut<Assets<EdgeMaterial>>,
    corner_entities: Query<&MaterialNode<CornerMaterial>>,
    edge_entities: Query<&MaterialNode<EdgeMaterial>>,
) {
    if !focus.is_changed() && !mode.is_changed() {
        return;
    }

    for (entity, frame, _cell) in cells.iter() {
        let is_focused = focus.entity == Some(entity);

        let frame_color = if !is_focused {
            theme.frame_unfocused
        } else {
            match mode.0 {
                EditorMode::Normal => theme.frame_focused,
                EditorMode::Input(_) => theme.frame_insert,
                EditorMode::Visual => theme.frame_visual,
            }
        };

        let frame_params = if !is_focused {
            theme.frame_params_unfocused
        } else {
            theme.frame_params_focused
        };

        let final_color = color_to_vec4(frame_color);

        // Update corner materials
        for &corner_entity in &frame.corners {
            if let Ok(mat_node) = corner_entities.get(corner_entity)
                && let Some(mat) = corner_materials.get_mut(&mat_node.0)
            {
                mat.color = final_color;
                mat.params = frame_params;
            }
        }

        // Update edge materials
        let edge_base_color = color_to_vec4(theme.frame_edge);
        let final_edge_color = if !is_focused {
            edge_base_color * theme.frame_edge_dim_unfocused
        } else {
            final_color * theme.frame_edge_dim_focused
        };
        let final_edge_params = frame_params * Vec4::new(1.0, 0.8, 1.0, 1.0);

        for edge_opt in &frame.edges {
            let Some(edge_entity) = edge_opt else { continue };

            if let Ok(mat_node) = edge_entities.get(*edge_entity)
                && let Some(mat) = edge_materials.get_mut(&mat_node.0)
            {
                mat.color = final_edge_color;
                mat.params = final_edge_params;
            }
        }
    }
}

// ============================================================================
// VISIBILITY SYNC SYSTEM
// ============================================================================

use crate::ui::state::{InputPresence, InputPresenceKind};

/// Syncs frame visibility with AppScreen state and InputPresence.
pub fn sync_frame_visibility(
    screen: Res<State<AppScreen>>,
    presence: Res<InputPresence>,
    new_frames: Query<&NineSliceFrame, Added<NineSliceFrame>>,
    all_frames: Query<&NineSliceFrame>,
    mut corners: Query<&mut Visibility, With<CornerMarker>>,
    mut edges: Query<&mut Visibility, (With<EdgeMarker>, Without<CornerMarker>)>,
) {
    let target_visibility = match screen.get() {
        AppScreen::Dashboard => Visibility::Hidden,
        AppScreen::Conversation => match presence.0 {
            InputPresenceKind::Docked | InputPresenceKind::Overlay => Visibility::Inherited,
            InputPresenceKind::Minimized | InputPresenceKind::Hidden => Visibility::Hidden,
        },
    };

    let frames_to_update: Box<dyn Iterator<Item = &NineSliceFrame>> =
        if screen.is_changed() || presence.is_changed() {
            Box::new(all_frames.iter())
        } else if !new_frames.is_empty() {
            Box::new(new_frames.iter())
        } else {
            return;
        };

    for frame in frames_to_update {
        for &corner_entity in &frame.corners {
            if let Ok(mut vis) = corners.get_mut(corner_entity) {
                *vis = target_visibility;
            }
        }

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
            for &corner in &frame.corners {
                commands.entity(corner).try_despawn();
            }
            for edge in frame.edges.iter().flatten() {
                commands.entity(*edge).try_despawn();
            }
        }
    }
}
