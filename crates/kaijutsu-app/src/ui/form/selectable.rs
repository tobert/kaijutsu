//! Selectable list component with automatic visual sync.
//!
//! Extracts the j/k-navigable list pattern shared by fork_form (model list) and model_picker.
//! The form owns the `SelectableList` component and mutates `selected` via `select_next()`/
//! `select_prev()`. Visual sync is automatic via `sync_selectable_list_visuals`.

use bevy::prelude::*;

use bevy_vello::prelude::UiVelloText;
use crate::text::{FontHandles, vello_style};
use crate::ui::theme::Theme;

// ============================================================================
// DATA TYPES
// ============================================================================

/// A single item in a selectable list.
#[derive(Clone, Debug)]
pub struct ListItem {
    /// Primary display text.
    pub label: String,
    /// Optional suffix (e.g. "  (inherited)").
    pub suffix: String,
    /// Whether this item is selectable (false = grayed out).
    pub enabled: bool,
    /// Non-selectable group header (rendered smaller, with top margin).
    pub is_header: bool,
}

impl ListItem {
    /// Create a normal selectable item.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            suffix: String::new(),
            enabled: true,
            is_header: false,
        }
    }

    /// Create a non-selectable group header.
    pub fn header(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            suffix: String::new(),
            enabled: true,
            is_header: true,
        }
    }

    /// Set the suffix text.
    pub fn with_suffix(mut self, suffix: impl Into<String>) -> Self {
        self.suffix = suffix.into();
        self
    }
}

// ============================================================================
// COMPONENT
// ============================================================================

/// A navigable list of items. Attach to a container entity.
///
/// Mutate `selected` via `select_next()`/`select_prev()`, or set it directly.
/// The `sync_selectable_list_visuals` system handles rendering.
#[derive(Component)]
pub struct SelectableList {
    /// The items to display.
    pub items: Vec<ListItem>,
    /// Index of the currently selected item (skips headers).
    pub selected: usize,
    /// Font size for item rows.
    pub font_size: f32,
    /// When true, triggers a full despawn+rebuild of child entities.
    pub dirty: bool,
    /// Background alpha for selected row (default 0.1).
    pub selected_bg_alpha: f32,
}

impl SelectableList {
    /// Create a new selectable list.
    pub fn new(items: Vec<ListItem>, font_size: f32) -> Self {
        let selected = items.iter().position(|i| !i.is_header).unwrap_or(0);
        Self {
            items,
            selected,
            font_size,
            dirty: true, // initial build
            selected_bg_alpha: 0.1,
        }
    }

    /// Move selection to the next non-header item. Returns true if changed.
    pub fn select_next(&mut self) -> bool {
        let old = self.selected;
        let mut next = self.selected + 1;
        while next < self.items.len() {
            if !self.items[next].is_header {
                self.selected = next;
                self.dirty = true;
                return self.selected != old;
            }
            next += 1;
        }
        false
    }

    /// Move selection to the previous non-header item. Returns true if changed.
    pub fn select_prev(&mut self) -> bool {
        let old = self.selected;
        if self.selected == 0 {
            return false;
        }
        let mut prev = self.selected - 1;
        loop {
            if !self.items[prev].is_header {
                self.selected = prev;
                self.dirty = true;
                return self.selected != old;
            }
            if prev == 0 {
                break;
            }
            prev -= 1;
        }
        false
    }

    /// Get the currently selected item, if any.
    pub fn selected_item(&self) -> Option<&ListItem> {
        self.items.get(self.selected)
    }
}

// ============================================================================
// CHILD MARKERS
// ============================================================================

/// Marker on each row entity spawned by the sync system.
#[derive(Component)]
pub struct SelectableListRow(pub usize);

/// Marker on the text entity within a row.
#[derive(Component)]
struct SelectableListRowText;

// ============================================================================
// SYNC SYSTEM
// ============================================================================

/// Rebuilds `SelectableList` visuals when the component changes and `dirty` is set.
///
/// Full despawn+rebuild approach. Lists are small (<100 items) so this is fine.
pub fn sync_selectable_list_visuals(
    mut commands: Commands,
    theme: Res<Theme>,
    font_handles: Res<FontHandles>,
    mut lists: Query<(Entity, &mut SelectableList), Changed<SelectableList>>,
    existing_rows: Query<(Entity, &SelectableListRow, &ChildOf)>,
) {
    for (list_entity, mut list) in lists.iter_mut() {
        if !list.dirty {
            continue;
        }

        // Despawn existing rows for this list
        for (entity, _row, child_of) in existing_rows.iter() {
            if child_of.0 == list_entity {
                commands.entity(entity).despawn();
            }
        }

        // Spawn new rows
        let font_size = list.font_size;
        let row_height = (font_size * 1.2).ceil() + 5.0;
        let header_font_size = font_size - 2.0;
        let font = &font_handles.mono;

        for (i, item) in list.items.iter().enumerate() {
            let is_selected = i == list.selected && !item.is_header;

            if item.is_header {
                // Header row: smaller font, top margin, no indicator
                let row = commands
                    .spawn((
                        SelectableListRow(i),
                        Node {
                            width: Val::Percent(100.0),
                            height: Val::Px((header_font_size * 1.2).ceil() + 4.0),
                            margin: UiRect::top(if i == 0 {
                                Val::Px(0.0)
                            } else {
                                Val::Px(8.0)
                            }),
                            ..default()
                        },
                        BackgroundColor(Color::NONE),
                    ))
                    .with_children(|r| {
                        r.spawn((
                            SelectableListRowText,
                            UiVelloText {
                                value: item.label.clone(),
                                style: vello_style(font, theme.fg_dim, header_font_size),
                                ..default()
                            },
                            Node {
                                width: Val::Percent(100.0),
                                ..default()
                            },
                        ));
                    })
                    .id();
                commands.entity(list_entity).add_child(row);
            } else {
                // Normal item row
                let indicator = if is_selected { "\u{25B8} " } else { "  " };
                let text = format!("{}{}{}", indicator, item.label, item.suffix);
                let color = if is_selected {
                    theme.accent
                } else if item.enabled {
                    theme.fg
                } else {
                    theme.fg_dim
                };
                let bg = if is_selected {
                    theme.accent.with_alpha(list.selected_bg_alpha)
                } else {
                    Color::NONE
                };

                let row = commands
                    .spawn((
                        SelectableListRow(i),
                        Node {
                            width: Val::Percent(100.0),
                            height: Val::Px(row_height),
                            padding: UiRect::left(Val::Px(12.0)),
                            ..default()
                        },
                        BackgroundColor(bg),
                        Interaction::None, // Touch-ready
                    ))
                    .with_children(|r| {
                        r.spawn((
                            SelectableListRowText,
                            UiVelloText {
                                value: text,
                                style: vello_style(font, color, font_size),
                                ..default()
                            },
                            Node {
                                width: Val::Percent(100.0),
                                ..default()
                            },
                        ));
                    })
                    .id();
                commands.entity(list_entity).add_child(row);
            }
        }

        list.dirty = false;
    }
}

// ============================================================================
// CLICK HANDLER
// ============================================================================

/// Select a list item when its row is clicked.
///
/// Supplements keyboard navigation — clicking a non-header row selects it.
pub fn handle_selectable_list_click(
    rows: Query<(&SelectableListRow, &Interaction, &ChildOf), Changed<Interaction>>,
    mut lists: Query<&mut SelectableList>,
) {
    for (row, interaction, child_of) in rows.iter() {
        if !matches!(interaction, Interaction::Pressed) {
            continue;
        }
        let Ok(mut list) = lists.get_mut(child_of.0) else {
            continue;
        };
        let idx = row.0;
        if idx < list.items.len() && !list.items[idx].is_header && idx != list.selected {
            list.selected = idx;
            list.dirty = true;
        }
    }
}
