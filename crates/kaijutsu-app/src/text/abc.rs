//! ABC notation intrinsic bounding-box math.
//!
//! `kaijutsu-abc`'s engraving IR (`Vec<EngravingElement>`) is
//! renderer-agnostic by design (glyph codepoint/x/y/scale, lines, paths,
//! text — all with source spans). This module supplies the ONE piece of
//! shared math every ABC render path needs before it can draw anything:
//! the content's intrinsic bounding box (for scale-to-fit) and the origin
//! it must translate by so content lands at (0, 0).
//!
//! No vello here — noteheads/clefs/rests/accidentals/text render through
//! `text::msdf::music_bridge` (MSDF atlas quads), and staff
//! lines/barlines/stems/ledgers/beams/slurs/ties/repeat-dots render through
//! `text::msdf::geometry` (flat-colored triangles). Lives in kaijutsu-app
//! (rather than kaijutsu-abc) only because `compute_engraving_bounds` reads
//! `kaijutsu_abc::engrave::font::font_cache()` for glyph advances, and
//! that's simplest to keep alongside its callers.

use kaijutsu_abc::engrave::font::font_cache;
use kaijutsu_abc::engrave::EngravingElement;

/// Intrinsic bounding box of a set of engraving elements: the content
/// extent (all element kinds, including glyphs) padded by `margin` on every
/// side, plus the origin the content must be translated by so it lands at
/// (0, 0). Every render path — `text::msdf::music_bridge`'s
/// `collect_music_glyphs`/`collect_music_geometry`/`collect_music_text_glyphs`
/// — shares this so glyph quads, geometry triangles, and text glyphs all
/// agree on where (0, 0) is and how big the content is.
pub struct EngravingBounds {
    pub width: f64,
    pub height: f64,
    pub origin_x: f64,
    pub origin_y: f64,
}

/// Compute `EngravingBounds` for `elements`, mirroring the logic from
/// `engrave/svg.rs`. Counts every element kind (glyphs included) regardless
/// of which render path ends up drawing them — the bounding box must be the
/// same whether glyphs are drawn as MSDF quads or not, since the
/// MSDF-glyph-plus-geometry hybrid still needs to scale-to-fit and position
/// against the *full* content extent.
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

#[cfg(test)]
mod golden_tests;

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The bbox must count Glyph elements even though they render as MSDF
    /// quads rather than vello fills — every render path needs to agree on
    /// intrinsic dimensions and origin regardless of which one actually
    /// draws the glyphs.
    #[test]
    fn glyph_only_elements_still_produce_a_nonzero_bbox() {
        let elements = vec![treble_clef(0.0, 0.0, 0.04)];

        let bounds = compute_engraving_bounds(&elements, 0.0);
        assert!(bounds.width > 0.0);
        assert!(bounds.height > 0.0);
    }
}
