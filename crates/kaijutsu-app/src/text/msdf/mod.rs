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
//! Vello still handles SVG and ABC notation (a CPU vector rasterizer, out of
//! the conversation-view de-vello scope). Everything else in the
//! conversation view is off vello: MSDF renders text content (PlainText,
//! Markdown, Output, border/role-divider labels), sparklines and the image
//! placeholder are plain UI rectangle geometry (`text::sparkline`), and
//! block borders / role dividers are an SDF shader (`cell::block_border` +
//! `shaders::BlockFxMaterial`).

pub mod atlas;
pub mod generator;
pub mod glyph;
pub mod layout_bridge;
pub mod renderer;

pub use atlas::MsdfAtlas;
pub use generator::MsdfGenerator;
pub use glyph::{FontId, PositionedGlyph};
pub use layout_bridge::collect_msdf_glyphs;
// MsdfBlockRenderer is used directly in the render world via crate::text::msdf::renderer

use bevy::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;

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
    pub version: u64,
    pub rainbow: bool,
}

/// Which renderer draws a block's base content into its texture (before the
/// `BlockFxMaterial` shader post-processes border/glow/label decoration on
/// top — that part is never Vello, regardless of this enum).
#[derive(Component, Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockRenderMethod {
    /// Vello rasterizes the block's content (SVG, ABC notation only).
    Vello,
    /// MSDF renders text glyphs (or nothing, for a block whose content is
    /// plain UI geometry — sparkline, image placeholder — spawned as
    /// sibling child entities instead of texture content).
    #[default]
    Msdf,
}
