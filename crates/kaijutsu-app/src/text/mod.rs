//! Text rendering module using Vello (vector graphics).
//!
//! Provides GPU-accelerated text rendering via bevy_vello, which uses
//! Parley for text layout and Vello for vector path rendering.

pub mod components;
pub mod markdown;
mod plugin;
mod resources;
pub mod rich;
pub mod sparkline;

pub use components::{KjText, KjTextEffects, bevy_color_to_brush, vello_style};
pub use plugin::KjTextPlugin;
pub use resources::{FontHandles, SvgFontDb, TextMetrics};
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
