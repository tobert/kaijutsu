//! Shared text rendering resources.

use bevy::prelude::*;
use glyphon::{
    Buffer, Cache, FontSystem, Metrics, Resolution, SwashCache, TextAtlas, TextRenderer, Viewport,
};
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
    /// Base font size for UI elements (labels, status). Default: 13.0
    pub ui_font_size: f32,
    /// Line height for UI elements. Default: 18.2 (1.4x font size)
    pub ui_line_height: f32,
    /// Window scale factor, updated from window resize events.
    /// Applied automatically in scaled_*_metrics() methods.
    pub scale_factor: f32,
}

impl Default for TextMetrics {
    fn default() -> Self {
        Self {
            cell_font_size: 15.0,
            cell_line_height: 22.5,  // 1.5x for comfortable reading
            ui_font_size: 13.0,
            ui_line_height: 18.2,    // 1.4x for compact UI
            scale_factor: 1.0,
        }
    }
}

impl TextMetrics {
    /// Get scaled metrics for content cells (conversation blocks, code).
    ///
    /// Use this for GlyphonTextBuffer in BlockCells, PromptCell, etc.
    pub fn scaled_cell_metrics(&self) -> Metrics {
        Metrics::new(
            self.cell_font_size * self.scale_factor,
            self.cell_line_height * self.scale_factor,
        )
    }

    /// Get scaled metrics for UI elements (labels, status bar, headers).
    ///
    /// Use this for GlyphonUiText and other UI chrome.
    pub fn scaled_ui_metrics(&self) -> Metrics {
        Metrics::new(
            self.ui_font_size * self.scale_factor,
            self.ui_line_height * self.scale_factor,
        )
    }

    /// Get unscaled cell metrics (for layout calculations before scaling).
    pub fn cell_metrics(&self) -> Metrics {
        Metrics::new(self.cell_font_size, self.cell_line_height)
    }

    /// Get unscaled UI metrics (for layout calculations before scaling).
    pub fn ui_metrics(&self) -> Metrics {
        Metrics::new(self.ui_font_size, self.ui_line_height)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// UI Text Components (for simple text rendered via glyphon)
// ─────────────────────────────────────────────────────────────────────────────

/// UI text rendered via glyphon (simpler than GlyphonTextBuffer).
/// Use this for static or dynamic labels, titles, status text, etc.
///
/// Automatically requires `Visibility` so text participates in Bevy's
/// visibility system (respects parent `Visibility::Hidden`).
#[derive(Component, Clone)]
#[require(Visibility)]
pub struct GlyphonUiText {
    pub text: String,
    pub metrics: Metrics,
    pub family: glyphon::Family<'static>,
    pub color: glyphon::Color,
}

impl GlyphonUiText {
    /// Create new UI text with default settings (14px SansSerif, light gray).
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            metrics: Metrics::new(14.0, 20.0),
            family: glyphon::Family::SansSerif,
            color: glyphon::Color::rgb(220, 220, 240),
        }
    }

    /// Set font size (adjusts both font_size and line_height).
    pub fn with_font_size(mut self, size: f32) -> Self {
        self.metrics = Metrics::new(size, size * 1.4);
        self
    }

    /// Set text color from a Bevy Color.
    pub fn with_color(mut self, color: Color) -> Self {
        self.color = bevy_to_glyphon_color(color);
        self
    }
}

/// Caches computed screen position from Bevy UI layout.
#[derive(Component, Default, Clone)]
pub struct UiTextPositionCache {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

/// Convert Bevy Color to glyphon Color.
///
/// Uses sRGB color space - glyphon expects 0-255 sRGB values.
pub fn bevy_to_glyphon_color(color: Color) -> glyphon::Color {
    let srgba = color.to_srgba();
    glyphon::Color::rgba(
        (srgba.red * 255.0) as u8,
        (srgba.green * 255.0) as u8,
        (srgba.blue * 255.0) as u8,
        (srgba.alpha * 255.0) as u8,
    )
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

/// Shared swash cache for glyph rasterization.
#[derive(Resource, Clone)]
pub struct SharedSwashCache(pub Arc<Mutex<SwashCache>>);

impl Default for SharedSwashCache {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(SwashCache::new())))
    }
}

/// Core text rendering resources managed by the render world.
/// These are created during render app setup and accessed by the render node.
pub struct TextRenderResources {
    /// Kept alive for glyphon internals (viewport/atlas reference it)
    pub _cache: Cache,
    pub viewport: Viewport,
    pub atlas: TextAtlas,
    pub renderer: TextRenderer,
}

/// A text buffer wrapper that can be used as a Bevy component.
/// Wraps a glyphon Buffer for use with the cosmic-text Editor.
#[derive(Component)]
pub struct GlyphonTextBuffer {
    buffer: Buffer,
    dirty: bool,
    /// Cached visual line count (after text wrapping).
    cached_visual_lines: usize,
    /// The wrap width used for the cached visual line count.
    cached_wrap_width: f32,
    /// Cached hash of text content for extract-phase optimization.
    /// When text hasn't changed, extraction can skip the expensive text() call.
    text_hash: u64,
}

impl GlyphonTextBuffer {
    /// Create a new text buffer with the given metrics.
    pub fn new(font_system: &mut FontSystem, metrics: Metrics) -> Self {
        Self {
            buffer: Buffer::new(font_system, metrics),
            dirty: true,
            cached_visual_lines: 1,
            cached_wrap_width: 0.0,
            text_hash: 0,
        }
    }

    /// Set the buffer text with default attributes.
    pub fn set_text(
        &mut self,
        font_system: &mut FontSystem,
        text: &str,
        attrs: &glyphon::Attrs,
        shaping: glyphon::Shaping,
    ) {
        self.buffer.set_text(font_system, text, attrs, shaping, None);
        self.dirty = true;
        // Update text hash for extraction optimization
        self.text_hash = Self::hash_str(text);
    }

    /// Get the cached text hash for extraction-phase optimization.
    pub fn text_hash(&self) -> u64 {
        self.text_hash
    }

    /// Hash a string (used for cache invalidation).
    fn hash_str(s: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        s.hash(&mut hasher);
        hasher.finish()
    }

    /// Get the text content as a string.
    pub fn text(&self) -> String {
        self.buffer
            .lines
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Get the visual line count after text wrapping.
    ///
    /// This shapes the buffer if the content or wrap width has changed,
    /// then returns the cached visual line count. The visual line count
    /// reflects actual wrapped lines, not just explicit newlines.
    pub fn visual_line_count(&mut self, font_system: &mut FontSystem, wrap_width: f32) -> usize {
        // Reshape if dirty or wrap width changed significantly
        let width_changed = (self.cached_wrap_width - wrap_width).abs() > 1.0;

        if self.dirty || width_changed {
            self.buffer.set_size(font_system, Some(wrap_width), None);
            self.buffer.shape_until_scroll(font_system, false);
            self.cached_visual_lines = self.buffer.layout_runs().count().max(1);
            self.cached_wrap_width = wrap_width;
            self.dirty = false;
        }

        self.cached_visual_lines
    }
}

/// Text area configuration for rendering a buffer.
#[derive(Component, Clone)]
pub struct TextAreaConfig {
    /// Position from left edge of the screen.
    pub left: f32,
    /// Position from top edge of the screen.
    pub top: f32,
    /// Scale factor for the text.
    pub scale: f32,
    /// Clipping bounds.
    pub bounds: glyphon::TextBounds,
    /// Default text color.
    pub default_color: glyphon::Color,
}

impl Default for TextAreaConfig {
    fn default() -> Self {
        Self {
            left: 0.0,
            top: 0.0,
            scale: 1.0,
            // Valid bounds to prevent "Invalid text bounds" warnings.
            // These are placeholders that get overwritten by layout systems.
            bounds: glyphon::TextBounds {
                left: 0,
                top: 0,
                right: 800,
                bottom: 600,
            },
            default_color: glyphon::Color::rgb(220, 220, 240),
        }
    }
}

/// Marker component for entities that should be rendered with glyphon.
///
/// Automatically requires `Visibility` so text participates in Bevy's
/// visibility system.
#[derive(Component)]
#[require(Visibility)]
pub struct GlyphonText;

/// Current screen resolution for text rendering.
#[derive(Resource)]
pub struct TextResolution(pub Resolution);

impl Default for TextResolution {
    fn default() -> Self {
        Self(Resolution {
            width: 1280,
            height: 800,
        })
    }
}
