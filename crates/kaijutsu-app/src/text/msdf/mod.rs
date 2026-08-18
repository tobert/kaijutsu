//! MSDF (Multi-channel Signed Distance Field) text rendering.
//!
//! Renders text blocks using MSDF textures for GPU-native text quality:
//! shader-based hinting, directional AA, stem darkening, and effects.
//!
//! Architecture:
//! ```text
//! Parley (layout + metrics)
//!     ↓
//! collect_msdf_glyphs() (glyph positions + colors)
//!     ↓
//! MsdfAtlas (glyph_id → MSDF texture region)
//!     ↓
//! MsdfBlockRenderer (per-block render pass → block texture)
//!     ↓
//! BlockFxMaterial (post-processing: glow, animation)
//! ```
//!
//! The conversation view is entirely off vello for block content: MSDF
//! renders text content (PlainText, Markdown, Output, border/role-divider
//! labels) and, together with `geometry`'s flat-colored triangles, ABC music
//! notation; SVG is a CPU raster (`text::svg_raster`, resvg/tiny-skia) into a
//! child `Image`; sparklines and the image placeholder are plain UI
//! rectangle geometry (`text::sparkline`); block borders / role dividers are
//! an SDF shader (`cell::block_border` + `shaders::BlockFxMaterial`).

pub mod atlas;
pub mod generator;
pub mod geometry;
pub mod glyph;
pub mod layout_bridge;
pub mod music_bridge;
pub mod music_geometry_renderer;
pub mod renderer;
pub mod surface_renderer;

pub use atlas::MsdfAtlas;
pub use generator::MsdfGenerator;
pub use geometry::GeometryVertex;
pub use glyph::{FontId, PositionedGlyph};
pub use layout_bridge::{collect_msdf_glyphs, collect_msdf_glyphs_styled};
pub use music_bridge::{collect_music_geometry, collect_music_glyphs, collect_music_text_glyphs};
// MsdfBlockRenderer is used directly in the render world via crate::text::msdf::renderer

use bevy::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// Map from FontId to raw font bytes (Arc-shared for async generation).
///
/// Populated during `build_block_scenes` when MSDF glyphs are collected.
/// The generator reads from this to spawn async MSDF generation tasks.
#[derive(Resource, Default)]
pub struct FontDataMap {
    data: HashMap<FontId, Arc<Vec<u8>>>,
}

impl FontDataMap {
    /// Register font data for a FontId (no-op if already present).
    pub fn register(&mut self, font: &parley::FontData) {
        let id = FontId::from_parley(font);
        self.data
            .entry(id)
            .or_insert_with(|| Arc::new(font.data.data().to_vec()));
    }

    /// Register a statically-known font's bytes (e.g. an embedded music
    /// font) under a `FontId::from_static` identity. Idempotent, same as
    /// `register` — a font already registered under this id is left alone.
    /// Unlike `register`, this doesn't need a live Parley layout to have run
    /// first: it's meant for startup registration of fonts the app knows
    /// about in advance (`kaijutsu_abc::engrave::font::font_bytes()`), so
    /// the async MSDF generator can always resolve them.
    pub fn register_static(&mut self, id: FontId, bytes: &'static [u8]) {
        self.data.entry(id).or_insert_with(|| Arc::new(bytes.to_vec()));
    }

    /// Get font data for a FontId.
    pub fn get(&self, id: &FontId) -> Option<&Arc<Vec<u8>>> {
        self.data.get(id)
    }

    /// Number of registered fonts.
    pub fn len(&self) -> usize {
        self.data.len()
    }
}

/// Per-block MSDF glyph data.
///
/// Stores positioned glyphs extracted from Parley layout for MSDF rendering.
/// Updated during `build_block_scenes` alongside the Vello scene.
#[derive(Component, Default)]
pub struct MsdfBlockGlyphs {
    pub glyphs: Vec<PositionedGlyph>,
    /// Monotonic re-render signal for the MSDF pass. Two writers bump it —
    /// scene rebuilds (`build_block_scenes`) and glyph arrivals
    /// (`poll_msdf_generator`) — and BOTH must advance it from its own
    /// previous value (`wrapping_add(1)`), never derive it from another
    /// counter. Deriving from `scene_version` once reset it below the
    /// extract gate's `last_rendered` high-water mark after arrival bumps,
    /// permanently silencing the composite for that block (staff-without-
    /// glyphs bug, msdf-music live verify 2026-07-16).
    pub version: u64,
    pub rainbow: bool,
}

/// Per-block flat-colored geometry — anything a block draws that is neither
/// a glyph nor text. Producer-agnostic by construction; two block kinds fill
/// it today:
///
/// - **ABC**: staff lines, barlines, stems, ledgers, beams, slurs, ties,
///   repeat dots, via `text::msdf::music_bridge::collect_music_geometry`.
/// - **Diff**: per-line add/remove background bands, via
///   `text::diff::build_diff_band_geometry`.
///
/// Deliberately has NO version field of its own. `MsdfBlockGlyphs.version`
/// already survived a real two-writers-disagree bug (see its doc comment);
/// giving geometry a second independent counter would just be a second
/// chance to reintroduce that class of bug. Every producer rebuilds geometry
/// and glyphs together in one arm of `build_block_scenes`, so they ride
/// `MsdfBlockGlyphs.version` as a single shared gate — extraction reads both
/// components whenever that version fires. A new producer must hold that
/// same rule: fill both, bump once.
#[derive(Component, Default)]
pub struct MsdfBlockGeometry {
    pub vertices: Vec<geometry::GeometryVertex>,
}

/// Which renderer draws a block's base content into its texture (before the
/// `BlockFxMaterial` shader post-processes border/glow/label decoration on
/// top). Single-variant today — every block cell texture is MSDF-rendered
/// (or carries no texture content at all, for a block whose content is
/// plain UI geometry: sparkline, image placeholder, SVG raster — spawned as
/// sibling child entities instead). Kept as a component/bundle marker
/// (`msdf_surface_bundle`) rather than collapsed away; a `Vello` variant
/// existed here until it was retired as dead weight (docs/issues.md,
/// 2026-08-12) — nothing had assigned it since ABC and SVG both moved off
/// vello.
#[derive(Component, Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockRenderMethod {
    /// MSDF renders text glyphs (or nothing, for a block whose content is
    /// plain UI geometry — sparkline, image placeholder — spawned as
    /// sibling child entities instead of texture content).
    #[default]
    Msdf,
}

/// Lazily-built codepoint → glyph-id map for the embedded music font
/// (Bravura, via `kaijutsu_abc::engrave::font::font_bytes()`). Built once
/// from the font's cmap table via `ttf_parser` — the same crate/version
/// `MsdfGenerator` uses to parse font data for MSDF generation, so glyph ids
/// resolved here are guaranteed valid inputs to that pipeline.
///
/// SMuFL codepoints (noteheads, clefs, rests, accidentals, ...) all live in
/// the Unicode Private Use Area, so scanning U+E000..=U+F8FF once at
/// startup captures every glyph the engraving IR can reference without
/// needing to know kaijutsu-abc's specific codepoint list.
static MUSIC_CMAP: OnceLock<HashMap<u32, u16>> = OnceLock::new();

fn build_music_cmap() -> HashMap<u32, u16> {
    let bytes = kaijutsu_abc::engrave::font::font_bytes();
    let mut map = HashMap::new();

    let Ok(face) = ttf_parser::Face::parse(bytes, 0) else {
        error!("music cmap: Bravura font_bytes() failed to parse — no music glyphs will resolve");
        return map;
    };

    for cp in 0xE000u32..=0xF8FF {
        let Some(ch) = char::from_u32(cp) else {
            continue;
        };
        if let Some(id) = face.glyph_index(ch) {
            map.insert(cp, id.0);
        }
    }

    map
}

/// Look up a music glyph id for a SMuFL codepoint via the lazily-built
/// Bravura cmap. Returns `None` for a codepoint the font doesn't map —
/// callers (the music glyph bridge) must skip it and log once, never panic.
pub fn music_glyph_id(codepoint: u32) -> Option<u16> {
    MUSIC_CMAP.get_or_init(build_music_cmap).get(&codepoint).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_static_then_get_returns_the_bytes() {
        static BYTES: &[u8] = b"pretend-music-font-bytes";
        let mut map = FontDataMap::default();
        let id = FontId::from_static(BYTES);

        map.register_static(id, BYTES);

        assert_eq!(map.get(&id).map(|b| b.as_slice()), Some(BYTES));
    }

    #[test]
    fn register_static_is_idempotent() {
        static BYTES: &[u8] = b"pretend-music-font-bytes-2";
        let mut map = FontDataMap::default();
        let id = FontId::from_static(BYTES);

        map.register_static(id, BYTES);
        let first_ptr = map.get(&id).unwrap().as_ptr();
        map.register_static(id, BYTES);
        let second_ptr = map.get(&id).unwrap().as_ptr();

        assert_eq!(map.len(), 1);
        assert_eq!(
            first_ptr, second_ptr,
            "a second register_static for the same id must not replace the stored Arc"
        );
    }

    #[test]
    fn music_cmap_maps_treble_clef_to_a_nonzero_glyph_id() {
        // U+E050 is the SMuFL treble (G) clef — also exercised by
        // generator.rs's `bravura_cff_glyph_produces_real_msdf_ink` test as
        // the go/no-go codepoint for the msdfgen/CFF bridge.
        let id = music_glyph_id(0xE050);
        assert!(id.is_some(), "Bravura's cmap should map U+E050 (treble clef)");
        assert_ne!(id.unwrap(), 0, "glyph id 0 is .notdef, not a real glyph");
    }

    #[test]
    fn music_cmap_returns_none_for_an_unmapped_codepoint() {
        // Outside the scanned PUA range entirely.
        assert_eq!(music_glyph_id(0x0041), None);
    }
}
