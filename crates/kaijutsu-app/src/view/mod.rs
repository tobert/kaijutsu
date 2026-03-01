//! View module — the block rendering pipeline.
//!
//! Owns all component types for the conversation view. During migration,
//! `cell/mod.rs` re-exports from here so existing `crate::cell::X` imports
//! continue to resolve.
//!
//! Phase 3 adds the core rendering systems:
//! - `format` — pure formatting functions (block_color, format_single_block)
//! - `lifecycle` — spawn/despawn block cell entities (TopLeft anchor, no UiTransform)
//! - `render` — buffer sync (text → UiVelloText), layout readback

pub mod components;
pub mod document;
pub mod fieldset;
pub mod cursor;
pub mod format;
pub mod lifecycle;
pub mod overlay;
pub mod render;
pub mod scroll;
pub mod submit;
pub mod sync;

// Re-export all public types
pub use components::*;
pub use cursor::CursorMarker;
pub use document::{CachedDocument, DocumentCache};
pub use lifecycle::EditorEntities;
