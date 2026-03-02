//! Tiling reconciler — turns TilingTree into Bevy entity tree.
//!
//! ## Architecture
//!
//! `setup_ui` spawns the structural skeleton. The reconciler populates
//! conversation content within ConversationRoot:
//! ```text
//! TilingRoot (column)
//!   [NorthDock — spawned by DockPlugin]
//!   ContentArea (column, flex-grow: 1)
//!     ConversationRoot (100%)
//!       [Content tree — recursively spawned by reconciler]
//!   [SouthDock — spawned by DockPlugin]
//! ```
//!
//! The content tree maps the TilingTree's Split/Leaf structure to Bevy
//! flex containers. For a single pane, this is just a ConversationContainer.
//!
//! After Alt+v split, it becomes:
//! ```text
//!   SplitContainer(Row)
//!     ConversationContainer1
//!     ConversationContainer2
//! ```
//!
//! Focus is tracked via `PaneFocus` marker + accent border on the
//! focused conversation container. Input is an ephemeral overlay
//! (not part of the tiling tree).

use bevy::prelude::*;

use super::tiling::*;
use super::theme::Theme;
use crate::cell::ConversationContainer;
use bevy_vello::prelude::UiVelloText;
use crate::text::{FontHandles, vello_style};

// ============================================================================
// MARKERS
// ============================================================================

/// Marker for the root of the tiling-managed UI tree.
///
/// Spawned by `setup_ui` — the reconciler finds this to insert docks.
#[derive(Component)]
pub struct TilingRoot;

/// Marker for the summary text shown in unfocused panes.
///
/// Despawned when the pane gains focus (block rendering takes over).
#[derive(Component)]
pub struct UnfocusedPaneSummary;

// ============================================================================
// STATE
// ============================================================================

/// Tracks reconciler state across frames.
#[derive(Resource, Default)]
pub struct ReconcilerState {
    /// Last structural generation we reconciled (entity rebuilds).
    last_structural_gen: u64,
    /// Last visual generation we synced (focus/resize style updates).
    last_visual_gen: u64,
    /// Whether we've done the initial spawn.
    initialized: bool,
}

// ============================================================================
// RECONCILER SYSTEM
// ============================================================================

/// The main reconciler system.
///
/// First frame: spawns docks + conversation content.
/// Subsequent frames: re-spawns if TilingTree.structural_gen changes.
pub fn reconcile_tiling_tree(
    mut commands: Commands,
    tree: Res<TilingTree>,
    theme: Res<Theme>,
    mut state: ResMut<ReconcilerState>,
    tiling_root: Query<Entity, With<TilingRoot>>,
    conversation_root: Query<(Entity, Option<&Children>), With<super::state::ConversationRoot>>,
    existing_panes: Query<Entity, With<PaneMarker>>,
    editor_entities: Res<crate::cell::EditorEntities>,
    block_containers: Query<&crate::cell::BlockCellContainer>,
) {
    let needs_rebuild = !state.initialized || tree.structural_gen != state.last_structural_gen;
    if !needs_rebuild {
        return;
    }

    // Need the structural entities to exist
    let Ok(_root_entity) = tiling_root.single() else {
        return;
    };
    let Ok((conv_root_entity, _conv_children)) = conversation_root.single() else {
        return;
    };

    if state.initialized {
        // Detach block cells from ConversationContainer before despawning panes.
        // Without this, despawn() recursively kills block cell children.
        if let Some(main_ent) = editor_entities.main_cell {
            if let Ok(container) = block_containers.get(main_ent) {
                for &entity in container.block_cells.values().chain(container.role_headers.iter()) {
                    commands.entity(entity).remove_parent_in_place();
                }
                info!(
                    "Detached {} block cells + {} role headers before pane rebuild",
                    container.block_cells.len(),
                    container.role_headers.len()
                );
            }
        }

        // Despawn all existing pane-managed entities for rebuild
        for entity in existing_panes.iter() {
            commands.entity(entity).despawn();
        }
    }

    // ════════════════════════════════════════════════════════════════════
    // SPAWN CONVERSATION CONTENT from TilingTree
    // ════════════════════════════════════════════════════════════════════

    spawn_conversation_content(&mut commands, &tree, &theme, conv_root_entity);

    state.last_structural_gen = tree.structural_gen;
    state.last_visual_gen = tree.visual_gen;
    state.initialized = true;

    info!(
        "Reconciled tiling tree (structural_gen={})",
        tree.structural_gen
    );
}

/// Spawn conversation content into ConversationRoot.
///
/// Walks the TilingTree root and recursively builds the flex container tree
/// for Split/Leaf content panes.
fn spawn_conversation_content(
    commands: &mut Commands,
    tree: &TilingTree,
    theme: &Theme,
    conv_root: Entity,
) {
    match &tree.root {
        TileNode::Split { children, ratios, .. } => {
            for (i, child) in children.iter().enumerate() {
                let ratio = ratios.get(i).copied().unwrap_or(1.0);
                spawn_content_subtree(commands, child, theme, conv_root, tree, ratio);
            }
        }
        TileNode::Leaf { .. } => {
            // Root is a single leaf — spawn it directly
            spawn_content_subtree(commands, &tree.root, theme, conv_root, tree, 1.0);
        }
    }
}

/// Recursively spawn a content subtree as Bevy flex entities.
///
/// - `Split` nodes → flex containers (Row or Column) with ratio-based sizing
/// - `Conversation` leaves → ConversationContainer entities
/// - Other leaves → ignored (widgets are in docks)
fn spawn_content_subtree(
    commands: &mut Commands,
    node: &TileNode,
    theme: &Theme,
    parent: Entity,
    tree: &TilingTree,
    flex_grow: f32,
) {
    match node {
        TileNode::Split {
            id,
            direction,
            children,
            ratios,
        } => {
            let flex_dir = match direction {
                SplitDirection::Row => FlexDirection::Row,
                SplitDirection::Column => FlexDirection::Column,
            };

            let entity = commands
                .spawn((
                    PaneMarker {
                        pane_id: *id,
                        content: PaneContent::Spacer,
                    },
                    *id,
                    Node {
                        flex_grow: if flex_grow > 0.0 { flex_grow } else { 1.0 },
                        flex_basis: if flex_grow > 0.0 {
                            Val::Px(0.0)
                        } else {
                            Val::Auto
                        },
                        flex_direction: flex_dir,
                        overflow: Overflow::clip(),
                        ..default()
                    },
                ))
                .id();
            commands.entity(parent).add_child(entity);

            for (i, child) in children.iter().enumerate() {
                let child_ratio = ratios.get(i).copied().unwrap_or(1.0);
                spawn_content_subtree(commands, child, theme, entity, tree, child_ratio);
            }
        }

        TileNode::Leaf {
            id,
            content: PaneContent::Conversation { document_id },
        } => {
            let is_focused = tree.focused == *id;
            let border_color = if is_focused {
                theme.accent
            } else {
                Color::NONE
            };

            let mut entity_cmd = commands.spawn((
                    PaneMarker {
                        pane_id: *id,
                        content: PaneContent::Conversation {
                            document_id: document_id.clone(),
                        },
                    },
                    *id,
                    crate::cell::ConversationContainer,
                    PaneSavedState {
                        document_id: document_id.clone(),
                        ..default()
                    },
                    Node {
                        flex_grow: if flex_grow > 0.0 { flex_grow } else { 1.0 },
                        flex_basis: if flex_grow > 0.0 {
                            Val::Px(0.0)
                        } else {
                            Val::Auto
                        },
                        flex_direction: FlexDirection::Column,
                        overflow: Overflow::scroll_y(),
                        padding: UiRect::axes(Val::Px(16.0), Val::Px(4.0)),
                        border: UiRect::all(Val::Px(2.0)),
                        ..default()
                    },
                    ScrollPosition::default(),
                    BorderColor::all(border_color),
                ));
            if is_focused {
                entity_cmd.insert(PaneFocus);
            }
            let entity = entity_cmd.id();
            commands.entity(parent).add_child(entity);
        }

        _ => {} // Non-conversation leaves ignored
    }
}

// ============================================================================
// FOCUS SYSTEM
// ============================================================================

/// System that maintains the `PaneFocus` marker and updates focus borders.
///
/// Adds `PaneFocus` to the focused conversation pane.
pub fn update_pane_focus(
    mut commands: Commands,
    tree: Res<TilingTree>,
    theme: Res<Theme>,
    pane_markers: Query<(Entity, &PaneMarker)>,
    focused_panes: Query<Entity, With<PaneFocus>>,
    mut conv_borders: Query<(&PaneMarker, &mut BorderColor), With<ConversationContainer>>,
) {
    if !tree.is_changed() {
        return;
    }

    // Remove stale focus markers
    for entity in focused_panes.iter() {
        commands.entity(entity).remove::<PaneFocus>();
    }

    // Add focus to the focused conversation pane
    for (entity, marker) in pane_markers.iter() {
        if marker.pane_id == tree.focused {
            commands.entity(entity).insert(PaneFocus);
            break;
        }
    }

    // Update border colors on conversation containers (focus indicator)
    let has_multiple = conv_borders.iter().count() > 1;
    for (marker, mut border) in conv_borders.iter_mut() {
        let is_focused = marker.pane_id == tree.focused;
        *border = if is_focused && has_multiple {
            BorderColor::all(theme.accent)
        } else {
            BorderColor::all(Color::NONE)
        };
    }
}

/// System that syncs visual-only tiling changes (focus + resize) to existing
/// entities without triggering a full reconciler rebuild.
///
/// Updates `flex_grow` on Split container entities when ratios change via resize.
pub fn sync_tiling_visuals(
    tree: Res<TilingTree>,
    mut state: ResMut<ReconcilerState>,
    mut pane_nodes: Query<(&PaneMarker, &mut Node)>,
) {
    if tree.visual_gen == state.last_visual_gen {
        return;
    }
    state.last_visual_gen = tree.visual_gen;

    // Walk the tiling tree and update flex_grow on existing entities
    for (marker, mut node) in pane_nodes.iter_mut() {
        if let Some(tile_node) = tree.root.find(marker.pane_id) {
            // For Split containers, find our ratio from the parent
            if let Some((parent_id, child_idx)) = tree.root.find_parent(marker.pane_id) {
                if let Some(TileNode::Split { ratios, .. }) = tree.root.find(parent_id) {
                    if let Some(&ratio) = ratios.get(child_idx) {
                        let new_grow = if ratio > 0.0 { ratio } else { 0.0 };
                        // Only for content panes that use flex_grow (not docks/widgets)
                        if matches!(tile_node, TileNode::Split { .. })
                            || matches!(
                                tile_node,
                                TileNode::Leaf {
                                    content: PaneContent::Conversation { .. },
                                    ..
                                }
                            )
                        {
                            if (node.flex_grow - new_grow).abs() > 0.001 {
                                node.flex_grow = new_grow;
                            }
                        }
                    }
                }
            }
        }
    }
}

// ============================================================================
// PANE FOCUS CHANGE — Save/restore per-pane state
// ============================================================================

/// System that saves/restores per-pane state when focus changes.
///
/// When the focused pane changes:
/// 1. **Save** outgoing: scroll state → old PaneSavedState
/// 2. **Restore** incoming: new PaneSavedState → scroll state
/// 3. If document_id differs, fire ContextSwitchRequested
pub fn handle_pane_focus_change(
    tree: Res<TilingTree>,
    mut last_focused: Local<Option<PaneId>>,
    mut scroll_state: ResMut<crate::cell::ConversationScrollState>,
    mut saved_states: Query<(&PaneMarker, &mut PaneSavedState), With<ConversationContainer>>,
    mut switch_writer: MessageWriter<crate::cell::ContextSwitchRequested>,
    mut focus: ResMut<crate::input::focus::FocusArea>,
) {
    // Detect focus change
    let current = tree.focused;
    let prev = *last_focused;
    if prev == Some(current) {
        return;
    }
    *last_focused = Some(current);

    // Dismiss overlay on pane switch (Compose → Conversation)
    if matches!(*focus, crate::input::focus::FocusArea::Compose) {
        *focus = crate::input::focus::FocusArea::Conversation;
    }

    // Only process if there was a previous pane (skip first frame)
    let Some(old_pane_id) = prev else {
        return;
    };

    let single_pane = tree.root.conversation_panes().len() < 2;

    // ── Save outgoing pane (only if it still exists) ─────────────────
    if !single_pane {
        for (marker, mut saved) in saved_states.iter_mut() {
            if marker.pane_id == old_pane_id {
                saved.scroll_offset = scroll_state.offset;
                saved.scroll_target = scroll_state.target_offset;
                saved.following = scroll_state.following;
                break;
            }
        }
    }

    // ── Restore incoming pane (always, even after 2→1 close) ─────────
    let mut incoming_doc_id = String::new();
    for (marker, saved) in saved_states.iter() {
        if marker.pane_id == current {
            scroll_state.offset = saved.scroll_offset;
            scroll_state.target_offset = saved.scroll_target;
            scroll_state.following = saved.following;
            incoming_doc_id = saved.document_id.clone();
            break;
        }
    }

    // ── Fire context switch if document differs (multi-pane only) ────
    if !single_pane && !incoming_doc_id.is_empty() {
        let outgoing_doc_id = saved_states
            .iter()
            .find_map(|(marker, saved)| {
                if marker.pane_id == old_pane_id {
                    Some(saved.document_id.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        if incoming_doc_id != outgoing_doc_id {
            if let Some(TileNode::Leaf {
                content: PaneContent::Conversation { document_id },
                ..
            }) = tree.root.find(current)
            {
                if !document_id.is_empty() {
                    if let Ok(ctx_id) = kaijutsu_types::ContextId::parse(document_id) {
                        switch_writer.write(crate::cell::ContextSwitchRequested {
                            context_id: ctx_id,
                        });
                        info!("Pane focus change: switching context to '{}'", document_id);
                    } else {
                        warn!("Pane focus change: invalid context ID '{}'", document_id);
                    }
                }
            }
        }
    }
}

// ============================================================================
// UNFOCUSED PANE SUMMARY
// ============================================================================

/// System that shows a summary text in unfocused panes and cleans it up
/// when the pane gains focus.
///
/// Unfocused panes show:
/// ```text
/// @context_name
/// N blocks · model_name
/// ```
///
/// Focused panes have their summary despawned (block rendering takes over).
pub fn sync_unfocused_pane_summaries(
    mut commands: Commands,
    tree: Res<TilingTree>,
    theme: Res<Theme>,
    font_handles: Res<FontHandles>,
    doc_cache: Res<crate::cell::DocumentCache>,
    conv_containers: Query<(Entity, &PaneMarker, &PaneSavedState, Option<&Children>), With<ConversationContainer>>,
    summaries: Query<Entity, With<UnfocusedPaneSummary>>,
) {
    // Only relevant with multiple panes
    if tree.root.conversation_panes().len() < 2 {
        // Clean up any stale summaries from before close
        for entity in summaries.iter() {
            commands.entity(entity).despawn();
        }
        return;
    }

    if !tree.is_changed() {
        return;
    }

    for (entity, marker, saved, children) in conv_containers.iter() {
        let is_focused = marker.pane_id == tree.focused;

        if is_focused {
            // Despawn any summary text children on the focused pane
            if let Some(children) = children {
                for child in children.iter() {
                    if summaries.contains(child) {
                        commands.entity(child).despawn();
                    }
                }
            }
        } else {
            // Check if this pane already has a summary
            let has_summary = children
                .map(|c| c.iter().any(|child| summaries.contains(child)))
                .unwrap_or(false);

            if !has_summary {
                // Build summary text
                let doc_id = &saved.document_id;
                let (context_label, block_count) = if doc_id.is_empty() {
                    ("No context".to_string(), 0)
                } else if let Some(ctx_id) = kaijutsu_types::ContextId::parse(doc_id).ok() {
                    if let Some(cached) = doc_cache.get(ctx_id) {
                        let name = if cached.context_name.is_empty() {
                            short_id(doc_id)
                        } else {
                            cached.context_name.clone()
                        };
                        let count = cached.synced.block_count();
                        (name, count)
                    } else {
                        (short_id(doc_id), 0)
                    }
                } else {
                    (short_id(doc_id), 0)
                };

                let summary_text = if block_count > 0 {
                    format!("@{}\n{} blocks", context_label, block_count)
                } else {
                    format!("@{}", context_label)
                };

                let summary_entity = commands
                    .spawn((
                        UnfocusedPaneSummary,
                        UiVelloText {
                            value: summary_text,
                            style: vello_style(&font_handles.mono, theme.fg_dim, 14.0),
                            ..default()
                        },
                        Node {
                            width: Val::Percent(100.0),
                            padding: UiRect::all(Val::Px(16.0)),
                            margin: UiRect::top(Val::Px(24.0)),
                            ..default()
                        },
                    ))
                    .id();
                commands.entity(entity).add_child(summary_entity);
            }
        }
    }
}

/// Shorten a document_id to a readable label.
fn short_id(id: &str) -> String {
    if id.len() > 12 {
        format!("{}…", &id[..12])
    } else {
        id.to_string()
    }
}

// ============================================================================
// MRU CONTEXT ASSIGNMENT — Wire new panes to next available context
// ============================================================================

/// System that assigns the next MRU context to new conversation panes
/// that have an empty document_id.
///
/// After a split, the new pane has `document_id: ""`. This system picks
/// the next MRU context from DocumentCache that isn't already visible in
/// another pane, and updates both the TilingTree and PaneSavedState.
pub fn assign_mru_to_empty_panes(
    mut tree: ResMut<TilingTree>,
    doc_cache: Res<crate::cell::DocumentCache>,
    mut saved_states: Query<(&mut PaneMarker, &mut PaneSavedState), With<ConversationContainer>>,
) {
    // Collect panes with empty document_id
    let conv_panes = tree.root.conversation_panes();
    let empty_panes: Vec<PaneId> = conv_panes
        .iter()
        .filter(|(_, doc_id)| doc_id.is_empty())
        .map(|(id, _)| *id)
        .collect();

    if empty_panes.is_empty() {
        return;
    }

    // Collect document_ids already assigned to visible panes
    let assigned: std::collections::HashSet<&str> = conv_panes
        .iter()
        .filter(|(_, doc_id)| !doc_id.is_empty())
        .map(|(_, doc_id)| *doc_id)
        .collect();

    // Find next MRU context not already visible
    let available: Vec<String> = doc_cache
        .mru_ids()
        .iter()
        .map(|s| s.to_string())
        .filter(|id| !assigned.contains(id.as_str()))
        .collect();

    for (i, pane_id) in empty_panes.iter().enumerate() {
        if let Some(doc_id) = available.get(i) {
            // Update the tiling tree
            tree.set_conversation_document(*pane_id, doc_id);

            // Update PaneMarker.content and PaneSavedState
            for (mut marker, mut saved) in saved_states.iter_mut() {
                if marker.pane_id == *pane_id {
                    marker.content = PaneContent::Conversation { document_id: doc_id.clone() };
                    saved.document_id = doc_id.clone();
                    break;
                }
            }

            info!(
                "Assigned MRU context '{}' to new pane {}",
                doc_id, pane_id
            );
        }
    }
}

// ============================================================================
// PLUGIN
// ============================================================================

/// System set phases for tiling reconciliation.
///
/// `Reconcile` runs the entity rebuild + ApplyDeferred so deferred commands
/// (spawn/despawn) are flushed before `PostReconcile` systems query the world.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum TilingPhase {
    Reconcile,
    PostReconcile,
}

/// Plugin for the tiling reconciler.
pub struct TilingReconcilerPlugin;

impl Plugin for TilingReconcilerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ReconcilerState>()
            .configure_sets(
                Update,
                TilingPhase::Reconcile.before(TilingPhase::PostReconcile),
            )
            .add_systems(
                Update,
                (
                    reconcile_tiling_tree,
                    ApplyDeferred.after(reconcile_tiling_tree),
                )
                    .in_set(TilingPhase::Reconcile),
            )
            .add_systems(
                Update,
                (
                    update_pane_focus,
                    sync_tiling_visuals,
                    handle_pane_focus_change.after(update_pane_focus),
                    assign_mru_to_empty_panes,
                    sync_unfocused_pane_summaries
                        .after(update_pane_focus)
                        .after(assign_mru_to_empty_panes),
                )
                    .in_set(TilingPhase::PostReconcile),
            );
    }
}
