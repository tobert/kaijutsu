//! Intermediate representation for music engraving with source spans.
//!
//! The IR maps rendered elements back to ABC source positions,
//! enabling future click-to-edit interactivity.

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
            color: "white".to_string(),
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
