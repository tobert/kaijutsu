//! MSDF glyph atlas management.
//!
//! The atlas stores pre-generated MSDF textures for glyphs, enabling efficient
//! GPU text rendering with smooth scaling at any zoom level.

use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use rect_packer::{Config as PackerConfig, Packer};
use std::collections::{HashMap, HashSet};

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
    /// Glyphs that permanently failed to pack (even after growing to the cap)
    /// or whose font data never arrived. Terminal: `request()` will not
    /// re-queue them, so a doomed glyph costs one loud log line, not an
    /// infinite per-frame respawn loop.
    pub failed: HashSet<GlyphKey>,
    /// Whether the texture needs to be re-uploaded to GPU.
    pub dirty: bool,
    /// Raw pixel data (RGBA).
    pub pixels: Vec<u8>,
    /// MSDF range in pixels (distance field extent).
    pub msdf_range: f32,
    /// Monotonic version counter bumped when new glyphs are inserted OR when
    /// the atlas grows (growth repacks in place, so every existing region's
    /// UVs change even though the glyph set doesn't).
    pub version: u64,
    /// Bumped every time `grow()` successfully repacks the atlas. Existing
    /// per-block glyph versions don't change on growth (region *count* is the
    /// same), so callers that only watch `regions.len()` for "did the atlas
    /// change" miss a grow — they must also watch this counter to force the
    /// same forced-re-render path (region positions moved, UVs are stale).
    pub growth_epoch: u64,
    /// Growth cap in pixels (both dimensions). Doubling stops here.
    max_dim: u32,
}

impl MsdfAtlas {
    /// Default MSDF range (distance field extent in pixels).
    pub const DEFAULT_RANGE: f32 = 4.0;
    /// Hard cap on atlas dimensions. A glyph that still won't fit at this
    /// size is marked permanently failed rather than growing forever.
    pub const MAX_DIM: u32 = 4096;

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
        let packer = Packer::new(Self::packer_config(width, height));

        Self {
            texture,
            width,
            height,
            regions: HashMap::new(),
            packer,
            pending: Vec::new(),
            failed: HashSet::new(),
            dirty: false,
            pixels,
            msdf_range: Self::DEFAULT_RANGE,
            version: 1,
            growth_epoch: 0,
            max_dim: Self::MAX_DIM,
        }
    }

    /// Override the growth cap. Test-only: production always uses `MAX_DIM`;
    /// tests use a small cap to exercise the growth/terminal-fail boundary
    /// without allocating multi-megabyte pixel buffers.
    #[cfg(test)]
    pub(crate) fn with_max_dim(mut self, max_dim: u32) -> Self {
        self.max_dim = max_dim;
        self
    }

    fn packer_config(width: u32, height: u32) -> PackerConfig {
        // Padding must be >= MSDF range to prevent atlas bleeding when sampling
        // with bilinear filtering near glyph edges.
        let padding = (Self::DEFAULT_RANGE as i32) + 2;
        PackerConfig {
            width: width as i32,
            height: height as i32,
            border_padding: padding,
            rectangle_padding: padding,
        }
    }

    /// Check if a glyph is present in the atlas.
    pub fn contains(&self, key: GlyphKey) -> bool {
        self.regions.contains_key(&key)
    }

    /// Queue a glyph for generation if not already present, pending, or
    /// permanently failed. `failed` is terminal — a doomed glyph is not
    /// retried.
    pub fn request(&mut self, key: GlyphKey) {
        if !self.contains(key) && !self.pending.contains(&key) && !self.failed.contains(&key) {
            self.pending.push(key);
        }
    }

    /// Terminally fail a glyph outside the pack path — e.g. its font data
    /// never registered within the generator's retry budget. Removes it
    /// from `pending` and adds it to `failed` so `request()` never re-queues
    /// it. Callers are responsible for their own loud logging (the reason
    /// for the failure lives with the caller, not the atlas).
    pub fn mark_failed(&mut self, key: GlyphKey) {
        self.pending.retain(|k| *k != key);
        self.failed.insert(key);
    }

    /// Insert a generated glyph into the atlas.
    ///
    /// On pack failure, grows the atlas (doubling, capped at `max_dim`) and
    /// retries until it fits or growth is exhausted. If the glyph still
    /// doesn't fit at the cap, it is moved into `failed` (loud, terminal —
    /// see `fail_permanently`) instead of being returned to `pending`, so the
    /// caller never respawns a generation task for it again.
    pub fn insert(
        &mut self,
        key: GlyphKey,
        width: u32,
        height: u32,
        anchor_x: f32,
        anchor_y: f32,
        data: &[u8],
        images: &mut Assets<Image>,
    ) -> Option<AtlasRegion> {
        if let Some(region) = self.try_pack_and_place(key, width, height, anchor_x, anchor_y, data)
        {
            return Some(region);
        }

        while self.grow(images) {
            if let Some(region) =
                self.try_pack_and_place(key, width, height, anchor_x, anchor_y, data)
            {
                return Some(region);
            }
        }

        self.fail_permanently(key, width, height);
        None
    }

    /// Attempt to pack+place a glyph at the atlas's current size. Returns
    /// `None` without side effects (beyond the packer's own internal state)
    /// if it doesn't fit — callers decide whether to grow or give up.
    fn try_pack_and_place(
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

    /// Double the atlas dimensions (capped at `max_dim`), re-packing every
    /// existing region into the larger space and copying its pixels across.
    /// Returns `false` if the atlas is already at (or beyond) the cap in
    /// both dimensions — the caller must treat that as "cannot grow
    /// further."
    fn grow(&mut self, images: &mut Assets<Image>) -> bool {
        if self.width >= self.max_dim && self.height >= self.max_dim {
            return false;
        }

        let old_width = self.width;
        let old_height = self.height;
        let new_width = (old_width * 2).min(self.max_dim);
        let new_height = (old_height * 2).min(self.max_dim);

        let mut new_packer = Packer::new(Self::packer_config(new_width, new_height));

        // Largest-first repack for a dense, stable layout.
        let mut entries: Vec<(GlyphKey, AtlasRegion)> =
            self.regions.iter().map(|(k, v)| (*k, *v)).collect();
        entries.sort_by_key(|(_, r)| std::cmp::Reverse(r.width as u64 * r.height as u64));

        let mut new_regions = HashMap::with_capacity(entries.len());
        let mut new_pixels = vec![0u8; (new_width as usize) * (new_height as usize) * 4];

        for (key, old_region) in &entries {
            let Some(rect) =
                new_packer.pack(old_region.width as i32, old_region.height as i32, false)
            else {
                // The new atlas is strictly larger than the old one and only needs
                // to hold the same rects that already fit once — with largest-first
                // insertion this should always succeed, though `rect_packer`'s shelf
                // algorithm isn't a guaranteed-optimal packer, so it isn't a proof.
                // Fail loud and bail rather than commit a partial repack either way.
                error!(
                    "MSDF atlas grow: repack failed for glyph {:?} ({}x{}) while growing \
                     {}x{} -> {}x{}; aborting growth, atlas stays at current size",
                    key, old_region.width, old_region.height, old_width, old_height, new_width,
                    new_height,
                );
                return false;
            };

            let new_region = AtlasRegion {
                x: rect.x as u32,
                y: rect.y as u32,
                width: old_region.width,
                height: old_region.height,
                anchor_x: old_region.anchor_x,
                anchor_y: old_region.anchor_y,
            };

            copy_rect_between_buffers(
                &self.pixels,
                old_width,
                old_region.x,
                old_region.y,
                &mut new_pixels,
                new_width,
                new_region.x,
                new_region.y,
                old_region.width,
                old_region.height,
            );

            new_regions.insert(*key, new_region);
        }

        self.packer = new_packer;
        self.pixels = new_pixels;
        self.width = new_width;
        self.height = new_height;
        self.regions = new_regions;
        self.dirty = true;
        self.version = self.version.wrapping_add(1);
        self.growth_epoch = self.growth_epoch.wrapping_add(1);

        if let Some(image) = images.get_mut(&self.texture) {
            image.resize(Extent3d {
                width: new_width,
                height: new_height,
                depth_or_array_layers: 1,
            });
        }

        info!(
            "MSDF atlas grew {}x{} -> {}x{} ({} regions repacked)",
            old_width,
            old_height,
            new_width,
            new_height,
            entries.len(),
        );

        true
    }

    /// Terminal failure path: a glyph that will never fit (even at the
    /// growth cap). Logs loud once, then moves the key out of `pending` and
    /// into `failed` so nothing ever respawns a generation task for it
    /// again. The block that wanted this glyph keeps rendering without it.
    fn fail_permanently(&mut self, key: GlyphKey, width: u32, height: u32) {
        error!(
            "MSDF atlas: glyph {:?} ({}x{}) does not fit — atlas {}x{} full even at the \
             {}px growth cap ({} regions packed, {} still pending). Marking permanently \
             missing; the block renders without this glyph instead of retrying every frame.",
            key,
            width,
            height,
            self.width,
            self.height,
            self.max_dim,
            self.regions.len(),
            self.pending.len(),
        );
        self.pending.retain(|k| *k != key);
        self.failed.insert(key);
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
    pub fn insert_placeholder(
        &mut self,
        key: GlyphKey,
        images: &mut Assets<Image>,
    ) -> Option<AtlasRegion> {
        const PLACEHOLDER_SIZE: u32 = 4;
        let data = vec![0u8; (PLACEHOLDER_SIZE * PLACEHOLDER_SIZE * 4) as usize];
        self.insert(
            key,
            PLACEHOLDER_SIZE,
            PLACEHOLDER_SIZE,
            0.0,
            0.0,
            &data,
            images,
        )
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

/// Copy a WxH RGBA8 rect from one packed buffer to another. The two buffers
/// may have different row strides (an atlas grow changes width mid-copy).
#[allow(clippy::too_many_arguments)]
fn copy_rect_between_buffers(
    src: &[u8],
    src_stride_px: u32,
    src_x: u32,
    src_y: u32,
    dst: &mut [u8],
    dst_stride_px: u32,
    dst_x: u32,
    dst_y: u32,
    width: u32,
    height: u32,
) {
    for row in 0..height {
        let src_offset = (((src_y + row) * src_stride_px + src_x) * 4) as usize;
        let dst_offset = (((dst_y + row) * dst_stride_px + dst_x) * 4) as usize;
        let len = (width * 4) as usize;
        let src_end = src_offset + len;
        let dst_end = dst_offset + len;

        if src_end <= src.len() && dst_end <= dst.len() {
            dst[dst_offset..dst_end].copy_from_slice(&src[src_offset..src_end]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::glyph::FontId;

    /// RED before the fix (verified against pre-fix `insert()`, which took
    /// no `images` param and had no side effect on `pending` at all — the
    /// key stayed there forever, so `MsdfGenerator::queue_pending`, which
    /// only skips keys already in `queued` and removes from `queued` once
    /// the task finishes, respawned a generation task for it every single
    /// frame). Growth is disabled here (`with_max_dim` == initial size) so
    /// this exercises the terminal-fail path specifically, not growth.
    #[test]
    fn pack_failure_removes_key_from_pending_not_left_stuck() {
        let mut images = Assets::<Image>::default();
        let mut atlas = MsdfAtlas::new(&mut images, 64, 64).with_max_dim(64);
        let key = GlyphKey::new(FontId::for_test(1), 0);
        atlas.request(key);
        assert!(atlas.pending.contains(&key));

        // A glyph larger than the atlas, with no growth headroom, can never pack.
        let data = vec![0u8; (70 * 70 * 4) as usize];
        let result = atlas.insert(key, 70, 70, 0.0, 0.0, &data, &mut images);

        assert!(result.is_none(), "an oversized glyph must fail to pack");
        assert!(
            !atlas.pending.contains(&key),
            "pack failure must remove the key from pending, not leave it to \
             respawn a generation task every frame"
        );
    }

    #[test]
    fn terminal_pack_failure_lands_in_failed_set() {
        let mut images = Assets::<Image>::default();
        let mut atlas = MsdfAtlas::new(&mut images, 64, 64).with_max_dim(64);
        let key = GlyphKey::new(FontId::for_test(1), 0);
        atlas.request(key);

        let data = vec![0u8; (70 * 70 * 4) as usize];
        atlas.insert(key, 70, 70, 0.0, 0.0, &data, &mut images);

        assert!(
            atlas.failed.contains(&key),
            "a glyph that never fits must land in the terminal `failed` set"
        );
        assert!(!atlas.contains(key));
    }

    #[test]
    fn request_does_not_requeue_a_failed_key() {
        let mut images = Assets::<Image>::default();
        let mut atlas = MsdfAtlas::new(&mut images, 64, 64).with_max_dim(64);
        let key = GlyphKey::new(FontId::for_test(1), 0);
        atlas.request(key);
        let data = vec![0u8; (70 * 70 * 4) as usize];
        atlas.insert(key, 70, 70, 0.0, 0.0, &data, &mut images);
        assert!(atlas.failed.contains(&key));

        // Simulate the caller re-requesting the same glyph on a later frame
        // (e.g. the block re-renders and asks for the same text again).
        atlas.request(key);

        assert!(
            !atlas.pending.contains(&key),
            "request() must not resurrect a permanently failed glyph — that \
             would reopen the infinite respawn loop this fix closes"
        );
    }

    #[test]
    fn insert_success_clears_pending_and_returns_region() {
        let mut images = Assets::<Image>::default();
        let mut atlas = MsdfAtlas::new(&mut images, 64, 64);
        let key = GlyphKey::new(FontId::for_test(1), 0);
        atlas.request(key);

        let data = vec![0u8; (8 * 8 * 4) as usize];
        let region = atlas.insert(key, 8, 8, 0.0, 0.0, &data, &mut images);

        assert!(region.is_some());
        assert!(atlas.contains(key));
        assert!(!atlas.pending.contains(&key));
        assert!(!atlas.failed.contains(&key));
    }

    /// A glyph that doesn't fit the initial size but does fit after one
    /// doubling should succeed via growth, not fail permanently.
    #[test]
    fn pack_failure_triggers_growth_and_then_succeeds() {
        let mut images = Assets::<Image>::default();
        let mut atlas = MsdfAtlas::new(&mut images, 64, 64).with_max_dim(128);
        let key = GlyphKey::new(FontId::for_test(1), 0);
        atlas.request(key);

        // Too big for 64x64 (minus padding) but comfortably fits 128x128.
        let data = vec![0u8; (90 * 90 * 4) as usize];
        let region = atlas.insert(key, 90, 90, 0.0, 0.0, &data, &mut images);

        assert!(region.is_some(), "glyph should fit after growth");
        assert!(atlas.contains(key));
        assert!(!atlas.failed.contains(&key));
        assert_eq!(atlas.width, 128);
        assert_eq!(atlas.height, 128);
        assert_eq!(atlas.growth_epoch, 1, "growth must be observable via growth_epoch");

        // Texture asset itself must have been resized to match.
        let image = images.get(&atlas.texture).unwrap();
        assert_eq!(image.texture_descriptor.size.width, 128);
        assert_eq!(image.texture_descriptor.size.height, 128);
    }

    /// Growth repacks every existing region — this proves the repack
    /// preserves pixel content for glyphs that were already in the atlas
    /// before the grow, not just the new arrival that triggered it.
    #[test]
    fn growth_preserves_existing_region_pixels() {
        let mut images = Assets::<Image>::default();
        let mut atlas = MsdfAtlas::new(&mut images, 64, 64).with_max_dim(128);

        // Glyph A: a small, distinctively-colored glyph inserted before growth.
        let key_a = GlyphKey::new(FontId::for_test(1), 0);
        let color_a = [10u8, 20, 30, 40];
        let mut data_a = Vec::new();
        for _ in 0..(8 * 8) {
            data_a.extend_from_slice(&color_a);
        }
        atlas.request(key_a);
        let region_a_before = atlas
            .insert(key_a, 8, 8, 0.0, 0.0, &data_a, &mut images)
            .expect("small glyph fits in the initial 64x64 atlas");

        // Sanity: the pixel is really there before growth.
        assert_eq!(
            pixel_at(&atlas.pixels, atlas.width, region_a_before.x, region_a_before.y),
            color_a
        );

        // Glyph B: too big for 64x64, forces a grow to 128x128.
        let key_b = GlyphKey::new(FontId::for_test(2), 1);
        let color_b = [200u8, 210, 220, 230];
        let mut data_b = Vec::new();
        for _ in 0..(90 * 90) {
            data_b.extend_from_slice(&color_b);
        }
        atlas.request(key_b);
        let region_b = atlas
            .insert(key_b, 90, 90, 0.0, 0.0, &data_b, &mut images)
            .expect("glyph fits after growth");

        assert_eq!(atlas.width, 128);
        assert_eq!(atlas.height, 128);

        // Glyph A must still be present (region tracked, possibly moved) with
        // its pixel content intact at the new position and new stride.
        let region_a_after = atlas.regions.get(&key_a).copied().expect("glyph A survives growth");
        assert_eq!(region_a_after.width, 8);
        assert_eq!(region_a_after.height, 8);
        assert_eq!(
            pixel_at(&atlas.pixels, atlas.width, region_a_after.x, region_a_after.y),
            color_a,
            "glyph A's pixels must survive the repack at its (possibly new) position"
        );

        // Glyph B's own pixels must also be correct post-grow.
        assert_eq!(
            pixel_at(&atlas.pixels, atlas.width, region_b.x, region_b.y),
            color_b
        );

        // UV rects must be computable and consistent with the new atlas dims
        // — this is the contract the render-world vertex builder relies on.
        let uv_a = region_a_after.uv_rect(atlas.width, atlas.height);
        assert!(uv_a.iter().all(|c| *c >= 0.0 && *c <= 1.0));
    }

    /// Even after growth succeeds for one glyph, a glyph that still won't
    /// fit at the cap must terminally fail rather than grow forever.
    #[test]
    fn glyph_too_large_even_at_growth_cap_fails_permanently() {
        let mut images = Assets::<Image>::default();
        let mut atlas = MsdfAtlas::new(&mut images, 64, 64).with_max_dim(128);
        let key = GlyphKey::new(FontId::for_test(1), 0);
        atlas.request(key);

        // Bigger than the 128x128 cap in both dimensions.
        let data = vec![0u8; (200 * 200 * 4) as usize];
        let result = atlas.insert(key, 200, 200, 0.0, 0.0, &data, &mut images);

        assert!(result.is_none());
        assert!(atlas.failed.contains(&key));
        assert!(!atlas.pending.contains(&key));
        // Growth was attempted up to the cap, then gave up — atlas should
        // have grown to (and stopped at) 128x128, not spun forever.
        assert_eq!(atlas.width, 128);
        assert_eq!(atlas.height, 128);
    }

    fn pixel_at(pixels: &[u8], stride: u32, x: u32, y: u32) -> [u8; 4] {
        let offset = ((y * stride + x) * 4) as usize;
        [
            pixels[offset],
            pixels[offset + 1],
            pixels[offset + 2],
            pixels[offset + 3],
        ]
    }

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
