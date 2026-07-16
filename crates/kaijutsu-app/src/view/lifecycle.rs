//! Entity lifecycle — spawn and despawn block cell entities.
//!
//! Owns spawn_main_cell, spawn_block_cells, sync_role_headers, and
//! track_conversation_container. Block cells use BlockScene + BlockTexture
//! + ImageNode for CPU-rasterized Vello rendering with Bevy flex layout.

use bevy::prelude::*;
use crate::text::shaping::VelloFont;

use crate::cell::{
    BlockCell, BlockCellContainer, BlockCellLayout, CellEditor, ConversationSpacer,
    FocusTarget, LayoutGeneration, MainCell, RoleGroupBorder, RoleGroupBorderLayout, SpacerEdge,
};
use crate::shaders::BlockFxMaterial;
use crate::view::geometry::{ConversationGeometry, plan_block_band, plan_header_band};

/// Consolidated resource tracking editor-related singleton entities.
#[derive(Resource, Default)]
pub struct EditorEntities {
    /// The main conversation cell entity.
    pub main_cell: Option<Entity>,
    /// The ConversationContainer entity (flex parent for BlockCells).
    pub conversation_container: Option<Entity>,
    /// The top spacer entity — first child of `conversation_container`,
    /// its `Node.height` stands in for virtualized-out content above the
    /// visible window. See `ensure_conversation_spacers`.
    pub top_spacer: Option<Entity>,
    /// The bottom spacer entity — last child of `conversation_container`.
    /// See `ensure_conversation_spacers`.
    pub bottom_spacer: Option<Entity>,
}
use crate::text::ShapingFonts;
use crate::ui::timeline::TimelineVisibility;

// ============================================================================
// LAYOUT CONSTANTS (legacy — migrated to Theme, kept for non-theme-aware code paths)
// ============================================================================

/// Horizontal indentation per nesting level (for nested tool results, etc.)
/// **Prefer `theme.indent_width`** in systems that have `Res<Theme>`.
#[allow(dead_code)] // Legacy constant; theme field is the source of truth
pub(crate) const INDENT_WIDTH: f32 = 24.0;

/// Height reserved for role transition headers (e.g., "User", "Assistant").
/// **Prefer `theme.role_header_height`** in systems that have `Res<Theme>`.
pub(crate) const ROLE_HEADER_HEIGHT: f32 = 20.0;

/// Spacing between role header and block content.
/// **Prefer `theme.role_header_spacing`** in systems that have `Res<Theme>`.
pub(crate) const ROLE_HEADER_SPACING: f32 = 4.0;

// ============================================================================
// SPAWN SYSTEMS
// ============================================================================

/// Spawn the main kernel cell on startup.
///
/// This is the primary workspace cell that displays kernel output, shell interactions,
/// and agent conversations. It fills the space between the header and prompt.
pub fn spawn_main_cell(
    mut commands: Commands,
    mut entities: ResMut<EditorEntities>,
    conversation_container: Query<Entity, Added<crate::cell::ConversationContainer>>,
) {
    if entities.main_cell.is_some() {
        return;
    }

    let Ok(conv_entity) = conversation_container.single() else {
        return;
    };

    entities.conversation_container = Some(conv_entity);

    let welcome_text = "No context joined";

    // MainCell does NOT get UiVelloText directly.
    // The BlockCell system handles per-block rendering.
    // MainCell only holds the CellEditor (source of truth for content).
    let entity = commands
        .spawn((
            CellEditor::default().with_text(welcome_text),
            MainCell,
            // The logical geometry model rides with the MainCell from birth
            // so every geometry-reading system (reorder, virtualize,
            // readback) finds it on the very first LayoutGeneration bump —
            // a deferred insert would eat that generation.
            crate::view::geometry::ConversationGeometry::default(),
        ))
        .id();

    entities.main_cell = Some(entity);
    info!("Spawned main kernel cell");
}

/// Track the focused ConversationContainer and re-parent block cells when it changes.
///
/// After a pane split, the reconciler despawns and rebuilds all PaneMarker entities.
/// This orphans block cells from the old container. This system detects when the
/// focused ConversationContainer changes (new entity with PaneFocus) and:
/// 1. Updates `EditorEntities.conversation_container`
/// 2. Re-parents existing block cells + role headers to the new container
pub fn track_conversation_container(
    mut commands: Commands,
    mut entities: ResMut<EditorEntities>,
    focused_containers: Query<
        Entity,
        (
            With<crate::cell::ConversationContainer>,
            With<crate::ui::tiling::PaneFocus>,
        ),
    >,
    containers: Query<&BlockCellContainer>,
    mut layout_gen: ResMut<LayoutGeneration>,
) {
    let Ok(focused) = focused_containers.single() else {
        return;
    };

    if entities.conversation_container == Some(focused) {
        return;
    }

    let Some(main_ent) = entities.main_cell else {
        return;
    };

    // RE-PARENTING: When the container changes (e.g. pane split),
    // all existing block cells must move to the new container. This
    // re-adds block cells then role headers, which interleaves them out of
    // document order — bump LayoutGeneration so sync_role_headers +
    // reorder_conversation_children repair the interleave on the next
    // frames instead of leaving it stuck until app restart.
    if let Ok(container) = containers.get(main_ent) {
        trace!(
            "Re-parenting {} block cells to new container {:?}",
            container.block_cells.len(),
            focused
        );
        for &block_ent in container.block_cells.values() {
            commands.entity(focused).add_child(block_ent);
        }
        for &header_ent in &container.role_headers {
            commands.entity(focused).add_child(header_ent);
        }
    }

    entities.conversation_container = Some(focused);
    layout_gen.bump();
}

/// Maintain the "the *focused* conversation container always has exactly one
/// Top and one Bottom spacer of its own" invariant.
///
/// The tiling reconciler rebuilds panes by despawning old ConversationContainer
/// entities recursively (`ui/tiling_reconciler.rs`), which takes any spacer
/// children down with it; `track_conversation_container` re-parents block
/// cells + role headers onto the new container but has no reference to
/// spacers to carry forward. Rather than special-casing spacer survival at
/// every site that can change `conversation_container`, this system runs
/// every frame and lazily (re)spawns whichever spacer is missing or dead —
/// so pane splits, container rebuilds, and first-ever creation all funnel
/// through one invariant-restoring path instead of N one-time spawn sites.
///
/// A spacer is only "still valid" if it is alive AND actually a child of the
/// *current* `conversation_container`. Without the parentage check, a spacer
/// that still belongs to a previously-focused pane would be treated as valid
/// on a focus switch, and `reorder_conversation_children` would then steal it
/// onto the new container — leaving the old pane spacer-less. There is one
/// ConversationContainer per pane (`tiling_reconciler` spawns them per-pane)
/// but `EditorEntities` tracks only the focused one, so re-anchoring the
/// tracked pair to the current container on every focus change is required.
pub fn ensure_conversation_spacers(
    mut commands: Commands,
    mut entities: ResMut<EditorEntities>,
    spacers: Query<Entity, With<ConversationSpacer>>,
    child_of: Query<&ChildOf>,
) {
    let Some(conv_entity) = entities.conversation_container else {
        return;
    };

    // Valid = alive, a ConversationSpacer, and parented to the current
    // container (ChildOf::parent() is Bevy 0.18's child→parent link).
    let is_valid = |ent: Option<Entity>| {
        ent.is_some_and(|e| {
            spacers.contains(e)
                && child_of.get(e).is_ok_and(|c| c.parent() == conv_entity)
        })
    };

    if !is_valid(entities.top_spacer) {
        let top = commands
            .spawn((
                ConversationSpacer {
                    edge: SpacerEdge::Top,
                },
                Node {
                    width: Val::Percent(100.0),
                    height: Val::Px(0.0),
                    ..default()
                },
            ))
            .id();
        commands.entity(conv_entity).add_child(top);
        entities.top_spacer = Some(top);
    }

    if !is_valid(entities.bottom_spacer) {
        let bottom = commands
            .spawn((
                ConversationSpacer {
                    edge: SpacerEdge::Bottom,
                },
                Node {
                    width: Val::Percent(100.0),
                    height: Val::Px(0.0),
                    ..default()
                },
            ))
            .id();
        commands.entity(conv_entity).add_child(bottom);
        entities.bottom_spacer = Some(bottom);
    }
}

/// Spawn or update BlockCell entities to match the MainCell's BlockStore.
///
/// This system diffs the current block IDs against existing BlockCell entities:
/// - Spawns new BlockCells for added blocks
/// - Despawns BlockCells for removed blocks
/// - Maintains order in BlockCellContainer
///
/// **Single-phase spawning:** BlockScene + BlockTexture + ImageNode are
/// included in the initial spawn bundle. Spawning is gated on font
/// availability — block cells appear once the font asset loads
/// (typically within the first few frames).
pub fn spawn_block_cells(
    mut commands: Commands,
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    mut containers: Query<&mut BlockCellContainer>,
    geometries: Query<&ConversationGeometry>,
    existing_block_cells: Query<Entity, With<BlockCell>>,
    mut layout_gen: ResMut<LayoutGeneration>,
    font_handles: Res<ShapingFonts>,
    fonts: Res<Assets<VelloFont>>,
    mut scroll_state: ResMut<crate::cell::ConversationScrollState>,
    focus: Res<FocusTarget>,
    mut fx_materials: ResMut<Assets<BlockFxMaterial>>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };

    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };

    let mut container = if let Ok(c) = containers.get_mut(main_ent) {
        c
    } else {
        commands
            .entity(main_ent)
            .insert(BlockCellContainer::default());
        return;
    };

    let current_blocks = editor.block_ids();
    let current_ids: std::collections::HashSet<_> = current_blocks.iter().collect();

    // Purge stale entity references — the tiling reconciler despawns pane
    // children recursively, which kills block cells without telling the container.
    let live_count = existing_block_cells.iter().count();
    let container_count = container.block_cells.len();
    if container_count > 0 && live_count != container_count {
        warn!(
            "spawn_block_cells: container has {} refs, {} alive BlockCells, {} editor blocks",
            container_count,
            live_count,
            current_blocks.len()
        );
    }
    let live_entities: std::collections::HashSet<Entity> = existing_block_cells.iter().collect();
    let stale: Vec<Entity> = container
        .block_cells
        .values()
        .filter(|e| !live_entities.contains(e))
        .copied()
        .collect();
    if !stale.is_empty() {
        warn!(
            "Purging {} stale block cell refs from container",
            stale.len()
        );
        for entity in stale {
            container.remove(entity);
        }
    }

    // Despawn blocks that are no longer in the editor (context switch, welcome→real, etc.)
    let to_remove: Vec<_> = container
        .block_cells
        .iter()
        .filter(|(id, _)| !current_ids.contains(id))
        .map(|(_, e)| *e)
        .collect();

    let had_removals = !to_remove.is_empty();
    for entity in to_remove {
        commands.entity(entity).try_despawn();
        container.remove(entity);
    }

    if current_blocks.is_empty() {
        // No blocks in the editor — nothing to spawn.
        // Stale cells from the previous context were already cleaned up above.
        if had_removals {
            layout_gen.bump();
        }
        return;
    }

    // Log diagnostics once when blocks first appear or counts change
    {
        static LAST_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let key = (current_blocks.len() as u64)
            .wrapping_mul(1000003)
            .wrapping_add(container.block_cells.len() as u64);
        if LAST_LOG.swap(key, std::sync::atomic::Ordering::Relaxed) != key {
            info!(
                "spawn_block_cells: editor has {} blocks, container has {} cells, conv={:?}",
                current_blocks.len(),
                container.block_cells.len(),
                entities.conversation_container.map(|e| e.index()),
            );
        }
    }

    // Band plan: entities exist only for rows around the viewport
    // (spawn inside ±SPAWN_MARGIN_SCREENS, despawn beyond
    // ±DESPAWN_MARGIN_SCREENS — see geometry.rs). The focused block is
    // exempt from despawn so FocusedBlockCell survives scrolling away.
    let Ok(geom) = geometries.get(main_ent) else {
        // Geometry rides the MainCell spawn bundle — its absence is a bug,
        // not a fallback path. Spawning all blocks here would silently
        // reintroduce the O(N)-entities behavior this band exists to kill.
        warn!("spawn_block_cells: MainCell has no ConversationGeometry; skipping spawn/despawn");
        return;
    };
    let plan = plan_block_band(
        geom.rows(),
        |id| container.contains(id),
        scroll_state.offset,
        scroll_state.visible_height,
        focus.block_id,
    );

    // Despawn beyond the keep band BEFORE the font gate — reclaiming render
    // resources (RTT texture, glyph buffers, scenes ride the entity) needs
    // no fonts. Measured heights persist in the geometry rows.
    let had_band_despawns = !plan.to_despawn.is_empty();
    for id in &plan.to_despawn {
        if let Some(entity) = container.get_entity(id) {
            commands.entity(entity).try_despawn();
            container.remove(entity);
        }
    }

    // Gate new spawns on font availability — build_block_scenes needs a valid
    // font to render text into the Vello scene. The plan is recomputed next
    // frame, so waiting loses nothing.
    let font_loaded = fonts.get(&font_handles.mono).is_some();

    let current_version = editor.version();
    let conv_entity = entities.conversation_container;

    let mut had_additions = false;
    let mut added_new_tail_blocks = false;

    if font_loaded {
        for block_id in &plan.to_spawn {
            // Single-phase spawn: BlockCell + BlockScene + BlockTexture + ImageNode + MaterialNode.
            // The GpuImage is prepared asynchronously by Bevy's RenderAssetPlugin, reacting
            // to AssetEvent::Modified/Created on the Image asset — one frame after this
            // spawn, not synchronously with it. ImageNode itself doesn't force preparation.
            // MaterialNode renders on top with shader effects (glow, animation).
            let material_handle = fx_materials.add(BlockFxMaterial::default());

            // Seed layout from the geometry row (sync_conversation_geometry
            // runs earlier this frame): estimated/cached height + document
            // y_offset place the block correctly for virtualize_conversation
            // BEFORE its first readback — an appended block enters at the
            // tail, a respawned one back at its old measured position, with
            // no one-frame spacer blowup.
            let row = geom.block_row(block_id);
            let layout_seed = BlockCellLayout {
                y_offset: row
                    .map(|r| r.y_offset)
                    .unwrap_or(scroll_state.content_height),
                height: row.map(|r| r.height).unwrap_or(0.0),
                indent_level: row.map(|r| r.indent_level).unwrap_or(0),
                last_measured_version: row.map(|r| r.measured_version).unwrap_or(0),
            };
            // A row minted at the current document version is genuinely new
            // content (streamed/appended); anything older is a respawn of a
            // block scrolling back into the band and must NOT trigger the
            // new-content scroll anchor.
            let is_new_content = row.is_some_and(|r| r.created_at_version == current_version);
            let entity = commands
                .spawn((
                    BlockCell::new(*block_id),
                    layout_seed,
                    crate::view::block_render::BlockScene::default(),
                    crate::view::ui_rtt::UiVectorScene::default(),
                    crate::view::ui_rtt::UiRttTexture::default(),
                    crate::text::msdf::MsdfBlockGlyphs::default(),
                    crate::text::msdf::BlockRenderMethod::default(),
                    ImageNode::default(),
                    MaterialNode(material_handle),
                    Node {
                        width: Val::Percent(100.0),
                        ..default()
                    },
                    TimelineVisibility {
                        // Persisted in the geometry row so a despawned block
                        // respawns with its true creation version (timeline
                        // dimming would otherwise mis-classify it as new).
                        created_at_version: row
                            .map(|r| r.created_at_version)
                            .unwrap_or(current_version),
                        opacity: 1.0,
                        is_past: false,
                    },
                ))
                .id();
            if let Some(conv) = conv_entity
                && let Ok(mut ec) = commands.get_entity(conv)
            {
                ec.add_child(entity);
            }
            container.add(*block_id, entity);
            had_additions = true;
            added_new_tail_blocks |= is_new_content;
        }
    }

    // Reorder container.block_cells to match document order. A pure position
    // change (server BlockMoved, or a merge that repositions an existing
    // block) adds/removes nothing, so it must be detected here and folded
    // into the bump decision below — otherwise reorder_conversation_children
    // never re-runs and the visual order goes stale until app restart.
    let order_changed = container.resort_to_document_order(&current_blocks);

    if had_additions || had_removals || had_band_despawns || order_changed {
        info!(
            "spawn_block_cells: additions={} removals={} band_despawns={} order_changed={} container_now={}",
            had_additions,
            had_removals,
            plan.to_despawn.len(),
            order_changed,
            container.block_cells.len(),
        );
        layout_gen.bump();
    }

    if added_new_tail_blocks {
        scroll_state.new_blocks_added = true;
    }
}

/// Sync RoleGroupBorder entities for role transitions — band-gated and
/// incremental.
///
/// Header rows come from [`ConversationGeometry`] (same tool-block-skipping
/// rules, no whole-document snapshot clone). Like block cells, header
/// entities exist only inside the spawn band around the viewport; scrolled
/// far away they despawn to reclaim their textures, and their geometry rows
/// keep the measured heights. The reconcile is incremental — surviving
/// headers are never despawn/respawned (no flash), unlike the old
/// drain-everything rebuild.
///
/// Runs every frame (the band moves with scroll, which bumps no
/// generation); the plan walk is cheap and the common case changes nothing.
pub fn sync_role_headers(
    mut commands: Commands,
    entities: Res<EditorEntities>,
    geometries: Query<&ConversationGeometry>,
    mut containers: Query<&mut BlockCellContainer>,
    role_header_query: Query<&RoleGroupBorder>,
    scroll_state: Res<crate::cell::ConversationScrollState>,
    mut layout_gen: ResMut<LayoutGeneration>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };

    let Ok(geom) = geometries.get(main_ent) else {
        return;
    };

    let Ok(mut container) = containers.get_mut(main_ent) else {
        return;
    };

    // Live headers by the block id they precede. Also collect entries whose
    // geometry row vanished (role run dissolved) — those despawn regardless
    // of band.
    let header_keys: std::collections::HashSet<kaijutsu_crdt::BlockId> = geom
        .rows()
        .iter()
        .filter_map(|row| match row.key {
            crate::view::geometry::RowKey::Header(id) => Some(id),
            crate::view::geometry::RowKey::Block(_) => None,
        })
        .collect();

    let mut live: std::collections::HashMap<kaijutsu_crdt::BlockId, Entity> =
        std::collections::HashMap::new();
    let mut dead: Vec<Entity> = Vec::new();
    for &ent in &container.role_headers {
        match role_header_query.get(ent) {
            Ok(h) if header_keys.contains(&h.block_id) => {
                live.insert(h.block_id, ent);
            }
            // Despawned externally or no longer a transition.
            _ => dead.push(ent),
        }
    }

    let (to_spawn, to_despawn) = plan_header_band(
        geom.rows(),
        |id| live.contains_key(id),
        scroll_state.offset,
        scroll_state.visible_height,
    );

    if dead.is_empty() && to_spawn.is_empty() && to_despawn.is_empty() {
        return;
    }

    for ent in dead {
        commands.entity(ent).try_despawn();
        container.role_headers.retain(|e| *e != ent);
    }
    for id in &to_despawn {
        if let Some(ent) = live.remove(id) {
            commands.entity(ent).try_despawn();
            container.role_headers.retain(|e| *e != ent);
        }
    }

    for (role, block_id) in to_spawn {
        let entity = commands
            .spawn((
                RoleGroupBorder { role, block_id },
                RoleGroupBorderLayout::default(),
                crate::view::ui_rtt::UiVectorScene::default(),
                crate::view::ui_rtt::UiRttTexture::default(),
                ImageNode::default(),
                Node {
                    width: Val::Percent(100.0),
                    min_height: Val::Px(ROLE_HEADER_HEIGHT),
                    margin: UiRect::bottom(Val::Px(ROLE_HEADER_SPACING)),
                    ..default()
                },
            ))
            .id();
        if let Some(conv) = entities.conversation_container
            && let Ok(mut ec) = commands.get_entity(conv)
        {
            ec.add_child(entity);
        }

        container.role_headers.push(entity);
    }

    // Header set changed — reorder must run so the new entities land in
    // document position (and stale ones leave Children).
    layout_gen.bump();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::ConversationContainer;
    use crate::ui::tiling::PaneFocus;
    use kaijutsu_crdt::{BlockKind, ContentType, Role, Status};

    /// The Slice-C invariant: entities beyond the keep band despawn (their
    /// render resources with them) while the geometry rows retain every
    /// block's height — content_height must not move when entities go.
    #[test]
    fn band_despawns_far_offscreen_blocks_and_keeps_geometry() {
        let mut app = App::new();
        app.init_resource::<EditorEntities>();
        app.init_resource::<LayoutGeneration>();
        app.init_resource::<crate::text::TextMetrics>();
        app.init_resource::<crate::ui::theme::Theme>();
        app.init_resource::<FocusTarget>();
        app.init_resource::<ShapingFonts>();
        app.insert_resource(Assets::<VelloFont>::default());
        app.insert_resource(Assets::<BlockFxMaterial>::default());
        app.insert_resource(crate::cell::ConversationScrollState {
            offset: 0.0,
            visible_height: 300.0,
            ..Default::default()
        });
        app.add_systems(
            Update,
            (
                crate::view::geometry::sync_conversation_geometry,
                spawn_block_cells,
            )
                .chain(),
        );

        // 100 one-line blocks with an entity each (as if all had spawned
        // before the band existed).
        let mut editor = CellEditor::new();
        let mut ids = Vec::new();
        for i in 0..100 {
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
            .spawn((
                editor,
                MainCell,
                crate::view::geometry::ConversationGeometry::default(),
            ))
            .id();
        let mut container = BlockCellContainer::default();
        for &id in &ids {
            let ent = app.world_mut().spawn(BlockCell::new(id)).id();
            container.add(id, ent);
        }
        app.world_mut().entity_mut(main_ent).insert(container);
        app.world_mut().resource_mut::<EditorEntities>().main_cell = Some(main_ent);

        app.update();

        let container = app.world().get::<BlockCellContainer>(main_ent).unwrap();
        let kept = container.block_cells.len();
        assert!(
            kept < 100,
            "far-offscreen blocks must despawn (kept {kept} of 100)"
        );
        assert!(
            kept > 30,
            "the keep band (viewport + hysteresis) must survive (kept {kept})"
        );
        // The head of the document (viewport at top) must be kept.
        assert!(container.contains(&ids[0]));
        // The tail entity must be despawned — really gone from the world.
        let tail_gone = !container.contains(&ids[99]);
        assert!(tail_gone, "tail block must leave the container");

        // Geometry retains ALL rows and the full content height.
        let geom = app
            .world()
            .get::<crate::view::geometry::ConversationGeometry>(main_ent)
            .unwrap();
        let block_rows = geom
            .rows()
            .iter()
            .filter(|r| matches!(r.key, crate::view::geometry::RowKey::Block(_)))
            .count();
        assert_eq!(block_rows, 100, "geometry must keep every block row");
        // header(24) + 100 × (30 + 12) = 4224
        assert!(
            (geom.content_height - 4224.0).abs() < 1.0,
            "content height must be unaffected by despawn, got {}",
            geom.content_height,
        );

        // Despawns count as layout changes — reorder must get a bump.
        assert!(
            app.world().resource::<LayoutGeneration>().0 > 0,
            "band despawns must bump LayoutGeneration"
        );
    }

    /// The focused block is exempt from band despawn — FocusedBlockCell
    /// rides the entity and must survive scrolling far away.
    #[test]
    fn band_despawn_exempts_the_focused_block() {
        let mut app = App::new();
        app.init_resource::<EditorEntities>();
        app.init_resource::<LayoutGeneration>();
        app.init_resource::<crate::text::TextMetrics>();
        app.init_resource::<crate::ui::theme::Theme>();
        app.init_resource::<ShapingFonts>();
        app.insert_resource(Assets::<VelloFont>::default());
        app.insert_resource(Assets::<BlockFxMaterial>::default());
        app.insert_resource(crate::cell::ConversationScrollState {
            offset: 0.0,
            visible_height: 300.0,
            ..Default::default()
        });
        app.add_systems(
            Update,
            (
                crate::view::geometry::sync_conversation_geometry,
                spawn_block_cells,
            )
                .chain(),
        );

        let mut editor = CellEditor::new();
        let mut ids = Vec::new();
        for i in 0..100 {
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
            .spawn((
                editor,
                MainCell,
                crate::view::geometry::ConversationGeometry::default(),
            ))
            .id();
        let mut container = BlockCellContainer::default();
        for &id in &ids {
            let ent = app.world_mut().spawn(BlockCell::new(id)).id();
            container.add(id, ent);
        }
        app.world_mut().entity_mut(main_ent).insert(container);
        app.world_mut().resource_mut::<EditorEntities>().main_cell = Some(main_ent);

        // Focus the LAST block (far outside the keep band at scroll top).
        app.insert_resource({
            let mut focus = FocusTarget::default();
            focus.block_id = Some(ids[99]);
            focus
        });

        app.update();

        let container = app.world().get::<BlockCellContainer>(main_ent).unwrap();
        assert!(
            container.contains(&ids[99]),
            "focused block must be exempt from band despawn"
        );
        assert!(
            !container.contains(&ids[98]),
            "its unfocused neighbor must still despawn"
        );
    }

    fn build_test_app() -> App {
        let mut app = App::new();
        app.init_resource::<EditorEntities>();
        app.init_resource::<LayoutGeneration>();
        app.add_systems(Update, track_conversation_container);
        app
    }

    /// Seed a MainCell with a BlockCellContainer holding `n` block cells and
    /// one role header, parented under a stale (pre-split) container —
    /// mirrors the state right after `tiling_reconciler` rebuilds panes and
    /// orphans the old container.
    fn seed_main_with_cells(app: &mut App, n: usize) -> (Entity, Vec<Entity>, Entity) {
        let stale_conv = app.world_mut().spawn_empty().id();

        let mut container = BlockCellContainer::default();
        let mut cell_entities = Vec::with_capacity(n);
        for i in 0..n {
            let ent = app.world_mut().spawn_empty().id();
            container.add(
                kaijutsu_crdt::BlockId::new(
                    kaijutsu_crdt::ContextId::new(),
                    kaijutsu_crdt::PrincipalId::new(),
                    i as u64,
                ),
                ent,
            );
            cell_entities.push(ent);
        }
        let header_ent = app.world_mut().spawn_empty().id();
        container.role_headers.push(header_ent);

        app.world_mut()
            .entity_mut(stale_conv)
            .add_children(&cell_entities);
        app.world_mut().entity_mut(stale_conv).add_child(header_ent);

        let main_ent = app.world_mut().spawn(container).id();

        {
            let mut entities = app.world_mut().resource_mut::<EditorEntities>();
            entities.main_cell = Some(main_ent);
            entities.conversation_container = Some(stale_conv);
        }

        (main_ent, cell_entities, header_ent)
    }

    #[test]
    fn track_conversation_container_reparents_and_bumps_generation_on_focus_change() {
        let mut app = build_test_app();
        let (_main_ent, cell_entities, header_ent) = seed_main_with_cells(&mut app, 2);

        let focused_conv = app
            .world_mut()
            .spawn((ConversationContainer, PaneFocus))
            .id();

        app.update();

        let entities = app.world().resource::<EditorEntities>();
        assert_eq!(
            entities.conversation_container,
            Some(focused_conv),
            "EditorEntities.conversation_container must track the newly focused pane"
        );

        let layout_gen = app.world().resource::<LayoutGeneration>();
        assert!(
            layout_gen.0 > 0,
            "reparenting block cells + role headers onto a new container must bump \
             LayoutGeneration so sync_role_headers/reorder_conversation_children repair \
             the interleaved order on the next frames — previously this bump was missing \
             and the interleave stuck until app restart"
        );

        let children: Vec<Entity> = app
            .world()
            .get::<Children>(focused_conv)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        for ent in cell_entities.iter().chain(std::iter::once(&header_ent)) {
            assert!(
                children.contains(ent),
                "block cell / role header {ent:?} must be reparented onto the focused container"
            );
        }
    }

    #[test]
    fn track_conversation_container_is_a_noop_once_focus_already_tracked() {
        let mut app = build_test_app();
        let (_main_ent, _cells, _header) = seed_main_with_cells(&mut app, 1);

        let focused_conv = app
            .world_mut()
            .spawn((ConversationContainer, PaneFocus))
            .id();

        app.update();
        let gen_after_first = app.world().resource::<LayoutGeneration>().0;
        assert!(gen_after_first > 0);

        // Running again with the same focused container already tracked
        // must not bump the generation a second time.
        app.update();
        let gen_after_second = app.world().resource::<LayoutGeneration>().0;
        assert_eq!(
            gen_after_second, gen_after_first,
            "no-op frames (focus unchanged) must not keep bumping LayoutGeneration"
        );
        assert_eq!(
            app.world().resource::<EditorEntities>().conversation_container,
            Some(focused_conv)
        );
    }
}
