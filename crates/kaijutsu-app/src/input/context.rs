//! Input context — derived from FocusArea to determine which bindings are active.
//!
//! Each `FocusArea` maps to a set of active `InputContext` values.
//! The dispatcher checks bindings against active contexts to determine matches.

use bevy::prelude::*;

use super::focus::FocusArea;

/// Binding context — determines when a binding is active.
///
/// Multiple contexts can be active simultaneously (e.g. Global + Navigation).
/// The dispatcher matches bindings whose context is in the active set.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Reflect)]
pub enum InputContext {
    /// Always active regardless of focus: F1, F12, tiling keys
    Global,
    /// Active when Compose or EditingBlock has focus: text chars, editing actions
    TextInput,
    /// Active when Conversation block list has focus: j/k, f, Tab
    Navigation,
    /// Active when Constellation is focused: hjkl spatial, zoom, fork
    Constellation,
    /// Active when a modal dialog is open: Enter/Escape/j/k
    Dialog,
    /// Active when Dashboard is shown: Enter, arrow nav
    Dashboard,
}

/// Resource tracking which input contexts are currently active.
///
/// Derived each frame by `sync_input_context` from `FocusArea`.
/// The dispatcher reads this to determine which bindings to evaluate.
#[derive(Resource, Default, Reflect)]
#[reflect(Resource)]
pub struct ActiveInputContexts(pub Vec<InputContext>);

impl ActiveInputContexts {
    /// Check if a context is currently active.
    pub fn contains(&self, ctx: InputContext) -> bool {
        self.0.contains(&ctx)
    }
}

/// System: derive active input contexts from the current FocusArea.
///
/// Runs every frame before dispatch. Maps FocusArea → set of InputContext values.
pub fn sync_input_context(
    focus: Res<FocusArea>,
    mut active: ResMut<ActiveInputContexts>,
) {
    // Only update if focus changed
    if !focus.is_changed() && !active.is_added() {
        return;
    }

    active.0.clear();

    // Global is always active
    active.0.push(InputContext::Global);

    match focus.as_ref() {
        FocusArea::Compose | FocusArea::EditingBlock { .. } => {
            active.0.push(InputContext::TextInput);
        }
        FocusArea::Conversation => {
            active.0.push(InputContext::Navigation);
        }
        FocusArea::Constellation => {
            active.0.push(InputContext::Constellation);
        }
        FocusArea::Dialog => {
            active.0.push(InputContext::Dialog);
        }
        FocusArea::Dashboard => {
            active.0.push(InputContext::Dashboard);
        }
    }
}
