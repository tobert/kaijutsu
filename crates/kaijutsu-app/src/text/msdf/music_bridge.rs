//! Engraving IR → MSDF glyph bridge for ABC music notation.
//!
//! Converts `EngravingElement::Glyph` entries from `kaijutsu-abc`'s
//! engraving IR (noteheads, clefs, rests, accidentals, ...) into
//! `PositionedGlyph`s for the MSDF atlas. Mirrors
//! `layout_bridge::collect_msdf_glyphs`'s atlas-queueing contract (queue
//! unknown glyphs via `atlas.request`, leave baselines unsnapped — physical
//! snapping happens later in `MsdfBlockRenderer::build_vertices`) but
//! sources glyph identity and position from the IR directly rather than a
//! Parley layout, per the locked "positioned from the engraving IR
//! directly, never through Parley" decision for music notation.
//!
//! Lines, paths (beams/slurs/ties), and text (titles/chords) are NOT
//! handled here — they stay on the vello pass
//! (`text::abc::render_engraving_notation_only`).

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use peniko::Brush;

use kaijutsu_abc::engrave::font::{font_bytes, font_cache};
use kaijutsu_abc::engrave::EngravingElement;

use super::atlas::MsdfAtlas;
use super::glyph::{FontId, GlyphKey, PositionedGlyph};
use super::music_glyph_id;

/// Codepoints already warned about missing from the music cmap — caps the
/// warning to once per codepoint (not once per occurrence) so a repeatedly
/// engraved accidental in a long tune doesn't spam the log every rebuild.
static WARNED_MISSING: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();

/// Extract positioned music glyphs from engraving elements for MSDF
/// rendering. Non-`Glyph` elements are ignored (the caller's vello pass
/// draws those).
///
/// # Arguments
/// * `elements` — engraving IR (glyphs and non-glyphs both)
/// * `offset` — block-local (pad_left, pad_top) offset, added after the
///   origin-relative, block-scaled position
/// * `origin` — `(origin_x, origin_y)` from
///   `text::abc::compute_engraving_bounds` — the same origin the sibling
///   vello pass subtracts, so glyph quads and vello-drawn lines/beams land
///   in the same coordinate space
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

    /// Non-`Glyph` elements (Line/Path/Text) are ignored entirely — the
    /// vello pass draws those.
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
}
