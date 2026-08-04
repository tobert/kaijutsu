//! View module — component types and rendering systems for every full-viewport
//! scene, not just the conversation.
//!
//! Owns all component types for the conversation view; `cell/mod.rs`
//! re-exports from here so existing `crate::cell::X` imports keep resolving.
//! Rough groupings of the modules below:
//! - Conversation block pipeline: `lifecycle` (spawn/despawn block cell
//!   entities), `render`/`block_render` (buffer sync, MSDF/SVG/scene
//!   content build), `format` (pure formatting helpers), `geometry` (row
//!   layout readback), `sync`/`submit`/`document`/`editor`/`role_divider`
//!   (server sync, prompt submission, the CellEditor buffer, role-divider
//!   layout math).
//! - Chat-adjacent surfaces: `overlay` (the input overlay), `shell_dock`,
//!   `scroll`.
//! - Room-level scenes reached from the shell: `room` (station carousel),
//!   `time_well`, `patch_bay`, `fsn`, `tracker` — sharing the
//!   `scene_geometry` datums (octagon shell, W-wall patch-wheel mount) they
//!   have to agree on without eyeballing each other.
//! - Styling: `scene_palette`.
//! - Render plumbing: `vello_rasterizer`, `ui_rtt` (render-to-texture
//!   sizing). Bevy Remote Protocol inspector glue lives in
//!   `kaish::brp_methods` (kaish-adjacent: agents drive both over the same
//!   protocol).
//! - `components` — the component/resource types shared across the above.

pub mod block_render;
pub mod components;
pub mod document;
pub mod diff_view;
pub mod editor;
pub mod format;
pub mod fsn;
pub mod geometry;
pub mod lifecycle;
pub mod overlay;
pub mod patch_bay;
pub mod render;
pub mod role_divider;
pub mod room;
pub mod scene_geometry;
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
