//! Parley-to-MSDF glyph bridge.
//!
//! Extracts positioned glyph data from Parley layouts for MSDF rendering.
//! Parley measures, MSDF renders — both use the same font data and metrics.

use peniko::Brush;

use super::atlas::MsdfAtlas;
use super::glyph::{FontId, GlyphKey, PositionedGlyph};
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
    collect_with(layout, offset, atlas, |glyph_run| {
        // Determine color from span brushes
        let text_range = glyph_run.run().text_range();
        let run_brush = crate::text::rich::brush_at_offset(span_brushes, text_range.start)
            .unwrap_or(fallback_brush);
        brush_to_rgba8(run_brush)
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
    collect_with(layout, offset, atlas, |glyph_run| {
        brush_to_rgba8(&glyph_run.style().brush)
    })
}

/// The glyph walk both collectors share; they differ only in where the color
/// comes from.
fn collect_with(
    layout: &parley::Layout<Brush>,
    offset: (f64, f64),
    atlas: &mut MsdfAtlas,
    color_of: impl Fn(&parley::GlyphRun<'_, Brush>) -> [u8; 4],
) -> Vec<PositionedGlyph> {
    let mut glyphs = Vec::new();

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
            let color = color_of(&glyph_run);

            for glyph in glyph_run.glyphs() {
                let gx = x + glyph.x;
                let gy = y - glyph.y;
                x += glyph.advance;

                let key = GlyphKey::new(font_id, glyph.id as u16);

                // Queue unknown glyphs for async generation
                atlas.request(key);

                glyphs.push(PositionedGlyph {
                    key,
                    x: gx + offset.0 as f32,
                    y: gy + offset.1 as f32,
                    font_size,
                    color,
                    importance: 0.5,
                });
            }
        }
    }

    glyphs
}

/// Convert a Brush to RGBA8.
fn brush_to_rgba8(brush: &Brush) -> [u8; 4] {
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
    use peniko::Color;

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
}
