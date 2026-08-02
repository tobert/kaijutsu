//! View module — component types and rendering systems for every full-viewport
//! scene, not just the conversation.
//!
//! Owns all component types for the conversation view; `cell/mod.rs`
//! re-exports from here so existing `crate::cell::X` imports keep resolving.
//! Rough groupings of the modules below:
//! - Conversation block pipeline: `lifecycle` (spawn/despawn block cell
//!   entities), `render`/`block_render` (buffer sync, MSDF/SVG/scene
//!   content build), `format` (pure formatting helpers), `geometry` (row
//!   layout readback), `sync`/`submit`/`document`/`editor`/`fieldset`
//!   (server sync, prompt submission, the CellEditor buffer, block fields).
//! - Chat-adjacent surfaces: `overlay` (the input overlay), `shell_dock`,
//!   `scroll`.
//! - Room-level scenes reached from the shell: `room` (station carousel),
//!   `time_well`, `patch_bay`, `fsn`, `tracker`.
//! - Styling: `palette`, `scene_palette`.
//! - Render plumbing: `vello_rasterizer`, `ui_rtt` (render-to-texture
//!   sizing), `brp_methods` (Bevy Remote Protocol inspector glue).
//! - `components` — the component/resource types shared across the above.

pub mod block_render;
pub mod brp_methods;
pub mod components;
pub mod document;
pub mod diff_view;
pub mod editor;
pub mod fieldset;
pub mod format;
pub mod fsn;
pub mod geometry;
pub mod lifecycle;
pub mod overlay;
pub mod palette;
pub mod patch_bay;
pub mod render;
pub mod room;
pub mod scene_palette;
pub mod shell_dock;
pub mod scroll;
pub mod submit;
pub mod sync;
pub mod time_well;
pub mod tracker;
pub mod vello_rasterizer;
pub mod ui_rtt;

// Re-export all public types
pub use components::*;
pub use document::{DocumentCache, ScrollOffsets};
pub use lifecycle::EditorEntities;
