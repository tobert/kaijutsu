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

// ============================================================================
// STATE
// ============================================================================

/// Tracks reconciler state across frames.
#[derive(Resource, Default)]
pub struct ReconcilerState {
    /// Last generation we reconciled.
    last_generation: u64,
    /// Whether we've done the initial spawn.
    initialized: bool,
}

// ============================================================================
// RECONCILER SYSTEM
// ============================================================================

/// The main reconciler system.
///
/// First frame: spawns docks + conversation content.
/// Subsequent frames: re-spawns if TilingTree.generation changes.
pub fn reconcile_tiling_tree(
    mut commands: Commands,
    tree: Res<TilingTree>,
    theme: Res<Theme>,
    mut state: ResMut<ReconcilerState>,
    tiling_root: Query<Entity, With<TilingRoot>>,
    conversation_root: Query<(Entity, Option<&Children>), With<super::state::ConversationRoot>>,
    existing_panes: Query<Entity, With<PaneMarker>>,
) {
    let needs_rebuild = !state.initialized || tree.generation != state.last_generation;
    if !needs_rebuild {
        return;
    }

    // Need the structural entities to exist
    let Ok(root_entity) = tiling_root.single() else {
        return;
    };
    let Ok((conv_root_entity, conv_children)) = conversation_root.single() else {
        return;
    };

    // Check if conversation root already has children (avoid double-spawn)
    let conv_has_children = conv_children
        .map(|c| !c.is_empty())
        .unwrap_or(false);

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

    if !conv_has_children || state.initialized {
        spawn_conversation_content(&mut commands, &tree, &theme, conv_root_entity);
    }

    state.last_generation = tree.generation;
    state.initialized = true;

    info!(
        "Reconciled tiling tree (generation {})",
        tree.generation
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
) {
    if let TileNode::Split { children, ratios, .. } = &tree.root {
        for (i, child) in children.iter().enumerate() {
            // Skip docks — they're handled by spawn_docks
            if matches!(child, TileNode::Dock { .. }) {
                continue;
            }
            let ratio = ratios.get(i).copied().unwrap_or(1.0);
            spawn_content_subtree(commands, child, theme, conv_root, tree, ratio);
        }
    }
}

/// Recursively spawn a content subtree as Bevy flex entities.
///
/// - `Split` nodes → flex containers (Row or Column) with ratio-based sizing
/// - `Conversation` leaves → ConversationContainer entities
/// - `Compose` leaves → ComposeBlock entities
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
                        content: PaneContent::Spacer, // Split containers don't have content
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

            let entity = commands
                .spawn((
                    PaneMarker {
                        pane_id: *id,
                        content: PaneContent::Conversation {
                            document_id: document_id.clone(),
                        },
                    },
                    *id,
                    ConversationContainer,
                    Node {
                        flex_grow: if flex_grow > 0.0 { flex_grow } else { 1.0 },
                        flex_basis: if flex_grow > 0.0 {
                            Val::Px(0.0)
                        } else {
                            Val::Auto
                        },
                        flex_direction: FlexDirection::Column,
                        overflow: Overflow::clip(),
                        padding: UiRect::axes(Val::Px(16.0), Val::Px(4.0)),
                        border: UiRect::all(Val::Px(2.0)),
                        ..default()
                    },
                    BorderColor::all(border_color),
                ))
                .id();
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
            let entity = commands
                .spawn((
                    PaneMarker {
                        pane_id: *id,
                        content: PaneContent::Compose {
                            target_pane: *target_pane,
                            writing_direction: *writing_direction,
                        },
                    },
                    *id,
                    ComposeBlock::default(),
                    MsdfText,
                    MsdfTextAreaConfig::default(),
                    Node {
                        width: Val::Percent(100.0),
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
                ))
                .id();
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

    // Add focus to the correct pane
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

// ============================================================================
// PLUGIN
// ============================================================================

/// Plugin for the tiling reconciler.
pub struct TilingReconcilerPlugin;

impl Plugin for TilingReconcilerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ReconcilerState>()
            .add_systems(
                Update,
                (
                    reconcile_tiling_tree,
                    update_pane_focus.after(reconcile_tiling_tree),
                ),
            );
    }
}
