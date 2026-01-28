//! Shared text rendering resources.

use bevy::prelude::*;
use cosmic_text::{FontSystem, Metrics};
use std::sync::{Arc, Mutex};

// ─────────────────────────────────────────────────────────────────────────────
// Text Metrics Resource (DPI-aware font sizing)
// ─────────────────────────────────────────────────────────────────────────────

/// Centralized text metrics for consistent, DPI-aware font sizing.
///
/// All text rendering should use this resource instead of hardcoding
/// Metrics::new() calls. The scale_factor is updated when the window
/// resizes and is automatically applied via scaled_*_metrics() methods.
///
/// **Customization:** These values can be hot-reloaded from theme or
/// user preferences in the future.
#[derive(Resource, Clone)]
pub struct TextMetrics {
    /// Base font size for content cells (blocks, code). Default: 15.0
    pub cell_font_size: f32,
    /// Line height for content cells. Default: 22.5 (1.5x font size)
    pub cell_line_height: f32,
    /// Window scale factor, updated from window resize events.
    /// Applied automatically in scaled_*_metrics() methods.
    pub scale_factor: f32,
}

impl Default for TextMetrics {
    fn default() -> Self {
        Self {
            cell_font_size: 15.0,
            cell_line_height: 22.5, // 1.5x for comfortable reading
            scale_factor: 1.0,
        }
    }
}

impl TextMetrics {
    /// Get scaled metrics for content cells (conversation blocks, code).
    ///
    /// Use this for MsdfTextBuffer in BlockCells, PromptCell, etc.
    pub fn scaled_cell_metrics(&self) -> Metrics {
        Metrics::new(
            self.cell_font_size * self.scale_factor,
            self.cell_line_height * self.scale_factor,
        )
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared Resources
// ─────────────────────────────────────────────────────────────────────────────

/// Shared font system for all text rendering.
/// Wrapped in Arc<Mutex> because FontSystem isn't Send+Sync but we need to share it.
#[derive(Resource, Clone)]
pub struct SharedFontSystem(pub Arc<Mutex<FontSystem>>);

impl Default for SharedFontSystem {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(FontSystem::new())))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Color Conversion
// ─────────────────────────────────────────────────────────────────────────────

/// Convert Bevy Color to cosmic-text Color.
///
/// Uses sRGB color space - cosmic-text expects 0-255 sRGB values.
#[allow(dead_code)]
pub fn bevy_to_cosmic_color(color: Color) -> cosmic_text::Color {
    let srgba = color.to_srgba();
    cosmic_text::Color::rgba(
        (srgba.red * 255.0) as u8,
        (srgba.green * 255.0) as u8,
        (srgba.blue * 255.0) as u8,
        (srgba.alpha * 255.0) as u8,
    )
}

/// Convert Bevy Color to RGBA8 array.
pub fn bevy_to_rgba8(color: Color) -> [u8; 4] {
    let srgba = color.to_srgba();
    [
        (srgba.red * 255.0) as u8,
        (srgba.green * 255.0) as u8,
        (srgba.blue * 255.0) as u8,
        (srgba.alpha * 255.0) as u8,
    ]
}

// ─────────────────────────────────────────────────────────────────────────────
// Text Resolution
// ─────────────────────────────────────────────────────────────────────────────

/// Current screen resolution for text rendering.
#[derive(Resource, Clone, Copy)]
pub struct TextResolution {
    pub width: u32,
    pub height: u32,
}

impl Default for TextResolution {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 800,
        }
    }
}
