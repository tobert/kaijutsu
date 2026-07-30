//! Engraving IR → MSDF glyph + geometry bridge for ABC music notation.
//!
//! Three converters, one per `EngravingElement` shape, all sharing the same
//! `offset`/`origin`/`block_scale` transform so their output lands in one
//! coordinate space:
//! - [`collect_music_glyphs`]: `Glyph` (noteheads, clefs, rests,
//!   accidentals) → `PositionedGlyph`s for the MSDF atlas. Mirrors
//!   `layout_bridge::collect_msdf_glyphs`'s atlas-queueing contract (queue
//!   unknown glyphs via `atlas.request`, leave baselines unsnapped —
//!   physical snapping happens later in `MsdfBlockRenderer::build_vertices`)
//!   but sources glyph identity and position from the IR directly rather
//!   than a Parley layout, per the locked "positioned from the engraving IR
//!   directly, never through Parley" decision for music notation.
//! - [`collect_music_geometry`]: `Line` (staff lines, barlines, stems,
//!   ledgers) and `Path` (beams, slurs/ties, repeat dots) → flat-colored
//!   `GeometryVertex` triangles, via `geometry::stroke_line_quad` and
//!   `geometry::{flatten_to_polygons, triangulate_polygon}`. No vello, no
//!   path rasterizer.
//! - [`collect_music_text_glyphs`]: `Text` (titles, tempo marks, chord
//!   symbols) → `PositionedGlyph`s, laying out each text run through Parley
//!   at its final on-screen size and handing the result to the SAME
//!   `layout_bridge::collect_msdf_glyphs` every other MSDF text block uses.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use kurbo::{Affine, BezPath, Point};
use peniko::Brush;

use kaijutsu_abc::engrave::font::{font_bytes, font_cache};
use kaijutsu_abc::engrave::EngravingElement;

use super::atlas::MsdfAtlas;
use super::geometry::{self, GeometryVertex};
use super::glyph::{FontId, GlyphKey, PositionedGlyph};
use super::music_glyph_id;

/// Codepoints already warned about missing from the music cmap — caps the
/// warning to once per codepoint (not once per occurrence) so a repeatedly
/// engraved accidental in a long tune doesn't spam the log every rebuild.
static WARNED_MISSING: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();

/// Extract positioned music glyphs from engraving elements for MSDF
/// rendering. Non-`Glyph` elements are ignored — `collect_music_geometry`
/// and `collect_music_text_glyphs` handle those.
///
/// # Arguments
/// * `elements` — engraving IR (glyphs and non-glyphs both)
/// * `offset` — block-local (pad_left, pad_top) offset, added after the
///   origin-relative, block-scaled position
/// * `origin` — `(origin_x, origin_y)` from
///   `text::abc::compute_engraving_bounds` — the same origin
///   `collect_music_geometry`/`collect_music_text_glyphs` subtract, so
///   glyphs, geometry, and text all land in the same coordinate space
/// * `block_scale` — the width/`SVG_MAX_HEIGHT` scale-to-fit factor applied
///   to the whole notation block
/// * `color` — brush for all music glyphs (the notation brush; ABC has no
///   per-glyph color)
/// * `atlas` — glyph atlas to queue missing glyphs
///
/// A codepoint the music cmap doesn't map is skipped (never panics in the
/// render path) and logged once via `warn!`.
pub fn collect_music_glyphs(
    elements: &[EngravingElement],
    offset: (f64, f64),
    origin: (f64, f64),
    block_scale: f64,
    color: &Brush,
    atlas: &mut MsdfAtlas,
) -> Vec<PositionedGlyph> {
    let font_id = FontId::from_static(font_bytes());
    let upem = font_cache().upem();
    let rgba = brush_to_rgba8(color);
    let mut glyphs = Vec::new();

    for elem in elements {
        let EngravingElement::Glyph {
            codepoint,
            x,
            y,
            scale,
            ..
        } = elem
        else {
            continue;
        };

        let Some(glyph_id) = music_glyph_id(*codepoint) else {
            warn_once_missing(*codepoint);
            continue;
        };

        let key = GlyphKey::new(font_id, glyph_id);
        atlas.request(key);

        let px = offset.0 + (*x - origin.0) * block_scale;
        let py = offset.1 + (*y - origin.1) * block_scale;
        let font_size = scale * upem * block_scale;

        glyphs.push(PositionedGlyph {
            key,
            x: px as f32,
            y: py as f32,
            font_size: font_size as f32,
            color: rgba,
            importance: 0.5,
        });
    }

    glyphs
}

/// Extract flat-colored geometry — staff lines, barlines, stems, ledgers
/// (`Line`), beams, slurs, ties, and repeat dots (`Path`) — from engraving
/// elements. The vello-free replacement for
/// `text::abc::render_engraving_notation_only`'s `scene.stroke`/
/// `scene.fill` calls. `Glyph` and `Text` elements are ignored (handled by
/// `collect_music_glyphs`/`collect_music_text_glyphs`).
///
/// `offset`/`origin`/`block_scale`/`color` mean exactly what they mean in
/// `collect_music_glyphs` — every `Line` endpoint and every `Path` point
/// goes through the same `offset + (p - origin) * block_scale` transform so
/// geometry, glyphs, and text all land in one coordinate space.
///
/// A `Path` whose `d` fails to parse, or that specifies `fill: false` (no
/// current engraver emitter produces a stroked Path — stroking an
/// arbitrary outline as a ribbon is a materially different problem from
/// filling one, not worth guessing at for input that doesn't exist yet),
/// is skipped and logged once, never panics: ABC input is arbitrary text,
/// not vetted before it reaches engraving.
pub fn collect_music_geometry(
    elements: &[EngravingElement],
    offset: (f64, f64),
    origin: (f64, f64),
    block_scale: f64,
    color: &Brush,
) -> Vec<GeometryVertex> {
    // Curve/arc flattening tolerance, in already-block-scaled PIXEL space
    // (the transform below is applied before flattening, precisely so a
    // single constant tolerance here is meaningful regardless of the
    // tune's font-unit scale). 0.35px is comfortably under one visible
    // pixel for the curve sizes music notation actually uses (repeat-dot
    // circles and slur/tie lenses render at a handful of px), matching
    // vello's own default flattening tolerances for on-screen vector art.
    const TOLERANCE_PX: f64 = 0.35;

    let rgba = brush_to_rgba8(color);
    let transform = Affine::translate(offset)
        * Affine::scale(block_scale)
        * Affine::translate((-origin.0, -origin.1));
    let mut vertices = Vec::new();

    for elem in elements {
        match elem {
            EngravingElement::Line {
                x1,
                y1,
                x2,
                y2,
                width,
                ..
            } => {
                let p1 = transform * Point::new(*x1, *y1);
                let p2 = transform * Point::new(*x2, *y2);
                let quad = geometry::stroke_line_quad(
                    p1.x,
                    p1.y,
                    p2.x,
                    p2.y,
                    width * block_scale,
                    rgba,
                );
                vertices.extend_from_slice(&quad);
            }
            EngravingElement::Path { d, fill, .. } => {
                if !*fill {
                    bevy::log::warn_once!(
                        "music geometry bridge: Path with fill=false is not \
                         supported (no current engraver emitter produces \
                         one) — skipping"
                    );
                    continue;
                }
                let Ok(path) = BezPath::from_svg(d) else {
                    bevy::log::warn_once!(
                        "music geometry bridge: failed to parse Path d={d:?} \
                         — skipping"
                    );
                    continue;
                };
                let path = transform * &path;
                for polygon in geometry::flatten_to_polygons(&path, TOLERANCE_PX) {
                    for tri in geometry::triangulate_polygon(&polygon) {
                        for p in tri {
                            vertices.push(GeometryVertex {
                                x: p.x as f32,
                                y: p.y as f32,
                                color: rgba,
                            });
                        }
                    }
                }
            }
            EngravingElement::Glyph { .. } | EngravingElement::Text { .. } => {}
        }
    }

    vertices
}

/// Extract MSDF glyphs for a tune's `Text` elements (titles, tempo marks,
/// chord symbols) by laying each one out through Parley at its final
/// on-screen size, then handing the result to
/// `layout_bridge::collect_msdf_glyphs` — the same function every other
/// MSDF text block (Markdown, Output, PlainText) uses. Unlike the removed
/// vello path (which built one sub-scene in unscaled IR units and let the
/// caller apply a single `Affine::scale`), each run here is laid out
/// directly at `size * block_scale` — real Parley shaping at the actual
/// rendered size, which is what every non-music MSDF text path already
/// does, rather than shaping small and scaling the raster after the fact.
///
/// `font: None` drops all text silently, matching
/// `render_engraving_elements`'s prior behavior ("If no font available,
/// text is silently dropped, matches the SVG path behavior").
pub fn collect_music_text_glyphs(
    elements: &[EngravingElement],
    offset: (f64, f64),
    origin: (f64, f64),
    block_scale: f64,
    color: &Brush,
    font: Option<&crate::text::shaping::VelloFont>,
    atlas: &mut MsdfAtlas,
    font_data_map: &mut super::FontDataMap,
) -> Vec<PositionedGlyph> {
    let Some(vello_font) = font else {
        return Vec::new();
    };

    let mut glyphs = Vec::new();

    for elem in elements {
        let EngravingElement::Text {
            content,
            x,
            y,
            size,
            ..
        } = elem
        else {
            continue;
        };

        let final_size = (*size * block_scale) as f32;
        if final_size <= 0.0 {
            continue;
        }

        let style = crate::text::shaping::VelloTextStyle {
            font_size: final_size,
            brush: color.clone(),
            ..Default::default()
        };
        let layout = vello_font.layout(
            content,
            &style,
            crate::text::shaping::VelloTextAlign::Left,
            None,
        );

        for line in layout.lines() {
            for item in line.items() {
                if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                    font_data_map.register(gr.run().font());
                }
            }
        }

        // Parley's layout baseline sits below y=0 by the line's ascent; ABC's
        // `y` is already the text's baseline. Shifting the layout's origin up
        // by `final_size` approximates "top of a single text line", mirroring
        // the removed vello path's `y - *size` shift at the new (already
        // block-scaled) font size.
        let px = offset.0 + (*x - origin.0) * block_scale;
        let py = offset.1 + (*y - origin.1) * block_scale - final_size as f64;

        let run_glyphs =
            super::layout_bridge::collect_msdf_glyphs(&layout, &[], color, (px, py), atlas);
        glyphs.extend(run_glyphs);
    }

    glyphs
}

fn warn_once_missing(codepoint: u32) {
    let set = WARNED_MISSING.get_or_init(|| Mutex::new(HashSet::new()));
    let mut set = set.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if set.insert(codepoint) {
        bevy::log::warn!(
            "music glyph bridge: no cmap entry for codepoint U+{:04X} — \
             skipping this glyph (render continues without it)",
            codepoint
        );
    }
}

/// Convert a Brush to RGBA8. Duplicated from `layout_bridge::brush_to_rgba8`
/// (private there) rather than shared — trivial and this module has no
/// other reason to depend on `layout_bridge`.
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
    use bevy::prelude::Assets;
    use bevy::prelude::Image;
    use kaijutsu_abc::engrave::EngravingElement;

    fn glyph_elem(codepoint: u32, x: f64, y: f64, scale: f64) -> EngravingElement {
        EngravingElement::Glyph {
            codepoint,
            x,
            y,
            scale,
            source_span: (0, 0),
        }
    }

    fn new_atlas() -> (Assets<Image>, MsdfAtlas) {
        let mut images = Assets::<Image>::default();
        let atlas = MsdfAtlas::new(&mut images, 64, 64);
        (images, atlas)
    }

    /// Hand-computed positions: offset=(5, 3), origin=(2, 4), block_scale=2.
    /// px = 5 + (10 - 2) * 2 = 21; py = 3 + (20 - 4) * 2 = 35.
    /// font_size = scale * upem * block_scale = 0.04 * 1000.0 * 2 = 80.
    /// (Bravura upem = 1000, confirmed by
    /// `kaijutsu_abc::engrave::font::font_cache().upem()`.)
    #[test]
    fn position_and_font_size_match_hand_computed_values() {
        let (_images, mut atlas) = new_atlas();
        let elements = vec![glyph_elem(0xE050, 10.0, 20.0, 0.04)];
        let brush = Brush::Solid(peniko::Color::new([1.0, 0.0, 0.0, 1.0]));

        let glyphs = collect_music_glyphs(
            &elements,
            (5.0, 3.0),
            (2.0, 4.0),
            2.0,
            &brush,
            &mut atlas,
        );

        assert_eq!(glyphs.len(), 1);
        let g = &glyphs[0];
        assert!((g.x - 21.0).abs() < 1e-4, "x = {}", g.x);
        assert!((g.y - 35.0).abs() < 1e-4, "y = {}", g.y);
        assert!(
            (g.font_size - 80.0).abs() < 1e-3,
            "font_size = {}",
            g.font_size
        );
        assert_eq!(g.color, [255, 0, 0, 255]);
    }

    /// U+E050 (treble clef) resolves to Bravura glyph id 74 — pinned so a
    /// regression in the cmap lookup (or a font swap that silently changes
    /// glyph ids) shows up here, not just visually.
    #[test]
    fn known_codepoint_resolves_to_the_expected_glyph_id() {
        let (_images, mut atlas) = new_atlas();
        let elements = vec![glyph_elem(0xE050, 0.0, 0.0, 1.0)];
        let brush = Brush::Solid(peniko::Color::WHITE);

        let glyphs = collect_music_glyphs(
            &elements,
            (0.0, 0.0),
            (0.0, 0.0),
            1.0,
            &brush,
            &mut atlas,
        );

        assert_eq!(glyphs.len(), 1);
        assert_eq!(glyphs[0].key.glyph_id, 74);
    }

    /// Glyphs are queued into the atlas exactly like `collect_msdf_glyphs`
    /// queues Parley-sourced glyphs — `request()` must see the key.
    #[test]
    fn unknown_glyph_is_queued_into_the_atlas() {
        let (_images, mut atlas) = new_atlas();
        let elements = vec![glyph_elem(0xE050, 0.0, 0.0, 1.0)];
        let brush = Brush::Solid(peniko::Color::WHITE);

        let glyphs = collect_music_glyphs(
            &elements,
            (0.0, 0.0),
            (0.0, 0.0),
            1.0,
            &brush,
            &mut atlas,
        );

        assert_eq!(atlas.pending, vec![glyphs[0].key]);
    }

    /// A codepoint the music cmap doesn't map must be skipped, not panic —
    /// the render path never crashes on unrecognized notation.
    #[test]
    fn unknown_codepoint_is_skipped_not_panicking() {
        let (_images, mut atlas) = new_atlas();
        // 0x0041 ('A') is outside the SMuFL PUA range the cmap scans.
        let elements = vec![
            glyph_elem(0x0041, 0.0, 0.0, 1.0),
            glyph_elem(0xE050, 10.0, 10.0, 1.0),
        ];
        let brush = Brush::Solid(peniko::Color::WHITE);

        let glyphs = collect_music_glyphs(
            &elements,
            (0.0, 0.0),
            (0.0, 0.0),
            1.0,
            &brush,
            &mut atlas,
        );

        // Only the recognized glyph survives; the unknown one is silently
        // skipped, not a panic and not a placeholder entry.
        assert_eq!(glyphs.len(), 1);
        assert_eq!(glyphs[0].key.glyph_id, 74);
    }

    /// Non-`Glyph` elements (Line/Path/Text) are ignored entirely by
    /// `collect_music_glyphs` — `collect_music_geometry`/
    /// `collect_music_text_glyphs` handle those.
    #[test]
    fn non_glyph_elements_are_ignored() {
        let (_images, mut atlas) = new_atlas();
        let elements = vec![
            EngravingElement::Line {
                x1: 0.0,
                y1: 0.0,
                x2: 10.0,
                y2: 0.0,
                width: 0.5,
                source_span: (0, 0),
            },
            EngravingElement::Text {
                content: "Title".to_string(),
                x: 0.0,
                y: 0.0,
                size: 12.0,
                source_span: (0, 0),
            },
            EngravingElement::Path {
                d: "M0 0 L1 1".to_string(),
                fill: false,
                source_span: (0, 0),
            },
        ];
        let brush = Brush::Solid(peniko::Color::WHITE);

        let glyphs = collect_music_glyphs(
            &elements,
            (0.0, 0.0),
            (0.0, 0.0),
            1.0,
            &brush,
            &mut atlas,
        );

        assert!(glyphs.is_empty());
        assert!(atlas.pending.is_empty());
    }

    // -- collect_music_geometry -------------------------------------------

    /// Hand-computed against the same offset/origin/block_scale as
    /// `position_and_font_size_match_hand_computed_values`: offset=(5, 3),
    /// origin=(2, 4), block_scale=2. A horizontal Line from (2,4) to
    /// (12,4) with width=1: p1 = (5 + (2-2)*2, 3 + (4-4)*2) = (5, 3);
    /// p2 = (5 + (12-2)*2, 3) = (25, 3); scaled width = 1*2 = 2, so the
    /// quad's y corners sit at 3 ± 1.
    #[test]
    fn line_geometry_matches_hand_computed_quad_corners() {
        let elements = vec![EngravingElement::Line {
            x1: 2.0,
            y1: 4.0,
            x2: 12.0,
            y2: 4.0,
            width: 1.0,
            source_span: (0, 0),
        }];
        let brush = Brush::Solid(peniko::Color::new([1.0, 1.0, 1.0, 1.0]));

        let verts = collect_music_geometry(&elements, (5.0, 3.0), (2.0, 4.0), 2.0, &brush);

        assert_eq!(verts.len(), 6, "one stroked line = 2 triangles = 6 vertices");
        let xs: Vec<f32> = verts.iter().map(|v| v.x).collect();
        let ys: Vec<f32> = verts.iter().map(|v| v.y).collect();
        assert!(xs.iter().any(|&x| (x - 5.0).abs() < 1e-4), "{xs:?}");
        assert!(xs.iter().any(|&x| (x - 25.0).abs() < 1e-4), "{xs:?}");
        assert!(ys.iter().any(|&y| (y - 2.0).abs() < 1e-4), "{ys:?}");
        assert!(ys.iter().any(|&y| (y - 4.0).abs() < 1e-4), "{ys:?}");
    }

    /// A beam's parallelogram path (matches `emit_beam`'s "M L L L Z"
    /// shape exactly) has no curves, so `collect_music_geometry` must
    /// triangulate it exactly — 2 triangles, 6 vertices, area preserved
    /// under the offset/origin/block_scale transform.
    #[test]
    fn straight_path_beam_triangulates_to_two_triangles_with_scaled_area() {
        // A 10x2 rectangle at the origin, in IR units.
        let d = "M 0 0 L 10 0 L 10 2 L 0 2 Z".to_string();
        let elements = vec![EngravingElement::Path {
            d,
            fill: true,
            source_span: (0, 0),
        }];
        let brush = Brush::Solid(peniko::Color::WHITE);

        let block_scale = 3.0;
        let verts = collect_music_geometry(&elements, (0.0, 0.0), (0.0, 0.0), block_scale, &brush);

        assert_eq!(verts.len(), 6);
        let area: f64 = {
            let tri_area = |a: &GeometryVertex, b: &GeometryVertex, c: &GeometryVertex| {
                0.5 * ((b.x - a.x) as f64 * (c.y - a.y) as f64
                    - (c.x - a.x) as f64 * (b.y - a.y) as f64)
                    .abs()
            };
            tri_area(&verts[0], &verts[1], &verts[2]) + tri_area(&verts[3], &verts[4], &verts[5])
        };
        // 10x2 rectangle scaled by block_scale on both axes: area * scale^2.
        let expected = 10.0 * 2.0 * block_scale * block_scale;
        assert!((area - expected).abs() < 1e-6, "area={area} expected={expected}");
    }

    /// A repeat-dot circle (matches `emit_repeat_dots`'s "M A A Z" shape —
    /// the same SVG-arc encoding a real tune emits) must flatten and
    /// triangulate to a filled polygon whose area approximates the true
    /// circle, not silently vanish just because it has no straight-line
    /// fast path.
    #[test]
    fn circular_repeat_dot_path_triangulates_to_a_circle_shaped_area() {
        let r = 4.0;
        let (cx, cy) = (0.0, 0.0);
        let d = format!(
            "M {:.3} {:.3} A {:.3} {:.3} 0 1 0 {:.3} {:.3} A {:.3} {:.3} 0 1 0 {:.3} {:.3} Z",
            cx - r, cy, r, r, cx + r, cy, r, r, cx - r, cy,
        );
        let elements = vec![EngravingElement::Path {
            d,
            fill: true,
            source_span: (0, 0),
        }];
        let brush = Brush::Solid(peniko::Color::WHITE);

        let verts = collect_music_geometry(&elements, (0.0, 0.0), (0.0, 0.0), 1.0, &brush);
        assert!(!verts.is_empty(), "a circle must produce fillable geometry");
        assert_eq!(verts.len() % 3, 0, "output is a flat triangle list");

        let area: f64 = verts
            .chunks_exact(3)
            .map(|t| {
                0.5 * ((t[1].x - t[0].x) as f64 * (t[2].y - t[0].y) as f64
                    - (t[2].x - t[0].x) as f64 * (t[1].y - t[0].y) as f64)
                    .abs()
            })
            .sum();
        // Generous relative bound: `TOLERANCE_PX` bounds absolute Hausdorff
        // deviation (~0.35px), not relative area error, and for a small
        // radius like this the two don't have the same magnitude (see
        // `geometry::tests::tighter_tolerance_...` for the precise
        // tolerance-vs-area relationship at unit-circle scale). This test's
        // job is to prove a repeat dot produces a plausible, non-vanishing,
        // non-wildly-wrong circle — not to pin the exact deficit.
        let expected = std::f64::consts::PI * r * r;
        let relative_err = (area - expected).abs() / expected;
        assert!(relative_err < 0.2, "area={area} expected={expected} relative_err={relative_err}");
    }

    /// A tie/slur lens (the exact "M Q Q Z" shape `emit_tie_or_slur`
    /// produces) is the genuinely non-convex case — must not panic and
    /// must produce a plausible (nonzero, bounded) filled area.
    #[test]
    fn tie_slur_lens_path_triangulates_without_panicking() {
        let d = "M 0 0 Q 10 -6 20 0 Q 10 -4 0 0 Z".to_string();
        let elements = vec![EngravingElement::Path {
            d,
            fill: true,
            source_span: (0, 0),
        }];
        let brush = Brush::Solid(peniko::Color::WHITE);

        let verts = collect_music_geometry(&elements, (0.0, 0.0), (0.0, 0.0), 1.0, &brush);
        assert!(!verts.is_empty());
        assert_eq!(verts.len() % 3, 0);

        let area: f64 = verts
            .chunks_exact(3)
            .map(|t| {
                0.5 * ((t[1].x - t[0].x) as f64 * (t[2].y - t[0].y) as f64
                    - (t[2].x - t[0].x) as f64 * (t[1].y - t[0].y) as f64)
                    .abs()
            })
            .sum();
        // Loose sanity bound: the lens sits inside a 20x6 bounding box.
        assert!(area > 0.0 && area < 120.0, "area={area}");
    }

    /// A stroke-only Path (`fill: false`) is not supported (nothing emits
    /// one today) — must be skipped, not silently mis-rendered as filled.
    #[test]
    fn unfilled_path_is_skipped_not_mis_rendered_as_filled() {
        let elements = vec![EngravingElement::Path {
            d: "M 0 0 L 10 0 L 10 10 Z".to_string(),
            fill: false,
            source_span: (0, 0),
        }];
        let brush = Brush::Solid(peniko::Color::WHITE);

        let verts = collect_music_geometry(&elements, (0.0, 0.0), (0.0, 0.0), 1.0, &brush);
        assert!(verts.is_empty());
    }

    /// A Path whose `d` fails to parse must be skipped, never panic.
    #[test]
    fn unparseable_path_is_skipped_not_panicking() {
        let elements = vec![EngravingElement::Path {
            d: "not an svg path at all".to_string(),
            fill: true,
            source_span: (0, 0),
        }];
        let brush = Brush::Solid(peniko::Color::WHITE);

        let verts = collect_music_geometry(&elements, (0.0, 0.0), (0.0, 0.0), 1.0, &brush);
        assert!(verts.is_empty());
    }

    /// Glyph and Text elements are ignored by `collect_music_geometry` —
    /// `collect_music_glyphs`/`collect_music_text_glyphs` handle those.
    #[test]
    fn glyph_and_text_elements_are_ignored_by_geometry_collection() {
        let elements = vec![
            glyph_elem(0xE050, 0.0, 0.0, 1.0),
            EngravingElement::Text {
                content: "Title".to_string(),
                x: 0.0,
                y: 0.0,
                size: 12.0,
                source_span: (0, 0),
            },
        ];
        let brush = Brush::Solid(peniko::Color::WHITE);

        let verts = collect_music_geometry(&elements, (0.0, 0.0), (0.0, 0.0), 1.0, &brush);
        assert!(verts.is_empty());
    }

    // -- collect_music_text_glyphs -----------------------------------------

    /// No font available: text is dropped silently, matching the removed
    /// vello path's documented behavior ("matches the SVG path behavior").
    #[test]
    fn text_glyphs_are_empty_without_a_font() {
        let (_images, mut atlas) = new_atlas();
        let mut font_data_map = crate::text::msdf::FontDataMap::default();
        let elements = vec![EngravingElement::Text {
            content: "Title".to_string(),
            x: 0.0,
            y: 0.0,
            size: 12.0,
            source_span: (0, 0),
        }];
        let brush = Brush::Solid(peniko::Color::WHITE);

        let glyphs = collect_music_text_glyphs(
            &elements,
            (0.0, 0.0),
            (0.0, 0.0),
            1.0,
            &brush,
            None,
            &mut atlas,
            &mut font_data_map,
        );
        assert!(glyphs.is_empty());
    }

    /// Non-`Text` elements are ignored entirely — no font needed to prove
    /// this since the function returns before laying anything out.
    #[test]
    fn non_text_elements_are_ignored_by_text_collection() {
        let (_images, mut atlas) = new_atlas();
        let mut font_data_map = crate::text::msdf::FontDataMap::default();
        let elements = vec![
            glyph_elem(0xE050, 0.0, 0.0, 1.0),
            EngravingElement::Line {
                x1: 0.0,
                y1: 0.0,
                x2: 1.0,
                y2: 0.0,
                width: 0.5,
                source_span: (0, 0),
            },
        ];
        let brush = Brush::Solid(peniko::Color::WHITE);

        let glyphs = collect_music_text_glyphs(
            &elements,
            (0.0, 0.0),
            (0.0, 0.0),
            1.0,
            &brush,
            None,
            &mut atlas,
            &mut font_data_map,
        );
        assert!(glyphs.is_empty());
    }
}
