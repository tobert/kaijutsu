//! Cell module — facade re-exporting from view/ during migration.
//!
//! All component types now live in `crate::view`. This module re-exports
//! them so existing `crate::cell::X` imports continue to work.

pub mod block_border;
mod components;
pub mod fieldset;
pub mod plugin;
mod systems;

// Re-export public API — types from components (which will delegate to view/ in Phase 6)
#[allow(unused_imports)]
pub use components::{
    BlockCell, BlockCellContainer, BlockCellLayout, BlockEditCursor, BlockId, BlockKind,
    BlockSnapshot, Cell, CellEditor, CellId, CellPosition, CellState, ComposeError,
    ContextId, ContextSwitchRequested, ConversationContainer, ConversationScrollState,
    DocumentCache, DriftKind, EditingBlockCell, FocusTarget, FocusedBlockCell, InputMode,
    InputOverlay, InputOverlayMarker, LayoutGeneration, MainCell, PendingContextSwitch,
    PrincipalId, PromptSubmitted, Role, RoleGroupBorder, RoleGroupBorderLayout,
    SessionAgent, Status, SubmitFailed, ViewingConversation, WorkspaceLayout,
};
pub use plugin::CellPlugin;
pub use systems::EditorEntities;
