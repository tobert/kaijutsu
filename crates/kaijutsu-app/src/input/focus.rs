//! Focus system — what has keyboard focus determines available actions.
//!
//! `FocusArea` replaces the old vim-style `CurrentMode`. "Mode" is emergent
//! from what has focus, not an explicit state machine.

use bevy::prelude::*;

// ============================================================================
// RUN CONDITIONS
// ============================================================================

/// System run condition: FocusArea is Conversation.
pub fn in_conversation(focus: Res<FocusArea>) -> bool {
    matches!(*focus, FocusArea::Conversation)
}

/// System run condition: FocusArea allows conversation scrolling.
pub fn scroll_context_active(focus: Res<FocusArea>) -> bool {
    matches!(*focus, FocusArea::Conversation | FocusArea::Compose)
}

/// What area of the UI currently has keyboard focus.
///
/// This is the single source of truth for "what should keyboard input do?"
/// Domain systems check `FocusArea` to know whether they should act.
///
/// BRP-queryable: `world_get_resources("kaijutsu_app::input::focus::FocusArea")`
#[derive(Resource, Clone, Default, PartialEq, Debug, Reflect)]
#[reflect(Resource)]
pub enum FocusArea {
    /// Compose text input area. Typing inserts text. Enter submits.
    #[default]
    Compose,
    /// Conversation block list. j/k navigates, Enter/i activates, f expands.
    Conversation,
    /// Modal dialog. Captures all input. Enter confirms, Escape cancels.
    Dialog,
}

// ============================================================================
// ACTIVE SURFACE — which input surface has focus within Compose
// ============================================================================

/// Which input surface is active when `FocusArea::Compose`.
///
/// Chat and Shell are spatially separated: Chat uses a floating overlay,
/// Shell uses a bottom-dock input row. Ctrl+Z toggles between them.
#[derive(Resource, Clone, Default, PartialEq, Debug, Reflect)]
#[reflect(Resource)]
pub enum ActiveSurface {
    /// Floating compose overlay — input goes to AI conversation.
    #[default]
    Chat,
    /// Bottom-dock shell input — input goes to kaish context shell.
    Shell,
}

impl ActiveSurface {
    pub fn is_shell(&self) -> bool {
        matches!(self, Self::Shell)
    }
}
