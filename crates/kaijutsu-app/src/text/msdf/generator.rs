//! MSDF glyph generation using msdfgen.
//!
//! Generates multi-channel signed distance field textures from font glyphs.

use bevy::prelude::*;
use bevy::tasks::{block_on, AsyncComputeTaskPool, Task};
use cosmic_text::fontdb::ID as FontId;
use cosmic_text::FontSystem;
use msdfgen::{Bitmap, FillRule, FontExt, MsdfGeneratorConfig, Range, Rgba};
use owned_ttf_parser::{Face, GlyphId};
use std::sync::Arc;

use super::atlas::{GlyphKey, MsdfAtlas};

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
#[derive(Resource, Default)]
pub struct MsdfGenerator {
    /// Pending generation tasks.
    tasks: Vec<Task<GeneratedGlyph>>,
    /// MSDF range in pixels.
    pub msdf_range: f64,
    /// Pixels per em for generation.
    pub px_per_em: f64,
    /// Angle threshold for edge coloring.
    pub angle_threshold: f64,
}

#[allow(dead_code)]
impl MsdfGenerator {
    /// Create a new generator with default settings.
    ///
    /// MSDF range of 4.0 at 32px/em gives ~1.9px antialiasing at 15px font size,
    /// which provides smooth edges without excessive bleed between characters.
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            msdf_range: 4.0,  // Reduced from 8.0 to minimize inter-character bleed
            px_per_em: 32.0,
            angle_threshold: 3.0,
        }
    }

    /// Create a new generator with custom settings.
    pub fn with_settings(msdf_range: f64, px_per_em: f64) -> Self {
        Self {
            tasks: Vec::new(),
            msdf_range,
            px_per_em,
            angle_threshold: 3.0,
        }
    }

    /// Queue glyph generation for pending glyphs in the atlas.
    pub fn queue_pending(&mut self, atlas: &MsdfAtlas, font_system: &FontSystem) {
        let pool = AsyncComputeTaskPool::get();

        for &key in &atlas.pending {
            // Get font data from cosmic-text's font database
            let Some(font_data) = get_font_data_vec(font_system, key.font_id) else {
                warn!("Font data not found for font_id {:?}", key.font_id);
                continue;
            };

            let msdf_range = self.msdf_range;
            let px_per_em = self.px_per_em;
            let angle_threshold = self.angle_threshold;
            let glyph_id = key.glyph_id;

            // Clone the Vec for the async task
            let font_data = Arc::new(font_data);

            let task = pool.spawn(async move {
                generate_glyph(
                    key,
                    &font_data,
                    glyph_id,
                    msdf_range,
                    px_per_em,
                    angle_threshold,
                )
            });

            self.tasks.push(task);
        }
    }

    /// Poll for completed generation tasks and insert into atlas.
    pub fn poll_completed(&mut self, atlas: &mut MsdfAtlas) {
        self.tasks.retain_mut(|task| {
            if task.is_finished() {
                // Use block_on to get the result from the finished task
                let glyph = block_on(async { task.await });
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
                false // Remove completed task
            } else {
                true // Keep pending task
            }
        });
    }

    /// Pre-generate common ASCII glyphs for a font.
    pub fn pregenerate_ascii(&mut self, font_system: &FontSystem, font_id: FontId, atlas: &mut MsdfAtlas) {
        // Queue printable ASCII characters
        for c in 32u8..=126u8 {
            let c = c as char;
            if let Some(glyph_id) = get_glyph_id(font_system, font_id, c) {
                let key = GlyphKey::new(font_id, glyph_id);
                atlas.request(key);
            }
        }

        // Queue generation for all pending
        self.queue_pending(atlas, font_system);
    }

    /// Check if there are pending tasks.
    pub fn has_pending(&self) -> bool {
        !self.tasks.is_empty()
    }
}

/// Get font data bytes from cosmic-text's font system as a Vec.
fn get_font_data_vec(font_system: &FontSystem, font_id: FontId) -> Option<Vec<u8>> {
    let db = font_system.db();
    let face_info = db.face(font_id)?;

    // cosmic-text's fontdb stores font data
    match &face_info.source {
        cosmic_text::fontdb::Source::Binary(data) => {
            // Convert Arc<dyn AsRef<[u8]>> to Vec<u8>
            Some((**data).as_ref().to_vec())
        }
        cosmic_text::fontdb::Source::File(path) => {
            std::fs::read(path).ok()
        }
        cosmic_text::fontdb::Source::SharedFile(path, _) => {
            std::fs::read(path).ok()
        }
    }
}

/// Get glyph ID for a character in a font.
#[allow(dead_code)]
fn get_glyph_id(font_system: &FontSystem, font_id: FontId, c: char) -> Option<u16> {
    let db = font_system.db();
    let face_info = db.face(font_id)?;

    // Load the font face to query glyph mapping
    let font_data = get_font_data_vec(font_system, font_id)?;
    let face = Face::parse(&font_data, face_info.index).ok()?;
    face.glyph_index(c).map(|id| id.0)
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
    // Parse the font
    let Ok(face) = Face::parse(font_data, 0) else {
        return placeholder_glyph(key);
    };

    // Get the glyph shape using msdfgen's FontExt trait
    let Some(mut shape) = face.glyph_shape(GlyphId(glyph_id)) else {
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
    //
    // The anchor represents where the glyph origin (0, 0) ends up in the bitmap,
    // expressed in em units (so it scales with font_size when rendering).
    //
    // COORDINATE SYSTEMS:
    // - Font units: origin at baseline/pen position, Y increases UP
    // - msdfgen bitmap: Y=0 at bottom, Y increases UP
    // - Screen/pipeline: Y=0 at top, Y increases DOWN
    //
    // For small glyphs that get expanded to minimum size (16x16), autoframe
    // centers the content. We need to account for the actual padding, not
    // assume fixed msdf_range padding.
    let content_width = bounds.width() * px_per_unit;
    let content_height = bounds.height() * px_per_unit;
    let actual_padding_x = (width as f64 - content_width) / 2.0;
    let actual_padding_y = (height as f64 - content_height) / 2.0;

    // Origin position: padding + offset from bounds edge to origin
    // bounds.left is where the shape starts relative to origin (can be negative)
    // So origin is at: padding + (-bounds.left * px_per_unit)
    let origin_bitmap_x = actual_padding_x - bounds.left * px_per_unit;
    let origin_from_bottom = actual_padding_y - bounds.bottom * px_per_unit;
    let origin_from_top = height as f64 - origin_from_bottom;

    // Convert from bitmap pixels to em units
    let anchor_x = origin_bitmap_x as f32 / px_per_em as f32;
    let anchor_y = origin_from_top as f32 / px_per_em as f32;

    // Debug logging for glyph generation
    trace!(
        "MSDF gen glyph_id={}: bounds=({:.1},{:.1})→({:.1},{:.1}), units_per_em={}, \
         bitmap={}x{}, origin_from_top={:.1}, anchor=({:.4}, {:.4}) em",
        glyph_id,
        bounds.left,
        bounds.bottom,
        bounds.left + bounds.width(),
        bounds.bottom + bounds.height(),
        units_per_em,
        width,
        height,
        origin_from_top,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_is_valid() {
        let key = GlyphKey::new(FontId::dummy(), 0);
        let glyph = placeholder_glyph(key);
        assert!(glyph.is_placeholder);
    }
}
