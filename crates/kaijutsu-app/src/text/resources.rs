//! Shared text rendering resources.

use bevy::prelude::*;
use glyphon::{
    Buffer, Cache, FontSystem, Metrics, Resolution, SwashCache, TextAtlas, TextRenderer, Viewport,
};
use std::sync::{Arc, Mutex};

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
    pub cache: Cache,
    pub viewport: Viewport,
    pub atlas: TextAtlas,
    pub renderer: TextRenderer,
}

/// A text buffer wrapper that can be used as a Bevy component.
/// Wraps a glyphon Buffer for use with the cosmic-text Editor.
#[derive(Component)]
pub struct TextBuffer {
    buffer: Buffer,
    dirty: bool,
    /// Cached visual line count (after text wrapping).
    cached_visual_lines: usize,
    /// The wrap width used for the cached visual line count.
    cached_wrap_width: f32,
}

impl TextBuffer {
    /// Create a new text buffer with the given metrics.
    pub fn new(font_system: &mut FontSystem, metrics: Metrics) -> Self {
        Self {
            buffer: Buffer::new(font_system, metrics),
            dirty: true,
            cached_visual_lines: 1,
            cached_wrap_width: 0.0,
        }
    }

    /// Get a reference to the underlying buffer.
    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    /// Get a mutable reference to the underlying buffer.
    pub fn buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffer
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
            bounds: glyphon::TextBounds::default(),
            default_color: glyphon::Color::rgb(220, 220, 240),
        }
    }
}

/// Marker component for entities that should be rendered with glyphon.
#[derive(Component)]
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
