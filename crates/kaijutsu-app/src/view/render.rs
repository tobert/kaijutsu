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
    BlockCell, BlockCellContainer, BlockCellLayout, CellEditor,
    EditorEntities, LayoutGeneration, MainCell, RoleGroupBorder,
    RoleGroupBorderLayout, ConversationScrollState, FocusedBlockCell,
};
use crate::text::{FontHandles, bevy_color_to_brush};
use crate::ui::theme::Theme;
use crate::ui::timeline::TimelineVisibility;
use bevy_vello::prelude::{UiVelloText, VelloFont};

use super::format::{block_color, format_single_block};
use super::lifecycle::{INDENT_WIDTH, BLOCK_SPACING, ROLE_HEADER_SPACING};

// ============================================================================
// BUFFER INIT / SYNC
// ============================================================================

/// Safety net: log a warning if any BlockCell lacks UiVelloText.
///
/// With single-phase spawning in `spawn_block_cells`, this should never fire.
/// If it does, something bypassed the normal spawn path.
pub fn init_block_cell_buffers(
    block_cells: Query<Entity, (With<BlockCell>, Without<UiVelloText>)>,
) {
    let count = block_cells.iter().count();
    if count > 0 {
        warn!(
            "{} BlockCell entities missing UiVelloText — spawn_block_cells should include it",
            count
        );
    }
}

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

    // Quick dirty check before allocating. 
    // We must run if the document has a new version, OR if any cell 
    // in the container has never been rendered (None).
    let needs_update = container.entities().any(|e| {
        block_cells
            .get(e)
            .map(|(bc, _, _)| bc.last_render_version.map_or(true, |v| v < doc_version))
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
    for &entity in container.block_cells.values() {
        let Ok((mut block_cell, mut vello_text, timeline_vis)) = block_cells.get_mut(entity) else {
            continue;
        };

        // Run if content changed OR if this is the first render pass
        if block_cell.last_render_version.map_or(false, |v| v >= doc_version) {
            continue;
        }

        let Some(&idx) = block_index.get(&block_cell.block_id) else {
            continue;
        };
        let block = &blocks_ordered[idx];

        let local_ctx = doc_cache.active_id();
        let text = format_single_block(block, local_ctx);

        // Debounce large blocks — only re-render when growth exceeds threshold
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
            commands.entity(entity).insert(crate::text::KjTextEffects { rainbow });
            block_cell.last_rainbow = rainbow;
        }

        // Always set the value if this is the first render for this block entity,
        // or if the text actually changed. This ensures `Changed<UiVelloText>`
        // triggers for `bevy_vello`'s measurement system.
        if vello_text.value != text || block_cell.last_render_version.is_none() {
            vello_text.value = text.clone();
        }

        // Rich content rendering for Model/Text blocks (markdown, sparklines)
        let is_rich_candidate = block.kind == kaijutsu_crdt::BlockKind::Text && block.role == kaijutsu_crdt::Role::Model;
        let mut actually_rich = false;
        if is_rich_candidate {
            if let Some(rich) = crate::text::detect_rich_content(&text, doc_version) {
                // For sparklines: clear text so Parley won't fight min_height
                let is_sparkline = matches!(rich.kind, crate::text::rich::RichContentKind::Sparkline(_));
                if is_sparkline {
                    vello_text.value = String::new();
                }
                vello_text.style.brush = bevy_color_to_brush(Color::NONE);
                commands.entity(entity).insert(rich);
                actually_rich = true;
                block_cell.is_rich = true;
            } else {
                commands.entity(entity).remove::<crate::text::RichContent>();
                commands.entity(entity).remove::<bevy_vello::prelude::UiVelloScene>();
                block_cell.is_rich = false;
            }
        } else {
            commands.entity(entity).remove::<crate::text::RichContent>();
            commands.entity(entity).remove::<bevy_vello::prelude::UiVelloScene>();
            block_cell.is_rich = false;
        }

        // Apply color (skip when rainbow or actively rendering rich content)
        if !rainbow && !actually_rich {
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

        block_cell.last_render_version = Some(doc_version);
    }

    if layout_changed {
        layout_gen.bump();
    }
}

/// Keep `UiVelloText.max_advance` in sync with `ComputedNode.size().x`.
///
/// This must be a separate system from `sync_block_cell_buffers` because:
/// - On spawn frame, Taffy hasn't run yet so ComputedNode.size() is zero
/// - `sync_block_cell_buffers` only runs when doc content changes
/// - This needs to run whenever the layout width changes (window resize, etc.)
pub fn sync_text_max_advance(
    mut block_cells: Query<
        (&mut UiVelloText, &ComputedNode),
        With<BlockCell>,
    >,
) {
    for (mut vello_text, computed_node) in block_cells.iter_mut() {
        let width = computed_node.size().x;
        if width > 0.0 {
            let new_advance = Some(width);
            if vello_text.max_advance != new_advance {
                vello_text.max_advance = new_advance;
            }
        }
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

    for &entity in container.block_cells.values() {
        let Ok((block_cell, mut block_layout)) = block_cells.get_mut(entity) else {
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
/// Sets margin (indent), width, min_height, and padding on block cell nodes.
/// Text block heights are determined by Parley via ContentSize.
/// Non-text rich content (sparklines) gets explicit min_height from the theme.
pub fn update_block_cell_nodes(
    entities: Res<EditorEntities>,
    containers: Query<&BlockCellContainer>,
    mut block_cells: Query<(
        &BlockCellLayout,
        &mut Node,
        Option<&crate::cell::block_border::BlockBorderStyle>,
        Option<&crate::text::RichContent>,
    ), With<BlockCell>>,
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
        let Ok((layout, mut node, border_style, rich_content)) = block_cells.get_mut(entity) else {
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

        // Set explicit min_height for non-text rich content (sparklines)
        let target_min_height = rich_content
            .and_then(|rc| rc.desired_height(&theme))
            .map(Val::Px)
            .unwrap_or(Val::Auto);
        if node.min_height != target_min_height {
            node.min_height = target_min_height;
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
        // Skip rich content blocks — their UiVelloText brush must stay transparent
        // so the rich renderer (UiVelloScene) is the only visible text layer.
        if block_cell.is_rich {
            continue;
        }
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
        let target = if should_show { Visibility::Inherited } else { Visibility::Hidden };
        if *vis != target {
            *vis = target;
        }
    }
}

