//! Cell module — facade re-exporting from view/ during migration.
//!
//! All component types now live in `crate::view`. This module re-exports
//! them so existing `crate::cell::X` imports continue to work.

pub mod block_border;
mod components;
pub mod fieldset;
pub mod plugin;
mod systems;

// Re-export public API — types from view/ via components facade
#[allow(unused_imports)]
pub use components::{
    BlockCell, BlockCellContainer, BlockCellLayout, BlockEditCursor, Cell, CellEditor,
    CellState, ComposeError, ContextSwitchRequested, ConversationContainer,
    ConversationScrollState, DocumentCache, EditingBlockCell,
    FocusTarget, FocusedBlockCell, InputMode, InputOverlay, InputOverlayMarker,
    MainCell, PromptSubmitted, RoleGroupBorder, SubmitFailed,
    ViewingConversation,
};
pub use plugin::CellPlugin;
pub use systems::EditorEntities;
