//! Cell module - the universal primitive for content.
//!
//! Cells are the fundamental unit of content in Kaijutsu. Everything is a cell:
//! code, markdown, conversation messages, tool output, etc.
//!
//! Each cell has:
//! - A unique ID
//! - A kind (code, markdown, output, system)
//! - A cosmic-text Editor for text manipulation
//! - A position in the workspace grid
//!
//! ## 9-Slice Frame System
//!
//! Cells are rendered with a 9-slice frame system that uses procedural shaders
//! for ornate cyberpunk borders. Frame configuration (colors, sizes, shader params)
//! comes from the Theme resource, loaded from ~/.config/kaijutsu/theme.rhai.

mod components;
pub mod frame_assembly;
pub mod plugin;
mod sync;
mod systems;

// Re-export public API
pub use components::{
    Cell, CellEditor, ComposeBlock, ConversationContainer, CurrentMode, EditorMode, FocusedCell,
    FocusedBlockCell, InputKind, MainCell, PromptContainer,
};
pub use plugin::CellPlugin;
