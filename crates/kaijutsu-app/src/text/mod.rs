//! Text rendering module using glyphon + cosmic-text.
//!
//! Provides GPU-accelerated text rendering that integrates with Bevy's render pipeline.
//! This module handles the low-level text rendering; cells use it via the CellEditor component.

mod plugin;
mod render;
mod resources;

pub use plugin::TextRenderPlugin;
pub use resources::{GlyphonText, SharedFontSystem, TextAreaConfig, TextBuffer};
