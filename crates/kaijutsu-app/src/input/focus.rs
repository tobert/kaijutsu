//! Focus system — what has keyboard focus determines available actions.
//!
//! `FocusArea` replaces the old vim-style `CurrentMode`. "Mode" is emergent
//! from what has focus, not an explicit state machine.

use bevy::prelude::*;
use kaijutsu_crdt::BlockId;

/// What area of the UI currently has keyboard focus.
///
/// This is the single source of truth for "what should keyboard input do?"
/// Domain systems check `FocusArea` to know whether they should act.
///
/// BRP-queryable: `world_get_resources("kaijutsu_app::input::focus::FocusArea")`
#[derive(Resource, Clone, PartialEq, Debug, Reflect)]
#[reflect(Resource)]
pub enum FocusArea {
    /// Compose text input area. Typing inserts text. Enter submits.
    Compose,
    /// Conversation block list. j/k navigates, Enter/i activates, f expands.
    Conversation,
    /// Inline editing of an existing block (User Text blocks only).
    EditingBlock {
        /// The block being edited (not reflected — BlockId has no Default)
        #[reflect(ignore)]
        block_id: Option<BlockId>,
    },
    /// Constellation node graph. hjkl spatial nav, Enter switches context.
    Constellation,
    /// Modal dialog. Captures all input. Enter confirms, Escape cancels.
    Dialog,
    /// Dashboard screen. Enter selects.
    Dashboard,
}

impl Default for FocusArea {
    fn default() -> Self {
        // Start on Dashboard until a conversation is joined
        FocusArea::Dashboard
    }
}

// Phase 2+: these methods are consumed by domain systems migrating to FocusArea
#[allow(dead_code)]
impl FocusArea {
    /// Create an EditingBlock focus with a block ID.
    pub fn editing(block_id: BlockId) -> Self {
        FocusArea::EditingBlock {
            block_id: Some(block_id),
        }
    }

    /// Check if focus is on text input (Compose or EditingBlock).
    pub fn is_text_input(&self) -> bool {
        matches!(self, FocusArea::Compose | FocusArea::EditingBlock { .. })
    }

    /// Check if focus is on navigation (Conversation blocks).
    pub fn is_navigation(&self) -> bool {
        matches!(self, FocusArea::Conversation)
    }

    /// Get the block ID if editing a block.
    pub fn editing_block_id(&self) -> Option<&BlockId> {
        match self {
            FocusArea::EditingBlock { block_id } => block_id.as_ref(),
            _ => None,
        }
    }

    /// Human-readable name for the current focus area (for hint widget).
    pub fn name(&self) -> &'static str {
        match self {
            FocusArea::Compose => "COMPOSE",
            FocusArea::Conversation => "NAVIGATE",
            FocusArea::EditingBlock { .. } => "EDITING",
            FocusArea::Constellation => "CONSTELLATION",
            FocusArea::Dialog => "DIALOG",
            FocusArea::Dashboard => "DASHBOARD",
        }
    }
}
