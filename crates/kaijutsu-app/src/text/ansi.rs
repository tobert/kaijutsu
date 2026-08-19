//! ANSI style spans → shaping input for the conversation surface.
//!
//! The kernel strips ANSI at ingestion and stores the projection: clean text
//! in `BlockSnapshot::content`, semantic styling in
//! `BlockSnapshot::style_spans` (`docs/ansi-and-beyond.md`). This module is
//! the app's half of that contract — it turns those byte-addressed, *theme
//! independent* spans into the two things the surface actually draws with:
//!
//! - [`StyledBrush`], the parley brush for the surface's ANSI shaping path.
//!   It is a **brush** and not just a color because parley splits its glyph
//!   runs on brush identity: two adjacent spans that are the same red but
//!   differ in bold would otherwise resolve to one run and lose the boundary.
//!   Carrying the style index and the MSDF weight *inside* the brush makes
//!   the split fall out of the same mechanism that already splits on color.
//! - [`StyledSpan`], the byte range that brush covers, plus the parts of a
//!   span that are **not** glyph paint: the background, the underline and the
//!   strikethrough. Those are geometry (`text::msdf::geometry`), built from
//!   the laid-out rects of the same byte range.
//!
//! ## Two color currencies, on purpose
//!
//! An `Indexed(n)` foreground stays *semantic* all the way to the GPU: the
//! glyph carries `style_index = n + 1` and the shader resolves it through a
//! theme-rebuilt palette table, so a retheme is a buffer write rather than a
//! re-shape. A truecolor `Rgb` foreground cannot retheme — it is an absolute
//! color — so it is resolved here and rides the glyph's own color with
//! `style_index = 0`.
//!
//! Geometry has no such indirection: a background quad's color lives in its
//! vertices, so [`StyledSpan::bg`] and [`StyledSpan::ink`] are resolved
//! CPU-side against the theme palette. **That is why an ANSI-spanned block
//! carries the theme epoch in its `ShapeKey`** (`view::surface::shape_cache`)
//! — baked vertex colors do not follow a retheme on their own.

use kaijutsu_types::{StyleAttrs, StyleColor, StyleSpan};

use crate::text::msdf::glyph::{IMPORTANCE_BOLD, IMPORTANCE_DIM, IMPORTANCE_NORMAL};

/// The parley brush for the conversation surface's ANSI path: everything about
/// a span that a *glyph* carries.
///
/// Parley's brush bound is `Clone + PartialEq + Default + Debug`, and the
/// `PartialEq` is the load-bearing one — it is what decides whether two
/// adjacent ranged styles collapse into a single glyph run. Every field here
/// participates, so "same color, different weight" is two runs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StyledBrush {
    /// Straight-alpha RGBA8, the encoding `PositionedGlyph::color` uses.
    ///
    /// For an `Indexed` foreground this is the block's *default* text color
    /// and the shader overrides it from the style table; for `Rgb` it is the
    /// truecolor value; with no foreground it is the default text color.
    pub color: [u8; 4],
    /// Style-table index: 0 = unstyled (`color` stands), `n + 1` = ANSI
    /// palette slot `n`.
    pub style_index: u32,
    /// MSDF stem weight — [`IMPORTANCE_NORMAL`] / [`IMPORTANCE_BOLD`] /
    /// [`IMPORTANCE_DIM`].
    pub importance: f32,
}

impl Default for StyledBrush {
    /// Parley requires `Default` on its brush type; the surface never renders
    /// it, because `layout_styled` always pushes an explicit default brush
    /// under the whole text. Written out rather than derived so the value is
    /// *normal white text* instead of a transparent zero-weight glyph — if it
    /// ever does reach the screen it should be visibly wrong text, not
    /// silently missing text.
    fn default() -> Self {
        Self {
            color: [255, 255, 255, 255],
            style_index: 0,
            importance: IMPORTANCE_NORMAL,
        }
    }
}

/// One ANSI span, resolved for this surface and this theme: the glyph brush
/// plus the decorations that are drawn as geometry rather than as glyphs.
///
/// Byte offsets are `usize` and rebased per shaping chunk (`view::surface::
/// chunk::slice_styled_spans`), matching `SpanBrush`; the kernel's `u32`
/// offsets are widened at the mapping boundary below.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StyledSpan {
    /// Byte offset of span start in the shaped text.
    pub start: usize,
    /// Byte offset of span end (exclusive).
    pub end: usize,
    /// What the glyphs in this range are painted with.
    pub brush: StyledBrush,
    /// Resolved background color, or `None` for no background quad.
    pub bg: Option<[u8; 4]>,
    /// Resolved foreground color for *geometry* (underline, strikethrough).
    ///
    /// Differs from `brush.color` for an `Indexed` foreground: the glyph defers
    /// that color to the GPU style table, but a decoration quad has nowhere to
    /// defer it to, so it is baked here.
    pub ink: [u8; 4],
    /// SGR 4.
    pub underline: bool,
    /// SGR 9.
    pub strikethrough: bool,
}

impl StyledSpan {
    /// Does this span draw anything in the geometry lane?
    pub fn has_geometry(&self) -> bool {
        self.bg.is_some() || self.underline || self.strikethrough
    }
}

/// Straight-alpha RGBA8 from a palette entry's sRGB floats.
fn palette_rgba8(entry: [f32; 4]) -> [u8; 4] {
    [
        (entry[0].clamp(0.0, 1.0) * 255.0) as u8,
        (entry[1].clamp(0.0, 1.0) * 255.0) as u8,
        (entry[2].clamp(0.0, 1.0) * 255.0) as u8,
        (entry[3].clamp(0.0, 1.0) * 255.0) as u8,
    ]
}

/// What a resolved foreground came out as: the color a glyph carries, the
/// style-table index that may override it, and the baked color decorations use.
struct ResolvedFg {
    glyph_color: [u8; 4],
    style_index: u32,
    ink: [u8; 4],
}

fn resolve_fg(
    color: Option<StyleColor>,
    default_fg: [u8; 4],
    palette: &[[f32; 4]; 256],
) -> ResolvedFg {
    match color {
        // Semantic to the last moment: the glyph keeps the default color and
        // the shader replaces it from the table, so a retheme is a buffer
        // write. `ink` is the same color resolved now, because geometry has
        // no table to read.
        Some(StyleColor::Indexed(n)) => ResolvedFg {
            glyph_color: default_fg,
            style_index: n as u32 + 1,
            ink: palette_rgba8(palette[n as usize]),
        },
        // Absolute colors do not retheme; there is nothing for the table to
        // add, so they ride the glyph color directly.
        Some(StyleColor::Rgb(r, g, b)) => ResolvedFg {
            glyph_color: [r, g, b, 255],
            style_index: 0,
            ink: [r, g, b, 255],
        },
        None => ResolvedFg {
            glyph_color: default_fg,
            style_index: 0,
            ink: default_fg,
        },
    }
}

fn resolve_bg(color: StyleColor, palette: &[[f32; 4]; 256]) -> [u8; 4] {
    match color {
        StyleColor::Indexed(n) => palette_rgba8(palette[n as usize]),
        StyleColor::Rgb(r, g, b) => [r, g, b, 255],
    }
}

/// The MSDF weight an attribute set asks for.
///
/// Bold beats dim when a stream sets both (SGR 1 then SGR 2 without a reset is
/// malformed-ish input that real programs do emit); picking the *more* legible
/// of the two is the conservative reading.
fn importance_of(attrs: StyleAttrs) -> f32 {
    if attrs.contains(StyleAttrs::BOLD) {
        IMPORTANCE_BOLD
    } else if attrs.contains(StyleAttrs::DIM) {
        IMPORTANCE_DIM
    } else {
        IMPORTANCE_NORMAL
    }
}

/// Map a block's kernel-side [`StyleSpan`]s onto this surface and this theme.
///
/// `default_fg` is the block's own text color (`view::format::block_color`) and
/// `default_bg` the surface background behind it — both already resolved to
/// RGBA8. `palette` is `Theme::ansi.palette_256()`.
///
/// Spans that would draw nothing are dropped: an empty or reversed range, and a
/// span with no color and no renderable attribute. That matters beyond tidiness
/// — a block whose styled-span list comes back empty takes the surface's
/// *plain* shaping path, which skips parley's run splitting entirely.
///
/// **Inverse is resolved here, not on the GPU.** Swapping foreground and
/// background means the glyph's color comes from the span's `bg` (or the
/// surface background when it has none), and neither of those can be expressed
/// as "palette slot n" for the glyph the way a normal foreground can. So an
/// inverted span bakes both colors CPU-side and carries `style_index = 0`: it
/// is correct, and it is **stale until the block re-shapes** on a retheme.
/// The theme epoch in the block's `ShapeKey` is what makes that re-shape
/// happen, so the staleness window is one theme swap's worth of frames — the
/// same window the background quads already live with.
pub fn resolve_style_spans(
    spans: &[StyleSpan],
    default_fg: [u8; 4],
    default_bg: [u8; 4],
    palette: &[[f32; 4]; 256],
) -> Vec<StyledSpan> {
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        if span.end <= span.start {
            continue;
        }
        let inverse = span.attrs.contains(StyleAttrs::INVERSE);
        let (fg, bg) = if inverse {
            // The swap: what was the background paints the glyphs, what was the
            // foreground paints the quad behind them. A missing side falls back
            // to the surface's own default for that role, which is what a
            // terminal does with `ESC[7m` on otherwise-default text.
            let swapped_fg = span
                .bg
                .map(|c| resolve_bg(c, palette))
                .unwrap_or(default_bg);
            let swapped_bg = span
                .fg
                .map(|c| resolve_bg(c, palette))
                .unwrap_or(default_fg);
            (
                ResolvedFg {
                    glyph_color: swapped_fg,
                    style_index: 0,
                    ink: swapped_fg,
                },
                Some(swapped_bg),
            )
        } else {
            (
                resolve_fg(span.fg, default_fg, palette),
                span.bg.map(|c| resolve_bg(c, palette)),
            )
        };

        let underline = span.attrs.contains(StyleAttrs::UNDERLINE);
        let strikethrough = span.attrs.contains(StyleAttrs::STRIKETHROUGH);
        let importance = importance_of(span.attrs);

        // Nothing to draw differently from the surrounding text: BLINK and
        // ITALIC are parsed and fingerprinted but not rendered yet
        // (`docs/ansi-and-beyond.md` deferred list), so a span carrying only
        // those is not a reason to leave the plain shaping path.
        if fg.style_index == 0
            && fg.glyph_color == default_fg
            && bg.is_none()
            && !underline
            && !strikethrough
            && importance == IMPORTANCE_NORMAL
        {
            continue;
        }

        out.push(StyledSpan {
            start: span.start as usize,
            end: span.end as usize,
            brush: StyledBrush {
                color: fg.glyph_color,
                style_index: fg.style_index,
                importance,
            },
            bg,
            ink: fg.ink,
            underline,
            strikethrough,
        });
    }
    out
}

/// Order-sensitive hash of resolved styled spans — every field, because every
/// one of them changes what gets drawn.
///
/// The pair of `view::surface::content::spans_fingerprint`: a span-only change
/// (a reprojection, a retheme) has to restamp the block's content version, and
/// the *only* way the surface learns about it is this hash moving. An attribute
/// that changed but is not hashed is a silently stale block.
pub fn styled_spans_fingerprint<H: std::hash::Hasher>(spans: &[StyledSpan], hasher: &mut H) {
    use std::hash::Hash;
    hasher.write_usize(spans.len());
    for span in spans {
        span.start.hash(hasher);
        span.end.hash(hasher);
        span.brush.color.hash(hasher);
        span.brush.style_index.hash(hasher);
        // f32 has no `Hash`; its bits do, and `importance` is one of three
        // constants, so bit equality is exactly value equality here.
        hasher.write_u32(span.brush.importance.to_bits());
        span.bg.hash(hasher);
        span.ink.hash(hasher);
        span.underline.hash(hasher);
        span.strikethrough.hash(hasher);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::AnsiColors;

    const DEFAULT_FG: [u8; 4] = [200, 200, 200, 255];
    const DEFAULT_BG: [u8; 4] = [10, 10, 20, 255];

    fn palette() -> [[f32; 4]; 256] {
        AnsiColors::default().palette_256()
    }

    fn span(start: u32, end: u32) -> StyleSpan {
        StyleSpan {
            start,
            end,
            fg: None,
            bg: None,
            attrs: StyleAttrs::default(),
        }
    }

    fn resolve(spans: &[StyleSpan]) -> Vec<StyledSpan> {
        resolve_style_spans(spans, DEFAULT_FG, DEFAULT_BG, &palette())
    }

    /// The whole reason the brush is a struct and not a color: parley collapses
    /// adjacent runs whose brushes compare equal, so bold-vs-not must be a
    /// brush difference or the boundary vanishes at shaping time.
    #[test]
    fn same_color_different_bold_are_distinct_brushes() {
        let plain = StyleSpan {
            fg: Some(StyleColor::Indexed(1)),
            ..span(0, 3)
        };
        let bold = StyleSpan {
            fg: Some(StyleColor::Indexed(1)),
            attrs: StyleAttrs::BOLD,
            ..span(3, 6)
        };
        let out = resolve(&[plain, bold]);
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].brush.color, out[1].brush.color,
            "test premise: the two spans are the same color",
        );
        assert_ne!(
            out[0].brush, out[1].brush,
            "same color, different weight must still be two brushes",
        );
    }

    /// Same property for the style index: two truecolor-free spans differing
    /// only in palette slot are distinct brushes even though both defer their
    /// color to the table (and therefore carry the same `color`).
    #[test]
    fn different_palette_slots_are_distinct_brushes() {
        let red = StyleSpan {
            fg: Some(StyleColor::Indexed(1)),
            ..span(0, 3)
        };
        let green = StyleSpan {
            fg: Some(StyleColor::Indexed(2)),
            ..span(3, 6)
        };
        let out = resolve(&[red, green]);
        assert_eq!(out[0].brush.color, out[1].brush.color);
        assert_ne!(out[0].brush, out[1].brush);
    }

    /// An underline-only span differs from its unstyled neighbour in no glyph
    /// property at all — the boundary lives entirely in the geometry lane, so
    /// the brushes are *allowed* to be equal and the span must still survive
    /// the drop filter.
    #[test]
    fn an_underline_only_span_survives_with_a_plain_brush() {
        let out = resolve(&[StyleSpan {
            attrs: StyleAttrs::UNDERLINE,
            ..span(0, 4)
        }]);
        assert_eq!(out.len(), 1);
        assert!(out[0].underline);
        assert!(out[0].has_geometry());
        assert_eq!(out[0].brush, StyledBrush {
            color: DEFAULT_FG,
            style_index: 0,
            importance: IMPORTANCE_NORMAL,
        });
    }

    #[test]
    fn indexed_fg_defers_to_the_style_table_and_bakes_ink() {
        let out = resolve(&[StyleSpan {
            fg: Some(StyleColor::Indexed(4)),
            ..span(0, 2)
        }]);
        assert_eq!(out[0].brush.style_index, 5, "slot n maps to index n + 1");
        assert_eq!(
            out[0].brush.color, DEFAULT_FG,
            "the shader overrides an indexed color; the glyph keeps the default",
        );
        assert_ne!(
            out[0].ink, DEFAULT_FG,
            "decoration geometry has no table to defer to and must bake slot 4",
        );
    }

    #[test]
    fn truecolor_fg_rides_the_glyph_color_with_no_table_index() {
        let out = resolve(&[StyleSpan {
            fg: Some(StyleColor::Rgb(1, 2, 3)),
            ..span(0, 2)
        }]);
        assert_eq!(out[0].brush.style_index, 0);
        assert_eq!(out[0].brush.color, [1, 2, 3, 255]);
        assert_eq!(out[0].ink, [1, 2, 3, 255]);
    }

    #[test]
    fn inverse_swaps_the_two_roles_and_gives_up_the_table() {
        let out = resolve(&[StyleSpan {
            fg: Some(StyleColor::Rgb(9, 9, 9)),
            bg: Some(StyleColor::Rgb(7, 7, 7)),
            attrs: StyleAttrs::INVERSE,
            ..span(0, 2)
        }]);
        assert_eq!(out[0].brush.color, [7, 7, 7, 255], "bg paints the glyphs");
        assert_eq!(out[0].bg, Some([9, 9, 9, 255]), "fg paints the quad");
        assert_eq!(
            out[0].brush.style_index, 0,
            "an inverted span is resolved CPU-side, stale until re-shape",
        );
    }

    /// `ESC[7m` on otherwise-default text: the surface's own two defaults trade
    /// places, which is what a terminal shows.
    #[test]
    fn inverse_with_no_colors_swaps_the_surface_defaults() {
        let out = resolve(&[StyleSpan {
            attrs: StyleAttrs::INVERSE,
            ..span(0, 2)
        }]);
        assert_eq!(out[0].brush.color, DEFAULT_BG);
        assert_eq!(out[0].bg, Some(DEFAULT_FG));
    }

    #[test]
    fn bold_and_dim_map_to_their_constants_and_bold_wins_a_tie() {
        let out = resolve(&[
            StyleSpan { attrs: StyleAttrs::BOLD, ..span(0, 1) },
            StyleSpan { attrs: StyleAttrs::DIM, ..span(1, 2) },
            StyleSpan {
                attrs: StyleAttrs::BOLD | StyleAttrs::DIM,
                ..span(2, 3)
            },
        ]);
        assert_eq!(out[0].brush.importance, IMPORTANCE_BOLD);
        assert_eq!(out[1].brush.importance, IMPORTANCE_DIM);
        assert_eq!(out[2].brush.importance, IMPORTANCE_BOLD);
    }

    /// Spans that draw nothing must not exist, or an unstyled block would take
    /// the ranged-style shaping path (and the theme-epoch `ShapeKey`) for no
    /// visible reason.
    #[test]
    fn spans_that_draw_nothing_are_dropped() {
        let out = resolve(&[
            span(0, 0),
            span(5, 2),
            span(0, 4),
            StyleSpan { attrs: StyleAttrs::BLINK, ..span(0, 4) },
            StyleSpan { attrs: StyleAttrs::ITALIC, ..span(0, 4) },
        ]);
        assert!(out.is_empty(), "got {out:?}");
    }

    #[test]
    fn the_fingerprint_sees_an_underline_only_difference() {
        use std::hash::Hasher;
        let hash = |spans: &[StyleSpan]| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            styled_spans_fingerprint(&resolve(spans), &mut h);
            h.finish()
        };
        let plain = [StyleSpan {
            fg: Some(StyleColor::Indexed(1)),
            ..span(0, 4)
        }];
        let underlined = [StyleSpan {
            fg: Some(StyleColor::Indexed(1)),
            attrs: StyleAttrs::UNDERLINE,
            ..span(0, 4)
        }];
        assert_ne!(hash(&plain), hash(&underlined));
    }

    #[test]
    fn the_fingerprint_sees_a_weight_only_difference() {
        use std::hash::Hasher;
        let hash = |spans: &[StyleSpan]| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            styled_spans_fingerprint(&resolve(spans), &mut h);
            h.finish()
        };
        let plain = [StyleSpan {
            fg: Some(StyleColor::Indexed(1)),
            ..span(0, 4)
        }];
        let bold = [StyleSpan {
            fg: Some(StyleColor::Indexed(1)),
            attrs: StyleAttrs::BOLD,
            ..span(0, 4)
        }];
        assert_ne!(hash(&plain), hash(&bold));
    }

    #[test]
    fn the_fingerprint_sees_a_background_only_difference() {
        use std::hash::Hasher;
        let hash = |spans: &[StyleSpan]| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            styled_spans_fingerprint(&resolve(spans), &mut h);
            h.finish()
        };
        let plain = [StyleSpan {
            fg: Some(StyleColor::Indexed(1)),
            ..span(0, 4)
        }];
        let on_bg = [StyleSpan {
            fg: Some(StyleColor::Indexed(1)),
            bg: Some(StyleColor::Indexed(4)),
            ..span(0, 4)
        }];
        assert_ne!(hash(&plain), hash(&on_bg));
    }
}
