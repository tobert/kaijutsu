//! Entity lifecycle — spawn and despawn block cell entities.
//!
//! Owns spawn_main_cell, spawn_block_cells, sync_role_headers, and
//! track_conversation_container. Block cells use BlockScene + BlockTexture
//! + ImageNode for CPU-rasterized Vello rendering with Bevy flex layout.

use bevy::prelude::*;
use bevy_vello::prelude::VelloFont;

use crate::cell::{
    BlockCell, BlockCellContainer, BlockCellLayout, BlockId, CellEditor, LayoutGeneration,
    MainCell, RoleGroupBorder, RoleGroupBorderLayout,
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
use crate::text::FontHandles;
use crate::ui::timeline::TimelineVisibility;

// ============================================================================
// LAYOUT CONSTANTS
// ============================================================================

/// Horizontal indentation per nesting level (for nested tool results, etc.)
pub(crate) const INDENT_WIDTH: f32 = 24.0;

/// Height reserved for role transition headers (e.g., "User", "Assistant").
pub(crate) const ROLE_HEADER_HEIGHT: f32 = 20.0;

/// Spacing between role header and block content.
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

    let welcome_text =
        "No context joined\n\nOpen constellation (Tab) to select or create a context";

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
    // all existing block cells must move to the new container.
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
    font_handles: Res<FontHandles>,
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
            // ImageNode ensures the GpuImage is prepared by Bevy's render asset pipeline.
            // MaterialNode renders on top with shader effects (glow, animation).
            let material_handle = fx_materials.add(BlockFxMaterial::default());
            let entity = commands
                .spawn((
                    BlockCell::new(*block_id),
                    BlockCellLayout::default(),
                    crate::view::block_render::BlockScene::default(),
                    crate::view::block_render::BlockTexture {
                        image: Handle::default(),
                        width: 1,
                        height: 1,
                    },
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

    if had_additions || had_removals {
        info!(
            "spawn_block_cells: additions={} removals={} container_now={}",
            had_additions,
            had_removals,
            container.block_cells.len(),
        );
        layout_gen.bump();
    }

    if had_additions {
        scroll_state.new_blocks_added = true;
    }

    // Reorder container.block_cells to match document order
    let block_order: std::collections::HashMap<&BlockId, usize> = current_blocks
        .iter()
        .enumerate()
        .map(|(i, id)| (id, i))
        .collect();
    container.block_cells.sort_by(|a, _, b, _| {
        let a_idx = block_order.get(a).copied().unwrap_or(usize::MAX);
        let b_idx = block_order.get(b).copied().unwrap_or(usize::MAX);
        a_idx.cmp(&b_idx)
    });
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
                crate::view::block_render::BlockScene::default(),
                crate::view::block_render::BlockTexture {
                    image: Handle::default(),
                    width: 1,
                    height: 1,
                },
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
