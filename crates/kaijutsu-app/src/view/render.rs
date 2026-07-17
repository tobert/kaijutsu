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
    ConversationSpacer, EditorEntities, FocusedBlockCell, LayoutGeneration, MainCell,
    RoleGroupBorder, RoleGroupBorderLayout,
};
use crate::ui::theme::Theme;
use crate::ui::timeline::TimelineVisibility;
use crate::view::block_render::BlockScene;
use crate::view::geometry::{ConversationGeometry, GeomRow, RowKey};

use super::format::{block_color, format_single_block};
// Layout constants now in Theme; lifecycle.rs constants kept for spawn-time defaults.

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

    let mut layout_changed = false;
    for (block_id, &entity) in container.block_cells.iter() {
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

        // One snapshot per dirty in-band entity — never a whole-document
        // clone; the container only holds the spawn band.
        let Some(block) = editor.block_snapshot(block_id) else {
            continue;
        };
        let block = &block;

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
                    | crate::text::rich::RichContentKind::Image { .. }
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
/// Heights are determined by Parley text measurement, not by manual
/// estimation. This system only sets indent_level.
pub fn layout_block_cells(
    entities: Res<EditorEntities>,
    containers: Query<&BlockCellContainer>,
    geometries: Query<&ConversationGeometry>,
    mut block_cells: Query<(&BlockCell, &mut BlockCellLayout)>,
    layout_gen: Res<LayoutGeneration>,
    mut last_layout_gen: Local<u64>,
) {
    if layout_gen.0 == *last_layout_gen {
        return;
    }

    let Some(main_ent) = entities.main_cell else {
        return;
    };

    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    let Ok(geom) = geometries.get(main_ent) else {
        return;
    };
    // Consume the generation only once the guards pass — an early-out above
    // must retry next frame, not wait for the next bump.
    *last_layout_gen = layout_gen.0;

    for &entity in container.block_cells.values() {
        let Ok((block_cell, mut block_layout)) = block_cells.get_mut(entity) else {
            continue;
        };

        // Indent captured at geometry seed time (tool blocks flush,
        // children indent) — no per-bump snapshot clone of the document.
        let indent_level = geom
            .block_row(&block_cell.block_id)
            .map(|r| r.indent_level)
            .unwrap_or(0);

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

    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(container) = containers.get(main_ent) else {
        return;
    };
    // Consume the generation only once the guards pass (see layout_block_cells).
    *last_gen = layout_gen.0;

    for &entity in container.block_cells.values() {
        let Ok((layout, mut node, border_style)) = block_cells.get_mut(entity) else {
            continue;
        };
        let target_padding = UiRect::ZERO;

        // Seamless join for OpenBottom→OpenTop (ToolCall→ToolResult): zero gap so
        // side edges form continuous vertical lines between the paired blocks.
        let bottom_spacing = if border_style.map(|s| s.kind)
            == Some(crate::cell::block_border::BorderKind::OpenBottom)
        {
            0.0
        } else {
            theme.block_spacing
        };
        // Borders are shader-drawn inside the MSDF texture (no Vello strokes to clip),
        // but keep a small margin so the shader glow doesn't clip at the node edge.
        let h_margin = if border_style.is_some() {
            theme.block_border_glow_radius * 0.5
        } else {
            0.0
        };
        let target_margin = UiRect {
            left: Val::Px(layout.indent_level as f32 * theme.indent_width + h_margin),
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
            let target = UiRect::bottom(Val::Px(theme.role_header_spacing));
            if node.margin != target {
                node.margin = target;
            }
        }
    }
}

/// Resolve geometry rows to live entities in document order — no spacers.
///
/// Pure function — no ECS access — so the ordering logic can be unit
/// tested without spinning up a Bevy `App`. Row structure (role-header
/// interleaving, tool-block rules) is already decided by
/// `ConversationGeometry::reconcile`; this only maps rows to entities. A
/// block row with no `container` entry is reported via `on_missing_block`:
/// today that gap is the signature of an upstream spawn/removal bug
/// (spawn_block_cells lagging or a stale container ref); once offscreen
/// blocks despawn by design, the caller's predicate decides which gaps are
/// expected. Header rows with no entity are skipped silently (header spawn
/// has always lagged one layout generation).
///
/// Shared by `compute_ordered_children` (which wraps the result with the
/// top/bottom spacers for `reorder_conversation_children`) — while
/// `readback_block_heights`/`virtualize_conversation` walk the geometry
/// rows directly, so all three see the same document order.
fn ordered_entities_from_rows(
    rows: &[GeomRow],
    container: &BlockCellContainer,
    header_map: &std::collections::HashMap<kaijutsu_crdt::BlockId, Entity>,
    mut on_missing_block: impl FnMut(&kaijutsu_crdt::BlockId),
) -> Vec<Entity> {
    let mut ordered = Vec::with_capacity(rows.len());

    for row in rows {
        match row.key {
            RowKey::Header(id) => {
                if let Some(&header_ent) = header_map.get(&id) {
                    ordered.push(header_ent);
                }
            }
            RowKey::Block(id) => match container.get_entity(&id) {
                Some(block_ent) => ordered.push(block_ent),
                None => on_missing_block(&id),
            },
        }
    }

    ordered
}

/// Compute the ConversationContainer's children in document order:
/// `[top_spacer, ...interleaved block/header entities..., bottom_spacer]`.
///
/// Pure function — no ECS access — so the ordering logic can be unit
/// tested without spinning up a Bevy `App`. The two spacers must always be
/// the first and last children so their `Node.height` can stand in for
/// virtualized-out (`Display::None`) content above/below the visible
/// window — see `ConversationSpacer`.
pub fn compute_ordered_children(
    rows: &[GeomRow],
    container: &BlockCellContainer,
    header_map: &std::collections::HashMap<kaijutsu_crdt::BlockId, Entity>,
    top_spacer: Entity,
    bottom_spacer: Entity,
    on_missing_block: impl FnMut(&kaijutsu_crdt::BlockId),
) -> Vec<Entity> {
    let mut ordered_children = Vec::with_capacity(rows.len() + 2);
    ordered_children.push(top_spacer);
    ordered_children.extend(ordered_entities_from_rows(
        rows,
        container,
        header_map,
        on_missing_block,
    ));
    ordered_children.push(bottom_spacer);
    ordered_children
}

/// Among a container's *current* children, find live entities that are
/// about to be dropped by a `replace_children` call to `ordered_children`.
///
/// `replace_children` silently un-parents anything missing from the new
/// list — it does not despawn it. An entity that falls out of
/// `ordered_children` while still alive as a `BlockCell` or
/// `RoleGroupBorder` becomes a root UI node: rendered at window scope,
/// never culled (culling walks the `BlockCellContainer` map), never
/// measured (readback walks `Children`), never despawned. This is the
/// orphan-leak bug — this function identifies which current children hit
/// it so the caller can despawn them explicitly instead of leaking them
/// until app restart.
///
/// `is_spacer` is an explicit exclusion, not just an omission: the two
/// `ConversationSpacer` entities are always included in `ordered_children`
/// (see `compute_ordered_children`) so in practice they never reach this
/// filter, but a spacer must never be despawned as a false-positive orphan
/// even if that invariant is ever violated upstream — despawning one would
/// desync `EditorEntities.top_spacer`/`bottom_spacer` from reality.
///
/// Pure function — takes membership predicates instead of `Query` so it's
/// unit-testable without ECS.
pub fn find_orphaned_children(
    current_children: &[Entity],
    ordered_children: &[Entity],
    is_block_cell: impl Fn(Entity) -> bool,
    is_role_header: impl Fn(Entity) -> bool,
    is_spacer: impl Fn(Entity) -> bool,
) -> Vec<Entity> {
    let ordered_set: std::collections::HashSet<Entity> =
        ordered_children.iter().copied().collect();
    current_children
        .iter()
        .copied()
        .filter(|&child| !ordered_set.contains(&child))
        .filter(|&child| (is_block_cell(child) || is_role_header(child)) && !is_spacer(child))
        .collect()
}

/// Reorder ConversationContainer children to match document order.
///
/// Interleaves role headers before their associated blocks. Any current
/// child that would otherwise be silently orphaned by `replace_children`
/// (a live `BlockCell`/`RoleGroupBorder` missing from the computed order)
/// is despawned explicitly and logged loudly — see `find_orphaned_children`.
/// Likewise, a live block with no container entry is logged loudly instead
/// of just vanishing from the render order — see `compute_ordered_children`.
pub fn reorder_conversation_children(
    entities: Res<EditorEntities>,
    mut commands: Commands,
    containers: Query<&BlockCellContainer>,
    geometries: Query<&ConversationGeometry>,
    scroll_state: Res<ConversationScrollState>,
    role_headers: Query<&RoleGroupBorder>,
    block_cell_entities: Query<Entity, With<BlockCell>>,
    spacer_entities: Query<Entity, With<ConversationSpacer>>,
    children_query: Query<&Children>,
    layout_gen: Res<LayoutGeneration>,
    mut last_gen: Local<u64>,
) {
    if layout_gen.0 == *last_gen {
        return;
    }

    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Some(conv_entity) = entities.conversation_container else {
        return;
    };
    // Spacers are spawned by `ensure_conversation_spacers`, which runs
    // earlier in the same frame (CellPhase::Spawn) whenever
    // `conversation_container` is set — so by the time this Layout-phase
    // system runs, both should already exist. If not (e.g. very first
    // frame ordering), leave the generation unconsumed so the retry happens
    // next frame, not at the next bump.
    let Some(top_spacer) = entities.top_spacer else {
        return;
    };
    let Some(bottom_spacer) = entities.bottom_spacer else {
        return;
    };
    let Ok(geom) = geometries.get(main_ent) else {
        return;
    };
    let Ok(container) = containers.get(main_ent) else {
        return;
    };
    *last_gen = layout_gen.0;

    let mut header_map: std::collections::HashMap<kaijutsu_crdt::BlockId, Entity> =
        std::collections::HashMap::new();
    for &header_ent in &container.role_headers {
        if let Ok(header) = role_headers.get(header_ent) {
            header_map.insert(header.block_id, header_ent);
        }
    }

    // Outside the spawn band, entity-less rows are the design (band
    // despawn); inside it they mean spawn_block_cells lagged (font gate) or
    // lost an entry — loud, but survivable for a frame.
    let vh = if scroll_state.visible_height > 0.0 {
        scroll_state.visible_height
    } else {
        600.0
    };
    let spawn_top = scroll_state.offset - crate::view::geometry::SPAWN_MARGIN_SCREENS * vh;
    let spawn_bottom =
        scroll_state.offset + vh + crate::view::geometry::SPAWN_MARGIN_SCREENS * vh;

    let ordered_children = compute_ordered_children(
        geom.rows(),
        container,
        &header_map,
        top_spacer,
        bottom_spacer,
        |block_id| {
            if let Some(row) = geom.block_row(block_id) {
                let bottom_edge = row.y_offset + row.height + row.margin_bottom;
                if bottom_edge >= spawn_top && row.y_offset <= spawn_bottom {
                    warn!(
                        "reorder_conversation_children: in-band block {block_id:?} has no \
                         BlockCellContainer entry — dropped from render order this frame; \
                         spawn_block_cells should have created it (font gate on first frames \
                         is the benign case)"
                    );
                }
            }
        },
    );

    let current_children: Vec<Entity> = children_query
        .get(conv_entity)
        .map(|c| c.iter().collect())
        .unwrap_or_default();

    // command-flush timing: this system runs in CellPhase::Layout, after the
    // CellPhase::Spawn and CellPhase::Buffer phases each end in an
    // ApplyDeferred, and no system between there and here despawns entities.
    // So every `current_children` entry here is either wanted (present in
    // `ordered_children`) or a genuine orphan — never a same-frame pending
    // despawn we'd be racing.
    let orphans = find_orphaned_children(
        &current_children,
        &ordered_children,
        |e| block_cell_entities.contains(e),
        |e| role_headers.contains(e),
        |e| spacer_entities.contains(e),
    );
    for orphan in orphans {
        error!(
            "reorder_conversation_children: entity {orphan:?} fell out of document order \
             (missing from container or an ordering bug) — despawning instead of leaking it \
             as an un-parented root UI node"
        );
        commands.entity(orphan).try_despawn();
    }

    let order_matches = current_children.len() == ordered_children.len()
        && current_children
            .iter()
            .zip(ordered_children.iter())
            .all(|(a, b)| a == b);

    if !order_matches && let Ok(mut ec) = commands.get_entity(conv_entity) {
        ec.replace_children(&ordered_children);
    }
}

/// Read a `Val::Px` margin as a plain `f32`, treating any other unit as 0.
/// Block/header margins are always set as `Val::Px` by
/// `update_block_cell_nodes`/`sync_role_headers`.
fn margin_bottom_px(node: &Node) -> f32 {
    match node.margin.bottom {
        Val::Px(px) => px,
        _ => 0.0,
    }
}

/// Read back block/header heights from Taffy layout into the
/// [`ConversationGeometry`] model (PostUpdate).
///
/// Runs after `UiSystems::Layout` so Parley has measured text and Taffy has
/// sized all boxes. The geometry rows are the sole source of truth for
/// heights; entity `BlockCellLayout`/`RoleGroupBorderLayout` are mirrors
/// kept for consumers that already hold the entity.
///
/// One walk over geometry rows (document order, entity-optional):
///
/// 1. **Measure visible only.** A live entity currently `Display::Flex`
///    (laid out) has its row height refreshed from `ComputedNode` and its
///    `measured_version` stamped (blocks: `last_render_version`; headers:
///    any non-zero). `Display::None` or entity-less rows keep their cached
///    (or estimated) height untouched — an estimate is replaced by a real
///    measurement just-in-time as the row first becomes visible.
/// 2. **Prefix sums from the model.** `recompute_offsets` rebuilds
///    `y_offset`/`content_height` from cached heights, so geometry stays
///    what it would be if nothing were virtualized out.
///
/// **Scroll anchoring:** when a row *fully above the viewport* changes
/// height (an estimate corrected by measurement, or a resize), everything
/// below shifts by the delta. The viewport is shifted by the same delta so
/// the content the user is looking at doesn't visually jump.
pub fn readback_block_heights(
    entities: Res<EditorEntities>,
    containers: Query<&BlockCellContainer>,
    mut geometries: Query<&mut ConversationGeometry>,
    role_header_query: Query<&RoleGroupBorder>,
    block_cells: Query<(&ComputedNode, &Node, &BlockCell), With<BlockCell>>,
    role_headers: Query<(&ComputedNode, &Node), (With<RoleGroupBorder>, Without<BlockCell>)>,
    mut block_layouts: Query<&mut BlockCellLayout, With<BlockCell>>,
    mut header_layouts: Query<&mut RoleGroupBorderLayout, Without<BlockCell>>,
    mut scroll_state: ResMut<ConversationScrollState>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(container) = containers.get(main_ent) else {
        return;
    };
    let Ok(mut geom) = geometries.get_mut(main_ent) else {
        return;
    };

    let mut header_map: std::collections::HashMap<kaijutsu_crdt::BlockId, Entity> =
        std::collections::HashMap::new();
    for &header_ent in &container.role_headers {
        if let Ok(header) = role_header_query.get(header_ent) {
            header_map.insert(header.block_id, header_ent);
        }
    }

    // Pass 1: measure laid-out entities into their geometry rows.
    // `anchor_delta` accumulates height changes of rows fully above the
    // viewport top (compared against pre-measure offsets, which are the
    // offsets the current scroll position was computed against).
    let viewport_top = scroll_state.offset;
    let mut anchor_delta = 0.0_f32;

    for i in 0..geom.rows().len() {
        let (key, old_y, old_h) = {
            let row = &geom.rows()[i];
            (row.key, row.y_offset, row.height)
        };
        let above_viewport = old_y + old_h <= viewport_top;

        let delta = match key {
            RowKey::Block(id) => {
                let Some(entity) = container.get_entity(&id) else {
                    continue;
                };
                let Ok((computed, node, block_cell)) = block_cells.get(entity) else {
                    continue;
                };
                if node.display == Display::None {
                    continue;
                }
                let version = block_cell.last_render_version.unwrap_or(0);
                // ComputedNode is physical px; geometry rows are logical.
                let height = crate::view::ui_rtt::logical_size(computed).y;
                geom.measure(key, height, margin_bottom_px(node), version)
            }
            RowKey::Header(id) => {
                let Some(&entity) = header_map.get(&id) else {
                    continue;
                };
                let Ok((computed, node)) = role_headers.get(entity) else {
                    continue;
                };
                if node.display == Display::None {
                    continue;
                }
                // Headers don't stream; any non-zero stamp means "measured".
                let height = crate::view::ui_rtt::logical_size(computed).y;
                geom.measure(key, height, margin_bottom_px(node), 1)
            }
        };

        if above_viewport {
            anchor_delta += delta;
        }
    }

    // Pass 2: prefix sums + mirrors for live entities.
    geom.recompute_offsets();

    for row in geom.rows() {
        match row.key {
            RowKey::Block(id) => {
                let Some(entity) = container.get_entity(&id) else {
                    continue;
                };
                let Ok(mut layout) = block_layouts.get_mut(entity) else {
                    continue;
                };
                if (layout.height - row.height).abs() > 0.01 {
                    layout.height = row.height;
                }
                if (layout.y_offset - row.y_offset).abs() > 0.01 {
                    layout.y_offset = row.y_offset;
                }
                if layout.last_measured_version != row.measured_version {
                    layout.last_measured_version = row.measured_version;
                }
            }
            RowKey::Header(id) => {
                let Some(&entity) = header_map.get(&id) else {
                    continue;
                };
                let Ok(mut layout) = header_layouts.get_mut(entity) else {
                    continue;
                };
                if (layout.height - row.height).abs() > 0.01 {
                    layout.height = row.height;
                }
                if (layout.y_offset - row.y_offset).abs() > 0.01 {
                    layout.y_offset = row.y_offset;
                }
            }
        }
    }

    // When new blocks were added this frame, record the pre-update content height
    // as an anchor. smooth_scroll uses min(max, anchor) next frame so the new
    // content is revealed from its start rather than jumping to its bottom.
    if scroll_state.new_blocks_added {
        scroll_state.pending_scroll_anchor = Some(scroll_state.content_height);
        scroll_state.new_blocks_added = false;
    }

    if (scroll_state.content_height - geom.content_height).abs() > 0.5 {
        scroll_state.content_height = geom.content_height;
    }

    // Anchor correction: keep the viewport visually pinned when content
    // above it changed size. Follow mode is exempt — it re-clamps to the
    // bottom every frame anyway, and correcting under it would fight the
    // clamp during streaming.
    if anchor_delta.abs() > 0.5 && !scroll_state.following {
        scroll_state.offset = (scroll_state.offset + anchor_delta).max(0.0);
        scroll_state.target_offset = (scroll_state.target_offset + anchor_delta).max(0.0);
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

/// Pure computation of top/bottom `ConversationSpacer` heights from the
/// logical geometry of the entities that end up shown (`Display::Flex`) this
/// frame, in document order.
///
/// `shown` is `(y_offset, height, margin_bottom)` for each shown entity, in
/// document order (blocks and headers interleaved, as walked by
/// `virtualize_conversation`). Pure/ECS-free so it's unit-testable without a
/// Bevy `App`.
///
/// - `top` = the first shown entity's logical `y_offset` (0 if it's already
///   at the document top, or if nothing is shown).
/// - `bottom` = `content_height` minus the last shown entity's logical
///   bottom edge (`y_offset + height + margin_bottom`) — 0 if it's already
///   at the document bottom, or if nothing is shown.
pub fn compute_spacer_heights(shown: &[(f32, f32, f32)], content_height: f32) -> (f32, f32) {
    let top = shown.first().map(|&(y, _, _)| y).unwrap_or(0.0).max(0.0);
    let bottom = shown
        .last()
        .map(|&(y, h, m)| (content_height - (y + h + m)).max(0.0))
        .unwrap_or(0.0);
    (top, bottom)
}

fn set_display(node: &mut Node, display: Display) {
    if node.display != display {
        node.display = display;
    }
}

fn set_visibility(vis: &mut Visibility, target: Visibility) {
    if *vis != target {
        *vis = target;
    }
}

/// Virtualize the conversation column: remove offscreen block/header nodes
/// from Taffy layout entirely (`Node.display = Display::None`) instead of
/// only hiding them (`Visibility::Hidden`), and size the top/bottom
/// `ConversationSpacer` nodes to stand in for the removed space.
///
/// This is the fix for the O(N) relayout problem: a `Visibility::Hidden`
/// node is still measured and positioned by Taffy every relayout; a
/// `Display::None` node is skipped entirely, so relayout cost tracks the
/// number of *visible* blocks, not the total. `Visibility::Hidden` is kept
/// alongside `Display::None` (same dual pattern as the shell dock,
/// `shell_dock.rs:73-92`) so `build_block_scenes` and friends keep their
/// existing Visibility-gated skip as a second line of defense.
///
/// A margin of one screen height above/below the viewport prevents pop-in
/// during fast scroll — same predicate as the `Visibility`-only culling this
/// replaces.
///
/// One exception forces an entity `Display::Flex` regardless of the window:
/// **streaming while offscreen** (`BlockCell.last_render_version >
/// row.measured_version`): a block's measured height goes stale once it's
/// `Display::None`, since `readback_block_heights` only remeasures
/// `Display::Flex` entities. Forcing one extra frame of `Display::Flex`
/// lets readback catch up before the block re-enters the window — otherwise
/// the stale height would produce a visible scrollbar/content jump. In
/// practice this is rare and geometrically local: streaming blocks are
/// appended at the document tail, which is exactly where a follow-mode
/// viewport already sits.
///
/// Never-measured rows get **no** exception (unlike the pre-geometry
/// version of this system, which force-showed them and paid an O(N) layout
/// pass on first load of a long conversation): their estimated
/// height/y_offset from [`ConversationGeometry`] place them in or out of
/// the window immediately, and the estimate is replaced by a real
/// measurement just-in-time as the row scrolls into the window.
pub fn virtualize_conversation(
    entities: Res<EditorEntities>,
    scroll_state: Res<ConversationScrollState>,
    containers: Query<&BlockCellContainer>,
    geometries: Query<&ConversationGeometry>,
    role_header_query: Query<&RoleGroupBorder>,
    mut block_cells: Query<(&BlockCell, &mut Node, &mut Visibility), With<BlockCell>>,
    mut role_headers: Query<
        (&mut Node, &mut Visibility),
        (With<RoleGroupBorder>, Without<BlockCell>),
    >,
    // Crossed Withouts against BOTH other `&mut Node` queries — a spacer is
    // never a block cell or a role header, but B0001 needs the static proof
    // (the startup panic the unit suite can't catch; schedules never init
    // in tests).
    mut spacers: Query<
        (&ConversationSpacer, &mut Node),
        (Without<BlockCell>, Without<RoleGroupBorder>),
    >,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(geom) = geometries.get(main_ent) else {
        return;
    };
    let Ok(container) = containers.get(main_ent) else {
        return;
    };

    let mut header_map: std::collections::HashMap<kaijutsu_crdt::BlockId, Entity> =
        std::collections::HashMap::new();
    for &header_ent in &container.role_headers {
        if let Ok(header) = role_header_query.get(header_ent) {
            header_map.insert(header.block_id, header_ent);
        }
    }

    let margin = scroll_state.visible_height;
    let top = scroll_state.offset - margin;
    let bottom = scroll_state.offset + scroll_state.visible_height + margin;

    let mut shown: Vec<(f32, f32, f32)> = Vec::new();

    for row in geom.rows() {
        let in_window = row.y_offset + row.height >= top && row.y_offset <= bottom;

        match row.key {
            RowKey::Block(id) => {
                let Some(entity) = container.get_entity(&id) else {
                    continue;
                };
                let Ok((block_cell, mut node, mut vis)) = block_cells.get_mut(entity) else {
                    continue;
                };
                let stale = block_cell
                    .last_render_version
                    .is_some_and(|v| v > row.measured_version);
                let should_show = in_window || stale;

                set_display(&mut node, if should_show { Display::Flex } else { Display::None });
                set_visibility(
                    &mut vis,
                    if should_show {
                        Visibility::Inherited
                    } else {
                        Visibility::Hidden
                    },
                );

                if should_show {
                    shown.push((row.y_offset, row.height, row.margin_bottom));
                }
            }
            RowKey::Header(id) => {
                let Some(&entity) = header_map.get(&id) else {
                    continue;
                };
                let Ok((mut node, mut vis)) = role_headers.get_mut(entity) else {
                    continue;
                };

                set_display(&mut node, if in_window { Display::Flex } else { Display::None });
                set_visibility(
                    &mut vis,
                    if in_window {
                        Visibility::Inherited
                    } else {
                        Visibility::Hidden
                    },
                );

                if in_window {
                    shown.push((row.y_offset, row.height, row.margin_bottom));
                }
            }
        }
    }

    // Write ONLY this container's two spacers, looked up through
    // `EditorEntities`. A global `spacers.iter_mut()` would clobber every
    // pane's spacers in a split view — overwriting background panes' heights
    // with the focused pane's geometry.
    let (top_h, bottom_h) = compute_spacer_heights(&shown, geom.content_height);
    if let Some(top_ent) = entities.top_spacer
        && let Ok((_, mut node)) = spacers.get_mut(top_ent)
    {
        let target = Val::Px(top_h);
        if node.height != target {
            node.height = target;
        }
    }
    if let Some(bottom_ent) = entities.bottom_spacer
        && let Ok((_, mut node)) = spacers.get_mut(bottom_ent)
    {
        let target = Val::Px(bottom_h);
        if node.height != target {
            node.height = target;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::SpacerEdge;
    use kaijutsu_crdt::{BlockId, BlockKind, ContextId, PrincipalId, Role};

    fn test_block_id(seq: u64) -> BlockId {
        BlockId::new(ContextId::new(), PrincipalId::new(), seq)
    }

    /// Build geometry rows for `(id, role)` text blocks via the production
    /// reconcile path — the same row structure the live systems walk.
    fn rows_for(blocks: &[(BlockId, Role)]) -> Vec<GeomRow> {
        use crate::view::geometry::{EstimateParams, RowSeed};
        let mut geom = ConversationGeometry::default();
        let ids: Vec<BlockId> = blocks.iter().map(|(id, _)| *id).collect();
        let roles: std::collections::HashMap<BlockId, Role> =
            blocks.iter().copied().collect();
        geom.reconcile(
            &ids,
            |id| {
                Some(RowSeed {
                    text_len: 10,
                    newline_count: 0,
                    role: roles[id],
                    kind: BlockKind::Text,
                    collapsed: false,
                    parent_id: None,
                })
            },
            &EstimateParams::default(),
            1,
        );
        geom.recompute_offsets();
        geom.rows().to_vec()
    }

    // ------------------------------------------------------------------
    // compute_ordered_children
    // ------------------------------------------------------------------

    #[test]
    fn compute_ordered_children_interleaves_header_at_role_transition() {
        let user_id = test_block_id(0);
        let model_id = test_block_id(1);
        let rows = rows_for(&[(user_id, Role::User), (model_id, Role::Model)]);

        let mut container = BlockCellContainer::default();
        let user_ent = Entity::from_raw_u32(1).unwrap();
        let model_ent = Entity::from_raw_u32(2).unwrap();
        container.add(user_id, user_ent);
        container.add(model_id, model_ent);

        // Only the Model transition has a spawned header entity — the User
        // header row exists in geometry but resolves to no entity (spawn
        // lag), which must be skipped silently.
        let header_ent = Entity::from_raw_u32(3).unwrap();
        let mut header_map = std::collections::HashMap::new();
        header_map.insert(model_id, header_ent);

        let top_spacer = Entity::from_raw_u32(4).unwrap();
        let bottom_spacer = Entity::from_raw_u32(5).unwrap();

        let mut missing = Vec::new();
        let ordered = compute_ordered_children(
            &rows,
            &container,
            &header_map,
            top_spacer,
            bottom_spacer,
            |id| missing.push(*id),
        );

        assert!(missing.is_empty());
        // No header entity for the first (User) block, so it opens the
        // list; the Model transition's header is interleaved before it.
        // The spacers always bracket the interleaved list.
        assert_eq!(
            ordered,
            vec![top_spacer, user_ent, header_ent, model_ent, bottom_spacer]
        );
    }

    #[test]
    fn compute_ordered_children_reports_block_with_no_container_entry() {
        let present_id = test_block_id(0);
        let missing_id = test_block_id(1);
        let rows = rows_for(&[(present_id, Role::User), (missing_id, Role::User)]);

        let mut container = BlockCellContainer::default();
        let present_ent = Entity::from_raw_u32(1).unwrap();
        container.add(present_id, present_ent);
        // missing_id deliberately has no container entry — this is the
        // upstream-bug signature (spawn_block_cells lag / stale container).

        let top_spacer = Entity::from_raw_u32(2).unwrap();
        let bottom_spacer = Entity::from_raw_u32(3).unwrap();

        let mut missing = Vec::new();
        let ordered = compute_ordered_children(
            &rows,
            &container,
            &std::collections::HashMap::new(),
            top_spacer,
            bottom_spacer,
            |id| missing.push(*id),
        );

        assert_eq!(ordered, vec![top_spacer, present_ent, bottom_spacer]);
        assert_eq!(missing, vec![missing_id]);
    }

    // ------------------------------------------------------------------
    // compute_spacer_heights
    // ------------------------------------------------------------------

    #[test]
    fn compute_spacer_heights_empty_shown_list_yields_zero_both() {
        assert_eq!(compute_spacer_heights(&[], 1000.0), (0.0, 0.0));
    }

    #[test]
    fn compute_spacer_heights_first_shown_at_document_top_yields_zero_top() {
        // The topmost entity in the document is itself shown, so nothing
        // is virtualized out above it — top spacer collapses to 0.
        let shown = vec![(0.0, 50.0, 8.0), (58.0, 30.0, 8.0)];
        let (top, _bottom) = compute_spacer_heights(&shown, 200.0);
        assert_eq!(top, 0.0);
    }

    #[test]
    fn compute_spacer_heights_middle_window_computes_both_gaps() {
        // Document: [0..100) virtualized out, [100..180) the shown window,
        // [180..500) virtualized out below it.
        let shown = vec![(100.0, 50.0, 5.0), (155.0, 20.0, 5.0)]; // last ends at 180
        let (top, bottom) = compute_spacer_heights(&shown, 500.0);
        assert_eq!(top, 100.0);
        assert_eq!(bottom, 500.0 - 180.0);
    }

    #[test]
    fn compute_spacer_heights_last_shown_at_document_bottom_yields_zero_bottom() {
        let shown = vec![(900.0, 100.0, 0.0)];
        let (_top, bottom) = compute_spacer_heights(&shown, 1000.0);
        assert_eq!(bottom, 0.0);
    }

    // ------------------------------------------------------------------
    // find_orphaned_children
    // ------------------------------------------------------------------

    #[test]
    fn find_orphaned_children_ignores_entities_still_in_ordered_list() {
        let a = Entity::from_raw_u32(1).unwrap();
        let b = Entity::from_raw_u32(2).unwrap();
        let orphans =
            find_orphaned_children(&[a, b], &[b, a], |_| true, |_| false, |_| false);
        assert!(orphans.is_empty());
    }

    #[test]
    fn find_orphaned_children_flags_live_block_cell_missing_from_order() {
        let kept = Entity::from_raw_u32(1).unwrap();
        let leaked = Entity::from_raw_u32(2).unwrap();
        // `leaked` is a live BlockCell that fell out of `ordered_children`
        // (e.g. missing container entry) — this is the leak this function
        // exists to catch, per the diagnosed replace_children orphan bug.
        let orphans = find_orphaned_children(
            &[kept, leaked],
            &[kept],
            |e| e == leaked,
            |_| false,
            |_| false,
        );
        assert_eq!(orphans, vec![leaked]);
    }

    #[test]
    fn find_orphaned_children_flags_live_role_header_missing_from_order() {
        let kept = Entity::from_raw_u32(1).unwrap();
        let leaked = Entity::from_raw_u32(2).unwrap();
        let orphans = find_orphaned_children(
            &[kept, leaked],
            &[kept],
            |_| false,
            |e| e == leaked,
            |_| false,
        );
        assert_eq!(orphans, vec![leaked]);
    }

    #[test]
    fn find_orphaned_children_leaves_neither_kind_alone() {
        // An entity that fell out of order but is neither a live BlockCell
        // nor a live RoleGroupBorder (e.g. already despawned, or some other
        // node type) is not this function's problem to solve — it's not
        // flagged.
        let kept = Entity::from_raw_u32(1).unwrap();
        let other = Entity::from_raw_u32(2).unwrap();
        let orphans = find_orphaned_children(
            &[kept, other],
            &[kept],
            |_| false,
            |_| false,
            |_| false,
        );
        assert!(orphans.is_empty());
    }

    #[test]
    fn find_orphaned_children_never_flags_a_spacer_even_if_it_matches_block_or_header() {
        // Defense-in-depth per the review correction: a ConversationSpacer
        // must never be swept as an orphan, even if it somehow also matched
        // the block-cell/role-header predicates (it never does in practice
        // — spacers are always included in `ordered_children` — but the
        // exclusion must win if that invariant is ever violated upstream).
        let kept = Entity::from_raw_u32(1).unwrap();
        let spacer = Entity::from_raw_u32(2).unwrap();
        let orphans = find_orphaned_children(
            &[kept, spacer],
            &[kept],
            |e| e == spacer,
            |_| false,
            |e| e == spacer,
        );
        assert!(orphans.is_empty());
    }

    // ------------------------------------------------------------------
    // reorder_conversation_children — full-system integration test
    // ------------------------------------------------------------------
    //
    // Exercises the real system (not just the extracted pure helpers) over
    // a minimal headless `App`: no rendering plugins are registered, only
    // the ECS resources/components reorder_conversation_children touches.
    // Bevy's parent/child relationship (ChildOf/Children) is core ECS
    // (component hooks), not a plugin, so `add_child`/`replace_children`
    // work without DefaultPlugins.

    use kaijutsu_crdt::{ContentType, Status};

    fn build_test_app() -> App {
        let mut app = App::new();
        app.init_resource::<EditorEntities>();
        app.init_resource::<LayoutGeneration>();
        app.init_resource::<ConversationScrollState>();
        app.init_resource::<crate::text::TextMetrics>();
        app.init_resource::<Theme>();
        // Production ordering: geometry reconciles before the reorder walks
        // its rows (CellPhase::Spawn precedes CellPhase::Layout in the real
        // schedule).
        app.add_systems(
            Update,
            (
                crate::view::geometry::sync_conversation_geometry,
                reorder_conversation_children,
            )
                .chain(),
        );
        app
    }

    /// Spawn a MainCell + CellEditor with `n` text blocks inserted in
    /// document order, plus matching BlockCell entities parented under a
    /// fresh conversation container bracketed by top/bottom spacers (in
    /// that same order — the "everything already agrees" starting point
    /// each test then perturbs).
    fn seed_conversation(app: &mut App, block_count: usize) -> (Vec<BlockId>, Entity) {
        let mut editor = CellEditor::new();
        let mut ids = Vec::with_capacity(block_count);
        for i in 0..block_count {
            let id = editor
                .store
                .insert_block(
                    None,
                    ids.last(),
                    Role::User,
                    BlockKind::Text,
                    format!("block {i}"),
                    Status::Done,
                    ContentType::Plain,
                )
                .expect("insert_block");
            ids.push(id);
        }

        let main_ent = app
            .world_mut()
            .spawn((editor, MainCell, ConversationGeometry::default()))
            .id();

        let mut container = BlockCellContainer::default();
        let mut cell_entities = Vec::with_capacity(block_count);
        for &id in &ids {
            let ent = app.world_mut().spawn(BlockCell::new(id)).id();
            container.add(id, ent);
            cell_entities.push(ent);
        }
        app.world_mut().entity_mut(main_ent).insert(container);

        let top_spacer = app
            .world_mut()
            .spawn(ConversationSpacer {
                edge: SpacerEdge::Top,
            })
            .id();
        let bottom_spacer = app
            .world_mut()
            .spawn(ConversationSpacer {
                edge: SpacerEdge::Bottom,
            })
            .id();

        let conv_ent = app.world_mut().spawn(Node::default()).id();
        app.world_mut().entity_mut(conv_ent).add_child(top_spacer);
        app.world_mut()
            .entity_mut(conv_ent)
            .add_children(&cell_entities);
        app.world_mut()
            .entity_mut(conv_ent)
            .add_child(bottom_spacer);

        {
            let mut entities = app.world_mut().resource_mut::<EditorEntities>();
            entities.main_cell = Some(main_ent);
            entities.conversation_container = Some(conv_ent);
            entities.top_spacer = Some(top_spacer);
            entities.bottom_spacer = Some(bottom_spacer);
        }

        (ids, conv_ent)
    }

    fn children_of(app: &App, entity: Entity) -> Vec<Entity> {
        app.world()
            .get::<Children>(entity)
            .map(|c| c.iter().collect())
            .unwrap_or_default()
    }

    #[test]
    fn reorder_repairs_children_after_order_only_change() {
        let mut app = build_test_app();
        let (ids, conv_ent) = seed_conversation(&mut app, 3);

        // Starting order already matches document order — confirm the
        // baseline, then run once so `last_gen` catches up to gen 0 (no
        // bump has happened yet, so the system should no-op).
        app.update();
        assert_eq!(children_of(&app, conv_ent), {
            let entities = app.world().resource::<EditorEntities>();
            let (top_spacer, bottom_spacer) =
                (entities.top_spacer.unwrap(), entities.bottom_spacer.unwrap());
            let container = app
                .world()
                .get::<BlockCellContainer>(entities.main_cell.unwrap())
                .unwrap();
            let mut expected = vec![top_spacer];
            expected.extend(ids.iter().map(|id| container.get_entity(id).unwrap()));
            expected.push(bottom_spacer);
            expected
        });

        // Reposition the last block to the front via move_block — a pure
        // order change with no additions/removals, same shape as a server
        // BlockMoved or a merge reposition.
        let main_ent = app.world().resource::<EditorEntities>().main_cell.unwrap();
        {
            let mut editor = app.world_mut().get_mut::<CellEditor>(main_ent).unwrap();
            editor.store.move_block(&ids[2], None).expect("move_block");
        }

        // Simulate spawn_block_cells's job: resort the container to match
        // the new document order and bump LayoutGeneration (fix #1).
        let new_order = {
            let editor = app.world().get::<CellEditor>(main_ent).unwrap();
            editor.block_ids()
        };
        {
            let mut container = app
                .world_mut()
                .get_mut::<BlockCellContainer>(main_ent)
                .unwrap();
            let changed = container.resort_to_document_order(&new_order);
            assert!(changed, "order-only move should be detected as a change");
        }
        app.world_mut().resource_mut::<LayoutGeneration>().bump();

        app.update();

        let (top_spacer, bottom_spacer) = {
            let entities = app.world().resource::<EditorEntities>();
            (entities.top_spacer.unwrap(), entities.bottom_spacer.unwrap())
        };
        let container = app.world().get::<BlockCellContainer>(main_ent).unwrap();
        let mut expected: Vec<Entity> = vec![top_spacer];
        expected.extend(new_order.iter().map(|id| container.get_entity(id).unwrap()));
        expected.push(bottom_spacer);
        assert_eq!(
            children_of(&app, conv_ent),
            expected,
            "reorder_conversation_children must repair Children to match the new document order"
        );
    }

    /// The Slice-B invariant: on the very first frame of a long conversation
    /// — nothing measured, every row an estimate — virtualize must show only
    /// the viewport window, NOT force-show never-measured blocks. (The
    /// pre-geometry implementation force-showed all N, which is exactly the
    /// O(N) first-load layout pass this model exists to kill.)
    #[test]
    fn virtualize_first_frame_shows_window_not_all_blocks() {
        let mut app = App::new();
        app.init_resource::<EditorEntities>();
        app.init_resource::<LayoutGeneration>();
        app.init_resource::<crate::text::TextMetrics>();
        app.init_resource::<Theme>();
        app.insert_resource(ConversationScrollState {
            offset: 0.0,
            visible_height: 300.0,
            ..default()
        });
        app.add_systems(
            Update,
            (
                crate::view::geometry::sync_conversation_geometry,
                virtualize_conversation,
            )
                .chain(),
        );

        let (ids, _conv) = seed_conversation(&mut app, 50);
        let main_ent = app.world().resource::<EditorEntities>().main_cell.unwrap();

        // Give block entities the components virtualize toggles, and the
        // spacers the Node it writes.
        let block_ents: Vec<Entity> = {
            let container = app.world().get::<BlockCellContainer>(main_ent).unwrap();
            ids.iter().map(|id| container.get_entity(id).unwrap()).collect()
        };
        for &ent in &block_ents {
            app.world_mut()
                .entity_mut(ent)
                .insert((Node::default(), Visibility::default()));
        }
        let (top_spacer, bottom_spacer) = {
            let e = app.world().resource::<EditorEntities>();
            (e.top_spacer.unwrap(), e.bottom_spacer.unwrap())
        };
        app.world_mut().entity_mut(top_spacer).insert(Node::default());
        app.world_mut().entity_mut(bottom_spacer).insert(Node::default());

        app.update();

        // Estimates: header 20+4, each one-line block 30+12 → window
        // [-300, 600] shows the first ~14 blocks; the rest must be
        // Display::None on frame one, with the bottom spacer standing in.
        let flex: Vec<usize> = block_ents
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                app.world().get::<Node>(**e).unwrap().display == Display::Flex
            })
            .map(|(i, _)| i)
            .collect();
        assert!(
            !flex.is_empty() && flex.len() < 20,
            "expected only the estimated window shown on frame one, got {} of {} blocks Flex",
            flex.len(),
            block_ents.len(),
        );
        assert_eq!(flex[0], 0, "window at scroll top must start at block 0");
        assert!(
            flex.windows(2).all(|w| w[1] == w[0] + 1),
            "shown set must be contiguous, got {flex:?}",
        );

        let bottom_node = app.world().get::<Node>(bottom_spacer).unwrap();
        let Val::Px(bottom_px) = bottom_node.height else {
            panic!("bottom spacer height must be set in px");
        };
        assert!(
            bottom_px > 1000.0,
            "bottom spacer must stand in for the ~36 estimated-out blocks, got {bottom_px}",
        );
    }

    #[test]
    fn reorder_despawns_orphaned_block_cell_instead_of_leaking_it() {
        let mut app = build_test_app();
        let (ids, conv_ent) = seed_conversation(&mut app, 2);
        let main_ent = app.world().resource::<EditorEntities>().main_cell.unwrap();

        // Remove the second block's container entry without despawning its
        // entity or removing it from editor.blocks() — simulating the
        // orphan-leak bug: replace_children would otherwise silently drop
        // this live BlockCell into a root UI node.
        let orphan_ent = {
            let container = app.world().get::<BlockCellContainer>(main_ent).unwrap();
            container.get_entity(&ids[1]).unwrap()
        };
        {
            let mut container = app
                .world_mut()
                .get_mut::<BlockCellContainer>(main_ent)
                .unwrap();
            container.remove(orphan_ent);
        }

        app.world_mut().resource_mut::<LayoutGeneration>().bump();
        app.update();

        assert!(
            app.world().get_entity(orphan_ent).is_err(),
            "orphaned BlockCell must be despawned, not left as a leaked root UI node"
        );
        assert!(
            !children_of(&app, conv_ent).contains(&orphan_ent),
            "orphaned BlockCell must not remain a child of the conversation container"
        );
    }
}
