//! Text-style data types fed into `VelloFont::layout`.
//!
//! Only what `layout()` reads — style, font axes, alignment.

/// Re-export so call sites keep `OverflowWrap` without a parley import.
pub use parley::OverflowWrap;

/// Styling applied to a shaped run.
///
/// The font itself is the `VelloFont::layout` receiver, not part of the style.
#[derive(Clone)]
pub struct VelloTextStyle {
    pub brush: vello::peniko::Brush,
    pub font_size: f32,
    /// Line height multiplier.
    pub line_height: f32,
    /// Extra spacing between words.
    pub word_spacing: f32,
    /// Extra spacing between letters.
    pub letter_spacing: f32,
    pub font_axes: VelloFontAxes,
    /// How to handle overflow when a word exceeds the line width.
    /// `Normal` (default) only breaks at word boundaries; `BreakWord` breaks at
    /// character boundaries when a word overflows; `Anywhere` breaks at any
    /// character (and affects min-content width).
    pub overflow_wrap: OverflowWrap,
}

impl Default for VelloTextStyle {
    fn default() -> Self {
        Self {
            brush: vello::peniko::Brush::Solid(vello::peniko::Color::WHITE),
            font_size: 24.0,
            line_height: 1.0,
            word_spacing: 0.0,
            letter_spacing: 0.0,
            font_axes: Default::default(),
            overflow_wrap: OverflowWrap::default(),
        }
    }
}

/// Variable-font axes; each is applied only if the font exposes it.
/// See <https://fonts.google.com/knowledge/introducing_type/introducing_variable_fonts>.
#[derive(Default, Clone)]
pub struct VelloFontAxes {
    /// wght — weight.
    pub weight: Option<f32>,
    /// wdth — width.
    pub width: Option<f32>,
    /// opsz — optical size.
    pub optical_size: Option<f32>,
    /// ital — italic. Mutually exclusive with `slant` (italic wins).
    pub italic: bool,
    /// slnt — slant. Ignored when `italic` is true.
    pub slant: Option<f32>,
    /// GRAD — grade.
    pub grade: Option<f32>,
    /// XOPQ — thick stroke.
    pub thick_stroke: Option<f32>,
    /// YOPQ — thin stroke.
    pub thin_stroke: Option<f32>,
    /// XTRA — counter width.
    pub counter_width: Option<f32>,
    /// YTUC — uppercase height.
    pub uppercase_height: Option<f32>,
    /// YTLC — lowercase height.
    pub lowercase_height: Option<f32>,
    /// YTAS — ascender height.
    pub ascender_height: Option<f32>,
    /// YTDE — descender depth.
    pub descender_depth: Option<f32>,
    /// YTFI — figure height.
    pub figure_height: Option<f32>,
}

/// Alignment of a parley layout.
#[derive(Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VelloTextAlign {
    /// `Left` for LTR text, `Right` for RTL.
    #[default]
    Start,
    /// `Right` for LTR text, `Left` for RTL.
    End,
    /// Align to the left edge (direction-unaware).
    Left,
    /// Center each line within the container.
    Middle,
    /// Align to the right edge (direction-unaware).
    Right,
    /// Justify each line except the last.
    Justified,
}

impl From<VelloTextAlign> for parley::Alignment {
    fn from(value: VelloTextAlign) -> Self {
        match value {
            VelloTextAlign::Start => parley::Alignment::Start,
            VelloTextAlign::End => parley::Alignment::End,
            VelloTextAlign::Left => parley::Alignment::Left,
            VelloTextAlign::Middle => parley::Alignment::Center,
            VelloTextAlign::Right => parley::Alignment::Right,
            VelloTextAlign::Justified => parley::Alignment::Justify,
        }
    }
}
