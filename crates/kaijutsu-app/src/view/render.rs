//! Buffer sync — text content → UiVelloText, layout measurement.
//!
//! This module owns the systems that format block content into display text,
//! set it on UiVelloText components, and manage layout readback from Taffy.
//!
//! Key architectural difference from the cell/ version:
//! `VelloTextAnchor::TopLeft` — text starts at the content box top-left.
//! No UiTransform hack needed.

use bevy::prelude::*;

use crate::cell::{
    BlockCell, BlockCellContainer, BlockCellLayout, Cell, CellEditor, CellState,
    EditorEntities, FocusTarget, LayoutGeneration, MainCell, RoleGroupBorder,
    RoleGroupBorderLayout, ConversationScrollState, FocusedBlockCell, WorkspaceLayout,
};
use crate::text::{KjText, KjTextEffects, TextMetrics, FontHandles, bevy_color_to_brush};
use crate::ui::theme::Theme;
use crate::ui::timeline::TimelineVisibility;
use bevy_vello::prelude::{UiVelloText, VelloFont, VelloTextAnchor};

use super::format::{block_color, format_single_block};
use super::lifecycle::{INDENT_WIDTH, BLOCK_SPACING, ROLE_HEADER_SPACING};

// ============================================================================
// BUFFER INIT / SYNC
// ============================================================================

/// Initialize UiVelloText for BlockCells that don't have one.
///
/// **Key change:** Uses `VelloTextAnchor::TopLeft` so text starts at the
/// content box origin. No UiTransform centering hack needed.
pub fn init_block_cell_buffers(
    mut commands: Commands,
    block_cells: Query<Entity, (With<BlockCell>, With<KjText>, Without<UiVelloText>)>,
    font_handles: Res<FontHandles>,
    fonts: Res<Assets<VelloFont>>,
    text_metrics: Res<TextMetrics>,
) {
    // Wait until the font asset is actually loaded. If we insert UiVelloText
    // before the font is ready, bevy_vello's content sizing skips the entity.
    if fonts.get(&font_handles.mono).is_none() {
        return;
    }

    for entity in block_cells.iter() {
        commands.entity(entity).try_insert((
            UiVelloText {
                value: "IF_YOU_SEE_THIS_THERE_IS_A_BUG".to_string(),
                style: bevy_vello::prelude::VelloTextStyle {
                    font: font_handles.mono.clone(),
                    brush: bevy_color_to_brush(Color::WHITE),
                    font_size: text_metrics.cell_font_size,
                    ..default()
                },
                ..default()
            },
            // TopLeft: text starts at content box top-left corner.
            // Bevy flex layout determines position; Parley measures height
            // via ContentSize. No manual offset needed.
            VelloTextAnchor::TopLeft,
        ));
    }
}

/// Sync BlockCell UiVelloText with their corresponding block content.
///
/// Only updates cells whose content has changed (tracked via version).
/// When any buffer is updated, bumps LayoutGeneration to trigger re-layout.
/// Also applies block-specific text colors based on BlockKind and Role.
pub fn sync_block_cell_buffers(
    mut commands: Commands,
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<(&mut BlockCell, &mut UiVelloText, Option<&TimelineVisibility>)>,
    theme: Res<Theme>,
    doc_cache: Res<crate::cell::DocumentCache>,
    mut layout_gen: ResMut<LayoutGeneration>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };

    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };

    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    let doc_version = editor.version();

    // Quick dirty check before allocating
    let needs_update = container.block_cells.iter().any(|e| {
        block_cells
            .get(*e)
            .map(|(bc, _, _)| bc.last_render_version < doc_version)
            .unwrap_or(false)
    });

    if !needs_update {
        return;
    }

    let blocks_ordered = editor.blocks();
    let block_index: std::collections::HashMap<&kaijutsu_crdt::BlockId, usize> = blocks_ordered.iter()
        .enumerate()
        .map(|(i, b)| (&b.id, i))
        .collect();

    let mut layout_changed = false;
    for entity in &container.block_cells {
        let Ok((mut block_cell, mut vello_text, timeline_vis)) = block_cells.get_mut(*entity) else {
            continue;
        };

        if block_cell.last_render_version >= doc_version {
            continue;
        }

        let Some(&idx) = block_index.get(&block_cell.block_id) else {
            continue;
        };
        let block = &blocks_ordered[idx];

        let local_ctx = doc_cache.active_id();
        let text = format_single_block(block, local_ctx);

        // Debounce large blocks
        const DEBOUNCE_CHARS: usize = 200;
        const DEBOUNCE_MIN_SIZE: usize = 10_000;
        if text.len() > DEBOUNCE_MIN_SIZE && block_cell.last_text_len > 0 {
            let growth = text.len().saturating_sub(block_cell.last_text_len);
            if growth > 0 && growth < DEBOUNCE_CHARS {
                continue;
            }
        }

        let base_color = block_color(block, &theme);

        // Rainbow effect for user text
        let rainbow = theme.font_rainbow && block.kind == kaijutsu_crdt::BlockKind::Text && block.role == kaijutsu_crdt::Role::User;
        if block_cell.last_rainbow != rainbow {
            commands.entity(*entity).insert(KjTextEffects { rainbow });
            block_cell.last_rainbow = rainbow;
        }

        if vello_text.value != text {
            vello_text.value = text.clone();
        }

        // Rich markdown rendering for Model/Text blocks
        let is_rich = block.kind == kaijutsu_crdt::BlockKind::Text && block.role == kaijutsu_crdt::Role::Model;
        if is_rich {
            if let Some(rich) = crate::text::parse_rich_content(&text, doc_version) {
                vello_text.style.brush = bevy_color_to_brush(Color::NONE);
                commands.entity(*entity).insert(rich);
            } else {
                commands.entity(*entity).remove::<crate::text::RichTextContent>();
            }
        } else {
            commands.entity(*entity).remove::<crate::text::RichTextContent>();
        }

        // Apply color (skip when rainbow or rich is active)
        if !rainbow && !is_rich {
            let color = if let Some(vis) = timeline_vis {
                base_color.with_alpha(base_color.alpha() * vis.opacity)
            } else {
                base_color
            };
            let new_brush = bevy_color_to_brush(color);
            if vello_text.style.brush != new_brush {
                vello_text.style.brush = new_brush;
            }
        }

        let text_len = text.len();
        if block_cell.last_text_len != text_len {
            block_cell.last_text_len = text_len;
            layout_changed = true;
        }

        block_cell.last_render_version = doc_version;
    }

    if layout_changed {
        layout_gen.bump();
    }
}

// ============================================================================
// LAYOUT SYSTEMS
// ============================================================================

/// Layout BlockCells — compute indentation levels from DAG nesting.
///
/// Heights are determined by Parley text measurement via bevy_vello's
/// ContentSize, not by manual estimation. This system only sets indent_level.
pub fn layout_block_cells(
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<(&BlockCell, &mut BlockCellLayout)>,
    layout_gen: Res<LayoutGeneration>,
    mut last_layout_gen: Local<u64>,
) {
    if layout_gen.0 == *last_layout_gen {
        return;
    }
    *last_layout_gen = layout_gen.0;

    let Some(main_ent) = entities.main_cell else {
        return;
    };

    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };

    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    let blocks_ordered = editor.blocks();
    let block_lookup: std::collections::HashMap<&kaijutsu_crdt::BlockId, &kaijutsu_crdt::BlockSnapshot> =
        blocks_ordered.iter().map(|b| (&b.id, b)).collect();

    for entity in &container.block_cells {
        let Ok((block_cell, mut block_layout)) = block_cells.get_mut(*entity) else {
            continue;
        };

        let indent_level = if let Some(block) = block_lookup.get(&block_cell.block_id) {
            if block.kind == kaijutsu_crdt::BlockKind::ToolResult && block.tool_call_id.is_some() {
                1
            } else if block.parent_id.is_some() {
                1
            } else {
                0
            }
        } else {
            0
        };

        if block_layout.indent_level != indent_level {
            block_layout.indent_level = indent_level;
        }
    }
}

/// Sync BlockCellLayout indentation to Bevy Node for flex layout.
///
/// Sets margin (indent), width, and padding on block cell nodes. Heights are
/// determined by Parley text measurement via bevy_vello's ContentSize.
pub fn update_block_cell_nodes(
    entities: Res<EditorEntities>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<(&BlockCellLayout, &mut Node, Option<&crate::cell::block_border::BlockBorderStyle>), With<BlockCell>>,
    mut role_header_nodes: Query<&mut Node, (With<RoleGroupBorder>, Without<BlockCell>)>,
    layout_gen: Res<LayoutGeneration>,
    mut last_gen: Local<u64>,
) {
    if layout_gen.0 == *last_gen {
        return;
    }
    *last_gen = layout_gen.0;

    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    for entity in &container.block_cells {
        let Ok((layout, mut node, border_style)) = block_cells.get_mut(*entity) else {
            continue;
        };
        let target_padding = if let Some(style) = border_style {
            UiRect {
                left: Val::Px(style.padding.left),
                right: Val::Px(style.padding.right),
                top: Val::Px(style.padding.top),
                bottom: Val::Px(style.padding.bottom),
            }
        } else {
            UiRect::ZERO
        };

        let target_margin = UiRect {
            left: Val::Px(layout.indent_level as f32 * INDENT_WIDTH),
            bottom: Val::Px(BLOCK_SPACING),
            ..default()
        };
        if node.margin != target_margin {
            node.margin = target_margin;
        }
        let target_width = if layout.indent_level > 0 {
            Val::Auto
        } else {
            Val::Percent(100.0)
        };
        if node.width != target_width {
            node.width = target_width;
        }
        if node.padding != target_padding {
            node.padding = target_padding;
        }
    }

    for entity in &container.role_headers {
        if let Ok(mut node) = role_header_nodes.get_mut(*entity) {
            let target = UiRect::bottom(Val::Px(ROLE_HEADER_SPACING));
            if node.margin != target {
                node.margin = target;
            }
        }
    }
}

/// Reorder ConversationContainer children to match document order.
///
/// Interleaves role headers before their associated blocks.
pub fn reorder_conversation_children(
    entities: Res<EditorEntities>,
    mut commands: Commands,
    containers: Query<&BlockCellContainer>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    role_headers: Query<&RoleGroupBorder>,
    children_query: Query<&Children>,
    layout_gen: Res<LayoutGeneration>,
    mut last_gen: Local<u64>,
) {
    if layout_gen.0 == *last_gen {
        return;
    }
    *last_gen = layout_gen.0;

    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Some(conv_entity) = entities.conversation_container else {
        return;
    };
    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };
    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    let blocks = editor.blocks();
    let mut prev_role: Option<kaijutsu_crdt::Role> = None;
    let mut ordered_children = Vec::new();

    let mut header_map: std::collections::HashMap<kaijutsu_crdt::BlockId, Entity> =
        std::collections::HashMap::new();
    for &header_ent in &container.role_headers {
        if let Ok(header) = role_headers.get(header_ent) {
            header_map.insert(header.block_id, header_ent);
        }
    }

    for block in &blocks {
        let is_transition = prev_role != Some(block.role);
        if is_transition {
            if let Some(&header_ent) = header_map.get(&block.id) {
                ordered_children.push(header_ent);
            }
        }
        if let Some(block_ent) = container.get_entity(&block.id) {
            ordered_children.push(block_ent);
        }
        prev_role = Some(block.role);
    }

    let current_children = children_query.get(conv_entity).ok();
    let order_matches = current_children
        .map(|children| {
            children.len() == ordered_children.len()
                && children.iter().zip(ordered_children.iter()).all(|(a, b)| a == *b)
        })
        .unwrap_or(false);

    if !order_matches {
        if let Ok(mut ec) = commands.get_entity(conv_entity) { ec.replace_children(&ordered_children); }
    }
}

/// Read back actual block heights from Taffy layout (PostUpdate).
///
/// Runs after `UiSystems::Layout` so Parley has measured text and Taffy has
/// sized all boxes. This is the sole source of truth for block heights.
pub fn readback_block_heights(
    entities: Res<EditorEntities>,
    children_query: Query<&Children>,
    block_cells: Query<(&ComputedNode, &Node), With<BlockCell>>,
    role_headers: Query<(&ComputedNode, &Node), (With<RoleGroupBorder>, Without<BlockCell>)>,
    mut block_layouts: Query<&mut BlockCellLayout, With<BlockCell>>,
    mut header_layouts: Query<&mut RoleGroupBorderLayout, Without<BlockCell>>,
    mut scroll_state: ResMut<ConversationScrollState>,
) {
    let Some(conv_entity) = entities.conversation_container else {
        return;
    };
    let Ok(children) = children_query.get(conv_entity) else {
        return;
    };

    let mut y_offset: f32 = 0.0;

    for child in children.iter() {
        if let Ok((computed, node)) = block_cells.get(child) {
            let height = computed.size().y;
            let margin_bottom = match node.margin.bottom {
                Val::Px(px) => px,
                _ => 0.0,
            };

            if let Ok(mut layout) = block_layouts.get_mut(child) {
                if (layout.y_offset - y_offset).abs() > 0.5 || (layout.height - height).abs() > 0.5 {
                    layout.y_offset = y_offset;
                    layout.height = height;
                }
            }

            y_offset += height + margin_bottom;
        } else if let Ok((computed, node)) = role_headers.get(child) {
            let height = computed.size().y;
            let margin_bottom = match node.margin.bottom {
                Val::Px(px) => px,
                _ => 0.0,
            };

            if let Ok(mut layout) = header_layouts.get_mut(child) {
                if (layout.y_offset - y_offset).abs() > 0.5 {
                    layout.y_offset = y_offset;
                }
            }

            y_offset += height + margin_bottom;
        }
    }

    if (scroll_state.content_height - y_offset).abs() > 0.5 {
        scroll_state.content_height = y_offset;
    }
}

/// Rebuild role group border Vello scenes when ComputedNode changes.
pub fn update_role_group_scenes(
    mut role_borders: Query<
        (&RoleGroupBorder, &mut bevy_vello::prelude::UiVelloScene, &ComputedNode),
        Changed<ComputedNode>,
    >,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<FontHandles>,
    theme: Res<Theme>,
) {
    let font = fonts.get(&font_handles.mono);

    for (border, mut scene_component, computed) in role_borders.iter_mut() {
        let size = computed.size();
        if size.x < 1.0 || size.y < 1.0 {
            continue;
        }

        let color = match border.role {
            kaijutsu_crdt::Role::User => theme.block_user,
            kaijutsu_crdt::Role::Model => theme.block_assistant,
            kaijutsu_crdt::Role::System => theme.fg_dim,
            kaijutsu_crdt::Role::Tool | kaijutsu_crdt::Role::Asset => theme.block_tool_call,
        };

        let label = match border.role {
            kaijutsu_crdt::Role::User => "USER",
            kaijutsu_crdt::Role::Model => "ASSISTANT",
            kaijutsu_crdt::Role::System => "SYSTEM",
            kaijutsu_crdt::Role::Tool => "TOOL",
            kaijutsu_crdt::Role::Asset => "ASSET",
        };

        let mut scene = bevy_vello::vello::Scene::new();
        crate::view::fieldset::build_role_group_line(
            &mut scene,
            size.x as f64,
            size.y as f64,
            label,
            color,
            font,
        );

        *scene_component = bevy_vello::prelude::UiVelloScene::from(scene);
    }
}

/// Highlight the focused block cell with a visual indicator.
pub fn highlight_focused_block(
    mut focused_cells: Query<(&BlockCell, &mut UiVelloText), With<FocusedBlockCell>>,
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    theme: Res<Theme>,
) {
    if focused_cells.is_empty() {
        return;
    }

    let Some(main_ent) = entities.main_cell else {
        return;
    };

    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };

    let blocks: std::collections::HashMap<_, _> = editor
        .blocks()
        .into_iter()
        .map(|b| (b.id, b))
        .collect();

    for (block_cell, mut vello_text) in focused_cells.iter_mut() {
        if let Some(block) = blocks.get(&block_cell.block_id) {
            let base_color = block_color(block, &theme);
            let srgba = base_color.to_srgba();
            let focused_color = Color::srgba(
                (srgba.red * 1.15).min(1.0),
                (srgba.green * 1.15).min(1.0),
                (srgba.blue * 1.15).min(1.0),
                srgba.alpha,
            );
            let new_brush = bevy_color_to_brush(focused_color);
            if vello_text.style.brush != new_brush {
                vello_text.style.brush = new_brush;
            }
        }
    }
}

// ============================================================================
// NON-BLOCK CELL SYSTEMS (kept from cell/systems.rs)
// ============================================================================

/// Initialize text for cells that don't have UiVelloText yet.
/// No-op — Vello text entities are complete at spawn time.
pub fn init_cell_buffers() {}

/// Sync UiVelloText from CellEditor when dirty.
///
/// For cells with content blocks, clears the text (BlockCells render per-block).
/// For plain text cells, uses the text directly.
pub fn sync_cell_buffers(
    mut cells: Query<(&CellEditor, &mut UiVelloText, Option<&BlockCellContainer>), Changed<CellEditor>>,
) {
    for (editor, mut vello_text, container) in cells.iter_mut() {
        if container.is_some_and(|c| !c.block_cells.is_empty()) {
            if !vello_text.value.is_empty() {
                vello_text.value.clear();
            }
            continue;
        }

        let display_text = if editor.has_blocks() {
            super::format::format_blocks_for_display(&editor.blocks())
        } else {
            editor.text()
        };

        if vello_text.value != display_text {
            vello_text.value = display_text;
        }
    }
}

/// Compute cell heights based on content (non-MainCell only).
pub fn compute_cell_heights(
    mut cells: Query<(&CellEditor, &mut CellState, Option<&MainCell>), Changed<CellEditor>>,
    text_metrics: Res<TextMetrics>,
    layout: Res<WorkspaceLayout>,
) {
    for (editor, mut state, main_cell) in cells.iter_mut() {
        if main_cell.is_some() {
            continue;
        }

        let display_text = if editor.has_blocks() {
            super::format::format_blocks_for_display(&editor.blocks())
        } else {
            editor.text()
        };
        let line_count = display_text.lines().count().max(1);
        let content_height = (line_count as f32) * text_metrics.cell_line_height + 4.0;
        let height = content_height.max(layout.min_cell_height);
        state.computed_height = if layout.max_cell_height > 0.0 {
            height.min(layout.max_cell_height)
        } else {
            height
        };
    }
}

/// Visual indication for focused cell.
pub fn highlight_focused_cell(
    focus: Res<FocusTarget>,
    mut cells: Query<(Entity, &mut UiVelloText), With<Cell>>,
    theme: Option<Res<Theme>>,
) {
    let Some(ref theme) = theme else {
        warn_once!("Theme resource unavailable for cell highlighting");
        return;
    };

    for (entity, mut vello_text) in cells.iter_mut() {
        let color = if Some(entity) == focus.entity {
            theme.accent
        } else {
            theme.fg_dim
        };
        let new_brush = bevy_color_to_brush(color);
        if vello_text.style.brush != new_brush {
            vello_text.style.brush = new_brush;
        }
    }
}

/// Click to focus a cell.
pub fn click_to_focus(
    mouse: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window>,
    cells: Query<(Entity, &ComputedNode, &UiGlobalTransform), With<Cell>>,
    mut focus: ResMut<FocusTarget>,
) {
    if !mouse.just_pressed(MouseButton::Left) {
        return;
    }

    let Some(cursor_pos) = windows.iter().next().and_then(|w| w.cursor_position()) else {
        return;
    };

    for (entity, computed, transform) in cells.iter() {
        let (_, _, translation) = transform.to_scale_angle_translation();
        let size = computed.size();
        let left = translation.x - size.x / 2.0;
        let top = translation.y - size.y / 2.0;
        let right = left + size.x;
        let bottom = top + size.y;

        if cursor_pos.x >= left
            && cursor_pos.x <= right
            && cursor_pos.y >= top
            && cursor_pos.y <= bottom
        {
            focus.entity = Some(entity);
            return;
        }
    }

    focus.entity = None;
}
