//! Text rendering resources for Vello-based text.

use bevy::prelude::*;
use bevy_vello::prelude::VelloFont;

/// Loaded font handles for consistent rendering.
#[derive(Resource, Clone, Default)]
pub struct FontHandles {
    /// Monospace font (primary — code, blocks, compose).
    pub mono: Handle<VelloFont>,
    /// Serif font (secondary — headings, prose).
    pub serif: Handle<VelloFont>,
    /// CJK font (Noto Sans CJK JP Light — Japanese/Chinese/Korean glyphs).
    pub cjk: Handle<VelloFont>,
}

/// Centralized text metrics for consistent, DPI-aware font sizing.
///
/// All text rendering should use this resource instead of hardcoding
/// font sizes. The scale_factor is updated when the window resizes.
/// `cell_line_height` and `cell_char_width` start as reasonable defaults
/// and are updated from the actual font metrics once the font asset loads.
#[derive(Resource, Clone)]
#[allow(dead_code)]
pub struct TextMetrics {
    /// Base font size for content cells (blocks, code). Default: 16.0
    pub cell_font_size: f32,
    /// Line height for content cells. Updated from font metrics on load.
    pub cell_line_height: f32,
    /// Character width for monospace font. Updated from font metrics on load.
    /// Default: 16.0 * 0.6 = 9.6 (approximation until font loads).
    pub cell_char_width: f32,
    /// Extra letter-spacing in pixels. Default: 1.0
    pub letter_spacing: f32,
    /// Window scale factor, updated from window resize events.
    pub scale_factor: f32,
    /// Whether cell_line_height has been measured from the actual font.
    pub cell_line_height_from_font: bool,
    /// Whether cell_char_width has been measured from the actual font.
    pub cell_char_width_from_font: bool,
}

impl Default for TextMetrics {
    fn default() -> Self {
        Self {
            cell_font_size: 20.0,
            cell_line_height: 30.0,
            cell_char_width: 20.0 * 0.6, // 12.0 — approximation until font loads
            letter_spacing: 1.0,
            scale_factor: 1.0,
            cell_line_height_from_font: false,
            cell_char_width_from_font: false,
        }
    }
}
