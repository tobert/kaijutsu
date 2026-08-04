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
    FontSettings, FontStyle, FontVariation, Layout, RangedBuilder, StyleProperty,
};
use peniko::Brush;

use super::context::{LOCAL_FONT_CONTEXT, LOCAL_LAYOUT_CONTEXT, get_global_font_context};
use super::types::{VelloFontAxes, VelloTextAlign, VelloTextStyle};
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
        LOCAL_FONT_CONTEXT.with_borrow_mut(|font_context| {
            if font_context.is_none() {
                *font_context = Some(get_global_font_context().clone());
            }
            let font_context = font_context.as_mut().unwrap();

            LOCAL_LAYOUT_CONTEXT.with_borrow_mut(|layout_context| {
                let mut builder = layout_context.ranged_builder(font_context, value, 1.0, true);

                apply_font_styles(&mut builder, style);
                apply_variable_axes(&mut builder, &style.font_axes);

                builder.push_default(StyleProperty::FontStack(parley::FontStack::Single(
                    parley::FontFamily::Named(Cow::Owned(self.family_name.clone())),
                )));
                builder.push_default(StyleProperty::Brush(style.brush.clone()));
                for span in spans {
                    if span.start >= span.end {
                        continue;
                    }
                    builder.push(
                        StyleProperty::Brush(span.brush.clone()),
                        span.start..span.end,
                    );
                }

                let mut layout = builder.build(value);
                layout.break_all_lines(max_advance);
                layout.align(
                    max_advance,
                    text_align.into(),
                    parley::AlignmentOptions::default(),
                );
                layout
            })
        })
    }
}

/// Applies size, line-height and spacing styles to the run builder.
fn apply_font_styles(builder: &mut RangedBuilder<'_, Brush>, style: &VelloTextStyle) {
    builder.push_default(StyleProperty::FontSize(style.font_size));
    builder.push_default(StyleProperty::LineHeight(parley::LineHeight::MetricsRelative(
        style.line_height,
    )));
    builder.push_default(StyleProperty::WordSpacing(style.word_spacing));
    builder.push_default(StyleProperty::LetterSpacing(style.letter_spacing));
    builder.push_default(StyleProperty::OverflowWrap(style.overflow_wrap));
}

/// Applies the variable-font axes (and italic/slant) to the run builder.
fn apply_variable_axes(builder: &mut RangedBuilder<'_, Brush>, axes: &VelloFontAxes) {
    let mut variable_axes: Vec<FontVariation> = vec![];

    let mut push = |tag: &str, value: Option<f32>| {
        if let Some(value) = value {
            variable_axes.push(parley::swash::Setting {
                tag: parley::swash::tag_from_str_lossy(tag),
                value,
            });
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

    builder.push_default(StyleProperty::FontVariations(FontSettings::List(
        variable_axes.into(),
    )));
}
