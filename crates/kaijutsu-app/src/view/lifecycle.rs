//! Entity lifecycle — spawn and despawn block cell entities.
//!
//! Owns spawn_main_cell, spawn_block_cells, sync_role_headers, and
//! track_conversation_container. The key architectural change from the
//! cell/ version: block cells use VelloTextAnchor::TopLeft with Bevy
//! flex layout for positioning — no UiTransform hack.

use bevy::prelude::*;
use bevy::ui::ContentSize;
use bevy::ui::ui_transform::UiTransform;
use bevy::camera::visibility::VisibilityClass;
use bevy_vello::prelude::{UiVelloText, VelloFont, VelloTextAnchor};

use crate::cell::{
    BlockCell, BlockCellContainer, BlockCellLayout, Cell, CellEditor, CellPosition, CellState,
    LayoutGeneration, MainCell, RoleGroupBorder, RoleGroupBorderLayout,
};

/// Consolidated resource tracking editor-related singleton entities.
#[derive(Resource, Default)]
pub struct EditorEntities {
    /// The cursor UI entity.
    pub cursor: Option<Entity>,
    /// The main conversation cell entity.
    pub main_cell: Option<Entity>,
    /// The ConversationContainer entity (flex parent for BlockCells).
    pub conversation_container: Option<Entity>,
}
use crate::text::{KjText, KjTextEffects, FontHandles, TextMetrics, bevy_color_to_brush};
use crate::ui::timeline::TimelineVisibility;

// ============================================================================
// LAYOUT CONSTANTS
// ============================================================================

/// Horizontal indentation per nesting level (for nested tool results, etc.)
pub(crate) const INDENT_WIDTH: f32 = 24.0;

/// Vertical spacing between blocks.
pub(crate) const BLOCK_SPACING: f32 = 8.0;

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

    let cell = Cell::new();
    let cell_id = cell.id.clone();

    let welcome_text = "Welcome to 会術 Kaijutsu\n\nPress 'i' to start typing...";

    // MainCell does NOT get UiVelloText directly.
    // The BlockCell system handles per-block rendering.
    // MainCell only holds the CellEditor (source of truth for content).
    let entity = commands
        .spawn((
            cell,
            CellEditor::default().with_text(welcome_text),
            CellState {
                computed_height: 400.0,
                collapsed: false,
            },
            CellPosition::new(0),
            MainCell,
        ))
        .id();

    entities.main_cell = Some(entity);
    info!("Spawned main kernel cell with id {:?}", cell_id.0);
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
    focused_containers: Query<Entity, (With<crate::cell::ConversationContainer>, With<crate::ui::tiling::PaneFocus>)>,
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
        trace!("Re-parenting {} block cells to new container {:?}", container.block_cells.len(), focused);
        for &block_ent in &container.block_cells {
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
/// **Single-phase spawning:** UiVelloText is included in the initial spawn
/// bundle to avoid font handle corruption during deferred try_insert.
/// Spawning is gated on font availability — block cells appear once the
/// font asset loads (typically within the first few frames).
pub fn spawn_block_cells(
    mut commands: Commands,
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    mut containers: Query<&mut BlockCellContainer>,
    existing_block_cells: Query<Entity, With<BlockCell>>,
    mut layout_gen: ResMut<LayoutGeneration>,
    font_handles: Res<FontHandles>,
    fonts: Res<Assets<VelloFont>>,
    text_metrics: Res<TextMetrics>,
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
        commands.entity(main_ent).insert(BlockCellContainer::default());
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
            container_count, live_count, current_blocks.len()
        );
    }
    let live_entities: std::collections::HashSet<Entity> =
        existing_block_cells.iter().collect();
    let stale: Vec<Entity> = container
        .block_cells
        .iter()
        .filter(|e| !live_entities.contains(e))
        .copied()
        .collect();
    if !stale.is_empty() {
        warn!("Purging {} stale block cell refs from container", stale.len());
        for entity in stale {
            container.remove(entity);
        }
    }

    // Despawn removed blocks
    let to_remove: Vec<_> = container
        .block_to_entity
        .iter()
        .filter(|(id, _)| !current_ids.contains(id))
        .map(|(_, e)| *e)
        .collect();

    let had_removals = !to_remove.is_empty();
    for entity in to_remove {
        commands.entity(entity).try_despawn();
        container.remove(entity);
    }

    // Gate new spawns on font availability — UiVelloText needs a valid font
    // handle at spawn time to avoid content sizing failures.
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
            // Single-phase spawn: BlockCell + UiVelloText in one bundle.
            // This avoids the font handle corruption seen with deferred
            // try_insert of UiVelloText onto an existing entity.
            let entity = commands
                .spawn((
                    BlockCell::new(*block_id),
                    BlockCellLayout::default(),
                    KjText,
                    KjTextEffects::default(),
                    UiVelloText {
                        value: String::new(),
                        style: bevy_vello::prelude::VelloTextStyle {
                            font: font_handles.mono.clone(),
                            brush: bevy_color_to_brush(Color::WHITE),
                            font_size: text_metrics.cell_font_size,
                            ..default()
                        },
                        // Initial estimate for width to allow word wrapping on Frame N.
                        // Will be corrected by `sync_text_max_advance` once the node is laid out.
                        max_advance: Some(1200.0),
                        ..default()
                    },
                    // Pre-emptively seed all required components for `UiVelloText`.
                    // Providing these manually prevents Bevy's requirement system 
                    // from triggering archetype moves later, which can reset 
                    // `UiVelloText` to its default state and corrupt the font handle.
                    // Note: `UiTransform` is a framework requirement for Bevy 0.18 UI
                    // but we still rely entirely on flex layout for positioning.
                    ContentSize::default(),
                    VelloTextAnchor::TopLeft,
                    UiTransform::default(),
                    Visibility::Inherited,
                    VisibilityClass::default(),
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
            if let Some(conv) = conv_entity {
                if let Ok(mut ec) = commands.get_entity(conv) { ec.add_child(entity); }
            }
            container.add(*block_id, entity);
            had_additions = true;
        }
    }

    if had_additions || had_removals {
        layout_gen.bump();
    }

    // Reorder container.block_cells to match document order
    let mut new_order = Vec::with_capacity(current_blocks.len());
    for block_id in &current_blocks {
        if let Some(entity) = container.get_entity(block_id) {
            new_order.push(entity);
        }
    }
    container.block_cells = new_order;
}

/// Sync RoleGroupBorder entities for role transitions.
///
/// Spawns role group border entities with `UiVelloScene` for Vello-drawn
/// horizontal lines with inset role labels.
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

    // Compute expected role transitions
    let blocks = editor.blocks();
    let mut expected: Vec<(kaijutsu_crdt::Role, kaijutsu_crdt::BlockId)> = Vec::new();
    let mut prev_role: Option<kaijutsu_crdt::Role> = None;
    for block in &blocks {
        if prev_role != Some(block.role) {
            expected.push((block.role, block.id));
        }
        prev_role = Some(block.role);
    }

    // Skip rebuild if transitions match (prevents despawn/respawn flash)
    let existing_matches = container.role_headers.len() == expected.len()
        && container.role_headers.iter().zip(expected.iter()).all(|(ent, (role, block_id))| {
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
                RoleGroupBorder {
                    role,
                    block_id,
                },
                RoleGroupBorderLayout::default(),
                Node {
                    width: Val::Percent(100.0),
                    min_height: Val::Px(ROLE_HEADER_HEIGHT),
                    margin: UiRect::bottom(Val::Px(ROLE_HEADER_SPACING)),
                    ..default()
                },
                bevy_vello::prelude::UiVelloScene::default(),
            ))
            .id();
        if let Some(conv) = entities.conversation_container {
            if let Ok(mut ec) = commands.get_entity(conv) { ec.add_child(entity); }
        }

        container.role_headers.push(entity);
    }
}
