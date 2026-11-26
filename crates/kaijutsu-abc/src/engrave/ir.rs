//! Intermediate representation for music engraving with source spans.
//!
//! The IR maps rendered elements back to ABC source positions,
//! enabling future click-to-edit interactivity.

use super::font::font_cache;

/// Byte offset range into ABC source text.
pub type SourceSpan = (usize, usize);

/// Options controlling the engraving layout.
#[derive(Debug, Clone)]
pub struct EngravingOptions {
    /// Distance between adjacent staff lines in SVG units.
    pub staff_spacing: f64,
    /// Page margin around the entire score.
    pub margin: f64,
    /// Foreground color for all notation elements (CSS color string).
    /// Default: `"white"` (for dark backgrounds).
    pub color: String,
}

impl Default for EngravingOptions {
    fn default() -> Self {
        EngravingOptions {
            staff_spacing: 10.0,
            margin: 20.0,
            color: "#ffffff".to_string(),
        }
    }
}

/// A positioned element in the engraving.
#[derive(Debug, Clone)]
pub enum EngravingElement {
    /// A music glyph (notehead, clef, rest, accidental, etc.)
    Glyph {
        codepoint: u32,
        x: f64,
        y: f64,
        scale: f64,
        source_span: SourceSpan,
    },
    /// A straight line (staff line, stem, barline, ledger line).
    Line {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        width: f64,
        source_span: SourceSpan,
    },
    /// An SVG path element.
    Path {
        d: String,
        fill: bool,
        source_span: SourceSpan,
    },
    /// Text (title, tempo, chord symbols — rendered with system font).
    Text {
        content: String,
        x: f64,
        y: f64,
        size: f64,
        source_span: SourceSpan,
    },
}

/// Padded dimensions and origin of an engraving.
pub struct EngravingBounds {
    pub width: f64,
    pub height: f64,
    pub origin_x: f64,
    pub origin_y: f64,
}

/// Measure positioned notation with space for the margins. Stroke paths are
/// represented by their surrounding staff and glyph extents.
pub fn engraving_bounds(elements: &[EngravingElement], margin: f64) -> EngravingBounds {
    let fc = font_cache();
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);

    for elem in elements {
        match elem {
            EngravingElement::Glyph {
                x,
                y,
                scale,
                codepoint,
                ..
            } => {
                let advance = fc.glyph_advance(*codepoint).unwrap_or(500.0) * scale;
                let glyph_height = fc.upem() * scale;
                min_x = min_x.min(*x);
                min_y = min_y.min(y - glyph_height);
                max_x = max_x.max(x + advance);
                max_y = max_y.max(y + glyph_height * 0.5);
            }
            EngravingElement::Line { x1, y1, x2, y2, .. } => {
                min_x = min_x.min(*x1).min(*x2);
                min_y = min_y.min(*y1).min(*y2);
                max_x = max_x.max(*x1).max(*x2);
                max_y = max_y.max(*y1).max(*y2);
            }
            EngravingElement::Text {
                x, y, size, content, ..
            } => {
                min_x = min_x.min(*x);
                min_y = min_y.min(y - size);
                max_x = max_x.max(x + content.len() as f64 * size * 0.6);
                max_y = max_y.max(*y);
            }
            EngravingElement::Path { .. } => {}
        }
    }

    // Guard against empty
    if min_x > max_x {
        min_x = 0.0;
        max_x = 100.0;
        min_y = 0.0;
        max_y = 100.0;
    }

    EngravingBounds {
        width: (max_x - min_x) + margin * 2.0,
        height: (max_y - min_y) + margin * 2.0,
        origin_x: min_x - margin,
        origin_y: min_y - margin,
    }
}
