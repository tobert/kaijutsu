//! Tiling reconciler — turns TilingTree into Bevy entity tree.
//!
//! ## Architecture
//!
//! `setup_ui` spawns the structural skeleton. The reconciler populates it:
//! ```text
//! TilingRoot (column)
//!   [NorthDock — spawned by reconciler]
//!   ContentArea (column, flex-grow: 1)
//!     DashboardRoot (100%, toggled by AppScreen state)
//!     ConversationRoot (100%, toggled by AppScreen state)
//!       [Content tree — recursively spawned by reconciler]
//!   [SouthDock — spawned by reconciler]
//! ```
//!
//! The content tree maps the TilingTree's Split/Leaf structure to Bevy
//! flex containers. For a single pane, this is just:
//!   `ConvGroup(Column) → [ConversationContainer, ComposeBlock]`
//!
//! After Super+v split, it becomes:
//! ```text
//!   SplitContainer(Row)
//!     ConvGroup1(Column) → [ConversationContainer1, ComposeBlock1]
//!     ConvGroup2(Column) → [ConversationContainer2, ComposeBlock2]
//! ```
//!
//! Focus is tracked via `PaneFocus` marker + accent border on the
//! focused conversation container.

use bevy::prelude::*;

use super::tiling::*;
use super::theme::Theme;
use crate::cell::ComposeBlock;
use crate::cell::ConversationContainer;
use crate::constants::ZLayer;
use crate::text::{MsdfText, MsdfTextAreaConfig, MsdfUiText, UiTextPositionCache};

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
    compose_query: Query<(&PaneMarker, &ComposeBlock)>,
) {
    let needs_rebuild = !state.initialized || tree.structural_gen != state.last_structural_gen;
    if !needs_rebuild {
        return;
    }

    // Need the structural entities to exist
    let Ok(root_entity) = tiling_root.single() else {
        return;
    };
    let Ok((conv_root_entity, _conv_children)) = conversation_root.single() else {
        return;
    };

    // ── Save compose state before despawn ─────────────────────────────
    let saved_compose: std::collections::HashMap<PaneId, (String, usize)> = if state.initialized {
        compose_query
            .iter()
            .map(|(marker, compose)| (marker.pane_id, (compose.text.clone(), compose.cursor)))
            .collect()
    } else {
        std::collections::HashMap::new()
    };

    if state.initialized {
        // Despawn all existing pane-managed entities for rebuild
        for entity in existing_panes.iter() {
            commands.entity(entity).despawn();
        }
    }

    // ════════════════════════════════════════════════════════════════════
    // SPAWN DOCKS from TilingTree
    // ════════════════════════════════════════════════════════════════════

    // Walk the tree to find dock nodes and spawn them
    spawn_docks(&mut commands, &tree.root, &theme, root_entity);

    // ════════════════════════════════════════════════════════════════════
    // SPAWN CONVERSATION CONTENT from TilingTree
    // ════════════════════════════════════════════════════════════════════

    spawn_conversation_content(&mut commands, &tree, &theme, conv_root_entity, &saved_compose);

    state.last_structural_gen = tree.structural_gen;
    state.last_visual_gen = tree.visual_gen;
    state.initialized = true;

    info!(
        "Reconciled tiling tree (structural_gen={})",
        tree.structural_gen
    );
}

/// Walk the tree and spawn Dock nodes as children of root_entity.
///
/// North docks are inserted at index 0 (before ContentArea),
/// South docks are appended (after ContentArea). This ensures
/// the flex column renders: NorthDock → ContentArea → SouthDock.
fn spawn_docks(
    commands: &mut Commands,
    node: &TileNode,
    theme: &Theme,
    root_entity: Entity,
) {
    match node {
        TileNode::Dock { id, edge, children } => {
            let dock_entity = spawn_dock(commands, *id, *edge, children, theme);
            match edge {
                Edge::North => {
                    commands.entity(root_entity).insert_children(0, &[dock_entity]);
                }
                _ => {
                    commands.entity(root_entity).add_child(dock_entity);
                }
            }
        }
        TileNode::Split { children, .. } => {
            for child in children {
                spawn_docks(commands, child, theme, root_entity);
            }
        }
        TileNode::Leaf { .. } => {}
    }
}

/// Spawn conversation content into ConversationRoot.
///
/// Walks the TilingTree's root children, skipping Dock nodes (handled separately),
/// and recursively builds the flex container tree for Split/Leaf content panes.
fn spawn_conversation_content(
    commands: &mut Commands,
    tree: &TilingTree,
    theme: &Theme,
    conv_root: Entity,
    saved_compose: &std::collections::HashMap<PaneId, (String, usize)>,
) {
    if let TileNode::Split { children, ratios, .. } = &tree.root {
        for (i, child) in children.iter().enumerate() {
            // Skip docks — they're handled by spawn_docks
            if matches!(child, TileNode::Dock { .. }) {
                continue;
            }
            let ratio = ratios.get(i).copied().unwrap_or(1.0);
            spawn_content_subtree(commands, child, theme, conv_root, tree, ratio, saved_compose);
        }
    }
}

/// Recursively spawn a content subtree as Bevy flex entities.
///
/// - `Split` nodes → flex containers (Row or Column) with ratio-based sizing
/// - `Conversation` leaves → ConversationContainer entities
/// - `Compose` leaves → ComposeBlock entities (with saved text restored)
/// - Other leaves → ignored (widgets are in docks)
fn spawn_content_subtree(
    commands: &mut Commands,
    node: &TileNode,
    theme: &Theme,
    parent: Entity,
    tree: &TilingTree,
    flex_grow: f32,
    saved_compose: &std::collections::HashMap<PaneId, (String, usize)>,
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
                        // Split containers use Spacer as a stand-in — they don't have
                        // meaningful content of their own. No code queries by Spacer to
                        // find splits specifically, so a dedicated SplitContainer variant
                        // isn't worth the match-arm cost.
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
                spawn_content_subtree(commands, child, theme, entity, tree, child_ratio, saved_compose);
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
                    ConversationContainer,
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
                        overflow: Overflow {
                            x: OverflowAxis::Clip,
                            y: OverflowAxis::Scroll,
                        },
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

        TileNode::Leaf {
            id,
            content:
                PaneContent::Compose {
                    target_pane,
                    writing_direction,
                },
        } => {
            // Restore saved compose text if this pane existed before the rebuild
            let compose = if let Some((text, cursor)) = saved_compose.get(id) {
                ComposeBlock {
                    text: text.clone(),
                    cursor: *cursor,
                }
            } else {
                ComposeBlock::default()
            };

            let is_compose_focused = tree.focused == *target_pane;
            let mut entity_cmd = commands.spawn((
                    PaneMarker {
                        pane_id: *id,
                        content: PaneContent::Compose {
                            target_pane: *target_pane,
                            writing_direction: *writing_direction,
                        },
                    },
                    *id,
                    compose,
                    MsdfText,
                    MsdfTextAreaConfig::default(),
                    Node {
                        min_height: Val::Px(60.0),
                        padding: UiRect::all(Val::Px(12.0)),
                        margin: UiRect::new(
                            Val::Px(20.0),
                            Val::Px(20.0),
                            Val::Px(8.0),
                            Val::Px(16.0),
                        ),
                        border: UiRect::all(Val::Px(1.0)),
                        border_radius: BorderRadius::all(Val::Px(4.0)),
                        ..default()
                    },
                    BorderColor::all(Color::srgba(0.4, 0.6, 0.9, 0.6)),
                    BackgroundColor(Color::srgba(0.1, 0.1, 0.15, 0.8)),
                ));
            if is_compose_focused {
                entity_cmd.insert(PaneFocus);
            }
            let entity = entity_cmd.id();
            commands.entity(parent).add_child(entity);
        }

        _ => {} // Docks handled separately, other leaves ignored
    }
}

// ============================================================================
// DOCK SPAWNING
// ============================================================================

/// Spawn a dock container with its widget children.
fn spawn_dock(
    commands: &mut Commands,
    id: PaneId,
    edge: Edge,
    children: &[TileNode],
    theme: &Theme,
) -> Entity {
    let border = match edge {
        Edge::North => UiRect::bottom(Val::Px(1.0)),
        Edge::South => UiRect::top(Val::Px(1.0)),
        Edge::East => UiRect::left(Val::Px(1.0)),
        Edge::West => UiRect::right(Val::Px(1.0)),
    };

    let padding = match edge {
        Edge::North => UiRect::axes(Val::Px(16.0), Val::Px(6.0)),
        Edge::South => UiRect::axes(Val::Px(12.0), Val::Px(4.0)),
        _ => UiRect::all(Val::Px(4.0)),
    };

    let mut dock_cmd = commands.spawn((
        PaneMarker {
            pane_id: id,
            content: PaneContent::Spacer,
        },
        id,
        Node {
            width: Val::Percent(100.0),
            height: Val::Auto,
            flex_direction: FlexDirection::Row,
            justify_content: JustifyContent::SpaceBetween,
            align_items: AlignItems::Center,
            padding,
            border,
            ..default()
        },
        BorderColor::all(theme.border),
        ZIndex(ZLayer::HUD),
    ));

    dock_cmd.with_children(|parent| {
        for child in children {
            spawn_dock_child(parent, child, theme);
        }
    });

    dock_cmd.id()
}

/// Spawn a dock child (widget leaf or spacer).
fn spawn_dock_child(
    parent: &mut ChildSpawnerCommands,
    node: &TileNode,
    theme: &Theme,
) {
    let TileNode::Leaf { id, content } = node else {
        return; // Only leaves in docks
    };

    match content {
        PaneContent::Title => {
            spawn_widget_text(parent, *id, content, theme, "会術 Kaijutsu", 24.0, theme.accent, true);
        }
        PaneContent::Mode => {
            spawn_widget_text(parent, *id, content, theme, "NORMAL", 14.0, theme.mode_normal, true);
        }
        PaneContent::Connection => {
            spawn_widget_text(parent, *id, content, theme, "Connecting...", 14.0, theme.fg_dim, false);
        }
        PaneContent::Contexts => {
            spawn_widget_text(parent, *id, content, theme, "", 11.0, theme.fg_dim, false);
        }
        PaneContent::Hints => {
            spawn_widget_text(parent, *id, content, theme, "Enter: submit │ Shift+Enter: newline │ Esc: normal", 11.0, theme.fg_dim, false);
        }
        PaneContent::Spacer => {
            parent.spawn((
                PaneMarker {
                    pane_id: *id,
                    content: PaneContent::Spacer,
                },
                *id,
                Node {
                    flex_grow: 1.0,
                    ..default()
                },
            ));
        }
        PaneContent::Text { template } => {
            spawn_widget_text(parent, *id, content, theme, template, 14.0, theme.fg, false);
        }
        _ => {} // Non-widget content shouldn't be in docks
    }
}

// ============================================================================
// WIDGET SPAWNER
// ============================================================================

/// Marker for widget text content in the tiling system.
///
/// Used by tiling_widgets systems to find and update text.
#[derive(Component, Debug, Clone)]
pub struct WidgetPaneText {
    pub widget_type: PaneContent,
}

/// Spawn a widget with MSDF text as a child entity.
fn spawn_widget_text(
    parent: &mut ChildSpawnerCommands,
    id: PaneId,
    content: &PaneContent,
    theme: &Theme,
    initial_text: &str,
    font_size: f32,
    color: Color,
    has_padding: bool,
) {
    let (min_width, min_height) = match content {
        PaneContent::Title => (180.0, 36.0),
        PaneContent::Mode => (80.0, 20.0),
        PaneContent::Connection => (200.0, 20.0),
        PaneContent::Contexts => (200.0, 16.0),
        PaneContent::Hints => (400.0, 16.0),
        _ => (60.0, 16.0),
    };

    parent
        .spawn((
            PaneMarker {
                pane_id: id,
                content: content.clone(),
            },
            id,
            WidgetPaneText {
                widget_type: content.clone(),
            },
            Node {
                padding: if has_padding {
                    UiRect::all(Val::Px(8.0))
                } else {
                    UiRect::ZERO
                },
                min_width: Val::Px(min_width),
                min_height: Val::Px(min_height),
                ..default()
            },
            BackgroundColor(if has_padding { theme.panel_bg } else { Color::NONE }),
        ))
        .with_children(|text_parent| {
            text_parent.spawn((
                MsdfUiText::new(initial_text)
                    .with_font_size(font_size)
                    .with_color(color),
                UiTextPositionCache::default(),
                Node {
                    min_width: Val::Px(if has_padding { min_width - 16.0 } else { min_width }),
                    min_height: Val::Px(min_height),
                    ..default()
                },
            ));
        });
}

// ============================================================================
// FOCUS SYSTEM
// ============================================================================

/// System that maintains the `PaneFocus` marker and updates focus borders.
///
/// Adds `PaneFocus` to both the focused conversation pane AND its paired
/// compose pane (matched via `PaneContent::Compose { target_pane }`).
/// This lets queries like `Query<&mut ComposeBlock, With<PaneFocus>>` work.
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

    // Also add PaneFocus to the compose pane paired with the focused conversation
    for (entity, marker) in pane_markers.iter() {
        if let PaneContent::Compose { target_pane, .. } = &marker.content {
            if *target_pane == tree.focused {
                commands.entity(entity).insert(PaneFocus);
                break;
            }
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
/// 1. **Save** outgoing: scroll state + compose text → old ConversationContainer's PaneSavedState
/// 2. **Restore** incoming: new PaneSavedState → scroll state + compose text
/// 3. If document_id differs, fire ContextSwitchRequested
pub fn handle_pane_focus_change(
    tree: Res<TilingTree>,
    mut last_focused: Local<Option<PaneId>>,
    mut scroll_state: ResMut<crate::cell::ConversationScrollState>,
    mut saved_states: Query<(&PaneMarker, &mut PaneSavedState), With<ConversationContainer>>,
    mut compose_blocks: Query<(&PaneMarker, &mut ComposeBlock)>,
    mut switch_writer: MessageWriter<crate::cell::ContextSwitchRequested>,
) {
    // Detect focus change
    let current = tree.focused;
    let prev = *last_focused;
    if prev == Some(current) {
        return;
    }
    *last_focused = Some(current);

    // Only process if there was a previous pane (skip first frame)
    let Some(old_pane_id) = prev else {
        return;
    };

    let single_pane = tree.root.conversation_panes().len() < 2;

    // ── Save outgoing pane (only if it still exists) ─────────────────
    if !single_pane {
        let old_compose = compose_blocks
            .iter()
            .find_map(|(marker, compose)| {
                if let PaneContent::Compose { target_pane, .. } = &marker.content {
                    if *target_pane == old_pane_id {
                        return Some((compose.text.clone(), compose.cursor));
                    }
                }
                None
            });

        for (marker, mut saved) in saved_states.iter_mut() {
            if marker.pane_id == old_pane_id {
                saved.scroll_offset = scroll_state.offset;
                saved.scroll_target = scroll_state.target_offset;
                saved.following = scroll_state.following;
                if let Some((text, cursor)) = &old_compose {
                    saved.compose_text = text.clone();
                    saved.compose_cursor = *cursor;
                }
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

            // Restore compose text
            for (cm, mut compose) in compose_blocks.iter_mut() {
                if let PaneContent::Compose { target_pane, .. } = &cm.content {
                    if *target_pane == current {
                        compose.text = saved.compose_text.clone();
                        compose.cursor = saved.compose_cursor;
                        break;
                    }
                }
            }
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
                    switch_writer.write(crate::cell::ContextSwitchRequested {
                        context_name: document_id.clone(),
                    });
                    info!(
                        "Pane focus change: switching context to '{}'",
                        document_id
                    );
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
                } else if let Some(cached) = doc_cache.get(doc_id) {
                    let name = if cached.context_name.is_empty() {
                        short_id(doc_id)
                    } else {
                        cached.context_name.clone()
                    };
                    let count = cached.doc.block_count();
                    (name, count)
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
                        MsdfUiText::new(&summary_text)
                            .with_font_size(14.0)
                            .with_color(theme.fg_dim),
                        UiTextPositionCache::default(),
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
    let available: Vec<&str> = doc_cache
        .mru_ids()
        .iter()
        .map(|s| s.as_str())
        .filter(|id| !assigned.contains(*id))
        .collect();

    for (i, pane_id) in empty_panes.iter().enumerate() {
        if let Some(&doc_id) = available.get(i) {
            // Update the tiling tree
            tree.set_conversation_document(*pane_id, doc_id);

            // Update PaneMarker.content and PaneSavedState
            for (mut marker, mut saved) in saved_states.iter_mut() {
                if marker.pane_id == *pane_id {
                    marker.content = PaneContent::Conversation { document_id: doc_id.to_string() };
                    saved.document_id = doc_id.to_string();
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
