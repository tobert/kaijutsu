//! Text rendering resources for Vello-based text.

use bevy::prelude::*;
use bevy_vello::prelude::VelloFont;

/// Loaded font handles for consistent rendering.
#[derive(Resource, Clone)]
pub struct FontHandles {
    /// Monospace font (primary — code, blocks, compose).
    pub mono: Handle<VelloFont>,
    /// Serif font (secondary — headings, prose).
    pub serif: Handle<VelloFont>,
}

impl Default for FontHandles {
    fn default() -> Self {
        Self {
            mono: Handle::default(),
            serif: Handle::default(),
        }
    }
}

/// Centralized text metrics for consistent, DPI-aware font sizing.
///
/// All text rendering should use this resource instead of hardcoding
/// font sizes. The scale_factor is updated when the window resizes.
#[derive(Resource, Clone)]
pub struct TextMetrics {
    /// Base font size for content cells (blocks, code). Default: 16.0
    pub cell_font_size: f32,
    /// Line height for content cells. Default: 24.0 (1.5x font size)
    pub cell_line_height: f32,
    /// Extra letter-spacing in pixels. Default: 1.0
    pub letter_spacing: f32,
    /// Window scale factor, updated from window resize events.
    pub scale_factor: f32,
}

impl Default for TextMetrics {
    fn default() -> Self {
        Self {
            cell_font_size: 16.0,
            cell_line_height: 24.0,
            letter_spacing: 1.0,
            scale_factor: 1.0,
        }
    }
}
