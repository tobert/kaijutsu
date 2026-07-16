//! MSDF glyph generation using msdfgen.
//!
//! Generates multi-channel signed distance field textures from font glyphs.
//! Font data comes from `peniko::Font.data` (via Parley glyph runs).

use bevy::prelude::*;
use bevy::tasks::{block_on, AsyncComputeTaskPool, Task};
use msdfgen::{Bitmap, FillRule, FontExt, Framing, MsdfGeneratorConfig, Projection, Rgba, Vector2};
use std::collections::{HashMap, HashSet};

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
    pub fn poll_completed(&mut self, atlas: &mut MsdfAtlas, images: &mut Assets<Image>) {
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
                atlas.insert_placeholder(glyph.key, images);
            } else {
                atlas.insert(
                    glyph.key,
                    glyph.width,
                    glyph.height,
                    glyph.anchor_x,
                    glyph.anchor_y,
                    &glyph.data,
                    images,
                );
            }
        }
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

    // Calculate framing for the glyph.
    //
    // NOTE: deliberately NOT using `Bound::autoframe` (msdfgen-rs
    // src/bound.rs) here, even though it looks like the obvious call:
    //
    //   - Its `scale: None` path (what `Range::Px(_)` + `None` used to
    //     invoke) derives its OWN per-glyph scale from whichever axis is
    //     the tighter fit — `(padded_target_px - range) / content_units`
    //     on ONE axis, applied isotropically to both. Since bitmap
    //     width/height are computed per-axis (`ceil(content_px) +
    //     padding`, above), the two axes' implied scales differ, and
    //     neither generally equals our intended `px_per_unit`. Every
    //     glyph rendered at a glyph-dependent size instead of the atlas's
    //     assumed constant `MSDF_PX_PER_EM`.
    //   - Its `scale: Some(_)` path looks like the fix (pass our own
    //     scale in) but has an upstream bug: it computes `res.translate`
    //     from the passed-in scale but never assigns `res.scale` itself,
    //     which stays at `Framing::default()`'s value — so it silently
    //     produces a wrong (unit) scale too.
    //
    // We build the `Framing` by hand instead: hold `scale` fixed at
    // `px_per_unit` on both axes, and solve `translate` to center the
    // glyph's content bounds in the padded bitmap. Projection semantics
    // (confirmed against msdfgen-rs's C++ `Projection::project`, and
    // `correct_sign`/`correct_msdf_error`, which consume the same
    // `Framing` we pass to `generate_mtsdf`): `bitmap_px = (shape_units +
    // translate) * scale`.
    let scale = px_per_unit;
    let content_width = bounds.width() * px_per_unit;
    let content_height = bounds.height() * px_per_unit;
    let margin_px_x = (width as f64 - content_width) / 2.0;
    let margin_px_y = (height as f64 - content_height) / 2.0;
    let translate_x = margin_px_x / scale - bounds.left;
    let translate_y = margin_px_y / scale - bounds.bottom;

    let framing = Framing {
        projection: Projection {
            scale: Vector2::new(scale, scale),
            translate: Vector2::new(translate_x, translate_y),
        },
        // `Framing.range` is in SHAPE units, not pixels — this is what
        // `autoframe`'s `Range::Px` branch computes as `range / scale`
        // (there `scale` is its own derived value; here it's ours).
        range: msdf_range / scale,
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

    // Calculate anchor (offset from bitmap origin to glyph pen origin, in
    // em units). Exact now that `scale` above is exactly `px_per_unit`
    // rather than autoframe's derived value: `translate_x/y * px_per_unit`
    // is precisely the bitmap-pixel offset from the shape origin to the
    // padded content box (see `bitmap_px = (shape_units + translate) *
    // scale` above), matching the anchor convention documented on
    // `AtlasRegion` (atlas.rs) and the V-flip in `build_vertices`
    // (renderer.rs): anchor_y is measured from the bitmap TOP.
    let anchor_x = (translate_x * px_per_unit / px_per_em) as f32;
    let anchor_y = ((height as f64 - translate_y * px_per_unit) / px_per_em) as f32;

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

    // `queue_pending`/`poll_completed` themselves call
    // `AsyncComputeTaskPool::get()`, which panics without a real Bevy App
    // (TaskPoolPlugin) to initialize the global pool — so the retry-budget
    // decision is tested here as the pure function it was factored into,
    // per the "practical without a real task pool" testing note.

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

    // -- Fix 1: per-glyph scale wobble -------------------------------------
    //
    // `generate_glyph` must size every glyph's MSDF at exactly
    // `px_per_unit` (== px_per_em / units_per_em). Before the fix it framed
    // the shape with `bounds.autoframe(width, height, Range::Px(4), None)`,
    // which derives its OWN isotropic scale from whichever axis
    // (`(padded_target_px - range) / content_units`) is the tighter fit,
    // and applies that single scale to BOTH axes. Because bitmap
    // width/height are computed per-axis (`ceil(content_px) + padding`),
    // the two axes' implied scales differ glyph-by-glyph — small for a
    // near-square glyph, large for one where padding dominates a tiny
    // content extent (e.g. '.'). Every glyph ends up rendered at a
    // glyph-dependent size instead of the atlas's assumed constant
    // `MSDF_PX_PER_EM`.

    /// A pixel counts as "inside" the glyph when the MSDF median channel —
    /// the same value the shader thresholds against — is >= 0.5 (128/255).
    /// Returns the inclusive bounding box of inside pixels, in bitmap pixel
    /// coordinates (orientation doesn't matter for measuring extent).
    fn ink_bbox(data: &[u8], width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
        let mut bbox: Option<(u32, u32, u32, u32)> = None;
        for row in 0..height {
            for col in 0..width {
                let idx = ((row * width + col) * 4) as usize;
                let mut rgb = [data[idx], data[idx + 1], data[idx + 2]];
                rgb.sort_unstable();
                let median = rgb[1];
                if median >= 128 {
                    bbox = Some(match bbox {
                        None => (col, col, row, row),
                        Some((min_x, max_x, min_y, max_y)) => (
                            min_x.min(col),
                            max_x.max(col),
                            min_y.min(row),
                            max_y.max(row),
                        ),
                    });
                }
            }
        }
        bbox
    }

    /// Generate a real glyph from the shipped Cascadia Code NF font and
    /// check its geometry.
    ///
    /// Ground truth for the ink SIZE (width/height) comes from
    /// `ttf_parser::Face::glyph_bounding_box`, NOT `msdfgen::Shape::get_bound()`
    /// — verified by hand against this exact font (glyph_id 1862, '.'):
    /// ttf-parser reports `x_min=452, x_max=748` (a tight, correct bbox from
    /// the font's own `glyf` table), but msdfgen's `Shape::get_bound()`
    /// reports `left=0.0`. That is a separate, pre-existing bug in
    /// msdfgen-rs itself (`Shape::get_bound()`/`Contour::get_bound()` seed
    /// their accumulator with `Bound::default()` == `(0,0,0,0)` and then
    /// only ever *shrink toward* an extreme — see
    /// `sys/lib/core/edge-segments.cpp`'s `boundPoint`, `if (p.x < l) l =
    /// p.x;` etc. — instead of the C++ `Shape::getBounds()` convenience's
    /// `±LARGE_VALUE` seed, which msdfgen-rs doesn't bind at all. So `left`
    /// silently stays `0.0` whenever the shape's true left edge is
    /// positive, which is the common case for any glyph with left-side
    /// bearing.) Not fixed here — out of scope (can't modify msdfgen-rs;
    /// see docs/issues.md) — but it means `bounds.width()*px_per_unit`
    /// cannot be trusted as ground truth for this assertion.
    ///
    /// The ink WIDTH/HEIGHT check is immune to that bug regardless: Fix 1's
    /// whole point is that `bitmap_px` is an affine function of shape units
    /// with slope exactly `px_per_unit` everywhere, so a *difference*
    /// between two shape-space x (or y) coordinates always scales
    /// correctly even when the `translate` offset itself was computed from
    /// a wrong `bounds.left`/`bounds.bottom`.
    ///
    /// The anchor check, by contrast, deliberately reuses
    /// `shape.get_bound()` (the same, possibly-buggy bounds `generate_glyph`
    /// itself sees) — it is a regression/self-consistency check that
    /// `generate_glyph`'s anchor formula matches its own inputs, not an
    /// independent check of visual correctness.
    fn assert_glyph_geometry_matches_bounds(ch: char) {
        let font_data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/fonts/CascadiaCodeNF.ttf"
        ))
        .expect("shipped test font must be present");

        let face = ttf_parser::Face::parse(&font_data, 0).expect("font must parse");
        let glyph_id = face
            .glyph_index(ch)
            .unwrap_or_else(|| panic!("font has no glyph for {ch:?}"));

        let msdf_range = 4.0;
        let px_per_em = 64.0;
        let angle_threshold = 3.0;

        let key = GlyphKey::new(FontId::for_test(1), glyph_id.0);
        let generated = generate_glyph(
            key,
            &font_data,
            glyph_id.0,
            msdf_range,
            px_per_em,
            angle_threshold,
        );
        assert!(
            !generated.is_placeholder,
            "{ch:?} must generate real MSDF data"
        );

        let units_per_em = face.units_per_em() as f64;
        let px_per_unit = px_per_em / units_per_em;

        let tight_bbox = face
            .glyph_bounding_box(glyph_id)
            .unwrap_or_else(|| panic!("{ch:?} must have a tight bbox"));
        let expected_content_w = (tight_bbox.x_max - tight_bbox.x_min) as f64 * px_per_unit;
        let expected_content_h = (tight_bbox.y_max - tight_bbox.y_min) as f64 * px_per_unit;

        let (min_x, max_x, min_y, max_y) =
            ink_bbox(&generated.data, generated.width, generated.height)
                .unwrap_or_else(|| panic!("{ch:?} must have visible ink"));
        let ink_w = (max_x - min_x + 1) as f64;
        let ink_h = (max_y - min_y + 1) as f64;

        assert!(
            (ink_w - expected_content_w).abs() <= 2.5,
            "{ch:?}: ink width {ink_w} should be within 2.5px of content width \
             {expected_content_w} (bitmap {}x{})",
            generated.width,
            generated.height,
        );
        assert!(
            (ink_h - expected_content_h).abs() <= 2.5,
            "{ch:?}: ink height {ink_h} should be within 2.5px of content height \
             {expected_content_h} (bitmap {}x{})",
            generated.width,
            generated.height,
        );

        // Anchor closed form (self-consistency, see doc comment above):
        // recompute the same formula `generate_glyph` uses, from the same
        // (possibly bounds-bug-affected) `shape.get_bound()` inputs.
        let mut shape = face.glyph_shape(glyph_id).expect("glyph must have a shape");
        shape.edge_coloring_simple(angle_threshold, 0);
        let bounds = shape.get_bound();
        let content_w = bounds.width() * px_per_unit;
        let content_h = bounds.height() * px_per_unit;
        let margin_x = (generated.width as f64 - content_w) / 2.0;
        let margin_y = (generated.height as f64 - content_h) / 2.0;
        let translate_x = margin_x / px_per_unit - bounds.left;
        let translate_y = margin_y / px_per_unit - bounds.bottom;
        let expected_anchor_x = (translate_x * px_per_unit / px_per_em) as f32;
        let expected_anchor_y =
            ((generated.height as f64 - translate_y * px_per_unit) / px_per_em) as f32;

        assert!(
            (generated.anchor_x - expected_anchor_x).abs() < 1e-3,
            "{ch:?}: anchor_x {} != expected {}",
            generated.anchor_x,
            expected_anchor_x,
        );
        assert!(
            (generated.anchor_y - expected_anchor_y).abs() < 1e-3,
            "{ch:?}: anchor_y {} != expected {}",
            generated.anchor_y,
            expected_anchor_y,
        );
    }

    #[test]
    fn generate_glyph_period_ink_matches_content_bounds() {
        // '.' is the sharpest regression case: padding (8px) is a large
        // fraction of its tiny content extent, so autoframe's derived scale
        // was off by close to 2x before the fix.
        assert_glyph_geometry_matches_bounds('.');
    }

    #[test]
    fn generate_glyph_capital_m_ink_matches_content_bounds() {
        // 'M' is wide and roughly square, so autoframe's derived scale was
        // off by a smaller (~+10%) but still-wrong amount before the fix —
        // this guards the "every glyph, not just narrow ones" claim.
        assert_glyph_geometry_matches_bounds('M');
    }
}
