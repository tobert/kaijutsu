//! Entity lifecycle — spawn and despawn block cell entities.
//!
//! Owns spawn_main_cell, spawn_block_cells, sync_role_headers, and
//! track_conversation_container. Block cells use BlockScene + BlockTexture
//! + ImageNode for CPU-rasterized Vello rendering with Bevy flex layout.

use bevy::prelude::*;
use crate::text::shaping::VelloFont;

use crate::cell::{
    BlockCell, BlockCellContainer, BlockCellLayout, CellEditor, LayoutGeneration, MainCell,
    RoleGroupBorder, RoleGroupBorderLayout,
};
use crate::shaders::BlockFxMaterial;

/// Consolidated resource tracking editor-related singleton entities.
#[derive(Resource, Default)]
pub struct EditorEntities {
    /// The main conversation cell entity.
    pub main_cell: Option<Entity>,
    /// The ConversationContainer entity (flex parent for BlockCells).
    pub conversation_container: Option<Entity>,
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
        .spawn((CellEditor::default().with_text(welcome_text), MainCell))
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
    existing_block_cells: Query<Entity, With<BlockCell>>,
    mut layout_gen: ResMut<LayoutGeneration>,
    font_handles: Res<ShapingFonts>,
    fonts: Res<Assets<VelloFont>>,
    mut scroll_state: ResMut<crate::cell::ConversationScrollState>,
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

    // Gate new spawns on font availability — build_block_scenes needs a valid
    // font to render text into the Vello scene.
    let font_loaded = fonts.get(&font_handles.mono).is_some();
    if !font_loaded && current_blocks.iter().any(|id| !container.contains(id)) {
        // Blocks waiting to spawn but font not ready yet — will retry next frame.
        return;
    }

    let current_version = editor.version();
    let conv_entity = entities.conversation_container;

    let mut had_additions = false;

    for block_id in &current_blocks {
        if !container.contains(block_id) {
            // Single-phase spawn: BlockCell + BlockScene + BlockTexture + ImageNode + MaterialNode.
            // The GpuImage is prepared asynchronously by Bevy's RenderAssetPlugin, reacting
            // to AssetEvent::Modified/Created on the Image asset — one frame after this
            // spawn, not synchronously with it. ImageNode itself doesn't force preparation.
            // MaterialNode renders on top with shader effects (glow, animation).
            let material_handle = fx_materials.add(BlockFxMaterial::default());
            let entity = commands
                .spawn((
                    BlockCell::new(*block_id),
                    BlockCellLayout::default(),
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
                        created_at_version: current_version,
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
        }
    }

    // Reorder container.block_cells to match document order. A pure position
    // change (server BlockMoved, or a merge that repositions an existing
    // block) adds/removes nothing, so it must be detected here and folded
    // into the bump decision below — otherwise reorder_conversation_children
    // never re-runs and the visual order goes stale until app restart.
    let order_changed = container.resort_to_document_order(&current_blocks);

    if had_additions || had_removals || order_changed {
        info!(
            "spawn_block_cells: additions={} removals={} order_changed={} container_now={}",
            had_additions,
            had_removals,
            order_changed,
            container.block_cells.len(),
        );
        layout_gen.bump();
    }

    if had_additions {
        scroll_state.new_blocks_added = true;
    }
}

/// Sync RoleGroupBorder entities for role transitions.
///
/// Spawns role group border entities with BlockScene + BlockTexture for
/// Vello-drawn horizontal lines with inset role labels.
pub fn sync_role_headers(
    mut commands: Commands,
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    mut containers: Query<&mut BlockCellContainer>,
    role_header_query: Query<&RoleGroupBorder>,
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

    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };

    let Ok(mut container) = containers.get_mut(main_ent) else {
        return;
    };

    // Compute expected role transitions (skip tool blocks — they use fieldset borders)
    let blocks = editor.blocks();
    let mut expected: Vec<(kaijutsu_crdt::Role, kaijutsu_crdt::BlockId)> = Vec::new();
    let mut prev_role: Option<kaijutsu_crdt::Role> = None;
    for block in &blocks {
        // Tool blocks get their attribution from the fieldset border label,
        // so they don't need role group headers ("ASSISTANT", "TOOL", etc.)
        if block.kind == kaijutsu_crdt::BlockKind::ToolCall
            || block.kind == kaijutsu_crdt::BlockKind::ToolResult
        {
            continue;
        }
        if prev_role != Some(block.role) {
            expected.push((block.role, block.id));
        }
        prev_role = Some(block.role);
    }

    // Skip rebuild if transitions match (prevents despawn/respawn flash)
    let existing_matches = container.role_headers.len() == expected.len()
        && container
            .role_headers
            .iter()
            .zip(expected.iter())
            .all(|(ent, (role, block_id))| {
                role_header_query
                    .get(*ent)
                    .map(|h| h.role == *role && h.block_id == *block_id)
                    .unwrap_or(false)
            });

    if existing_matches {
        return;
    }

    for entity in container.role_headers.drain(..) {
        commands.entity(entity).try_despawn();
    }

    for (role, block_id) in expected {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::ConversationContainer;
    use crate::ui::tiling::PaneFocus;

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
