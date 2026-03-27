//! MSDF glyph atlas management.
//!
//! The atlas stores pre-generated MSDF textures for glyphs, enabling efficient
//! GPU text rendering with smooth scaling at any zoom level.

use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use rect_packer::{Config as PackerConfig, Packer};
use std::collections::HashMap;

use super::glyph::GlyphKey;

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
    /// Used to align glyph quads with Parley pen positions.
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
    /// Monotonic version counter bumped when new glyphs are inserted.
    pub version: u64,
}

impl MsdfAtlas {
    /// Default MSDF range (distance field extent in pixels).
    pub const DEFAULT_RANGE: f32 = 4.0;
    /// Create a new atlas with the given initial dimensions.
    pub fn new(images: &mut Assets<Image>, width: u32, height: u32) -> Self {
        let pixels = vec![0u8; (width * height * 4) as usize];

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
        let padding = (Self::DEFAULT_RANGE as i32) + 2;
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
            version: 1,
        }
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
        let rect = self.packer.pack(width as i32, height as i32, false)?;

        let region = AtlasRegion {
            x: rect.x as u32,
            y: rect.y as u32,
            width,
            height,
            anchor_x,
            anchor_y,
        };

        self.copy_pixels(region.x, region.y, width, height, data);
        self.regions.insert(key, region);
        self.dirty = true;
        self.pending.retain(|k| *k != key);
        self.version = self.version.wrapping_add(1);

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

    /// Insert a placeholder glyph (used for spaces or failed generation).
    pub fn insert_placeholder(&mut self, key: GlyphKey) -> Option<AtlasRegion> {
        const PLACEHOLDER_SIZE: u32 = 4;
        let data = vec![0u8; (PLACEHOLDER_SIZE * PLACEHOLDER_SIZE * 4) as usize];
        self.insert(key, PLACEHOLDER_SIZE, PLACEHOLDER_SIZE, 0.0, 0.0, &data)
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
            x: 0,
            y: 0,
            width: 32,
            height: 32,
            anchor_x: 0.0,
            anchor_y: 0.0,
        };
        let b = AtlasRegion {
            x: 16,
            y: 16,
            width: 32,
            height: 32,
            anchor_x: 0.0,
            anchor_y: 0.0,
        };
        let c = AtlasRegion {
            x: 64,
            y: 64,
            width: 32,
            height: 32,
            anchor_x: 0.0,
            anchor_y: 0.0,
        };

        assert!(a.overlaps(&b), "a and b should overlap");
        assert!(!a.overlaps(&c), "a and c should not overlap");
    }
}
