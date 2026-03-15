//! Input context — derived from FocusArea and Screen to determine which bindings are active.
//!
//! Each `FocusArea` maps to a set of active `InputContext` values.
//! `Screen::Constellation` activates the `Constellation` input context.
//! The dispatcher checks bindings against active contexts to determine matches.

use bevy::prelude::*;

use super::focus::FocusArea;
use crate::ui::screen::Screen;

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
    /// Active when Constellation screen is active: hjkl spatial, zoom, fork
    Constellation,
    /// Active when a modal dialog is open: Enter/Escape/j/k
    Dialog,
}

/// Resource tracking which input contexts are currently active.
///
/// Derived each frame by `sync_input_context` from `FocusArea` + `State<Screen>`.
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

/// System: derive active input contexts from the current FocusArea and Screen state.
///
/// Runs every frame before dispatch. Maps FocusArea + Screen → set of InputContext values.
pub fn sync_input_context(
    focus: Res<FocusArea>,
    screen: Res<State<Screen>>,
    mut active: ResMut<ActiveInputContexts>,
) {
    // Only update if focus or screen changed
    if !focus.is_changed() && !screen.is_changed() && !active.is_added() {
        return;
    }

    active.0.clear();

    // Global is always active
    active.0.push(InputContext::Global);

    // Screen-level context: Constellation input bindings are active on the
    // Constellation screen (unless a FocusStack dialog overrides)
    if matches!(screen.get(), Screen::Constellation) && !matches!(*focus, FocusArea::Dialog) {
        active.0.push(InputContext::Constellation);
        return;
    }

    // Within-conversation focus areas
    match focus.as_ref() {
        FocusArea::Compose => {
            active.0.push(InputContext::TextInput);
        }
        FocusArea::Conversation => {
            active.0.push(InputContext::Navigation);
        }
        FocusArea::Dialog => {
            active.0.push(InputContext::Dialog);
            active.0.push(InputContext::TextInput);
        }
    }
}
