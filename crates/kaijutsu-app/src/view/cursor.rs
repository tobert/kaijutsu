//! Cursor systems — spawn, position, and blink the editing cursor.
//!
//! Currently uses CursorBeamMaterial (shader-based). Future: Vello-drawn cursor.

use bevy::prelude::*;
use bevy::math::Vec4;

use crate::cell::{
    BlockCell, BlockCellLayout, BlockEditCursor, CellEditor, EditingBlockCell,
    EditorEntities, FocusTarget, InputOverlay, InputOverlayMarker, MainCell,
};
use crate::input::FocusArea;
use crate::shaders::{CursorBeamMaterial, CursorMode};
use crate::text::TextMetrics;
use crate::ui::theme::Theme;

/// Marker component for the cursor UI entity.
#[derive(Component)]
pub struct CursorMarker;

/// Calculate row and column from text and cursor offset.
///
/// This is the shared helper for cursor positioning. O(N) string scan
/// but only runs when cursor position changes (caching at call sites).
#[inline]
pub fn cursor_row_col(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let row = before.matches('\n').count();
    let col = match before.rfind('\n') {
        Some(pos) => before[pos + 1..].chars().count(),
        None => before.chars().count(),
    };
    (row, col)
}

/// Spawn the cursor entity if it doesn't exist.
pub fn spawn_cursor(
    mut commands: Commands,
    mut entities: ResMut<EditorEntities>,
    mut cursor_materials: ResMut<Assets<CursorBeamMaterial>>,
    theme: Res<Theme>,
    text_metrics: Res<TextMetrics>,
) {
    if entities.cursor.is_some() {
        return;
    }

    let color = theme.cursor_normal;

    let material = cursor_materials.add(CursorBeamMaterial {
        color,
        params: Vec4::new(0.25, 1.2, 2.0, 0.0),
        time: Vec4::new(0.0, CursorMode::Block as u8 as f32, 0.0, 0.0),
    });

    let char_width = text_metrics.cell_char_width + text_metrics.letter_spacing;
    let line_height = text_metrics.cell_line_height;

    let entity = commands
        .spawn((
            CursorMarker,
            Node {
                position_type: PositionType::Absolute,
                width: Val::Px(char_width + 8.0),
                height: Val::Px(line_height + 4.0),
                ..default()
            },
            BackgroundColor(Color::NONE),
            MaterialNode(material),
            ZIndex(crate::constants::ZLayer::CURSOR),
            Visibility::Hidden,
        ))
        .id();

    entities.cursor = Some(entity);
    info!("Spawned cursor entity");
}

/// Update cursor position and visibility based on focused cell and focus area.
pub fn update_cursor(
    focus: Res<FocusTarget>,
    focus_area: Res<FocusArea>,
    entities: Res<EditorEntities>,
    mut cells: Query<(&mut CellEditor, &ComputedNode, &UiGlobalTransform)>,
    mut cursor_query: Query<(&mut Node, &mut Visibility, &MaterialNode<CursorBeamMaterial>), With<CursorMarker>>,
    mut cursor_materials: ResMut<Assets<CursorBeamMaterial>>,
    theme: Res<Theme>,
    text_metrics: Res<TextMetrics>,
) {
    let Some(cursor_ent) = entities.cursor else {
        return;
    };

    let Ok((mut node, mut visibility, material_node)) = cursor_query.get_mut(cursor_ent) else {
        return;
    };

    let Some(focused_entity) = focus.entity else {
        *visibility = Visibility::Hidden;
        return;
    };

    let Ok((mut editor, computed, transform)) = cells.get_mut(focused_entity) else {
        *visibility = Visibility::Hidden;
        return;
    };

    *visibility = Visibility::Inherited;

    let (row, col) = cursor_position(&mut editor);

    let (_, _, translation) = transform.to_scale_angle_translation();
    let node_size = computed.size();
    let cell_left = translation.x - node_size.x / 2.0;
    let cell_top = translation.y - node_size.y / 2.0;

    let char_width = text_metrics.cell_char_width + text_metrics.letter_spacing;
    let line_height = text_metrics.cell_line_height;
    // Round to integer pixels to prevent subpixel jitter from float drift.
    // Beam draws at uv.x=0.3 within node width (char_width + 8.0); subtract to align.
    let node_width = char_width + 8.0;
    let beam_frac = 0.3_f32;
    let x = (cell_left + col as f32 * char_width - beam_frac * node_width).round();
    let y = (cell_top + (row as f32 * line_height)).round();

    let target_left = Val::Px(x);
    let target_top = Val::Px(y);
    if node.left != target_left {
        node.left = target_left;
    }
    if node.top != target_top {
        node.top = target_top;
    }

    let (cursor_mode, color, params) = if focus_area.is_text_input() {
        (CursorMode::Beam, theme.cursor_insert, Vec4::new(0.25, 1.2, 2.0, 0.0))
    } else {
        (CursorMode::Block, theme.cursor_normal, Vec4::new(0.2, 1.0, 1.5, 0.6))
    };
    if let Some(material) = cursor_materials.get_mut(&material_node.0) {
        let mode_f = cursor_mode as u8 as f32;
        if material.time.y != mode_f || material.color != color {
            material.time.y = mode_f;
            material.color = color;
            material.params = params;
        }
    }
}

/// Calculate cursor row and column (cached).
fn cursor_position(editor: &mut CellEditor) -> (usize, usize) {
    let current_version = editor.version();

    if editor.cursor_cache.version == current_version {
        return (editor.cursor_cache.row, editor.cursor_cache.col);
    }

    let (row, col) = compute_cursor_position(editor);
    editor.cursor_cache.row = row;
    editor.cursor_cache.col = col;
    editor.cursor_cache.version = current_version;

    (row, col)
}

/// Compute cursor position by walking blocks (O(N) string scan).
fn compute_cursor_position(editor: &CellEditor) -> (usize, usize) {
    let Some(ref cursor_block_id) = editor.cursor.block_id else {
        return (0, 0);
    };

    let blocks = editor.store.blocks_ordered();
    let mut row = 0;

    for (i, block) in blocks.iter().enumerate() {
        let text = &block.content;

        if &block.id == cursor_block_id {
            let (block_row, col) = cursor_row_col(text, editor.cursor.offset);
            return (row + block_row, col);
        }

        row += text.matches('\n').count();

        if i < blocks.len() - 1 {
            row += 2;
        }
    }

    (row, 0)
}

/// Update cursor rendering for editing BlockCell.
pub fn update_block_edit_cursor(
    editing_cells: Query<(&BlockCell, &BlockEditCursor, &BlockCellLayout, &ComputedNode, &UiGlobalTransform), With<EditingBlockCell>>,
    entities: Res<EditorEntities>,
    focus_area: Res<FocusArea>,
    mut cursor_query: Query<(&mut Node, &mut Visibility), With<CursorMarker>>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    text_metrics: Res<TextMetrics>,
) {
    let Ok((block_cell, cursor, _layout, computed, transform)) = editing_cells.single() else {
        return;
    };

    let Some(cursor_ent) = entities.cursor else {
        return;
    };

    let Ok((mut node, mut visibility)) = cursor_query.get_mut(cursor_ent) else {
        return;
    };

    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };

    let Some(block) = editor.store.get_block_snapshot(&block_cell.block_id) else {
        return;
    };

    let (row, col) = cursor_row_col(&block.content, cursor.offset);

    *visibility = if focus_area.is_text_input() {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };

    let (_, _, translation) = transform.to_scale_angle_translation();
    let node_size = computed.size();
    let cell_left = translation.x - node_size.x / 2.0;
    let cell_top = translation.y - node_size.y / 2.0;

    let char_width = text_metrics.cell_char_width + text_metrics.letter_spacing;
    let line_height = text_metrics.cell_line_height;
    let node_width = char_width + 8.0;
    let beam_frac = 0.3_f32;
    let x = (cell_left + col as f32 * char_width - beam_frac * node_width).round();
    let y = (cell_top + (row as f32 * line_height)).round();

    let target_left = Val::Px(x);
    let target_top = Val::Px(y);
    if node.left != target_left {
        node.left = target_left;
    }
    if node.top != target_top {
        node.top = target_top;
    }
}

/// Position the cursor in the InputOverlay.
pub fn update_input_overlay_cursor(
    focus_area: Res<FocusArea>,
    entities: Res<EditorEntities>,
    overlay_query: Query<
        (&InputOverlay, &ComputedNode, &UiGlobalTransform),
        With<InputOverlayMarker>,
    >,
    editing_blocks: Query<Entity, With<EditingBlockCell>>,
    mut cursor_query: Query<(&mut Node, &mut Visibility, &MaterialNode<CursorBeamMaterial>), With<CursorMarker>>,
    mut cursor_materials: ResMut<Assets<CursorBeamMaterial>>,
    theme: Res<Theme>,
    text_metrics: Res<TextMetrics>,
) {
    if !matches!(*focus_area, FocusArea::Compose) {
        return;
    }
    if !editing_blocks.is_empty() {
        return;
    }

    let Some(cursor_ent) = entities.cursor else { return };
    let Ok((overlay, computed, transform)) = overlay_query.single() else { return };
    let Ok((mut node, mut visibility, material_node)) = cursor_query.get_mut(cursor_ent) else { return };

    *visibility = Visibility::Inherited;

    let (_, _, translation) = transform.to_scale_angle_translation();
    // UiGlobalTransform.translation is the node CENTER in screen px.
    // Vello renders UiVelloText at the node's top-left edge (TopLeft anchor = -size/2 offset),
    // ignoring padding. Use node top-left (not content_box) so cursor aligns with text.
    let node_size = computed.size();
    let cell_left = translation.x - node_size.x / 2.0;
    let cell_top = translation.y - node_size.y / 2.0;

    let display = overlay.display_text();
    let (row, col) = cursor_row_col(&display, overlay.display_cursor_offset());

    let char_width = text_metrics.cell_char_width + text_metrics.letter_spacing;
    let line_height = text_metrics.cell_line_height;
    let node_width = char_width + 8.0;
    let beam_frac = 0.3_f32;
    let x = (cell_left + col as f32 * char_width - beam_frac * node_width).round();
    let y = (cell_top + (row as f32 * line_height)).round();

    let target_left = Val::Px(x);
    let target_top = Val::Px(y);
    if node.left != target_left {
        node.left = target_left;
    }
    if node.top != target_top {
        node.top = target_top;
    }

    if let Some(material) = cursor_materials.get_mut(&material_node.0) {
        let mode_f = CursorMode::Beam as u8 as f32;
        if material.time.y != mode_f || material.color != theme.cursor_insert {
            material.time.y = mode_f;
            material.color = theme.cursor_insert;
            material.params = Vec4::new(0.25, 1.2, 2.0, 0.0);
        }
    }
}
