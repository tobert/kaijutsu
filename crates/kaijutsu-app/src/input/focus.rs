//! Focus system — what has keyboard focus determines available actions.
//!
//! `FocusArea` replaces the old vim-style `CurrentMode`. "Mode" is emergent
//! from what has focus, not an explicit state machine.

use bevy::prelude::*;

/// Modal focus stack — push when opening a modal, pop when closing.
///
/// ## Two-Stack Architecture
///
/// Kaijutsu has two independent stacks that work together:
///
/// ### FocusStack (Modal Overlay Focus)
/// Manages keyboard focus for modal dialogs (model picker, create context).
/// - `push()`: Save current FocusArea, switch to Dialog
/// - `pop()`: Restore previous FocusArea
/// - Used by: model_picker, create_dialog
///
/// ### ViewStack (Content Navigation)
/// Manages which content view is displayed (conversation, expanded block, etc.).
/// - `push()`: Show overlay view (ExpandedBlock)
/// - `pop()`: Return to previous view
/// - Used by: handle_expand_block, handle_unfocus (Escape)
///
/// They are orthogonal:
/// - FocusStack is about *who gets keyboard input*
/// - ViewStack is about *what content is visible*
/// - A dialog (FocusStack) can be open over an expanded block (ViewStack)
#[derive(Resource, Default, Reflect)]
#[reflect(Resource)]
pub struct FocusStack(pub Vec<FocusArea>);

impl FocusStack {
    /// Push current focus onto the stack and switch to new focus.
    pub fn push(&mut self, focus: &mut FocusArea, new: FocusArea) {
        self.0.push(focus.clone());
        *focus = new;
    }

    /// Pop and restore previous focus. Returns None if stack empty.
    pub fn pop(&mut self, focus: &mut FocusArea) -> Option<FocusArea> {
        if let Some(prev) = self.0.pop() {
            let was = std::mem::replace(focus, prev.clone());
            Some(was)
        } else {
            None
        }
    }

    /// True if a modal layer is active (anything pushed).
    pub fn is_modal(&self) -> bool {
        !self.0.is_empty()
    }
}

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

/// System run condition: FocusArea is EditingBlock.
pub fn in_editing_block(focus: Res<FocusArea>) -> bool {
    matches!(*focus, FocusArea::EditingBlock)
}

/// System run condition: FocusArea is Constellation.
pub fn in_constellation(focus: Res<FocusArea>) -> bool {
    matches!(*focus, FocusArea::Constellation)
}

/// System run condition: any text input mode (Compose or EditingBlock).
#[allow(dead_code)]
pub fn in_text_input(focus: Res<FocusArea>) -> bool {
    focus.is_text_input()
}

/// System run condition: FocusArea allows conversation scrolling.
pub fn scroll_context_active(focus: Res<FocusArea>) -> bool {
    matches!(
        *focus,
        FocusArea::Conversation | FocusArea::Compose | FocusArea::EditingBlock
    )
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
    Compose,
    /// Conversation block list. j/k navigates, Enter/i activates, f expands.
    Conversation,
    /// Inline editing of an existing block (User Text blocks only).
    EditingBlock,
    /// Constellation node graph. hjkl spatial nav, Enter switches context.
    #[default]
    Constellation,
    /// Modal dialog. Captures all input. Enter confirms, Escape cancels.
    Dialog,
}

impl FocusArea {
    /// Check if focus is on text input (Compose or EditingBlock).
    pub fn is_text_input(&self) -> bool {
        matches!(self, FocusArea::Compose | FocusArea::EditingBlock)
    }

    /// Check if focus is on navigation (Conversation blocks).
    #[allow(dead_code)] // Useful for domain guards
    pub fn is_navigation(&self) -> bool {
        matches!(self, FocusArea::Conversation)
    }

    /// Human-readable name for the current focus area (for hint widget).
    pub fn name(&self) -> &'static str {
        match self {
            FocusArea::Compose => "COMPOSE",
            FocusArea::Conversation => "NAVIGATE",
            FocusArea::EditingBlock => "EDITING",
            FocusArea::Constellation => "CONSTELLATION",
            FocusArea::Dialog => "DIALOG",
        }
    }
}
