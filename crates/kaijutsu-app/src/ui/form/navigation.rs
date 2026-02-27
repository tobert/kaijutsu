//! Form navigation — shared keyboard handling for all forms.
//!
//! Exports a *function*, not a system, to avoid message-consumption conflicts.
//! Domain code calls `handle_form_action` from its own input handler after
//! reading `ActionFired` / `TextInputReceived`.

use crate::input::action::Action;
use crate::ui::form::field::ActiveFormField;
use crate::ui::form::schema::{Form, FormFieldContainer};
use crate::ui::form::selectable::SelectableList;
use crate::ui::form::tree::{TreeCursorTarget, TreeView};

use bevy::prelude::*;

/// Result of `handle_form_action`.
pub enum FormActionResult {
    /// The action was consumed by form navigation (Tab, j/k, Space).
    Consumed,
    /// Enter pressed and the cursor is NOT on a tree category — domain should submit.
    Submit,
    /// Esc pressed — domain should cancel/close.
    Cancel,
    /// Not a form-level action — domain code should handle it.
    Ignored,
}

/// Handle form-level keyboard actions.
///
/// Call from your domain input handler. Returns what happened so the caller
/// can decide whether to submit, cancel, or fall through to domain-specific input.
///
/// Handles:
/// - `CycleFocusForward` / `CycleFocusBackward` (Tab/Shift+Tab) — field cycling
/// - `FocusNextBlock` / `FocusPrevBlock` (j/k) — list/tree navigation
/// - `SpatialNav` (hjkl) — list/tree navigation (vertical component)
/// - `Activate` (Enter) — submit or toggle-expand on tree category
/// - `Unfocus` (Esc) — cancel
///
/// Space-to-toggle for TreeView is handled via `handle_form_space` (separate call
/// since it comes from TextInputReceived, not ActionFired).
pub fn handle_form_action(
    action: &Action,
    form: &Form,
    active_field: &mut ActiveFormField,
    lists: &mut Query<(&FormFieldContainer, &mut SelectableList)>,
    trees: &mut Query<(&FormFieldContainer, &mut TreeView)>,
) -> FormActionResult {
    match action {
        // ── Field cycling ──
        Action::CycleFocusForward => {
            active_field.0 = (active_field.0 + 1) % form.field_count;
            FormActionResult::Consumed
        }
        Action::CycleFocusBackward => {
            active_field.0 = (active_field.0 + form.field_count - 1) % form.field_count;
            FormActionResult::Consumed
        }

        // ── j/k navigation ──
        Action::FocusNextBlock => {
            navigate_active_field(active_field.0, lists, trees, Direction::Next);
            FormActionResult::Consumed
        }
        Action::FocusPrevBlock => {
            navigate_active_field(active_field.0, lists, trees, Direction::Prev);
            FormActionResult::Consumed
        }

        // ── Spatial nav (hjkl) — only vertical for lists/trees ──
        Action::SpatialNav(dir) => {
            if dir.y > 0.0 {
                navigate_active_field(active_field.0, lists, trees, Direction::Next);
                FormActionResult::Consumed
            } else if dir.y < 0.0 {
                navigate_active_field(active_field.0, lists, trees, Direction::Prev);
                FormActionResult::Consumed
            } else {
                FormActionResult::Ignored
            }
        }

        // ── Enter — submit or toggle-expand ──
        Action::Activate => {
            // Check if the active field has a tree with cursor on a category
            let is_tree_category = trees
                .iter()
                .find(|(ffc, _)| ffc.0 == active_field.0)
                .and_then(|(_, tree)| tree.resolve_cursor())
                .map(|target| matches!(target, TreeCursorTarget::Category(_)))
                .unwrap_or(false);

            if is_tree_category {
                // Toggle expand on category
                for (ffc, mut tree) in trees.iter_mut() {
                    if ffc.0 == active_field.0 {
                        tree.toggle_expand();
                        break;
                    }
                }
                FormActionResult::Consumed
            } else {
                FormActionResult::Submit
            }
        }

        // ── Esc ──
        Action::Unfocus => FormActionResult::Cancel,

        _ => FormActionResult::Ignored,
    }
}

/// Handle Space key from TextInputReceived for TreeView toggle.
///
/// Call from your domain input handler when processing text events on a field
/// that contains a TreeView. Returns true if the toggle was handled.
pub fn handle_form_space(
    active_field_id: u8,
    trees: &mut Query<(&FormFieldContainer, &mut TreeView)>,
) -> bool {
    for (ffc, mut tree) in trees.iter_mut() {
        if ffc.0 == active_field_id {
            return tree.toggle_item();
        }
    }
    false
}

// ============================================================================
// INTERNALS
// ============================================================================

enum Direction {
    Next,
    Prev,
}

fn navigate_active_field(
    active_field_id: u8,
    lists: &mut Query<(&FormFieldContainer, &mut SelectableList)>,
    trees: &mut Query<(&FormFieldContainer, &mut TreeView)>,
    dir: Direction,
) {
    // Try lists first
    for (ffc, mut list) in lists.iter_mut() {
        if ffc.0 == active_field_id {
            match dir {
                Direction::Next => {
                    list.select_next();
                }
                Direction::Prev => {
                    list.select_prev();
                }
            }
            return;
        }
    }

    // Then trees
    for (ffc, mut tree) in trees.iter_mut() {
        if ffc.0 == active_field_id {
            match dir {
                Direction::Next => {
                    tree.cursor_next();
                }
                Direction::Prev => {
                    tree.cursor_prev();
                }
            }
            return;
        }
    }
}
