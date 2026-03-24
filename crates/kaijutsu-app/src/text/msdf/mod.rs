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
//! Vello continues to handle SVG, sparkline, ABC, and border rendering.
//! MSDF replaces Vello only for text content (PlainText, Markdown, Output).

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
    pub fn register(&mut self, font: &bevy_vello::parley::FontData) {
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

/// Which renderer handles a block's text content.
#[derive(Component, Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockRenderMethod {
    /// Vello rasterizes the entire scene (SVG, sparkline, ABC, borders).
    Vello,
    /// MSDF renders text glyphs; Vello renders borders only.
    #[default]
    Msdf,
}
