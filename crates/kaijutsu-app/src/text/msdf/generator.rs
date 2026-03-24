//! MSDF glyph generation using msdfgen.
//!
//! Generates multi-channel signed distance field textures from font glyphs.
//! Font data comes from `peniko::Font.data` (via Parley glyph runs).

use bevy::prelude::*;
use bevy::tasks::{block_on, AsyncComputeTaskPool, Task};
use msdfgen::{Bitmap, FillRule, FontExt, MsdfGeneratorConfig, Range, Rgba};
use std::collections::HashSet;

use super::atlas::MsdfAtlas;
use super::glyph::GlyphKey;

/// Result of generating an MSDF glyph.
pub struct GeneratedGlyph {
    pub key: GlyphKey,
    pub width: u32,
    pub height: u32,
    pub anchor_x: f32,
    pub anchor_y: f32,
    pub data: Vec<u8>,
    pub is_placeholder: bool,
}

/// MSDF generator resource.
///
/// Handles async generation of MSDF glyphs from font data.
/// Font bytes are provided directly from `peniko::Font.data`.
#[derive(Resource)]
pub struct MsdfGenerator {
    /// Pending generation tasks.
    tasks: Vec<Task<GeneratedGlyph>>,
    /// Glyphs with in-flight async tasks (prevents duplicate spawns).
    queued: HashSet<GlyphKey>,
    /// MSDF range in pixels.
    pub msdf_range: f64,
    /// Pixels per em for generation.
    pub px_per_em: f64,
    /// Angle threshold for edge coloring.
    pub angle_threshold: f64,
}

impl Default for MsdfGenerator {
    fn default() -> Self {
        Self::new()
    }
}

impl MsdfGenerator {
    /// Create a new generator with default settings.
    ///
    /// MSDF range of 4.0 at 64px/em gives ~4px effective AA at 16px font size.
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            queued: HashSet::new(),
            msdf_range: 4.0,
            px_per_em: 64.0,
            angle_threshold: 3.0,
        }
    }

    /// Queue glyph generation for pending glyphs in the atlas.
    ///
    /// `font_data_map` provides raw font bytes keyed by FontId.
    /// Skips glyphs that already have in-flight tasks.
    pub fn queue_pending(
        &mut self,
        atlas: &MsdfAtlas,
        font_data_map: &super::FontDataMap,
    ) {
        let pool = AsyncComputeTaskPool::get();

        for &key in &atlas.pending {
            if self.queued.contains(&key) {
                continue;
            }

            let Some(font_data) = font_data_map.get(&key.font_id) else {
                warn!("Font data not found for font_id {:?}", key.font_id);
                continue;
            };

            let msdf_range = self.msdf_range;
            let px_per_em = self.px_per_em;
            let angle_threshold = self.angle_threshold;
            let glyph_id = key.glyph_id;
            let font_data = font_data.clone();

            let task = pool.spawn(async move {
                generate_glyph(key, &font_data, glyph_id, msdf_range, px_per_em, angle_threshold)
            });

            self.queued.insert(key);
            self.tasks.push(task);
        }
    }

    /// Poll for completed generation tasks and insert into atlas.
    pub fn poll_completed(&mut self, atlas: &mut MsdfAtlas) {
        let mut completed = Vec::new();
        self.tasks.retain_mut(|task| {
            if task.is_finished() {
                completed.push(block_on(async { task.await }));
                false
            } else {
                true
            }
        });

        for glyph in completed {
            self.queued.remove(&glyph.key);
            if glyph.is_placeholder {
                atlas.insert_placeholder(glyph.key);
            } else {
                atlas.insert(
                    glyph.key,
                    glyph.width,
                    glyph.height,
                    glyph.anchor_x,
                    glyph.anchor_y,
                    &glyph.data,
                );
            }
        }
    }

    /// Check if there are pending tasks.
    pub fn has_pending(&self) -> bool {
        !self.tasks.is_empty()
    }
}

/// Generate MSDF for a single glyph.
fn generate_glyph(
    key: GlyphKey,
    font_data: &[u8],
    glyph_id: u16,
    msdf_range: f64,
    px_per_em: f64,
    angle_threshold: f64,
) -> GeneratedGlyph {
    // Parse the font using ttf-parser (version must match msdfgen's)
    let Ok(face) = ttf_parser::Face::parse(font_data, 0) else {
        return placeholder_glyph(key);
    };

    // Get the glyph shape using msdfgen's FontExt trait (impl'd for ttf_parser::Face)
    let glyph_ttf_id = ttf_parser::GlyphId(glyph_id);
    let Some(mut shape) = face.glyph_shape(glyph_ttf_id) else {
        return placeholder_glyph(key);
    };

    // Color the edges for MSDF
    shape.edge_coloring_simple(angle_threshold, 0);

    // Calculate dimensions
    let bounds = shape.get_bound();
    let units_per_em = face.units_per_em() as f64;
    let px_per_unit = px_per_em / units_per_em;

    // Minimum size with padding for MSDF range
    let padding = (msdf_range * 2.0).ceil() as u32;
    let width = ((bounds.width() * px_per_unit).ceil() as u32 + padding).max(16);
    let height = ((bounds.height() * px_per_unit).ceil() as u32 + padding).max(16);

    // Calculate framing for the glyph
    let range = Range::Px(msdf_range);
    let Some(framing) = bounds.autoframe(width, height, range, None) else {
        return placeholder_glyph(key);
    };

    // Generate the MSDF
    let config = MsdfGeneratorConfig::default();
    let mut bitmap = Bitmap::<Rgba<f32>>::new(width, height);
    shape.generate_mtsdf(&mut bitmap, &framing, config);
    shape.correct_sign(&mut bitmap, &framing, FillRule::default());
    shape.correct_msdf_error(&mut bitmap, &framing, config);

    // Convert to RGBA8
    let data: Vec<u8> = bitmap
        .pixels()
        .iter()
        .flat_map(|p| {
            [
                (p.r.clamp(0.0, 1.0) * 255.0) as u8,
                (p.g.clamp(0.0, 1.0) * 255.0) as u8,
                (p.b.clamp(0.0, 1.0) * 255.0) as u8,
                (p.a.clamp(0.0, 1.0) * 255.0) as u8,
            ]
        })
        .collect();

    // Calculate anchor (offset for positioning)
    let content_width = bounds.width() * px_per_unit;
    let content_height = bounds.height() * px_per_unit;
    let actual_padding_x = (width as f64 - content_width) / 2.0;
    let actual_padding_y = (height as f64 - content_height) / 2.0;

    let origin_bitmap_x = actual_padding_x - bounds.left * px_per_unit;
    let origin_from_bottom = actual_padding_y - bounds.bottom * px_per_unit;
    let origin_from_top = height as f64 - origin_from_bottom;

    let anchor_x = origin_bitmap_x as f32 / px_per_em as f32;
    let anchor_y = origin_from_top as f32 / px_per_em as f32;

    trace!(
        "MSDF gen glyph_id={}: bitmap={}x{}, anchor=({:.4}, {:.4}) em",
        glyph_id,
        width,
        height,
        anchor_x,
        anchor_y
    );

    GeneratedGlyph {
        key,
        width,
        height,
        anchor_x,
        anchor_y,
        data,
        is_placeholder: false,
    }
}

/// Create a placeholder glyph for failed generation.
fn placeholder_glyph(key: GlyphKey) -> GeneratedGlyph {
    GeneratedGlyph {
        key,
        width: 0,
        height: 0,
        anchor_x: 0.0,
        anchor_y: 0.0,
        data: Vec::new(),
        is_placeholder: true,
    }
}
