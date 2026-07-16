//! MSDF glyph generation using msdfgen.
//!
//! Generates multi-channel signed distance field textures from font glyphs.
//! Font data comes from `peniko::Font.data` (via Parley glyph runs).

use bevy::prelude::*;
use bevy::tasks::{block_on, AsyncComputeTaskPool, Task};
use msdfgen::{Bitmap, FillRule, FontExt, MsdfGeneratorConfig, Range, Rgba};
use std::collections::{HashMap, HashSet};

use super::atlas::{AtlasRegion, MsdfAtlas};
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
    /// Consecutive frames a pending glyph has been skipped because its
    /// font's data hasn't reached `FontDataMap` yet. Fonts register during
    /// scene builds, so the data can legitimately arrive a frame or two
    /// late — this bounds how long we tolerate that before treating it as a
    /// real problem instead of retrying silently forever.
    font_wait_attempts: HashMap<GlyphKey, u32>,
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
    /// Number of frames to tolerate missing font data for a pending glyph
    /// before giving up on it (loud, terminal) instead of retrying forever.
    pub const FONT_WAIT_MAX_ATTEMPTS: u32 = 120;

    /// Create a new generator with default settings.
    ///
    /// MSDF range of 4.0 at 64px/em gives ~4px effective AA at 16px font size.
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            queued: HashSet::new(),
            font_wait_attempts: HashMap::new(),
            msdf_range: 4.0,
            px_per_em: 64.0,
            angle_threshold: 3.0,
        }
    }

    /// Queue glyph generation for pending glyphs in the atlas.
    ///
    /// `font_data_map` provides raw font bytes keyed by FontId.
    /// Skips glyphs that already have in-flight tasks. A glyph whose font
    /// data hasn't shown up after `FONT_WAIT_MAX_ATTEMPTS` frames is moved
    /// to the atlas's terminal `failed` set (loud, once) instead of being
    /// probed forever.
    pub fn queue_pending(&mut self, atlas: &mut MsdfAtlas, font_data_map: &super::FontDataMap) {
        let pool = AsyncComputeTaskPool::get();

        // Snapshot: the loop body can call `atlas.mark_failed`, which
        // mutates `atlas.pending` — can't hold a borrow of it across that.
        let pending: Vec<GlyphKey> = atlas.pending.clone();

        for key in pending {
            if self.queued.contains(&key) {
                continue;
            }

            let Some(font_data) = font_data_map.get(&key.font_id) else {
                let attempts = self.font_wait_attempts.entry(key).or_insert(0);
                *attempts += 1;
                if should_retry_font_wait(*attempts, Self::FONT_WAIT_MAX_ATTEMPTS) {
                    debug!(
                        "Font data not found for font_id {:?} (attempt {}/{})",
                        key.font_id, attempts, Self::FONT_WAIT_MAX_ATTEMPTS,
                    );
                } else {
                    error!(
                        "Font data never registered for font_id {:?} after {} frames — \
                         giving up on glyph {:?} (marking permanently missing instead of \
                         retrying every frame)",
                        key.font_id, attempts, key,
                    );
                    self.font_wait_attempts.remove(&key);
                    atlas.mark_failed(key);
                }
                continue;
            };

            self.font_wait_attempts.remove(&key);

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
    ///
    /// Returns the set of `GlyphKey`s that actually landed a region in the
    /// atlas this call — a successful `insert` or `insert_placeholder`. A
    /// glyph whose pack failed (returned `None`, e.g. still growing, or
    /// terminally too big) is NOT included: nothing changed for it, so no
    /// block needs a version bump on its account. Callers use this set to
    /// bump only the blocks that reference a newly-visible glyph instead of
    /// every text block on screen (see `poll_msdf_generator`).
    pub fn poll_completed(
        &mut self,
        atlas: &mut MsdfAtlas,
        images: &mut Assets<Image>,
    ) -> HashSet<GlyphKey> {
        let mut completed = Vec::new();
        self.tasks.retain_mut(|task| {
            if task.is_finished() {
                completed.push(block_on(async { task.await }));
                false
            } else {
                true
            }
        });

        let mut landed = HashSet::new();
        for glyph in completed {
            self.queued.remove(&glyph.key);
            let result = if glyph.is_placeholder {
                atlas.insert_placeholder(glyph.key, images)
            } else {
                atlas.insert(
                    glyph.key,
                    glyph.width,
                    glyph.height,
                    glyph.anchor_x,
                    glyph.anchor_y,
                    &glyph.data,
                    images,
                )
            };
            record_landed(&mut landed, glyph.key, result);
        }
        landed
    }

}

/// Record `key` as landed iff `insert_result` shows it actually occupies a
/// region now. Pack failure (`None`) must NOT be recorded — factored out of
/// `poll_completed`'s loop so it can be exercised without a real task pool:
/// call `MsdfAtlas::insert`/`insert_placeholder` directly (they don't touch
/// `AsyncComputeTaskPool`) and feed the `Option<AtlasRegion>` result in.
fn record_landed(
    landed: &mut HashSet<GlyphKey>,
    key: GlyphKey,
    insert_result: Option<AtlasRegion>,
) {
    if insert_result.is_some() {
        landed.insert(key);
    }
}

/// Pure retry-budget check, factored out for unit testing without a real
/// task pool: is a pending glyph still within its font-data wait budget?
fn should_retry_font_wait(attempts: u32, max_attempts: u32) -> bool {
    attempts < max_attempts
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

#[cfg(test)]
mod tests {
    use super::super::glyph::FontId;
    use super::*;

    // `queue_pending` calls `AsyncComputeTaskPool::get()`, which panics
    // without a real Bevy App (TaskPoolPlugin) to initialize the global
    // pool — so the retry-budget decision is tested here as the pure
    // function it was factored into, per the "practical without a real task
    // pool" testing note. `record_landed` is exercised the same way: not by
    // driving `poll_completed`'s task-draining loop, but by calling
    // `MsdfAtlas::insert`/`insert_placeholder` directly (no task pool
    // involved) and feeding their results through the helper.

    #[test]
    fn retries_while_under_budget() {
        assert!(should_retry_font_wait(0, MsdfGenerator::FONT_WAIT_MAX_ATTEMPTS));
        assert!(should_retry_font_wait(1, MsdfGenerator::FONT_WAIT_MAX_ATTEMPTS));
        assert!(should_retry_font_wait(
            MsdfGenerator::FONT_WAIT_MAX_ATTEMPTS - 1,
            MsdfGenerator::FONT_WAIT_MAX_ATTEMPTS
        ));
    }

    #[test]
    fn stops_retrying_once_budget_exhausted() {
        assert!(!should_retry_font_wait(
            MsdfGenerator::FONT_WAIT_MAX_ATTEMPTS,
            MsdfGenerator::FONT_WAIT_MAX_ATTEMPTS
        ));
        assert!(!should_retry_font_wait(
            MsdfGenerator::FONT_WAIT_MAX_ATTEMPTS + 50,
            MsdfGenerator::FONT_WAIT_MAX_ATTEMPTS
        ));
    }

    #[test]
    fn zero_budget_never_retries() {
        assert!(!should_retry_font_wait(0, 0));
    }

    // -- record_landed -------------------------------------------------

    #[test]
    fn record_landed_includes_a_successful_pack_but_not_a_failed_one() {
        let mut images = Assets::<Image>::default();
        let mut atlas = MsdfAtlas::new(&mut images, 64, 64).with_max_dim(64);
        let key_ok = GlyphKey::new(FontId::for_test(1), 0);
        let key_fail = GlyphKey::new(FontId::for_test(2), 0);

        let mut landed = HashSet::new();

        // Fits comfortably in the 64x64 atlas.
        let data_ok = vec![0u8; 8 * 8 * 4];
        let result_ok = atlas.insert(key_ok, 8, 8, 0.0, 0.0, &data_ok, &mut images);
        record_landed(&mut landed, key_ok, result_ok);

        // Too big for the atlas, with growth capped at the current size —
        // mirrors `pack_failure_removes_key_from_pending_not_left_stuck` in
        // atlas.rs.
        let data_fail = vec![0u8; 70 * 70 * 4];
        let result_fail = atlas.insert(key_fail, 70, 70, 0.0, 0.0, &data_fail, &mut images);
        record_landed(&mut landed, key_fail, result_fail);

        assert!(
            landed.contains(&key_ok),
            "a glyph that successfully packed must be recorded as landed"
        );
        assert!(
            !landed.contains(&key_fail),
            "a glyph whose pack failed must NOT be recorded as landed — nothing \
             changed for it, so no block should be bumped on its account"
        );
    }

    #[test]
    fn record_landed_includes_a_successful_placeholder() {
        let mut images = Assets::<Image>::default();
        let mut atlas = MsdfAtlas::new(&mut images, 64, 64);
        let key = GlyphKey::new(FontId::for_test(1), 0);

        let mut landed = HashSet::new();
        let result = atlas.insert_placeholder(key, &mut images);
        record_landed(&mut landed, key, result);

        assert!(
            landed.contains(&key),
            "a successful placeholder insert is as much a landing as a real glyph"
        );
    }
}
