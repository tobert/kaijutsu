//! MSDF text buffer using cosmic-text for layout.
//!
//! This wraps cosmic-text's Buffer for text shaping and layout,
//! while providing glyph information needed for MSDF rendering.

use bevy::prelude::*;
use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping};

use super::atlas::GlyphKey;

/// A positioned glyph ready for rendering.
#[derive(Clone, Debug)]
pub struct PositionedGlyph {
    /// Key for looking up in the atlas.
    pub key: GlyphKey,
    /// X position in pixels (pen position from cosmic-text).
    pub x: f32,
    /// Y position in pixels (baseline from cosmic-text).
    pub y: f32,
    /// Font size used for this glyph (needed to scale MSDF region).
    pub font_size: f32,
    /// Color (RGBA).
    pub color: [u8; 4],
}

/// MSDF text buffer component.
///
/// Uses cosmic-text for shaping and layout, but stores positioned
/// glyphs for MSDF rendering instead of rasterized images.
#[derive(Component)]
pub struct MsdfTextBuffer {
    /// The underlying cosmic-text buffer.
    buffer: Buffer,
    /// Cached positioned glyphs after layout.
    glyphs: Vec<PositionedGlyph>,
    /// Whether the buffer needs reshaping.
    dirty: bool,
    /// Cached visual line count.
    cached_visual_lines: usize,
    /// Cached wrap width for invalidation.
    cached_wrap_width: f32,
    /// Cached text hash for change detection.
    text_hash: u64,
    /// Default text color.
    default_color: [u8; 4],
}

#[allow(dead_code)]
impl MsdfTextBuffer {
    /// Create a new buffer with the given metrics.
    pub fn new(font_system: &mut FontSystem, metrics: Metrics) -> Self {
        Self {
            buffer: Buffer::new(font_system, metrics),
            glyphs: Vec::new(),
            dirty: true,
            cached_visual_lines: 1,
            cached_wrap_width: 0.0,
            text_hash: 0,
            default_color: [220, 220, 240, 255],
        }
    }

    /// Create a buffer with a specific wrap width.
    pub fn new_with_width(font_system: &mut FontSystem, metrics: Metrics, width: f32) -> Self {
        let mut buffer = Buffer::new(font_system, metrics);
        buffer.set_size(font_system, Some(width), None);
        Self {
            buffer,
            glyphs: Vec::new(),
            dirty: true,
            cached_visual_lines: 1,
            cached_wrap_width: width,
            text_hash: 0,
            default_color: [220, 220, 240, 255],
        }
    }

    /// Set the text content.
    pub fn set_text(
        &mut self,
        font_system: &mut FontSystem,
        text: &str,
        attrs: Attrs,
        shaping: Shaping,
    ) {
        self.buffer.set_text(font_system, text, &attrs, shaping, None);
        self.dirty = true;
        self.text_hash = Self::hash_str(text);
    }

    /// Set the default text color.
    pub fn set_color(&mut self, color: Color) {
        let srgba = color.to_srgba();
        self.default_color = [
            (srgba.red * 255.0) as u8,
            (srgba.green * 255.0) as u8,
            (srgba.blue * 255.0) as u8,
            (srgba.alpha * 255.0) as u8,
        ];
    }

    /// Get the text content.
    pub fn text(&self) -> String {
        self.buffer
            .lines
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Get the cached text hash.
    pub fn text_hash(&self) -> u64 {
        self.text_hash
    }

    /// Get the visual line count after wrapping.
    pub fn visual_line_count(&mut self, font_system: &mut FontSystem, wrap_width: f32) -> usize {
        let width_changed = (self.cached_wrap_width - wrap_width).abs() > 1.0;

        if self.dirty || width_changed {
            self.buffer.set_size(font_system, Some(wrap_width), None);
            self.buffer.shape_until_scroll(font_system, false);
            self.cached_visual_lines = self.buffer.layout_runs().count().max(1);
            self.cached_wrap_width = wrap_width;
            self.update_glyphs();
            self.dirty = false;
        }

        self.cached_visual_lines
    }

    /// Update positioned glyphs from buffer layout.
    fn update_glyphs(&mut self) {
        self.glyphs.clear();
        let font_size = self.buffer.metrics().font_size;

        for run in self.buffer.layout_runs() {
            let line_y = run.line_y;

            for glyph in run.glyphs {
                // Get font ID and glyph ID directly from LayoutGlyph
                let font_id = glyph.font_id;
                let glyph_id = glyph.glyph_id;

                let key = GlyphKey::new(font_id, glyph_id);

                self.glyphs.push(PositionedGlyph {
                    key,
                    // x is pen position, x_offset contains kerning and other per-glyph adjustments
                    x: glyph.x + glyph.x_offset,
                    // line_y is baseline position, glyph.y + y_offset for vertical adjustments
                    y: line_y + glyph.y + glyph.y_offset,
                    font_size,
                    color: self.default_color,
                });
            }
        }
    }

    /// Get the positioned glyphs for rendering.
    pub fn glyphs(&self) -> &[PositionedGlyph] {
        &self.glyphs
    }

    /// Get glyph positions (for testing).
    #[cfg(test)]
    pub fn glyph_positions(&self) -> Vec<(f32, f32)> {
        self.glyphs.iter().map(|g| (g.x, g.y)).collect()
    }

    /// Get the number of lines (for testing).
    #[cfg(test)]
    pub fn line_count(&self) -> usize {
        self.cached_visual_lines
    }

    /// Get the underlying buffer metrics.
    pub fn metrics(&self) -> Metrics {
        self.buffer.metrics()
    }

    /// Hash a string for change detection.
    fn hash_str(s: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        s.hash(&mut hasher);
        hasher.finish()
    }
}

/// Configuration for rendering a text area.
#[derive(Component, Clone)]
pub struct MsdfTextAreaConfig {
    /// Position from left edge of the screen.
    pub left: f32,
    /// Position from top edge of the screen.
    pub top: f32,
    /// Scale factor for the text.
    pub scale: f32,
    /// Clipping bounds.
    pub bounds: TextBounds,
    /// Default text color.
    pub default_color: Color,
}

impl Default for MsdfTextAreaConfig {
    fn default() -> Self {
        Self {
            left: 0.0,
            top: 0.0,
            scale: 1.0,
            bounds: TextBounds {
                left: 0,
                top: 0,
                right: 800,
                bottom: 600,
            },
            default_color: Color::srgba(0.86, 0.86, 0.94, 1.0),
        }
    }
}

/// Text clipping bounds.
#[derive(Clone, Copy, Debug, Default)]
pub struct TextBounds {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[allow(dead_code)]
impl TextBounds {
    /// Create bounds from position and size.
    pub fn from_rect(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            left: x as i32,
            top: y as i32,
            right: (x + width) as i32,
            bottom: (y + height) as i32,
        }
    }

    /// Get the width.
    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    /// Get the height.
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: Full tests require a FontSystem which is expensive to create.
    // These are placeholder tests that verify the API compiles correctly.

    #[test]
    fn text_bounds_from_rect() {
        let bounds = TextBounds::from_rect(10.0, 20.0, 100.0, 50.0);
        assert_eq!(bounds.left, 10);
        assert_eq!(bounds.top, 20);
        assert_eq!(bounds.right, 110);
        assert_eq!(bounds.bottom, 70);
        assert_eq!(bounds.width(), 100);
        assert_eq!(bounds.height(), 50);
    }

    #[test]
    fn hash_consistency() {
        let hash1 = MsdfTextBuffer::hash_str("hello");
        let hash2 = MsdfTextBuffer::hash_str("hello");
        let hash3 = MsdfTextBuffer::hash_str("world");

        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
    }
}
