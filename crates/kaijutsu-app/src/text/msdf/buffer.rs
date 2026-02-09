//! MSDF text buffer using cosmic-text for layout.
//!
//! This wraps cosmic-text's Buffer for text shaping and layout,
//! while providing glyph information needed for MSDF rendering.

use bevy::prelude::*;
use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping};

use super::atlas::GlyphKey;
use super::generator::FontMetricsCache;

/// A positioned glyph ready for rendering.
#[derive(Clone, Debug)]
pub struct PositionedGlyph {
    /// Key for looking up in the atlas.
    pub key: GlyphKey,
    /// X position in pixels (pen position from cosmic-text).
    pub x: f32,
    /// Y position in pixels (baseline from cosmic-text, pixel-aligned when metrics available).
    pub y: f32,
    /// Font size used for this glyph (needed to scale MSDF region).
    pub font_size: f32,
    /// Advance width in pixels from cosmic-text.
    /// Used by tests for boundary analysis (no longer needed by pipeline).
    #[allow(dead_code)]
    pub advance_width: f32,
    /// Color (RGBA).
    pub color: [u8; 4],
    /// Fractional pixel offset from baseline snapping (for potential subpixel rendering).
    /// Currently tracked but not used - available for future LCD subpixel rendering.
    #[allow(dead_code)]
    pub subpixel_offset: f32,
    /// Semantic importance for weight adjustment (0.0 = faded/thin, 1.0 = bold/emphasized).
    /// Used by the shader to vary stroke weight based on context:
    /// - Cursor proximity: glyphs near cursor are bolder
    /// - Agent activity: code being edited by AI gets emphasis
    /// - Selection: selected text rendered with heavier weight
    ///
    /// Currently passed through pipeline but cursor proximity calculation
    /// will be implemented when cursor tracking is added.
    pub importance: f32,
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
    /// Snap glyph x-positions to pixel boundaries.
    /// Enables uniform character cell widths for monospace fonts where
    /// fractional positioning causes visible spacing inconsistency.
    snap_x: bool,
    /// Extra pixels added between each glyph (letter-spacing / tracking).
    /// Applied as cumulative offset: glyph N gets N * letter_spacing extra px.
    letter_spacing: f32,
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
            snap_x: false,
            letter_spacing: 0.0,
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
            snap_x: false,
            letter_spacing: 0.0,
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

    /// Set rich text with per-span attributes (bold, italic, color, etc.).
    ///
    /// This wraps cosmic-text's `Buffer::set_rich_text()` for markdown rendering.
    /// Per-span colors are propagated through `LayoutGlyph::color_opt` and read
    /// back in `update_glyphs()` as per-glyph colors.
    pub fn set_rich_text<'r, 's, I>(
        &mut self,
        font_system: &mut FontSystem,
        spans: I,
        default_attrs: &Attrs<'_>,
        shaping: Shaping,
    ) where
        I: IntoIterator<Item = (&'s str, Attrs<'r>)>,
    {
        self.buffer.set_rich_text(font_system, spans, default_attrs, shaping, None);
        self.dirty = true;
        // Hash the buffer text for change detection
        self.text_hash = Self::hash_str(&self.text());
    }

    /// Enable horizontal pixel snapping for monospace fonts.
    ///
    /// When enabled, glyph x-positions are rounded to pixel boundaries so that
    /// every character cell starts at an integer pixel offset. This prevents
    /// visible spacing inconsistency where different glyphs land at different
    /// sub-pixel offsets and appear slightly wider or narrower than neighbors.
    pub fn set_snap_x(&mut self, snap: bool) {
        if self.snap_x != snap {
            self.snap_x = snap;
            self.dirty = true;
        }
    }

    /// Set extra letter-spacing in pixels.
    ///
    /// Each glyph gets `glyph_index * spacing` extra horizontal offset,
    /// widening the gaps between characters beyond what the font recommends.
    /// Useful for improving readability at small sizes where glyphs crowd.
    pub fn set_letter_spacing(&mut self, spacing: f32) {
        if (self.letter_spacing - spacing).abs() > f32::EPSILON {
            self.letter_spacing = spacing;
            self.dirty = true;
        }
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
    ///
    /// If a `FontMetricsCache` is provided, glyphs will be pixel-aligned for
    /// crisper rendering at small font sizes (webgl_fonts technique).
    pub fn visual_line_count(
        &mut self,
        font_system: &mut FontSystem,
        wrap_width: f32,
        metrics_cache: Option<&mut FontMetricsCache>,
    ) -> usize {
        let width_changed = (self.cached_wrap_width - wrap_width).abs() > 1.0;

        if self.dirty || width_changed {
            self.buffer.set_size(font_system, Some(wrap_width), None);
            self.buffer.shape_until_scroll(font_system, false);
            self.cached_visual_lines = self.buffer.layout_runs().count().max(1);
            self.cached_wrap_width = wrap_width;
            self.update_glyphs(font_system, metrics_cache);
            self.dirty = false;
        }

        self.cached_visual_lines
    }

    /// Update positioned glyphs from buffer layout.
    ///
    /// Applies CPU-side pixel alignment when metrics are available:
    /// 1. Snaps baseline (line_y) to nearest pixel
    /// 2. Applies x-height grid fitting so lowercase letters span whole pixels
    ///
    /// This technique from webgl_fonts helps horizontal strokes land cleanly
    /// on pixel rows instead of blurring across two.
    fn update_glyphs(
        &mut self,
        font_system: &FontSystem,
        mut metrics_cache: Option<&mut FontMetricsCache>,
    ) {
        self.glyphs.clear();
        let font_size = self.buffer.metrics().font_size;

        for run in self.buffer.layout_runs() {
            // Pixel-align baseline: snap line_y to nearest pixel
            let line_y_snapped = run.line_y.round();
            let baseline_offset = line_y_snapped - run.line_y;

            for (glyph_idx, glyph) in run.glyphs.iter().enumerate() {
                let font_id = glyph.font_id;
                let glyph_id = glyph.glyph_id;
                let key = GlyphKey::new(font_id, glyph_id);

                // Get font metrics if cache available
                let metrics = metrics_cache
                    .as_mut()
                    .map(|c| c.get_or_extract(font_system, font_id));

                // Apply x-height grid fitting if metrics available
                // This ensures lowercase letters span whole pixels
                let y_adjusted = if let Some(m) = metrics {
                    let x_height_px = m.x_height_em() * font_size;
                    if x_height_px > 0.1 {
                        // Only apply if we have valid x-height
                        let x_height_snapped = x_height_px.round();
                        let scale_adjustment = x_height_snapped / x_height_px;

                        // Apply scale adjustment to glyph's vertical offset
                        line_y_snapped + (glyph.y * scale_adjustment)
                    } else {
                        line_y_snapped + glyph.y
                    }
                } else {
                    line_y_snapped + glyph.y
                };

                // Apply letter-spacing then snap to pixel boundary
                let x_raw = glyph.x + (glyph_idx as f32 * self.letter_spacing);
                let x = if self.snap_x { x_raw.round() } else { x_raw };

                // Per-glyph color from cosmic-text rich text (ARGB packed u32 → [R,G,B,A])
                let color = glyph
                    .color_opt
                    .map(|c| {
                        let (r, g, b, a) = c.as_rgba_tuple();
                        [r, g, b, a]
                    })
                    .unwrap_or(self.default_color);

                self.glyphs.push(PositionedGlyph {
                    key,
                    x,
                    // Pixel-aligned baseline + grid-fitted vertical offset
                    y: y_adjusted,
                    font_size,
                    advance_width: glyph.w,
                    color,
                    // Store fractional offset for potential subpixel rendering
                    subpixel_offset: baseline_offset,
                    // Default importance 0.5 = normal weight
                    // Will be updated by cursor proximity or agent activity
                    importance: 0.5,
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

    /// Set alternating glyph colors for overlap detection tests.
    ///
    /// Even-indexed glyphs get `color_a`, odd-indexed get `color_b`.
    /// When rendered, channel separation at advance boundaries reveals
    /// whether adjacent glyph quads bleed into each other's cells.
    #[cfg(test)]
    pub fn set_alternating_colors(&mut self, color_a: [u8; 4], color_b: [u8; 4]) {
        for (i, glyph) in self.glyphs.iter_mut().enumerate() {
            glyph.color = if i % 2 == 0 { color_a } else { color_b };
        }
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
