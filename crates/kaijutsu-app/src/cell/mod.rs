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
//! for ornate cyberpunk borders. Frame styles are configured via RON files
//! in `assets/frames/`.

mod components;
pub mod frame_assembly;
pub mod frame_style;
pub mod plugin;
mod sync;
mod systems;

pub use components::{
    Cell, CellEditor, CellKind, CellPosition, CellState, CurrentMode, EditOp,
    EditorMode, FocusedCell, WorkspaceLayout,
};
pub use frame_style::{FrameStyle, FrameStyleMapping};
pub use plugin::CellPlugin;
pub use sync::CellRegistry;
