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
        Self::new()
    }
}

impl SharedFontSystem {
    /// Create a new SharedFontSystem with bundled Noto fonts.
    ///
    /// This ensures consistent rendering across different systems by loading
    /// our own fonts rather than relying on system font fallback.
    pub fn new() -> Self {
        let mut font_system = FontSystem::new();

        // Load bundled Noto fonts for consistent rendering
        // These paths are relative to the working directory (project root)
        let font_paths = [
            "assets/fonts/NotoMono-Regular.ttf",
            "assets/fonts/NotoSerif-Regular.ttf",
            // Fallback to test location if fonts haven't been moved yet
            "assets/test/fonts/NotoMono-Regular.ttf",
            "assets/test/fonts/NotoSerif-Regular.ttf",
        ];

        for path in &font_paths {
            let path = std::path::Path::new(path);
            if path.exists() {
                font_system.db_mut().load_font_file(path).ok();
                info!("Loaded font: {}", path.display());
            }
        }

        Self(Arc::new(Mutex::new(font_system)))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Color Conversion
// ─────────────────────────────────────────────────────────────────────────────

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
// MSDF Render Configuration
// ─────────────────────────────────────────────────────────────────────────────

/// Render configuration for MSDF text.
///
/// This is the **single source of truth** for MSDF rendering parameters.
/// It must be explicitly set before rendering can occur.
///
/// In windowed mode: Set by `sync_render_config_from_window` system.
/// In headless/test mode: Set directly by the test harness.
///
/// The render world extracts this resource - it never guesses or falls back.
#[derive(Resource, Clone, Copy, Debug)]
pub struct MsdfRenderConfig {
    /// Viewport resolution in physical pixels.
    pub resolution: [f32; 2],
    /// Texture format for the render target.
    pub format: bevy::render::render_resource::TextureFormat,
    /// Whether this config has been initialized.
    /// Systems will skip rendering if this is false.
    pub initialized: bool,
}

impl Default for MsdfRenderConfig {
    fn default() -> Self {
        Self {
            resolution: [0.0, 0.0],
            // Use bevy_default() as fallback for headless/test mode.
            // In windowed mode, init_msdf_resources queries ExtractedWindows for the
            // actual swap chain format, which is the authoritative source.
            format: bevy::render::render_resource::TextureFormat::bevy_default(),
            initialized: false,
        }
    }
}

#[allow(dead_code)]
impl MsdfRenderConfig {
    /// Create a new initialized config with the given resolution.
    ///
    /// Uses `bevy_default()` format as fallback for headless/test mode.
    /// In windowed mode, init_msdf_resources will query the actual swap chain format.
    /// Use `with_format()` to override if needed.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            resolution: [width as f32, height as f32],
            format: bevy::render::render_resource::TextureFormat::bevy_default(),
            initialized: true,
        }
    }

    /// Create config with explicit format (for matching window swap chain).
    pub fn with_format(mut self, format: bevy::render::render_resource::TextureFormat) -> Self {
        self.format = format;
        self
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.resolution[0] as u32
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.resolution[1] as u32
    }
}
