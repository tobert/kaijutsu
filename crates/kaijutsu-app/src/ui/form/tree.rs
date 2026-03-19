//! Expandable tree view component with automatic visual rebuild.
//!
//! Extracts the tool tree pattern from fork_form: categories that expand/collapse,
//! items that toggle on/off, and a flat cursor over the visible rows.

use bevy::prelude::*;

use crate::text::{FontHandles, vello_style};
use crate::ui::theme::Theme;
use bevy_vello::prelude::{UiVelloScene, UiVelloText};

// ============================================================================
// DATA TYPES
// ============================================================================

/// A category (group header) in the tree.
#[derive(Clone, Debug)]
pub struct TreeCategory {
    pub name: String,
    pub expanded: bool,
    pub items: Vec<TreeItem>,
}

impl TreeCategory {
    /// Number of enabled items in this category.
    pub fn enabled_count(&self) -> usize {
        self.items.iter().filter(|t| t.enabled).count()
    }

    /// Total number of items in this category.
    pub fn total_count(&self) -> usize {
        self.items.len()
    }

    /// Number of visible rows: 1 (self) + items if expanded.
    pub fn visible_rows(&self) -> usize {
        if self.expanded {
            1 + self.items.len()
        } else {
            1
        }
    }
}

/// A leaf item in a tree category.
#[derive(Clone, Debug)]
pub struct TreeItem {
    pub label: String,
    pub enabled: bool,
}

/// What the cursor is pointing at in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeCursorTarget {
    Category(usize),
    Item(usize, usize), // (category_idx, item_idx)
}

// ============================================================================
// COMPONENT
// ============================================================================

/// An expandable tree view. Attach to a container entity.
///
/// Mutate via the provided methods. The `rebuild_tree_view` system handles rendering.
#[derive(Component)]
pub struct TreeView {
    pub categories: Vec<TreeCategory>,
    /// Flat index into visible rows.
    pub cursor: usize,
    pub font_size: f32,
    /// When true, triggers a full despawn+rebuild.
    pub dirty: bool,
}

#[allow(dead_code)]
impl TreeView {
    pub fn new(categories: Vec<TreeCategory>, font_size: f32) -> Self {
        Self {
            categories,
            cursor: 0,
            font_size,
            dirty: true,
        }
    }

    /// Resolve the flat cursor index to a `(category, item)` target.
    pub fn resolve_cursor(&self) -> Option<TreeCursorTarget> {
        let mut pos = 0;
        for (ci, cat) in self.categories.iter().enumerate() {
            if pos == self.cursor {
                return Some(TreeCursorTarget::Category(ci));
            }
            pos += 1;
            if cat.expanded {
                for ti in 0..cat.items.len() {
                    if pos == self.cursor {
                        return Some(TreeCursorTarget::Item(ci, ti));
                    }
                    pos += 1;
                }
            }
        }
        None
    }

    /// Total number of visible rows across all categories.
    pub fn total_visible_rows(&self) -> usize {
        self.categories.iter().map(|c| c.visible_rows()).sum()
    }

    /// Move cursor to next visible row. Returns true if changed.
    pub fn cursor_next(&mut self) -> bool {
        let max = self.total_visible_rows();
        if self.cursor + 1 < max {
            self.cursor += 1;
            self.dirty = true;
            true
        } else {
            false
        }
    }

    /// Move cursor to previous visible row. Returns true if changed.
    pub fn cursor_prev(&mut self) -> bool {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.dirty = true;
            true
        } else {
            false
        }
    }

    /// Toggle expand/collapse on a category (if cursor is on one). Returns true if changed.
    pub fn toggle_expand(&mut self) -> bool {
        if let Some(TreeCursorTarget::Category(ci)) = self.resolve_cursor() {
            self.categories[ci].expanded = !self.categories[ci].expanded;
            self.dirty = true;
            true
        } else {
            false
        }
    }

    /// Toggle enable/disable on the item under cursor. Returns true if changed.
    pub fn toggle_item(&mut self) -> bool {
        match self.resolve_cursor() {
            Some(TreeCursorTarget::Item(ci, ti)) => {
                self.categories[ci].items[ti].enabled = !self.categories[ci].items[ti].enabled;
                self.dirty = true;
                true
            }
            Some(TreeCursorTarget::Category(ci)) => {
                // Toggle all items in the category
                let all_enabled = self.categories[ci].items.iter().all(|t| t.enabled);
                let new_state = !all_enabled;
                for item in &mut self.categories[ci].items {
                    item.enabled = new_state;
                }
                self.dirty = true;
                true
            }
            None => false,
        }
    }

    /// Collect names of all disabled items (for building deny lists).
    pub fn disabled_items(&self) -> Vec<String> {
        self.categories
            .iter()
            .flat_map(|cat| cat.items.iter())
            .filter(|t| !t.enabled)
            .map(|t| t.label.clone())
            .collect()
    }
}

// ============================================================================
// CHILD MARKERS
// ============================================================================

/// Marker on each row entity spawned by the rebuild system.
#[derive(Component)]
pub struct TreeViewRow(pub usize);

// ============================================================================
// REBUILD SYSTEM
// ============================================================================

/// Rebuilds `TreeView` visuals when the component changes and `dirty` is set.
///
/// Full despawn+rebuild approach (tree is small, <100 rows).
/// Rows are flat entities with `UiVelloText` + `UiVelloScene` (no child text entities).
pub fn rebuild_tree_view(
    mut commands: Commands,
    theme: Res<Theme>,
    font_handles: Res<FontHandles>,
    mut trees: Query<(Entity, &mut TreeView), Changed<TreeView>>,
    existing_rows: Query<(Entity, &TreeViewRow, &ChildOf)>,
) {
    for (tree_entity, mut tree) in trees.iter_mut() {
        if !tree.dirty {
            continue;
        }

        // Despawn existing rows for this tree
        for (entity, _row, child_of) in existing_rows.iter() {
            if child_of.0 == tree_entity {
                commands.entity(entity).despawn();
            }
        }

        let font_size = tree.font_size;
        let row_height = (font_size * 1.2).ceil() + 5.0;
        let font = &font_handles.mono;

        let mut flat_idx = 0;
        for cat in &tree.categories {
            let is_cursor = flat_idx == tree.cursor;
            let arrow = if cat.expanded { "\u{25BE}" } else { "\u{25B8}" }; // ▾ or ▸
            let label = format!(
                "{} {} ({}/{})",
                arrow,
                cat.name,
                cat.enabled_count(),
                cat.total_count()
            );
            let color = if is_cursor { theme.accent } else { theme.fg };

            // Scene filled in by sync_row_highlights (PostUpdate, after layout)
            let row = commands
                .spawn((
                    TreeViewRow(flat_idx),
                    Node {
                        width: Val::Percent(100.0),
                        height: Val::Px(row_height),
                        ..default()
                    },
                    UiVelloText {
                        value: label.clone(),
                        style: vello_style(font, color, font_size),
                        ..default()
                    },
                    UiVelloScene::default(),
                    Interaction::None,
                ))
                .id();
            commands.entity(tree_entity).add_child(row);
            flat_idx += 1;

            if cat.expanded {
                for item in &cat.items {
                    let is_cursor = flat_idx == tree.cursor;
                    let checkbox = if item.enabled { "[x]" } else { "[ ]" };
                    let label = format!("  {} {}", checkbox, item.label);
                    let color = if is_cursor {
                        theme.accent
                    } else if item.enabled {
                        theme.fg
                    } else {
                        theme.fg_dim
                    };

                    let row = commands
                        .spawn((
                            TreeViewRow(flat_idx),
                            Node {
                                width: Val::Percent(100.0),
                                height: Val::Px(row_height),
                                padding: UiRect::left(Val::Px(8.0)),
                                ..default()
                            },
                            UiVelloText {
                                value: label.clone(),
                                style: vello_style(font, color, font_size),
                                ..default()
                            },
                            UiVelloScene::default(),
                            Interaction::None,
                        ))
                        .id();
                    commands.entity(tree_entity).add_child(row);
                    flat_idx += 1;
                }
            }
        }

        tree.dirty = false;
    }
}

// ============================================================================
// CLICK HANDLER
// ============================================================================

/// Handle clicks on tree rows.
///
/// Clicking a category row toggles expand/collapse.
/// Clicking an item row moves the cursor there and toggles the item.
pub fn handle_tree_view_click(
    rows: Query<(&TreeViewRow, &Interaction, &ChildOf), Changed<Interaction>>,
    mut trees: Query<&mut TreeView>,
) {
    for (row, interaction, child_of) in rows.iter() {
        if !matches!(interaction, Interaction::Pressed) {
            continue;
        }
        let Ok(mut tree) = trees.get_mut(child_of.0) else {
            continue;
        };
        let idx = row.0;
        if idx < tree.total_visible_rows() && idx != tree.cursor {
            tree.cursor = idx;
            tree.dirty = true;
        }
        // After moving cursor, toggle (expand/collapse for categories, enable/disable for items)
        match tree.resolve_cursor() {
            Some(TreeCursorTarget::Category(_)) => {
                tree.toggle_expand();
            }
            Some(TreeCursorTarget::Item(_, _)) => {
                tree.toggle_item();
            }
            None => {}
        }
    }
}
