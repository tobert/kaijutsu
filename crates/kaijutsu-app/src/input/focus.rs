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

/// System run condition: FocusArea is Compose.
pub fn in_compose(focus: Res<FocusArea>) -> bool {
    matches!(*focus, FocusArea::Compose)
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

impl FocusArea {
    /// Check if focus is on navigation (Conversation blocks).
    #[allow(dead_code)] // Useful for domain guards
    pub fn is_navigation(&self) -> bool {
        matches!(self, FocusArea::Conversation)
    }

    /// Human-readable name for the current focus area (for hint widget).
    #[allow(dead_code)] // Hint widget not yet wired to this label.
    pub fn name(&self) -> &'static str {
        match self {
            FocusArea::Compose => "COMPOSE",
            FocusArea::Conversation => "NAVIGATE",
            FocusArea::Dialog => "DIALOG",
        }
    }
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
    /// Symmetric toggle between Chat and Shell.
    pub fn toggle(&mut self) {
        *self = match self {
            Self::Chat => Self::Shell,
            Self::Shell => Self::Chat,
        };
    }

    pub fn is_shell(&self) -> bool {
        matches!(self, Self::Shell)
    }
}

/// System run condition: ActiveSurface is Shell.
#[allow(dead_code)] // Used by Phase 2 shell dock systems
pub fn on_shell_surface(surface: Res<ActiveSurface>) -> bool {
    surface.is_shell()
}

/// System run condition: ActiveSurface is Chat.
#[allow(dead_code)] // Used by Phase 2 shell dock systems
pub fn on_chat_surface(surface: Res<ActiveSurface>) -> bool {
    !surface.is_shell()
}
