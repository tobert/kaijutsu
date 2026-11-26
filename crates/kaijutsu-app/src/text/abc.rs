//! Engraving checks for the app's notation renderer.

#[cfg(test)]
use kaijutsu_abc::engrave::EngravingElement;
#[cfg(test)]
pub use kaijutsu_abc::engrave::ir::engraving_bounds as compute_engraving_bounds;

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
