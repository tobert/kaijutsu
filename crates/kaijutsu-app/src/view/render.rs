//! Buffer sync — text content → BlockScene, layout measurement.
//!
//! This module owns the systems that format block content into display text,
//! set it on BlockScene components, and manage layout readback from Taffy.
//!
//! BlockScene is the data source for `block_render::build_block_scenes`, which
//! renders text and rich content into UiVelloScene via Parley + Vello.

use bevy::prelude::*;

use crate::cell::{
    BlockCell, BlockCellContainer, BlockCellLayout, CellEditor, ConversationScrollState,
    EditorEntities, FocusedBlockCell, LayoutGeneration, MainCell, RoleGroupBorder,
    RoleGroupBorderLayout,
};
use crate::ui::theme::Theme;
use crate::ui::timeline::TimelineVisibility;
use crate::view::block_render::BlockScene;

use super::format::{block_color, format_single_block};
use super::lifecycle::{INDENT_WIDTH, ROLE_HEADER_SPACING};

// ============================================================================
// BUFFER INIT / SYNC
// ============================================================================

pub fn sync_block_cell_buffers(
    mut commands: Commands,
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<(
        &mut BlockCell,
        &mut BlockScene,
        Option<&TimelineVisibility>,
    )>,
    theme: Res<Theme>,
    svg_fontdb: Res<crate::text::SvgFontDb>,
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

    // Quick dirty check before allocating.
    // We must run if the document has a new version, OR if any cell
    // in the container has never been rendered (None).
    let needs_update = container.entities().any(|e| {
        block_cells
            .get(e)
            .map(|(bc, _, _)| bc.last_render_version.is_none_or(|v| v < doc_version))
            .unwrap_or(false)
    });

    if !needs_update {
        return;
    }

    let blocks_ordered = editor.blocks();
    let block_index: std::collections::HashMap<&kaijutsu_crdt::BlockId, usize> = blocks_ordered
        .iter()
        .enumerate()
        .map(|(i, b)| (&b.id, i))
        .collect();

    let mut layout_changed = false;
    for &entity in container.block_cells.values() {
        let Ok((mut block_cell, mut block_scene, timeline_vis)) = block_cells.get_mut(entity) else {
            continue;
        };

        // Run if content changed OR if this is the first render pass
        if block_cell
            .last_render_version
            .is_some_and(|v| v >= doc_version)
        {
            continue;
        }

        let Some(&idx) = block_index.get(&block_cell.block_id) else {
            continue;
        };
        let block = &blocks_ordered[idx];

        let local_ctx = doc_cache.active_id();
        let text = format_single_block(block, local_ctx);

        // Debounce large blocks — only while still streaming. Once a block
        // reaches Done/Error, always render the final text so trim_end takes
        // effect and the measured height converges to the actual content.
        const DEBOUNCE_CHARS: usize = 200;
        const DEBOUNCE_MIN_SIZE: usize = 10_000;
        if block.status == kaijutsu_crdt::Status::Running
            && text.len() > DEBOUNCE_MIN_SIZE
            && block_cell.last_text_len > 0
        {
            let growth = text.len().saturating_sub(block_cell.last_text_len);
            if growth > 0 && growth < DEBOUNCE_CHARS {
                continue;
            }
        }

        let base_color = block_color(block, &theme);

        // Rainbow effect for user text
        let rainbow = theme.font_rainbow
            && block.kind == kaijutsu_crdt::BlockKind::Text
            && block.role == kaijutsu_crdt::Role::User;
        if block_cell.last_rainbow != rainbow {
            commands
                .entity(entity)
                .insert(crate::text::KjTextEffects { rainbow });
            block_cell.last_rainbow = rainbow;
        }

        // Always set the value if this is the first render for this block entity,
        // or if the text actually changed.
        if block_scene.text != text || block_cell.last_render_version.is_none() {
            block_scene.text = text.clone();
        }

        // Rich content rendering for Text blocks from Model or Tool roles (markdown, sparklines, SVG)
        let is_rich_candidate = block.kind == kaijutsu_crdt::BlockKind::Text
            && matches!(
                block.role,
                kaijutsu_crdt::Role::Model | kaijutsu_crdt::Role::Tool
            );
        // ToolResult blocks with an explicit content_type (e.g. text/markdown from `kj help`)
        let is_typed_result =
            block.kind == kaijutsu_crdt::BlockKind::ToolResult && block.content_type != kaijutsu_crdt::ContentType::Plain;
        // Rich content for ToolResult blocks with structured OutputData
        let is_output_candidate = block.kind == kaijutsu_crdt::BlockKind::ToolResult
            && block.output.is_some()
            && !block.is_error;

        let mut actually_rich = false;
        if is_rich_candidate
            && let Some(rich) = crate::text::rich::detect_rich_content_typed(
                &text,
                doc_version,
                block.content_type,
                Some(&svg_fontdb),
            )
        {
            // For sparklines and SVGs: clear text so Parley doesn't re-measure
            // large source text every frame. Height is driven by min_height
            // set in render_rich_content.
            let needs_text_cleared = matches!(
                rich.kind,
                crate::text::rich::RichContentKind::Sparkline(_)
                    | crate::text::rich::RichContentKind::Svg { .. }
                    | crate::text::rich::RichContentKind::Abc { .. }
            );
            if needs_text_cleared {
                block_scene.text = String::new();
            }
            commands.entity(entity).insert(rich);
            actually_rich = true;
        }
        if !actually_rich
            && is_typed_result
            && let Some(rich) = crate::text::rich::detect_rich_content_typed(
                &text,
                doc_version,
                block.content_type,
                Some(&svg_fontdb),
            )
        {
            commands.entity(entity).insert(rich);
            actually_rich = true;
        }
        if !actually_rich
            && is_output_candidate
            && let Some(ref output) = block.output
            && let Some(rich) = crate::text::rich::detect_output_content(output, doc_version)
        {
            commands.entity(entity).insert(rich);
            actually_rich = true;
        }
        if !actually_rich {
            commands.entity(entity).remove::<crate::text::RichContent>();
        }

        // Store color on BlockScene for build_block_scenes
        {
            let color = if let Some(vis) = timeline_vis {
                base_color.with_alpha(base_color.alpha() * vis.opacity)
            } else {
                base_color
            };
            if block_scene.color != color {
                block_scene.color = color;
            }
        }

        let text_len = text.len();
        if block_cell.last_text_len != text_len {
            block_cell.last_text_len = text_len;
            layout_changed = true;
        }
        // Status changes affect border kind/animation (Running→Done turns off chase)
        if block_cell.last_status != block.status {
            block_cell.last_status = block.status;
            layout_changed = true;
        }

        block_cell.last_render_version = Some(doc_version);
        block_scene.content_version = doc_version;
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
    let block_lookup: std::collections::HashMap<
        kaijutsu_crdt::BlockId,
        &kaijutsu_crdt::BlockSnapshot,
    > = blocks_ordered.iter().map(|b| (b.id, b)).collect();

    for &entity in container.block_cells.values() {
        let Ok((block_cell, mut block_layout)) = block_cells.get_mut(entity) else {
            continue;
        };

        let indent_level = if let Some(block) = block_lookup.get(&block_cell.block_id) {
            // Tool call/result blocks are flush (no indent) — they form unified boxes
            if block.kind == kaijutsu_crdt::BlockKind::ToolCall
                || block.kind == kaijutsu_crdt::BlockKind::ToolResult
            {
                0
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
/// Sets margin (indent), width, min_height, and padding on block cell nodes.
/// Text block heights are determined by Parley via ContentSize.
/// Sparklines get explicit min_height from the theme; SVG heights are set
/// in `render_rich_content` where the actual scale factor is known.
pub fn update_block_cell_nodes(
    entities: Res<EditorEntities>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<
        (
            &BlockCellLayout,
            &mut Node,
            Option<&crate::cell::block_border::BlockBorderStyle>,
        ),
        With<BlockCell>,
    >,
    mut role_header_nodes: Query<&mut Node, (With<RoleGroupBorder>, Without<BlockCell>)>,
    layout_gen: Res<LayoutGeneration>,
    theme: Res<Theme>,
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

    for &entity in container.block_cells.values() {
        let Ok((layout, mut node, border_style)) = block_cells.get_mut(entity) else {
            continue;
        };
        let target_padding = UiRect::ZERO;

        // Tiny gap for OpenBottom (ToolCall → ToolResult) so the divider line has breathing room
        let bottom_spacing = if border_style.map(|s| s.kind)
            == Some(crate::cell::block_border::BorderKind::OpenBottom)
        {
            1.0
        } else {
            theme.block_spacing
        };
        // Bordered blocks need horizontal margin so the stroke isn't clipped at the node edge.
        // Use stroke width as the margin — just enough to clear the stroke on each side.
        let h_margin = if border_style.is_some() {
            theme.block_border_thickness
        } else {
            0.0
        };
        let target_margin = UiRect {
            left: Val::Px(layout.indent_level as f32 * INDENT_WIDTH + h_margin),
            right: Val::Px(h_margin),
            bottom: Val::Px(bottom_spacing),
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

        // min_height no longer needed — heights are set explicitly by build_block_scenes.
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
        // Skip tool blocks for role transition tracking (they use fieldset borders)
        let dominated_by_border = block.kind == kaijutsu_crdt::BlockKind::ToolCall
            || block.kind == kaijutsu_crdt::BlockKind::ToolResult;

        if !dominated_by_border {
            let is_transition = prev_role != Some(block.role);
            if is_transition && let Some(&header_ent) = header_map.get(&block.id) {
                ordered_children.push(header_ent);
            }
            prev_role = Some(block.role);
        }
        if let Some(block_ent) = container.get_entity(&block.id) {
            ordered_children.push(block_ent);
        }
    }

    let current_children = children_query.get(conv_entity).ok();
    let order_matches = current_children
        .map(|children| {
            children.len() == ordered_children.len()
                && children
                    .iter()
                    .zip(ordered_children.iter())
                    .all(|(a, b)| a == *b)
        })
        .unwrap_or(false);

    if !order_matches && let Ok(mut ec) = commands.get_entity(conv_entity) {
        ec.replace_children(&ordered_children);
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

            if let Ok(mut layout) = block_layouts.get_mut(child)
                && ((layout.y_offset - y_offset).abs() > 0.5
                    || (layout.height - height).abs() > 0.5)
            {
                layout.y_offset = y_offset;
                layout.height = height;
            }

            y_offset += height + margin_bottom;
        } else if let Ok((computed, node)) = role_headers.get(child) {
            let height = computed.size().y;
            let margin_bottom = match node.margin.bottom {
                Val::Px(px) => px,
                _ => 0.0,
            };

            if let Ok(mut layout) = header_layouts.get_mut(child)
                && (layout.y_offset - y_offset).abs() > 0.5
            {
                layout.y_offset = y_offset;
            }

            y_offset += height + margin_bottom;
        }
    }

    // When new blocks were added this frame, record the pre-update content height
    // as an anchor. smooth_scroll uses min(max, anchor) next frame so the new
    // content is revealed from its start rather than jumping to its bottom.
    if scroll_state.new_blocks_added {
        scroll_state.pending_scroll_anchor = Some(scroll_state.content_height);
        scroll_state.new_blocks_added = false;
    }

    if (scroll_state.content_height - y_offset).abs() > 0.5 {
        scroll_state.content_height = y_offset;
    }
}

// Role group scene building moved to block_render::build_role_group_scenes.

/// Highlight the focused block cell with a visual indicator.
pub fn highlight_focused_block(
    mut focused_cells: Query<(&BlockCell, &mut BlockScene), With<FocusedBlockCell>>,
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

    let blocks: std::collections::HashMap<_, _> =
        editor.blocks().into_iter().map(|b| (b.id, b)).collect();

    for (block_cell, mut block_scene) in focused_cells.iter_mut() {
        if let Some(block) = blocks.get(&block_cell.block_id) {
            let base_color = block_color(block, &theme);
            let srgba = base_color.to_srgba();
            let focused_color = Color::srgba(
                (srgba.red * 1.15).min(1.0),
                (srgba.green * 1.15).min(1.0),
                (srgba.blue * 1.15).min(1.0),
                srgba.alpha,
            );
            if block_scene.color != focused_color {
                block_scene.color = focused_color;
                // Force rebuild so the highlight is visible
                block_scene.content_version = block_scene.content_version.wrapping_add(1);
            }
        }
    }
}

/// Cull off-screen block cells by toggling Visibility.
///
/// Blocks entirely outside the visible scroll range are hidden so bevy_vello
/// skips them during extract — no Parley layout, no Vello scene encoding.
/// A margin of one screen height above/below prevents pop-in during fast scroll.
///
/// This dramatically reduces per-frame rendering work when large tool results
/// (thousands of lines) are in the document but not on screen.
pub fn cull_offscreen_blocks(
    entities: Res<EditorEntities>,
    scroll_state: Res<ConversationScrollState>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<(&BlockCellLayout, &mut Visibility), With<BlockCell>>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    let margin = scroll_state.visible_height;
    let top = scroll_state.offset - margin;
    let bottom = scroll_state.offset + scroll_state.visible_height + margin;

    for &entity in container.block_cells.values() {
        let Ok((layout, mut vis)) = block_cells.get_mut(entity) else {
            continue;
        };
        let block_top = layout.y_offset;
        let block_bottom = layout.y_offset + layout.height;
        let should_show = block_bottom >= top && block_top <= bottom;
        let target = if should_show {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
        if *vis != target {
            *vis = target;
        }
    }
}
