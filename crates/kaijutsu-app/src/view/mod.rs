//! View module — component types and rendering systems for every full-viewport
//! scene, not just the conversation.
//!
//! Owns all component types for the conversation view; `cell/mod.rs`
//! re-exports from here so existing `crate::cell::X` imports keep resolving.
//! Rough groupings of the modules below:
//! - Conversation block pipeline: `lifecycle` (the MainCell + focused-pane
//!   singleton bookkeeping), `surface` (the conversation renderer —
//!   entity-free content/shape/measure/window spine), `block_render` (the
//!   shared MSDF/SVG render plumbing every text surface, including
//!   `surface`, draws through), `format` (pure formatting helpers),
//!   `geometry` (row layout model), `sync`/`submit`/`document`/`editor`/
//!   `role_divider` (server sync, prompt submission, the CellEditor buffer,
//!   role-divider layout math).
//! - Chat-adjacent surfaces: `overlay` (the input overlay), `shell_dock`,
//!   `scroll`.
//! - Room-level scenes reached from the shell: `room` (station carousel) and
//!   `time_well` — sharing the `scene_geometry` datums (the octagon shell)
//!   they have to agree on without eyeballing each other.
//! - Styling: `scene_palette`.
//! - Render plumbing: `ui_rtt` (the generic render-to-texture primitive +
//!   sizing helpers; vello is gone from the app). Bevy
//!   Remote Protocol inspector glue lives in `kaish::brp_methods`
//!   (kaish-adjacent: agents drive both over the same protocol).
//! - `components` — the component/resource types shared across the above.

pub mod block_render;
pub mod components;
pub mod document;
pub mod diff_view;
pub mod editor;
pub mod geometry;
pub mod lifecycle;
pub mod overlay;
pub mod render_store;
pub mod role_divider;
pub mod room;
pub mod scene_geometry;
pub mod scene_palette;
pub mod shell_dock;
pub mod scroll;
pub mod submit;
pub mod surface;
pub mod sync;
pub mod time_well;
pub mod ui_rtt;

// Re-export all public types
pub use components::*;
pub use document::{DocumentCache, ScrollOffsets};
pub use lifecycle::EditorEntities;
