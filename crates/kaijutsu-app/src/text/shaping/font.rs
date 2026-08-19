//! The `VelloFont` asset and its parley shaping entry point (phase 3).
//!
//! `layout()` is the one method our code uses: it shapes a string into a
//! `parley::Layout<Brush>` that callers walk themselves (`GlyphRun`s) to either
//! extract MSDF glyphs or encode a vello scene. The fork's `render_with_layout`
//! and anchor helpers were the UI-render path (phase 4) and are intentionally
//! not ported.

use std::borrow::Cow;

use bevy::{prelude::*, reflect::TypePath};
use parley::{
    FontFamily, FontFamilyName, FontStyle, FontVariation, FontVariations, Layout, RangedBuilder,
    StyleProperty,
};
use parley::setting::Tag;
use peniko::Brush;

use super::context::{
    LOCAL_FONT_CONTEXT, LOCAL_LAYOUT_CONTEXT, LOCAL_STYLED_LAYOUT_CONTEXT, get_global_font_context,
};
use super::types::{VelloFontAxes, VelloTextAlign, VelloTextStyle};
use crate::text::ansi::{StyledBrush, StyledSpan};
use crate::text::rich::SpanBrush;

/// A loaded font, identified by the family name the loader registered into the
/// shared collection. The glyph bytes live in that collection, not here —
/// `layout()` resolves the font by family name.
#[derive(Asset, TypePath, Debug, Clone)]
pub struct VelloFont {
    /// The family name as registered into the shaping collection by the loader.
    pub(crate) family_name: String,
}

impl VelloFont {
    /// Shape `value` with `style`, returning a positioned parley layout.
    ///
    /// The layout is built against this thread's clone of the shared font
    /// context, so every caller — MSDF or scene — shapes identically.
    pub fn layout(
        &self,
        value: &str,
        style: &VelloTextStyle,
        text_align: VelloTextAlign,
        max_advance: Option<f32>,
    ) -> Layout<Brush> {
        self.layout_spanned(value, style, text_align, max_advance, &[])
    }

    /// [`layout`](Self::layout) with per-byte-range **brushes pushed into the
    /// layout itself**.
    ///
    /// The colored-text path everywhere else in the app resolves a brush from
    /// each shaping *run's* start byte (`text::msdf::collect_msdf_glyphs`),
    /// which is exact only while every span starts where a run does — true for
    /// line-granular coloring, false the moment a span highlights a *word*
    /// inside a line. Pushing the brushes as real parley
    /// [`StyleProperty::Brush`] ranges makes parley split its glyph runs on the
    /// span boundaries instead, so a mid-run span colors exactly its own
    /// glyphs. Read the color back with
    /// `text::msdf::collect_msdf_glyphs_styled`, which takes it from the glyph
    /// run's own style.
    ///
    /// `spans` are byte ranges into `value`; **later spans win** where they
    /// overlap (parley applies ranged properties in push order), and
    /// `style.brush` is the default under all of them. Splitting runs costs
    /// nothing in shaping — brush is not a shaping property, so metrics and
    /// line breaking are byte-for-byte what [`layout`](Self::layout) produces.
    pub fn layout_spanned(
        &self,
        value: &str,
        style: &VelloTextStyle,
        text_align: VelloTextAlign,
        max_advance: Option<f32>,
        spans: &[SpanBrush],
    ) -> Layout<Brush> {
        with_font_context(|font_context| {
            LOCAL_LAYOUT_CONTEXT.with_borrow_mut(|layout_context| {
                self.layout_ranged(
                    font_context,
                    layout_context,
                    value,
                    style,
                    text_align,
                    max_advance,
                    style.brush.clone(),
                    spans.iter().map(|s| (s.start..s.end, s.brush.clone())),
                )
            })
        })
    }

    /// [`layout_spanned`](Self::layout_spanned) in the surface's **ANSI brush
    /// currency**.
    ///
    /// Same shaping, different color type: [`StyledBrush`] carries the style
    /// table index and the MSDF weight alongside the color, and parley splits
    /// glyph runs on brush *identity* — which is the only reason two adjacent
    /// spans that are the same red but differ in bold survive as two runs.
    /// Read it back with `text::msdf::collect_msdf_glyphs_ansi_deferred`.
    pub fn layout_styled(
        &self,
        value: &str,
        style: &VelloTextStyle,
        text_align: VelloTextAlign,
        max_advance: Option<f32>,
        spans: &[StyledSpan],
    ) -> Layout<StyledBrush> {
        let default = StyledBrush {
            color: crate::text::msdf::layout_bridge::brush_to_rgba8(&style.brush),
            style_index: 0,
            importance: crate::text::msdf::glyph::IMPORTANCE_NORMAL,
        };
        with_font_context(|font_context| {
            LOCAL_STYLED_LAYOUT_CONTEXT.with_borrow_mut(|layout_context| {
                self.layout_ranged(
                    font_context,
                    layout_context,
                    value,
                    style,
                    text_align,
                    max_advance,
                    default,
                    spans.iter().map(|s| (s.start..s.end, s.brush)),
                )
            })
        })
    }

    /// The one shaping body both spanned entry points use, generic over the
    /// brush currency parley resolves runs with.
    ///
    /// `default_brush` goes under the whole text; `spans` are byte ranges into
    /// `value` and **later spans win** where they overlap (parley applies
    /// ranged properties in push order). Parley's `LayoutContext` is itself
    /// brush-typed, which is why the caller hands one in rather than this
    /// reaching for a thread-local: there is one cache per currency.
    #[allow(clippy::too_many_arguments)]
    fn layout_ranged<B: parley::Brush>(
        &self,
        font_context: &mut parley::FontContext,
        layout_context: &mut parley::LayoutContext<B>,
        value: &str,
        style: &VelloTextStyle,
        text_align: VelloTextAlign,
        max_advance: Option<f32>,
        default_brush: B,
        spans: impl Iterator<Item = (std::ops::Range<usize>, B)>,
    ) -> Layout<B> {
        let mut builder = layout_context.ranged_builder(font_context, value, 1.0, true);

        apply_font_styles(&mut builder, style);
        apply_variable_axes(&mut builder, &style.font_axes);

        // parley 0.9 renamed the pair: the old `FontStack` (a list of
        // families) is now `FontFamily`, and the old `FontFamily` (one
        // family) is now `FontFamilyName`. Same single-family meaning.
        builder.push_default(StyleProperty::FontFamily(FontFamily::Single(
            FontFamilyName::Named(Cow::Owned(self.family_name.clone())),
        )));
        builder.push_default(StyleProperty::Brush(default_brush));
        for (range, brush) in spans {
            if range.start >= range.end {
                continue;
            }
            builder.push(StyleProperty::Brush(brush), range);
        }

        let mut layout = builder.build(value);
        layout.break_all_lines(max_advance);
        // 0.9 drops `align`'s `max_advance`: it aligns against the width
        // `break_all_lines` already laid the lines out to.
        layout.align(text_align.into(), parley::AlignmentOptions::default());
        layout
    }
}

/// Run `f` against this thread's clone of the shared font collection, seeding
/// it on first use — the step every shaping entry point owes before it can
/// build anything.
fn with_font_context<R>(f: impl FnOnce(&mut parley::FontContext) -> R) -> R {
    LOCAL_FONT_CONTEXT.with_borrow_mut(|font_context| {
        if font_context.is_none() {
            *font_context = Some(get_global_font_context().clone());
        }
        f(font_context.as_mut().unwrap())
    })
}

/// Applies size, line-height and spacing styles to the run builder.
fn apply_font_styles<B: parley::Brush>(builder: &mut RangedBuilder<'_, B>, style: &VelloTextStyle) {
    builder.push_default(StyleProperty::FontSize(style.font_size));
    builder.push_default(StyleProperty::LineHeight(parley::LineHeight::MetricsRelative(
        style.line_height,
    )));
    builder.push_default(StyleProperty::WordSpacing(style.word_spacing));
    builder.push_default(StyleProperty::LetterSpacing(style.letter_spacing));
    builder.push_default(StyleProperty::OverflowWrap(style.overflow_wrap));
}

/// Applies the variable-font axes (and italic/slant) to the run builder.
fn apply_variable_axes<B: parley::Brush>(builder: &mut RangedBuilder<'_, B>, axes: &VelloFontAxes) {
    let mut variable_axes: Vec<FontVariation> = vec![];

    // 0.9 stopped re-exporting swash and owns its own settings types. Tags are
    // the 4-ASCII-char OpenType axis names below, so a parse failure is a typo
    // in this file, not runtime data — panic rather than silently drop an axis
    // (the old `tag_from_str_lossy` could not fail and would have masked one).
    let mut push = |tag: &str, value: Option<f32>| {
        if let Some(value) = value {
            let tag = Tag::parse(tag).expect("variable-axis tags are 4 ASCII chars");
            variable_axes.push(FontVariation::new(tag, value));
        }
    };
    push("wght", axes.weight);
    push("wdth", axes.width);
    push("opsz", axes.optical_size);
    push("GRAD", axes.grade);
    push("XOPQ", axes.thick_stroke);
    push("YOPQ", axes.thin_stroke);
    push("XTRA", axes.counter_width);
    push("YTUC", axes.uppercase_height);
    push("YTLC", axes.lowercase_height);
    push("YTAS", axes.ascender_height);
    push("YTDE", axes.descender_depth);
    push("YTFI", axes.figure_height);

    if axes.italic {
        builder.push_default(StyleProperty::FontStyle(FontStyle::Italic));
    } else if axes.slant.is_some() {
        builder.push_default(StyleProperty::FontStyle(FontStyle::Oblique(axes.slant)));
    }

    builder.push_default(StyleProperty::FontVariations(FontVariations::List(
        variable_axes.into(),
    )));
}
