//! Parley-to-MSDF glyph bridge.
//!
//! Extracts positioned glyph data from Parley layouts for MSDF rendering.
//! Parley measures, MSDF renders — both use the same font data and metrics.

use std::collections::HashSet;

use peniko::Brush;

use super::atlas::MsdfAtlas;
use super::glyph::{FontId, GlyphKey, IMPORTANCE_NORMAL, PositionedGlyph};
use crate::text::ansi::StyledBrush;
use crate::text::rich::SpanBrush;

/// Extract positioned glyphs from a Parley layout for MSDF rendering.
///
/// Iterates glyph runs, maps per-run brush to RGBA8 color, and queues
/// unknown glyphs to the atlas for async generation. Baselines are left as
/// raw (unsnapped) logical pixels here — snapping to a pixel boundary only
/// makes sense in PHYSICAL pixels (a fractional HiDPI `scale_factor` means
/// a logical-integer baseline isn't necessarily a physical scanline), so it
/// happens later, in `MsdfBlockRenderer::build_vertices` (renderer.rs),
/// which knows the render target's physical size and scale.
///
/// # Arguments
/// * `layout` — Parley layout (already computed by `VelloFont::layout()`)
/// * `span_brushes` — per-span byte-range to brush mapping
/// * `fallback_brush` — brush for glyphs outside any span range
/// * `offset` — block-local (pad_left, pad_top) offset
/// * `atlas` — glyph atlas to queue missing glyphs
pub fn collect_msdf_glyphs(
    layout: &parley::Layout<Brush>,
    span_brushes: &[SpanBrush],
    fallback_brush: &Brush,
    offset: (f64, f64),
    atlas: &mut MsdfAtlas,
) -> Vec<PositionedGlyph> {
    let (glyphs, keys) =
        collect_msdf_glyphs_deferred(layout, span_brushes, fallback_brush, offset);
    for key in keys {
        atlas.request(key);
    }
    glyphs
}

/// [`collect_msdf_glyphs`] without the atlas mutation — safe to call from an
/// `AsyncComputeTaskPool` task, which has no access to the main-thread-only
/// [`MsdfAtlas`] resource.
///
/// Returns the positioned glyphs plus the deduplicated list of keys the
/// caller must `atlas.request()` on the main thread to reproduce
/// `collect_msdf_glyphs`'s effect. Deduplication is sound because
/// `MsdfAtlas::request` is itself idempotent — it only ever pushes a key
/// into `pending` the first time it is neither known, pending, nor
/// permanently failed — so requesting a key once has the same observable
/// effect as requesting it once per glyph occurrence.
pub fn collect_msdf_glyphs_deferred(
    layout: &parley::Layout<Brush>,
    span_brushes: &[SpanBrush],
    fallback_brush: &Brush,
    offset: (f64, f64),
) -> (Vec<PositionedGlyph>, Vec<GlyphKey>) {
    collect_with(layout, offset, |glyph_run| {
        // Determine color from span brushes
        let text_range = glyph_run.run().text_range();
        let run_brush = crate::text::rich::brush_at_offset(span_brushes, text_range.start)
            .unwrap_or(fallback_brush);
        GlyphPaint::plain(brush_to_rgba8(run_brush))
    })
}

/// Extract positioned glyphs, taking each glyph run's color **from the
/// layout's own ranged styles**.
///
/// The pair of [`crate::text::shaping::VelloFont::layout_spanned`]: brushes
/// pushed as parley `StyleProperty::Brush` ranges make parley split its glyph
/// runs on the span boundaries, and `GlyphRun::style()` then hands back the
/// brush for exactly those glyphs. That is what [`collect_msdf_glyphs`] cannot
/// do — it resolves one brush per *shaping run* from the run's start byte, so a
/// span starting mid-run (word-level diff highlighting) is invisible to it.
///
/// There is no fallback argument because there is no fallback: every glyph run
/// carries a style, and `layout_spanned` pushes the default brush under the
/// whole text.
pub fn collect_msdf_glyphs_styled(
    layout: &parley::Layout<Brush>,
    offset: (f64, f64),
    atlas: &mut MsdfAtlas,
) -> Vec<PositionedGlyph> {
    let (glyphs, keys) = collect_msdf_glyphs_styled_deferred(layout, offset);
    for key in keys {
        atlas.request(key);
    }
    glyphs
}

/// [`collect_msdf_glyphs_styled`] without the atlas mutation — see
/// [`collect_msdf_glyphs_deferred`] for the contract (dedup, idempotence)
/// this shares with it.
pub fn collect_msdf_glyphs_styled_deferred(
    layout: &parley::Layout<Brush>,
    offset: (f64, f64),
) -> (Vec<PositionedGlyph>, Vec<GlyphKey>) {
    collect_with(layout, offset, |glyph_run| {
        GlyphPaint::plain(brush_to_rgba8(&glyph_run.style().brush))
    })
}

/// [`collect_msdf_glyphs_styled_deferred`] in the conversation surface's ANSI
/// currency ([`StyledBrush`], pushed by `VelloFont::layout_styled`).
///
/// The pair that makes the whole ANSI path work: the brush parley resolved for
/// each glyph run carries the style-table index and the MSDF weight as well as
/// the color, so a run boundary that exists only because two spans differ in
/// *bold* arrives here with the bold still attached.
pub fn collect_msdf_glyphs_ansi_deferred(
    layout: &parley::Layout<StyledBrush>,
    offset: (f64, f64),
) -> (Vec<PositionedGlyph>, Vec<GlyphKey>) {
    collect_with(layout, offset, |glyph_run| {
        let brush = &glyph_run.style().brush;
        GlyphPaint {
            color: brush.color,
            style_index: brush.style_index,
            importance: brush.importance,
        }
    })
}

/// Everything a glyph run contributes to the glyphs it produces, beyond where
/// they sit: the color, the style-table index that may override it on the GPU,
/// and the MSDF stem weight.
///
/// One struct rather than three closure returns because the three always move
/// together — a collector that set the color from a brush and left the weight
/// at the default would be a silent half-styled glyph.
#[derive(Clone, Copy, Debug, PartialEq)]
struct GlyphPaint {
    color: [u8; 4],
    style_index: u32,
    importance: f32,
}

impl GlyphPaint {
    /// Unstyled paint: a color and nothing else — what every collector that
    /// predates the style table produces.
    fn plain(color: [u8; 4]) -> Self {
        Self {
            color,
            style_index: 0,
            importance: IMPORTANCE_NORMAL,
        }
    }
}

/// The glyph walk every deferred collector shares; they differ only in where
/// the paint comes from. Returns the glyphs plus the deduplicated,
/// first-occurrence-ordered list of keys an atlas `request()` pass would
/// need to see to reproduce the non-deferred collectors' effect.
///
/// Generic over parley's brush currency: the surface's ANSI path shapes into
/// [`StyledBrush`] while everything else shapes into `peniko::Brush`, and the
/// walk itself never looks at a brush — `paint_of` does.
fn collect_with<B: parley::Brush>(
    layout: &parley::Layout<B>,
    offset: (f64, f64),
    paint_of: impl Fn(&parley::GlyphRun<'_, B>) -> GlyphPaint,
) -> (Vec<PositionedGlyph>, Vec<GlyphKey>) {
    let mut glyphs = Vec::new();
    let mut keys = Vec::new();
    let mut seen = HashSet::new();

    for line in layout.lines() {
        for item in line.items() {
            let parley::PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                continue;
            };

            let mut x = glyph_run.offset();
            let y = glyph_run.baseline();
            let run = glyph_run.run();
            let font = run.font();
            let font_size = run.font_size();
            let font_id = FontId::from_parley(font);
            let paint = paint_of(&glyph_run);

            for glyph in glyph_run.glyphs() {
                let gx = x + glyph.x;
                let gy = y - glyph.y;
                x += glyph.advance;

                let key = GlyphKey::new(font_id, glyph.id as u16);

                // Queue unknown glyphs for async generation. `request()` is
                // idempotent (see doc comment on `collect_msdf_glyphs_deferred`),
                // so collapsing repeats here is safe.
                if seen.insert(key) {
                    keys.push(key);
                }

                glyphs.push(PositionedGlyph {
                    key,
                    x: gx + offset.0 as f32,
                    y: gy + offset.1 as f32,
                    font_size,
                    color: paint.color,
                    importance: paint.importance,
                    style_index: paint.style_index,
                });
            }
        }
    }

    (glyphs, keys)
}

/// Convert a Brush to RGBA8.
///
/// `pub(crate)` because it is the one place that decides what color a brush
/// *is* for this renderer, and the conversation surface fingerprints span
/// colors against exactly that decision — a second copy of the match would be
/// a second answer.
pub(crate) fn brush_to_rgba8(brush: &Brush) -> [u8; 4] {
    match brush {
        Brush::Solid(color) => {
            let [r, g, b, a] = color.components;
            [
                (r.clamp(0.0, 1.0) * 255.0) as u8,
                (g.clamp(0.0, 1.0) * 255.0) as u8,
                (b.clamp(0.0, 1.0) * 255.0) as u8,
                (a.clamp(0.0, 1.0) * 255.0) as u8,
            ]
        }
        _ => [255, 255, 255, 255],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::shaping::{
        VelloFont, VelloTextAlign, VelloTextStyle, load_into_font_context,
    };
    use bevy::prelude::{Assets, Image};
    use kaijutsu_types::{StyleAttrs, StyleColor, StyleSpan};
    use peniko::Color;

    use super::super::glyph::{IMPORTANCE_BOLD, IMPORTANCE_DIM};

    const RED: [u8; 4] = [255, 0, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];

    fn brush(rgba: [u8; 4]) -> Brush {
        Brush::Solid(Color::from_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]))
    }

    /// The shipped mono font, registered into the shared collection exactly as
    /// the asset loader does. Shaping for real is the point: the property
    /// under test is *parley's* run splitting, not our arithmetic.
    fn mono() -> VelloFont {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/fonts/NotoMono-Regular.ttf"
        ))
        .expect("shipped test font must be present");
        load_into_font_context(bytes)
    }

    fn style(base: [u8; 4]) -> VelloTextStyle {
        VelloTextStyle {
            brush: brush(base),
            font_size: 16.0,
            ..Default::default()
        }
    }

    fn colors(layout: &parley::Layout<Brush>) -> Vec<[u8; 4]> {
        let mut images = Assets::<Image>::default();
        let mut atlas = MsdfAtlas::new(&mut images, 64, 64);
        collect_msdf_glyphs_styled(layout, (0.0, 0.0), &mut atlas)
            .iter()
            .map(|g| g.color)
            .collect()
    }

    /// **The word-level highlight contract** (`docs/diff.md` slice 6): a brush
    /// span that starts in the *middle* of a shaping run colors exactly its own
    /// glyphs. `aaabbbccc` is one run — the byte-offset path
    /// (`collect_msdf_glyphs`) reads one brush from its start byte and paints
    /// all nine glyphs with it, which is why the ranged-style path exists.
    #[test]
    fn a_brush_span_starting_mid_run_colors_exactly_its_own_glyphs() {
        let font = mono();
        let text = "aaabbbccc";
        let spans = [SpanBrush {
            start: 3,
            end: 6,
            brush: brush(BLUE),
        }];
        let layout =
            font.layout_spanned(text, &style(RED), VelloTextAlign::Left, None, &spans);

        assert_eq!(
            colors(&layout),
            vec![RED, RED, RED, BLUE, BLUE, BLUE, RED, RED, RED],
        );
    }

    /// The failure this replaces, pinned so it cannot come back unnoticed: the
    /// run-start lookup cannot see a span that begins mid-run.
    #[test]
    fn the_run_start_lookup_misses_a_mid_run_span() {
        let font = mono();
        let text = "aaabbbccc";
        let spans = [SpanBrush {
            start: 3,
            end: 6,
            brush: brush(BLUE),
        }];
        let layout = font.layout(text, &style(RED), VelloTextAlign::Left, None);
        let mut images = Assets::<Image>::default();
        let mut atlas = MsdfAtlas::new(&mut images, 64, 64);
        let glyphs = collect_msdf_glyphs(&layout, &spans, &brush(RED), (0.0, 0.0), &mut atlas);
        assert!(
            glyphs.iter().all(|g| g.color == RED),
            "the byte-offset path is expected to miss the mid-run span",
        );
    }

    /// Later spans win where they overlap — the property the diff's per-line
    /// color plus per-word emphasis relies on if the two ever overlap.
    #[test]
    fn a_later_span_overrides_an_earlier_one() {
        let font = mono();
        let text = "abcdef";
        let spans = [
            SpanBrush { start: 0, end: 6, brush: brush(RED) },
            SpanBrush { start: 2, end: 4, brush: brush(BLUE) },
        ];
        let layout = font.layout_spanned(
            text,
            &style([9, 9, 9, 255]),
            VelloTextAlign::Left,
            None,
            &spans,
        );
        assert_eq!(colors(&layout), vec![RED, RED, BLUE, BLUE, RED, RED]);
    }

    /// Brush is not a shaping property: splitting runs on color must not move
    /// a glyph or change where the text wraps.
    #[test]
    fn spanned_and_plain_layouts_position_glyphs_identically() {
        let font = mono();
        let text = "the quick brown fox jumps over the lazy dog";
        let spans = [SpanBrush { start: 5, end: 12, brush: brush(BLUE) }];
        let plain = font.layout(text, &style(RED), VelloTextAlign::Left, Some(120.0));
        let spanned =
            font.layout_spanned(text, &style(RED), VelloTextAlign::Left, Some(120.0), &spans);

        assert_eq!(plain.len(), spanned.len(), "line count changed");
        let mut images = Assets::<Image>::default();
        let mut atlas = MsdfAtlas::new(&mut images, 64, 64);
        let a = collect_msdf_glyphs(&plain, &[], &brush(RED), (0.0, 0.0), &mut atlas);
        let b = collect_msdf_glyphs_styled(&spanned, (0.0, 0.0), &mut atlas);
        assert_eq!(a.len(), b.len(), "glyph count changed");
        for (a, b) in a.iter().zip(b.iter()) {
            assert_eq!(a.key, b.key);
            assert!((a.x - b.x).abs() < 0.001, "glyph moved: {} vs {}", a.x, b.x);
            assert!((a.y - b.y).abs() < 0.001);
        }
    }

    /// Two [`PositionedGlyph`]s are the same glyph placement — field-by-field,
    /// since the type has no `PartialEq` (its `f32`s make deriving one a
    /// footgun for real callers).
    fn assert_glyphs_eq(a: &[PositionedGlyph], b: &[PositionedGlyph]) {
        assert_eq!(a.len(), b.len(), "glyph count differs");
        for (a, b) in a.iter().zip(b.iter()) {
            assert_eq!(a.key, b.key);
            assert_eq!(a.x, b.x);
            assert_eq!(a.y, b.y);
            assert_eq!(a.font_size, b.font_size);
            assert_eq!(a.color, b.color);
            assert_eq!(a.importance, b.importance);
            assert_eq!(a.style_index, b.style_index);
        }
    }

    /// `collect_msdf_glyphs` must be a thin wrapper: its glyph output is
    /// byte-identical to `collect_msdf_glyphs_deferred`'s, and its atlas
    /// effect is identical to requesting the deferred key list directly —
    /// the property that makes it safe to move the deferred half onto an
    /// `AsyncComputeTaskPool` task and finish on the main thread with a
    /// plain `request()` loop.
    #[test]
    fn deferred_and_wrapper_agree_for_collect_msdf_glyphs() {
        let font = mono();
        let text = "the quick brown fox jumps over the lazy dog";
        let spans = [SpanBrush { start: 5, end: 12, brush: brush(BLUE) }];
        let layout =
            font.layout_spanned(text, &style(RED), VelloTextAlign::Left, Some(120.0), &spans);

        let (deferred_glyphs, keys) =
            collect_msdf_glyphs_deferred(&layout, &spans, &brush(RED), (3.0, 7.0));

        let mut images = Assets::<Image>::default();
        let mut wrapper_atlas = MsdfAtlas::new(&mut images, 64, 64);
        let wrapper_glyphs =
            collect_msdf_glyphs(&layout, &spans, &brush(RED), (3.0, 7.0), &mut wrapper_atlas);

        assert_glyphs_eq(&deferred_glyphs, &wrapper_glyphs);

        // Replaying the deferred key list through `request()` must leave the
        // atlas in the same state the wrapper's per-glyph `request()` calls
        // did.
        let mut images = Assets::<Image>::default();
        let mut replayed_atlas = MsdfAtlas::new(&mut images, 64, 64);
        for key in &keys {
            replayed_atlas.request(*key);
        }
        assert_eq!(replayed_atlas.pending, wrapper_atlas.pending);
    }

    /// Same parity property as above, for the ranged-style collector.
    #[test]
    fn deferred_and_wrapper_agree_for_collect_msdf_glyphs_styled() {
        let font = mono();
        let text = "aaabbbccc";
        let spans = [SpanBrush { start: 3, end: 6, brush: brush(BLUE) }];
        let layout =
            font.layout_spanned(text, &style(RED), VelloTextAlign::Left, None, &spans);

        let (deferred_glyphs, keys) = collect_msdf_glyphs_styled_deferred(&layout, (0.0, 0.0));

        let mut images = Assets::<Image>::default();
        let mut wrapper_atlas = MsdfAtlas::new(&mut images, 64, 64);
        let wrapper_glyphs =
            collect_msdf_glyphs_styled(&layout, (0.0, 0.0), &mut wrapper_atlas);

        assert_glyphs_eq(&deferred_glyphs, &wrapper_glyphs);

        let mut images = Assets::<Image>::default();
        let mut replayed_atlas = MsdfAtlas::new(&mut images, 64, 64);
        for key in &keys {
            replayed_atlas.request(*key);
        }
        assert_eq!(replayed_atlas.pending, wrapper_atlas.pending);
    }

    // ---- the ANSI currency ------------------------------------------------

    /// Resolve `spans` over `text` against the default text color `base`, the
    /// way `shape_cache` does, and hand back the glyphs.
    fn ansi_glyphs(text: &str, base: [u8; 4], spans: &[StyleSpan]) -> Vec<PositionedGlyph> {
        use crate::ui::theme::AnsiColors;
        let font = mono();
        let resolved = crate::text::ansi::resolve_style_spans(
            spans,
            base,
            [0, 0, 0, 255],
            &AnsiColors::default().palette_256(),
        );
        let layout =
            font.layout_styled(text, &style(base), VelloTextAlign::Left, None, &resolved);
        collect_msdf_glyphs_ansi_deferred(&layout, (0.0, 0.0)).0
    }

    fn ansi_span(start: u32, end: u32) -> StyleSpan {
        StyleSpan {
            start,
            end,
            fg: None,
            bg: None,
            attrs: StyleAttrs::default(),
        }
    }

    /// **The granularity contract.** Two adjacent spans that are the same ANSI
    /// color and differ only in bold have identical resolved *colors* — the
    /// palette slot lives in the style index, not the color — so if the brush
    /// carried a color alone, parley would merge them into one run and the
    /// boundary would be gone before any collector ran. Shaping for real is
    /// the point of this test.
    #[test]
    fn same_color_different_bold_shapes_as_two_weights() {
        let glyphs = ansi_glyphs(
            "aaabbb",
            RED,
            &[
                StyleSpan { fg: Some(StyleColor::Indexed(2)), ..ansi_span(0, 3) },
                StyleSpan {
                    fg: Some(StyleColor::Indexed(2)),
                    attrs: StyleAttrs::BOLD,
                    ..ansi_span(3, 6)
                },
            ],
        );
        assert_eq!(
            glyphs.iter().map(|g| g.importance).collect::<Vec<_>>(),
            vec![
                IMPORTANCE_NORMAL,
                IMPORTANCE_NORMAL,
                IMPORTANCE_NORMAL,
                IMPORTANCE_BOLD,
                IMPORTANCE_BOLD,
                IMPORTANCE_BOLD,
            ],
        );
        assert!(
            glyphs.iter().all(|g| g.style_index == 3),
            "both halves are ANSI slot 2, so both defer to table index 3",
        );
    }

    /// An indexed foreground stays semantic all the way to the glyph: the
    /// color is still the block's default (the shader replaces it from the
    /// style table), and the boundary shows up in `style_index` alone.
    #[test]
    fn an_indexed_span_sets_style_index_on_exactly_its_own_glyphs() {
        let glyphs = ansi_glyphs(
            "aaabbbccc",
            RED,
            &[StyleSpan { fg: Some(StyleColor::Indexed(4)), ..ansi_span(3, 6) }],
        );
        assert_eq!(
            glyphs.iter().map(|g| g.style_index).collect::<Vec<_>>(),
            vec![0, 0, 0, 5, 5, 5, 0, 0, 0],
        );
        assert!(
            glyphs.iter().all(|g| g.color == RED),
            "an indexed color never reaches the glyph; the default stands",
        );
    }

    /// Truecolor is the other half of the split: an absolute color cannot
    /// retheme, so it is resolved at shape time and rides the glyph color with
    /// no table index at all.
    #[test]
    fn a_truecolor_span_rides_the_glyph_color_with_index_zero() {
        let glyphs = ansi_glyphs(
            "aaabbb",
            RED,
            &[StyleSpan { fg: Some(StyleColor::Rgb(0, 0, 255)), ..ansi_span(3, 6) }],
        );
        assert_eq!(
            glyphs.iter().map(|g| g.color).collect::<Vec<_>>(),
            vec![RED, RED, RED, BLUE, BLUE, BLUE],
        );
        assert!(glyphs.iter().all(|g| g.style_index == 0));
    }

    /// Dim is a weight, not a fade — and it must not leak past its own span.
    #[test]
    fn dim_thins_exactly_its_own_glyphs() {
        let glyphs = ansi_glyphs(
            "aabb",
            RED,
            &[StyleSpan { attrs: StyleAttrs::DIM, ..ansi_span(2, 4) }],
        );
        assert_eq!(
            glyphs.iter().map(|g| g.importance).collect::<Vec<_>>(),
            vec![
                IMPORTANCE_NORMAL,
                IMPORTANCE_NORMAL,
                IMPORTANCE_DIM,
                IMPORTANCE_DIM,
            ],
        );
    }

    /// The `style_index = 0` regression guard: text with no ANSI spans at all
    /// shapes to exactly the same glyphs through the ANSI path as through the
    /// plain one, so an unspanned block renders pixel-identically.
    #[test]
    fn unspanned_text_through_the_ansi_path_matches_the_plain_path() {
        let font = mono();
        let text = "the quick brown fox";
        let plain = font.layout(text, &style(RED), VelloTextAlign::Left, None);
        let (plain_glyphs, _) = collect_msdf_glyphs_deferred(&plain, &[], &brush(RED), (0.0, 0.0));
        let ansi = ansi_glyphs(text, RED, &[]);

        assert_glyphs_eq(&plain_glyphs, &ansi);
        assert!(ansi.iter().all(|g| g.style_index == 0));
    }

    /// The deferred key list is the deduplicated, first-occurrence-ordered
    /// set of the glyphs' keys — sound because `MsdfAtlas::request` is
    /// idempotent (see its doc comment), so a repeated key contributes
    /// nothing more the second time it's requested.
    #[test]
    fn deferred_key_list_is_deduped_first_occurrence_ordered_glyph_keys() {
        let font = mono();
        // Repeats of "a" and "b" force repeated GlyphKeys within one run.
        let text = "aabba";
        let layout = font.layout(text, &style(RED), VelloTextAlign::Left, None);

        let (glyphs, keys) = collect_msdf_glyphs_deferred(&layout, &[], &brush(RED), (0.0, 0.0));

        assert_eq!(glyphs.len(), 5, "one glyph per character");

        // No duplicates in the key list.
        let unique: HashSet<_> = keys.iter().copied().collect();
        assert_eq!(unique.len(), keys.len(), "deferred key list must be deduped");

        // Every key in the list came from a glyph, and every glyph's key is
        // in the list — same set either way.
        let glyph_keys: HashSet<_> = glyphs.iter().map(|g| g.key).collect();
        assert_eq!(unique, glyph_keys);

        // First-occurrence order: "a" (index 0) precedes "b" (index 2) in
        // the source text, so it must precede it in the key list too.
        let a_key = glyphs[0].key;
        let b_key = glyphs[2].key;
        assert_ne!(a_key, b_key, "\"a\" and \"b\" must shape to distinct glyphs");
        let a_pos = keys.iter().position(|k| *k == a_key).unwrap();
        let b_pos = keys.iter().position(|k| *k == b_key).unwrap();
        assert!(a_pos < b_pos, "key list must preserve first-occurrence order");
    }
}
