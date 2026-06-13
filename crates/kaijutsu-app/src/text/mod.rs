//! Text rendering module.
//!
//! GPU-accelerated text via two paths sharing one Parley shaping source: MSDF
//! glyphs (shader-quality text) and Vello-rasterized vector content (SVG, ABC,
//! sparklines, borders).

pub mod abc;
pub mod components;
pub mod markdown;
pub mod msdf;
mod plugin;
mod resources;
pub mod rich;
pub mod shaping;
pub mod sparkline;

pub use components::{KjTextEffects, bevy_color_to_brush};
pub use plugin::KjTextPlugin;
pub use resources::{ShapingFonts, SvgFontDb, TextMetrics};
pub use rich::RichContent;

/// Char-aware truncation (safe for multi-byte UTF-8).
///
/// Returns the original string if it fits within `max` chars,
/// otherwise truncates to `max - 1` chars and appends '…'.
pub fn truncate_chars(s: &str, max: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max - 1).collect();
        format!("{truncated}…")
    }
}
