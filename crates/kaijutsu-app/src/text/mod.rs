//! Text rendering module using MSDF (Multi-channel Signed Distance Fields).
//!
//! Provides GPU-accelerated text rendering that integrates with Bevy's render pipeline.
//! Uses cosmic-text for shaping/layout and MSDF textures for crisp rendering at any scale.
//!
//! ## Architecture
//!
//! ```text
//! cosmic-text (shaping + layout)
//!     ↓
//! MsdfTextBuffer (positioned glyphs)
//!     ↓
//! MsdfAtlas (glyph_id → texture region)
//!     ↓
//! msdf_text.wgsl (GPU rendering with effects)
//! ```

pub mod msdf;
mod plugin;
mod resources;

pub use msdf::{
    FontMetricsCache, MsdfText, MsdfTextBuffer, MsdfTextAreaConfig, MsdfUiText,
    TextBounds, UiTextPositionCache,
};
pub use plugin::TextRenderPlugin;
pub use resources::{bevy_to_rgba8, SharedFontSystem, TextMetrics};
