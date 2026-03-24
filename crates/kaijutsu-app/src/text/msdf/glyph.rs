//! MSDF glyph types for Parley-based font identification.
//!
//! Uses `peniko::Font` data pointer as stable identity — each font's
//! Arc-backed blob has a unique address for the lifetime of the process.

/// Stable font identity derived from Parley `FontData` pointer.
///
/// `FontData` (from linebender_resource_handle) contains `data: Blob<u8>`
/// (Arc-backed) and `index: u32`. The data pointer is stable for the
/// lifetime of the Arc, so we can use it as a hash key.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub struct FontId(usize);

impl FontId {
    /// Create a FontId from a Parley FontData's data pointer.
    pub fn from_parley(font: &bevy_vello::parley::FontData) -> Self {
        Self(font.data.data().as_ptr() as usize)
    }
}

/// Key for looking up glyphs in the atlas.
///
/// Combines font identity with glyph ID for unique glyph identification.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub struct GlyphKey {
    /// Font identity (from data pointer).
    pub font_id: FontId,
    /// Glyph ID within the font.
    pub glyph_id: u16,
}

impl GlyphKey {
    pub fn new(font_id: FontId, glyph_id: u16) -> Self {
        Self { font_id, glyph_id }
    }
}

/// A glyph positioned within a block's local coordinate space.
///
/// Produced by `collect_msdf_glyphs()` from Parley layout data.
/// Consumed by the MSDF renderer to build vertex buffers.
#[derive(Clone, Debug)]
pub struct PositionedGlyph {
    /// Key for atlas lookup.
    pub key: GlyphKey,
    /// X position in block-local pixels.
    pub x: f32,
    /// Y position in block-local pixels (baseline).
    pub y: f32,
    /// Font size for scaling MSDF region to screen size.
    pub font_size: f32,
    /// Color (RGBA8) from span brush mapping.
    pub color: [u8; 4],
    /// Semantic importance for weight adjustment.
    /// 0.5 = normal, modulated for effects (bold=thicker, dim=thinner).
    pub importance: f32,
}
