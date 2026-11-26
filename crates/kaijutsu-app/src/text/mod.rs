//! Text rendering module.
//!
//! GPU-accelerated text via two paths sharing one Parley shaping source: MSDF
//! glyphs (shader-quality text, plus ABC music notation together with
//! `msdf::geometry`'s flat-colored triangles) and CPU-rasterized inline SVG
//! (`svg_raster`: usvg + resvg + tiny-skia → a Bevy `Image`/`ImageNode`).
//! Sparklines are plain UI rectangle geometry (`sparkline`); block
//! borders/role dividers are an SDF shader (`cell::block_border`/`shaders`).
//! Nothing here rasterizes through vello any more — ABC and SVG were its last
//! two consumers and came off on separate branches; vello survives only as
//! the Parley shaping / `Brush` source behind the MSDF path.

pub mod abc;
pub mod ansi;
pub mod components;
pub mod diff;
pub mod msdf;
mod plugin;
mod resources;
pub mod rich;
pub mod shaping;
pub mod sparkline;
pub mod svg_raster;

pub use components::{bevy_color_to_brush, color_to_rgba8};
pub use plugin::KjTextPlugin;
pub use resources::{ShapingFonts, SvgFontDb, TextMetrics};

pub fn tool_input_head(block: &kaijutsu_types::BlockSnapshot, field: &str, max: usize) -> Option<String> {
    let input: serde_json::Value = serde_json::from_str(block.tool_input.as_deref()?).ok()?;
    let value = input.get(field)?.as_str()?;
    let line = value.lines().map(str::trim).find(|line| !line.is_empty())?;
    Some(truncate_chars(line, max))
}

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
