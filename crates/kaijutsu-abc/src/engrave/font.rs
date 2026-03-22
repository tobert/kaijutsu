//! SMuFL font loading and glyph path extraction via skrifa.
//!
//! Embeds Bravura.otf at compile time and provides glyph outlines
//! as SVG path `d` attribute strings.

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::OnceLock;

use kurbo::BezPath;
use skrifa::instance::Size;
use skrifa::outline::DrawSettings;
use skrifa::raw::FontRef;
use skrifa::raw::TableProvider;
use skrifa::MetadataProvider;

/// Bravura OTF embedded at compile time.
static BRAVURA_OTF: &[u8] = include_bytes!("../../assets/Bravura.otf");

/// Cached font data: maps Unicode codepoint → (SVG path d, advance width).
static FONT_CACHE: OnceLock<FontCache> = OnceLock::new();

/// Font cache holding pre-extracted glyph outlines.
pub struct FontCache {
    glyphs: HashMap<u32, GlyphData>,
    bezpaths: HashMap<u32, BezPath>,
    upem: f64,
}

struct GlyphData {
    path_d: String,
    advance: f64,
}

/// SVG path pen that converts skrifa outline commands to SVG path `d` attribute.
struct SvgPathPen {
    d: String,
}

impl SvgPathPen {
    fn new() -> Self {
        SvgPathPen { d: String::new() }
    }
}

impl skrifa::outline::OutlinePen for SvgPathPen {
    fn move_to(&mut self, x: f32, y: f32) {
        // Flip Y: font coordinates have Y-up, SVG has Y-down
        write!(self.d, "M{:.1} {:.1}", x, -y).unwrap();
    }

    fn line_to(&mut self, x: f32, y: f32) {
        write!(self.d, "L{:.1} {:.1}", x, -y).unwrap();
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        write!(self.d, "Q{:.1} {:.1} {:.1} {:.1}", cx0, -cy0, x, -y).unwrap();
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        write!(
            self.d,
            "C{:.1} {:.1} {:.1} {:.1} {:.1} {:.1}",
            cx0, -cy0, cx1, -cy1, x, -y
        )
        .unwrap();
    }

    fn close(&mut self) {
        self.d.push('Z');
    }
}

/// Outline pen that builds a `kurbo::BezPath` instead of an SVG string.
/// Same Y-flip convention as `SvgPathPen` (font Y-up → display Y-down).
struct BezPathPen {
    path: BezPath,
}

impl BezPathPen {
    fn new() -> Self {
        BezPathPen {
            path: BezPath::new(),
        }
    }
}

impl skrifa::outline::OutlinePen for BezPathPen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.path.move_to((x as f64, -y as f64));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.path.line_to((x as f64, -y as f64));
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.path
            .quad_to((cx0 as f64, -cy0 as f64), (x as f64, -y as f64));
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.path.curve_to(
            (cx0 as f64, -cy0 as f64),
            (cx1 as f64, -cy1 as f64),
            (x as f64, -y as f64),
        );
    }

    fn close(&mut self) {
        self.path.close_path();
    }
}

/// Codepoints we pre-cache from Bravura (SMuFL).
const PRELOAD_CODEPOINTS: &[u32] = &[
    0xE050, // Treble clef
    0xE0A2, // Whole notehead
    0xE0A3, // Half notehead
    0xE0A4, // Quarter/filled notehead
    0xE4E3, // Whole rest
    0xE4E4, // Half rest
    0xE4E5, // Quarter rest
    0xE4E6, // Eighth rest
    0xE4E7, // Sixteenth rest
    0xE260, // Flat
    0xE261, // Natural
    0xE262, // Sharp
    0xE080, // Time sig digit 0
    0xE081, // Time sig digit 1
    0xE082, // Time sig digit 2
    0xE083, // Time sig digit 3
    0xE084, // Time sig digit 4
    0xE085, // Time sig digit 5
    0xE086, // Time sig digit 6
    0xE087, // Time sig digit 7
    0xE088, // Time sig digit 8
    0xE089, // Time sig digit 9
    0xE240, // Flag 8th up
    0xE241, // Flag 8th down
    0xE242, // Flag 16th up
    0xE243, // Flag 16th down
];

impl FontCache {
    fn load() -> Self {
        let font = FontRef::new(BRAVURA_OTF).expect("Failed to parse Bravura.otf");
        let upem = font.head().expect("no head table").units_per_em() as f64;
        let outline_glyphs = font.outline_glyphs();

        let charmap = font.charmap();
        let size = Size::unscaled();

        let mut glyphs = HashMap::new();
        let mut bezpaths = HashMap::new();

        for &cp in PRELOAD_CODEPOINTS {
            let Some(ch) = char::from_u32(cp) else {
                continue;
            };
            let Some(glyph_id) = charmap.map(ch) else {
                continue;
            };

            let Some(outline) = outline_glyphs.get(glyph_id) else {
                continue;
            };

            // Draw SVG path
            let mut svg_pen = SvgPathPen::new();
            let settings = DrawSettings::unhinted(size, skrifa::instance::LocationRef::default());
            if outline.draw(settings, &mut svg_pen).is_err() {
                continue;
            }

            // Draw BezPath
            let mut bez_pen = BezPathPen::new();
            let settings = DrawSettings::unhinted(size, skrifa::instance::LocationRef::default());
            if outline.draw(settings, &mut bez_pen).is_ok() {
                bezpaths.insert(cp, bez_pen.path);
            }

            // Get advance width from hmtx
            let advance = font
                .glyph_metrics(size, skrifa::instance::LocationRef::default())
                .advance_width(glyph_id)
                .unwrap_or(0.0) as f64;

            glyphs.insert(
                cp,
                GlyphData {
                    path_d: svg_pen.d,
                    advance,
                },
            );
        }

        FontCache {
            glyphs,
            bezpaths,
            upem,
        }
    }

    /// Get the SVG path `d` attribute for a glyph.
    pub fn glyph_path(&self, codepoint: u32) -> Option<&str> {
        self.glyphs.get(&codepoint).map(|g| g.path_d.as_str())
    }

    /// Get the advance width for a glyph (in font units).
    pub fn glyph_advance(&self, codepoint: u32) -> Option<f64> {
        self.glyphs.get(&codepoint).map(|g| g.advance)
    }

    /// Get the `kurbo::BezPath` outline for a glyph.
    pub fn glyph_bezpath(&self, codepoint: u32) -> Option<&BezPath> {
        self.bezpaths.get(&codepoint)
    }

    /// Units per em for this font.
    pub fn upem(&self) -> f64 {
        self.upem
    }
}

/// Get the global font cache (initialized on first call).
pub fn font_cache() -> &'static FontCache {
    FONT_CACHE.get_or_init(FontCache::load)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_loads_and_has_treble_clef() {
        let cache = font_cache();
        let path = cache.glyph_path(0xE050);
        assert!(path.is_some(), "Bravura should have treble clef glyph");
        assert!(
            !path.unwrap().is_empty(),
            "Treble clef path should not be empty"
        );
    }

    #[test]
    fn all_preloaded_codepoints_present() {
        let cache = font_cache();
        for &cp in PRELOAD_CODEPOINTS {
            assert!(
                cache.glyph_path(cp).is_some(),
                "Missing glyph for codepoint U+{:04X}",
                cp
            );
        }
    }

    #[test]
    fn glyph_bezpath_treble_clef() {
        let cache = font_cache();
        let path = cache.glyph_bezpath(0xE050);
        assert!(path.is_some(), "Bravura should have treble clef BezPath");
        // Non-empty BezPath should have path elements
        let elements: Vec<_> = path.unwrap().elements().to_vec();
        assert!(
            !elements.is_empty(),
            "Treble clef BezPath should not be empty"
        );
    }

    #[test]
    fn upem_is_reasonable() {
        let cache = font_cache();
        // Most fonts use 1000 or 2048
        assert!(cache.upem() > 500.0 && cache.upem() < 5000.0);
    }
}
