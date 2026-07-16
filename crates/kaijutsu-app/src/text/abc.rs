//! ABC notation → Vello scene rendering.
//!
//! Converts `Vec<EngravingElement>` from `kaijutsu-abc` into a `vello::Scene`.
//! Lives in kaijutsu-app (which has both the vello render deps and
//! kaijutsu-abc) so kaijutsu-abc stays a leaf crate.

use bevy::prelude::default;
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};
use vello::kurbo::{Affine, BezPath, Cap, Line, Stroke};
use vello::peniko::{Brush, Fill};

use kaijutsu_abc::engrave::font::font_cache;
use kaijutsu_abc::engrave::EngravingElement;

use super::TextMetrics;

/// Intrinsic bounding box of a set of engraving elements: the content
/// extent (all element kinds, including glyphs) padded by `margin` on every
/// side, plus the origin the content must be translated by so it lands at
/// (0, 0). Both render paths — the full vello scene
/// (`render_engraving_to_scene`) and the glyph-skipping one
/// (`render_engraving_notation_only`), plus the MSDF music glyph bridge
/// (`text::msdf::music_bridge::collect_music_glyphs`) — share this so a
/// block's vello-drawn lines/text and MSDF-drawn glyphs agree on where
/// (0, 0) is and how big the content is.
pub struct EngravingBounds {
    pub width: f64,
    pub height: f64,
    pub origin_x: f64,
    pub origin_y: f64,
}

/// Compute `EngravingBounds` for `elements`, mirroring the logic from
/// `engrave/svg.rs`. Counts every element kind (glyphs included) regardless
/// of which render path ends up drawing them — the bounding box must be the
/// same whether or not glyphs are actually rasterized here, since the
/// MSDF-glyph-plus-vello-lines hybrid path still needs to scale-to-fit and
/// position against the *full* content extent.
pub fn compute_engraving_bounds(elements: &[EngravingElement], margin: f64) -> EngravingBounds {
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

/// Render engraving elements to a vello Scene.
///
/// Returns `(scene, width, height)` where width/height are the intrinsic
/// dimensions of the notation content (including margin).
pub fn render_engraving_to_scene(
    elements: &[EngravingElement],
    margin: f64,
    color: &Brush,
    font: Option<&VelloFont>,
    text_metrics: &TextMetrics,
) -> (vello::Scene, f64, f64) {
    render_engraving_elements(elements, margin, color, font, text_metrics, false)
}

/// Render engraving elements to a vello Scene, omitting `Glyph` elements —
/// used when an MSDF atlas is available and
/// `text::msdf::music_bridge::collect_music_glyphs` is drawing the
/// noteheads/clefs/rests/accidentals separately as atlas quads. Lines
/// (staff lines, stems, barlines, ledgers), paths (beams/slurs/ties), and
/// text (titles/chords) still rasterize here exactly as in
/// `render_engraving_to_scene`. The returned width/height still include the
/// glyphs' contribution to the bounding box (via `compute_engraving_bounds`,
/// which never skips any element kind) so both render paths agree on
/// intrinsic dimensions and origin.
pub fn render_engraving_notation_only(
    elements: &[EngravingElement],
    margin: f64,
    color: &Brush,
    font: Option<&VelloFont>,
    text_metrics: &TextMetrics,
) -> (vello::Scene, f64, f64) {
    render_engraving_elements(elements, margin, color, font, text_metrics, true)
}

fn render_engraving_elements(
    elements: &[EngravingElement],
    margin: f64,
    color: &Brush,
    font: Option<&VelloFont>,
    _text_metrics: &TextMetrics,
    skip_glyphs: bool,
) -> (vello::Scene, f64, f64) {
    let fc = font_cache();
    let mut scene = vello::Scene::new();

    let bounds = compute_engraving_bounds(elements, margin);
    let origin_x = bounds.origin_x;
    let origin_y = bounds.origin_y;

    // Render elements with origin offset so (origin_x, origin_y) maps to (0,0)
    let offset = Affine::translate((-origin_x, -origin_y));

    for elem in elements {
        match elem {
            EngravingElement::Glyph {
                codepoint,
                x,
                y,
                scale,
                ..
            } => {
                if skip_glyphs {
                    continue;
                }
                if let Some(bezpath) = fc.glyph_bezpath(*codepoint) {
                    let transform =
                        offset * Affine::translate((*x, *y)) * Affine::scale(*scale);
                    scene.fill(Fill::NonZero, transform, color, None, bezpath);
                }
            }
            EngravingElement::Line {
                x1,
                y1,
                x2,
                y2,
                width: line_width,
                ..
            } => {
                let line = Line::new((*x1, *y1), (*x2, *y2));
                // Butt caps: stroked lines (barlines, stems, ledgers, staff
                // lines) end exactly at their endpoints instead of overhanging
                // by half the stroke width via the default round cap.
                let stroke = Stroke::new(*line_width).with_caps(Cap::Butt);
                scene.stroke(&stroke, offset, color, None, &line);
            }
            EngravingElement::Path { d, fill, .. } => {
                if let Ok(bezpath) = BezPath::from_svg(d) {
                    if *fill {
                        scene.fill(Fill::NonZero, offset, color, None, &bezpath);
                    } else {
                        scene.stroke(&Stroke::new(0.5), offset, color, None, &bezpath);
                    }
                }
            }
            EngravingElement::Text {
                content,
                x,
                y,
                size,
                ..
            } => {
                // Render text using Parley via VelloFont if available
                if let Some(vello_font) = font {
                    let style = VelloTextStyle {
                        font_size: *size as f32,
                        brush: color.clone(),
                        ..default()
                    };
                    let layout = vello_font.layout(
                        content,
                        &style,
                        VelloTextAlign::Left,
                        None,
                    );
                    // Parley baseline is at y=0 of the layout; ABC puts y at the baseline
                    let text_offset = (-origin_x + x, -origin_y + y - *size);
                    super::rich::render_layout_with_brushes(
                        &mut scene,
                        &layout,
                        &[],
                        color,
                        text_offset,
                    );
                }
                // If no font available, text is silently dropped (matches SVG path behavior)
            }
        }
    }

    (scene, bounds.width, bounds.height)
}

#[cfg(test)]
mod golden_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_abc::engrave::EngravingElement;

    fn treble_clef(x: f64, y: f64, scale: f64) -> EngravingElement {
        EngravingElement::Glyph {
            codepoint: 0xE050,
            x,
            y,
            scale,
            source_span: (0, 0),
        }
    }

    /// Hand-computed against Bravura's real metrics (upem=1000,
    /// glyph_advance(0xE050)=671.0 — the treble clef; confirmed via
    /// `kaijutsu_abc::engrave::font::font_cache()`):
    ///   glyph at (0,0), scale=0.04: advance=671*0.04=26.84,
    ///     glyph_height=1000*0.04=40 → contributes x:[0, 26.84], y:[-40, 20]
    ///   line (0,-10)-(50,-10): contributes x:[0, 50], y:[-10, -10]
    ///   combined extent: x:[0, 50], y:[-40, 20]
    ///   width = 50 + margin*2 = 60; height = 60 + margin*2 = 70
    ///   origin = (0 - margin, -40 - margin) = (-5, -45)
    #[test]
    fn compute_engraving_bounds_matches_hand_computed_values() {
        let elements = vec![
            treble_clef(0.0, 0.0, 0.04),
            EngravingElement::Line {
                x1: 0.0,
                y1: -10.0,
                x2: 50.0,
                y2: -10.0,
                width: 0.5,
                source_span: (0, 0),
            },
        ];

        let bounds = compute_engraving_bounds(&elements, 5.0);

        assert!((bounds.width - 60.0).abs() < 1e-6, "width = {}", bounds.width);
        assert!((bounds.height - 70.0).abs() < 1e-6, "height = {}", bounds.height);
        assert!(
            (bounds.origin_x - -5.0).abs() < 1e-6,
            "origin_x = {}",
            bounds.origin_x
        );
        assert!(
            (bounds.origin_y - -45.0).abs() < 1e-6,
            "origin_y = {}",
            bounds.origin_y
        );
    }

    /// The bbox must count Glyph elements even though the MSDF-hybrid path
    /// (Commit 3) skips drawing them in vello — both render paths need to
    /// agree on intrinsic dimensions and origin regardless of which one
    /// actually rasterizes the glyphs.
    #[test]
    fn glyph_only_elements_still_produce_a_nonzero_bbox() {
        let elements = vec![treble_clef(0.0, 0.0, 0.04)];

        let bounds = compute_engraving_bounds(&elements, 0.0);
        assert!(bounds.width > 0.0);
        assert!(bounds.height > 0.0);
    }

    /// `render_engraving_notation_only` must return the exact same
    /// intrinsic width/height as `render_engraving_to_scene` for identical
    /// input — the bounding box doesn't shrink just because the glyph-skip
    /// variant doesn't rasterize glyphs into its scene.
    #[test]
    fn notation_only_and_full_render_agree_on_intrinsic_dimensions() {
        let elements = vec![
            treble_clef(0.0, 0.0, 0.04),
            EngravingElement::Line {
                x1: 0.0,
                y1: -10.0,
                x2: 50.0,
                y2: -10.0,
                width: 0.5,
                source_span: (0, 0),
            },
        ];
        let brush = Brush::Solid(vello::peniko::Color::WHITE);
        let metrics = TextMetrics::default();

        let (_full_scene, full_w, full_h) =
            render_engraving_to_scene(&elements, 5.0, &brush, None, &metrics);
        let (_notation_scene, notation_w, notation_h) =
            render_engraving_notation_only(&elements, 5.0, &brush, None, &metrics);

        assert_eq!(full_w, notation_w);
        assert_eq!(full_h, notation_h);
    }
}

