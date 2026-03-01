//! View module — the block rendering pipeline.
//!
//! Owns all component types for the conversation view. During migration,
//! `cell/mod.rs` re-exports from here so existing `crate::cell::X` imports
//! continue to resolve.

mod components;
pub mod document;
pub mod fieldset;

// Re-export all public types for the facade
pub use components::*;
pub use document::{CachedDocument, DocumentCache};
