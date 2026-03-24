//! Parley-to-MSDF glyph bridge.
//!
//! Extracts positioned glyph data from Parley layouts for MSDF rendering.
//! Parley measures, MSDF renders — both use the same font data and metrics.

use bevy_vello::parley;
use bevy_vello::vello::peniko::Brush;

use super::atlas::MsdfAtlas;
use super::glyph::{FontId, GlyphKey, PositionedGlyph};
use crate::text::rich::SpanBrush;

/// Extract positioned glyphs from a Parley layout for MSDF rendering.
///
/// Iterates glyph runs, snaps baselines to pixel boundaries, applies
/// x-height grid fitting, maps per-run brush to RGBA8 color, and
/// queues unknown glyphs to the atlas for async generation.
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

            // Snap baseline to pixel boundary
            let y_snapped = y.round();

            // Determine color from span brushes
            let text_range = run.text_range();
            let run_brush = crate::text::rich::brush_at_offset(span_brushes, text_range.start)
                .unwrap_or(fallback_brush);
            let color = brush_to_rgba8(run_brush);

            for glyph in glyph_run.glyphs() {
                let gx = x + glyph.x;
                let gy = y_snapped - glyph.y;
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
