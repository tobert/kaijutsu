//! MSDF glyph atlas management.
//!
//! The atlas stores pre-generated MSDF textures for glyphs, enabling efficient
//! GPU text rendering with smooth scaling at any zoom level.

use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use cosmic_text::fontdb::ID as FontId;
use rect_packer::{Config as PackerConfig, Packer};
use std::collections::HashMap;

/// Key for looking up glyphs in the atlas.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub struct GlyphKey {
    /// Font ID from cosmic-text's font database.
    pub font_id: FontId,
    /// Glyph ID within the font.
    pub glyph_id: u16,
}

impl GlyphKey {
    pub fn new(font_id: FontId, glyph_id: u16) -> Self {
        Self { font_id, glyph_id }
    }
}

/// Region within the atlas texture where a glyph is stored.
#[derive(Clone, Copy, Debug)]
pub struct AtlasRegion {
    /// Position in the atlas (top-left corner).
    pub x: u32,
    pub y: u32,
    /// Size of the glyph in the atlas.
    pub width: u32,
    pub height: u32,
    /// Anchor point: offset from bitmap origin to glyph origin in em units.
    /// Used by the pipeline to align glyph quads with cosmic-text pen positions.
    pub anchor_x: f32,
    pub anchor_y: f32,
}

impl AtlasRegion {
    /// Get UV coordinates for this region given the atlas dimensions.
    pub fn uv_rect(&self, atlas_width: u32, atlas_height: u32) -> [f32; 4] {
        let u0 = self.x as f32 / atlas_width as f32;
        let v0 = self.y as f32 / atlas_height as f32;
        let u1 = (self.x + self.width) as f32 / atlas_width as f32;
        let v1 = (self.y + self.height) as f32 / atlas_height as f32;
        [u0, v0, u1, v1]
    }

    /// Check if this region overlaps with another (for testing).
    #[cfg(test)]
    pub fn overlaps(&self, other: &AtlasRegion) -> bool {
        !(self.x + self.width <= other.x
            || other.x + other.width <= self.x
            || self.y + self.height <= other.y
            || other.y + other.height <= self.y)
    }
}

/// MSDF glyph atlas resource.
///
/// Manages a texture atlas containing MSDF representations of glyphs.
/// Glyphs are generated on-demand and packed into the atlas using a
/// rectangle packing algorithm.
#[derive(Resource)]
pub struct MsdfAtlas {
    /// The GPU texture handle.
    pub texture: Handle<Image>,
    /// Current texture dimensions.
    pub width: u32,
    pub height: u32,
    /// Mapping from glyph keys to their atlas regions.
    pub regions: HashMap<GlyphKey, AtlasRegion>,
    /// Rectangle packer for efficient space utilization.
    packer: Packer,
    /// Glyphs that are pending generation.
    pub pending: Vec<GlyphKey>,
    /// Whether the texture needs to be re-uploaded to GPU.
    pub dirty: bool,
    /// Raw pixel data (RGBA).
    pub pixels: Vec<u8>,
    /// MSDF range in pixels (distance field extent).
    pub msdf_range: f32,
}

#[allow(dead_code)]
impl MsdfAtlas {
    /// Default MSDF range (distance field extent in pixels).
    /// Must match MsdfGenerator::new() msdf_range for correct shader AA math.
    pub const DEFAULT_RANGE: f32 = 6.0;
    /// Default pixels per em for MSDF generation.
    pub const DEFAULT_PX_PER_EM: f64 = 64.0;

    /// Create a new atlas with the given initial dimensions.
    pub fn new(images: &mut Assets<Image>, width: u32, height: u32) -> Self {
        let pixels = vec![0u8; (width * height * 4) as usize];

        // Create the Bevy image
        let image = Image::new(
            Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            pixels.clone(),
            TextureFormat::Rgba8Unorm,
            default(),
        );

        let texture = images.add(image);

        // Padding must be >= MSDF range to prevent atlas bleeding when sampling
        // with bilinear filtering near glyph edges
        let padding = (Self::DEFAULT_RANGE as i32) + 2; // 4 + 2 = 6 pixels
        let packer_config = PackerConfig {
            width: width as i32,
            height: height as i32,
            border_padding: padding,
            rectangle_padding: padding,
        };

        Self {
            texture,
            width,
            height,
            regions: HashMap::new(),
            packer: Packer::new(packer_config),
            pending: Vec::new(),
            dirty: false,
            pixels,
            msdf_range: Self::DEFAULT_RANGE,
        }
    }

    /// Get the atlas region for a glyph, or None if not present.
    pub fn get(&self, key: GlyphKey) -> Option<&AtlasRegion> {
        self.regions.get(&key)
    }

    /// Check if a glyph is present in the atlas.
    pub fn contains(&self, key: GlyphKey) -> bool {
        self.regions.contains_key(&key)
    }

    /// Queue a glyph for generation if not already present.
    pub fn request(&mut self, key: GlyphKey) {
        if !self.contains(key) && !self.pending.contains(&key) {
            self.pending.push(key);
        }
    }

    /// Insert a generated glyph into the atlas.
    ///
    /// Returns the region if successful, or None if the atlas is full.
    pub fn insert(
        &mut self,
        key: GlyphKey,
        width: u32,
        height: u32,
        anchor_x: f32,
        anchor_y: f32,
        data: &[u8],
    ) -> Option<AtlasRegion> {
        // Try to pack the glyph
        let rect = self.packer.pack(width as i32, height as i32, false)?;

        let region = AtlasRegion {
            x: rect.x as u32,
            y: rect.y as u32,
            width,
            height,
            anchor_x,
            anchor_y,
        };

        // Copy pixel data into the atlas
        self.copy_pixels(region.x, region.y, width, height, data);

        // Store the region
        self.regions.insert(key, region);
        self.dirty = true;

        // Remove from pending
        self.pending.retain(|k| *k != key);

        Some(region)
    }

    /// Copy pixel data into the atlas at the given position.
    fn copy_pixels(&mut self, x: u32, y: u32, width: u32, height: u32, data: &[u8]) {
        for row in 0..height {
            let src_offset = (row * width * 4) as usize;
            let dst_offset = ((y + row) * self.width * 4 + x * 4) as usize;
            let src_end = src_offset + (width * 4) as usize;
            let dst_end = dst_offset + (width * 4) as usize;

            if src_end <= data.len() && dst_end <= self.pixels.len() {
                self.pixels[dst_offset..dst_end].copy_from_slice(&data[src_offset..src_end]);
            }
        }
    }

    /// Insert a placeholder glyph (used when generation fails or for unknown glyphs like space).
    ///
    /// For MSDF, we need all channels to be 0 (outside the glyph) so it renders as invisible.
    pub fn insert_placeholder(&mut self, key: GlyphKey) -> Option<AtlasRegion> {
        const PLACEHOLDER_SIZE: u32 = 4; // Small since it's invisible anyway

        // All zeros = sd of 0 = fully outside glyph = invisible
        let data = vec![0u8; (PLACEHOLDER_SIZE * PLACEHOLDER_SIZE * 4) as usize];

        self.insert(key, PLACEHOLDER_SIZE, PLACEHOLDER_SIZE, 0.0, 0.0, &data)
    }

    /// Grow the atlas to accommodate more glyphs.
    ///
    /// Returns true if growth was successful.
    pub fn grow(&mut self, images: &mut Assets<Image>) -> bool {
        let new_width = self.width * 2;
        let new_height = self.height * 2;

        if new_width > 8192 || new_height > 8192 {
            warn!("MSDF atlas cannot grow beyond 8192x8192");
            return false;
        }

        info!("Growing MSDF atlas from {}x{} to {}x{}", self.width, self.height, new_width, new_height);

        // Create new pixel buffer
        let mut new_pixels = vec![0u8; (new_width * new_height * 4) as usize];

        // Copy old pixels
        for y in 0..self.height {
            let src_offset = (y * self.width * 4) as usize;
            let dst_offset = (y * new_width * 4) as usize;
            let row_size = (self.width * 4) as usize;
            new_pixels[dst_offset..dst_offset + row_size]
                .copy_from_slice(&self.pixels[src_offset..src_offset + row_size]);
        }

        self.pixels = new_pixels;
        self.width = new_width;
        self.height = new_height;

        // Recreate packer with new dimensions — use same padding as new()
        let padding = (Self::DEFAULT_RANGE as i32) + 2;
        let packer_config = PackerConfig {
            width: new_width as i32,
            height: new_height as i32,
            border_padding: padding,
            rectangle_padding: padding,
        };
        self.packer = Packer::new(packer_config);

        // Re-pack existing regions by marking their space as used
        // Note: The old pixel data is preserved, we just need to update the packer
        for region in self.regions.values() {
            let _ = self.packer.pack(region.width as i32, region.height as i32, false);
        }

        // Update the texture
        let image = Image::new(
            Extent3d {
                width: new_width,
                height: new_height,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            self.pixels.clone(),
            TextureFormat::Rgba8Unorm,
            default(),
        );

        if let Some(img) = images.get_mut(&self.texture) {
            *img = image;
        }

        self.dirty = true;
        true
    }

    /// Sync the CPU pixel data to the GPU texture.
    pub fn sync_to_gpu(&mut self, images: &mut Assets<Image>) {
        if !self.dirty {
            return;
        }

        if let Some(image) = images.get_mut(&self.texture) {
            image.data = Some(self.pixels.clone());
        }

        self.dirty = false;
    }

    /// Get current atlas dimensions.
    pub fn texture_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Get an iterator over all regions (for testing).
    #[cfg(test)]
    pub fn regions_iter(&self) -> impl Iterator<Item = &AtlasRegion> {
        self.regions.values()
    }

    /// Dump the atlas texture to a raw RGBA file for debugging.
    ///
    /// The output can be converted to PNG using imagemagick:
    /// ```bash
    /// convert -size 1024x1024 -depth 8 rgba:/tmp/msdf_atlas.raw /tmp/msdf_atlas.png
    /// ```
    ///
    /// # Arguments
    /// * `path` - Path to write the raw RGBA data
    ///
    /// # Returns
    /// The atlas dimensions as (width, height) for the convert command
    #[cfg(debug_assertions)]
    pub fn dump_to_file(&self, path: &std::path::Path) -> std::io::Result<(u32, u32)> {
        use std::io::Write;
        let mut file = std::fs::File::create(path)?;
        file.write_all(&self.pixels)?;
        info!(
            "Dumped MSDF atlas ({}x{}, {} glyphs) to {:?}",
            self.width,
            self.height,
            self.regions.len(),
            path
        );
        info!(
            "Convert with: convert -size {}x{} -depth 8 rgba:{} {}.png",
            self.width,
            self.height,
            path.display(),
            path.display()
        );
        Ok((self.width, self.height))
    }

    /// Get debug statistics about the atlas.
    #[cfg(debug_assertions)]
    pub fn debug_stats(&self) -> AtlasDebugStats {
        let total_pixels = (self.width * self.height) as usize;
        let used_pixels: usize = self.regions.values()
            .map(|r| (r.width * r.height) as usize)
            .sum();
        let utilization = used_pixels as f32 / total_pixels as f32 * 100.0;

        AtlasDebugStats {
            width: self.width,
            height: self.height,
            glyph_count: self.regions.len(),
            pending_count: self.pending.len(),
            used_pixels,
            utilization,
        }
    }
}

/// Debug statistics for the atlas.
#[cfg(debug_assertions)]
#[derive(Debug)]
#[allow(dead_code)] // Fields are read via Debug formatting
pub struct AtlasDebugStats {
    pub width: u32,
    pub height: u32,
    pub glyph_count: usize,
    pub pending_count: usize,
    pub used_pixels: usize,
    pub utilization: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atlas_region_uv_calculation() {
        let region = AtlasRegion {
            x: 64,
            y: 128,
            width: 32,
            height: 32,
            anchor_x: 0.0,
            anchor_y: 0.0,
        };

        let [u0, v0, u1, v1] = region.uv_rect(512, 512);
        assert!((u0 - 0.125).abs() < 0.001);
        assert!((v0 - 0.25).abs() < 0.001);
        assert!((u1 - 0.1875).abs() < 0.001);
        assert!((v1 - 0.3125).abs() < 0.001);
    }

    #[test]
    fn atlas_region_overlap_detection() {
        let a = AtlasRegion {
            x: 0, y: 0, width: 32, height: 32,
            anchor_x: 0.0, anchor_y: 0.0,
        };
        let b = AtlasRegion {
            x: 16, y: 16, width: 32, height: 32,
            anchor_x: 0.0, anchor_y: 0.0,
        };
        let c = AtlasRegion {
            x: 64, y: 64, width: 32, height: 32,
            anchor_x: 0.0, anchor_y: 0.0,
        };

        assert!(a.overlaps(&b), "a and b should overlap");
        assert!(!a.overlaps(&c), "a and c should not overlap");
    }
}
