//! CPU rasterization for inline SVG blocks.
//!
//! Replaces the old `vello_svg` path (see `text::rich`'s
//! `RichContentKind::Svg`): `usvg` parses SVG markup to a `Tree` (already
//! resolving `<text>` outlines via a `fontdb`, same as before — see
//! `text::rich::try_parse_svg`); `resvg` + `tiny-skia` rasterize that tree
//! straight to a premultiplied RGBA8 `Pixmap` sized to the block's PHYSICAL
//! pixel box. `view::block_render` computes that box from `ComputedNode` +
//! `TextMetrics::scale_factor` and drives this module — rasterizing at the
//! wrong size is exactly the "blurry at 1x, crisp on HiDPI" class of bug this
//! module exists to avoid. [`unpremultiply_to_straight_rgba`] converts the
//! result to the straight-alpha layout Bevy's `TextureFormat::Rgba8UnormSrgb`
//! expects, before it becomes an `Image` + `ImageNode`.
//!
//! Same stack as sister project scry-mcp (`~/src/scry-mcp/src/render.rs`):
//! resvg 0.47 + usvg 0.47 + tiny-skia 0.12.

/// Ceiling on the physical pixel dimension we'll allocate a raster for. A
/// runaway `content_width` or DPI multiplier must clamp to this, not attempt
/// a multi-gigabyte allocation — mirrors the GPU/vello ceilings elsewhere in
/// this crate (`view::block_render`'s `FALLBACK_MAX_TEXTURE_DIM` /
/// `VELLO_MAX_TEXTURE_DIM`).
pub const SVG_RASTER_MAX_DIM: u32 = 8192;

/// Fit an SVG's intrinsic size into a `(max_width, max_height)` box,
/// preserving aspect ratio. **Shrink-only**: the box is a CEILING, not a
/// target. Content that already fits draws at its intrinsic size (scale
/// 1.0); oversized content shrinks until whichever axis binds first fits
/// exactly, and is narrower/shorter than the box on the other axis
/// (left-aligned when drawn).
///
/// The `.min(1.0)` is the whole law and it is not cosmetic. An SVG's
/// `width`/`height` are real px — a 200x200 cat is a 200x200 cat, the same
/// contract an `<img>` has in a browser. Scaling it up to whatever the
/// conversation column happens to be produced the "giant cat" bug: a 4.74x
/// blow-up to 948x948, wider than the window, painting through every
/// neighbouring block's text. Note this differs deliberately from the `Abc`
/// arm in `view::block_render`, which reuses the same `min(w, h)` shape but
/// *does* fill the width — an engraved score has no intrinsic px size, only
/// an aspect ratio, so stretching it to the column is right there and wrong
/// here.
///
/// Returns `(fit_scale, draw_width, draw_height)` in the same units as
/// `max_width`/`max_height` (LOGICAL px when called from `block_render`).
/// `fit_scale` converts SVG user units into that space. Returns `None` for a
/// degenerate SVG (non-positive intrinsic size) or box — callers must not
/// rasterize either.
pub fn fit_svg_to_box(
    svg_width: f32,
    svg_height: f32,
    max_width: f32,
    max_height: f32,
) -> Option<(f64, f32, f32)> {
    if svg_width <= 0.0 || svg_height <= 0.0 || max_width <= 0.0 || max_height <= 0.0 {
        return None;
    }
    let w_scale = (max_width / svg_width) as f64;
    let h_scale = (max_height / svg_height) as f64;
    let fit_scale = w_scale.min(h_scale).min(1.0);
    let draw_width = (svg_width as f64 * fit_scale) as f32;
    let draw_height = (svg_height as f64 * fit_scale) as f32;
    Some((fit_scale, draw_width, draw_height))
}

/// Render `tree` into a fresh straight-alpha RGBA8 buffer of exactly
/// `target_width`×`target_height` physical pixels. `content_scale` maps SVG
/// user-unit space directly to that physical pixel space (logical fit-scale
/// × DPI scale, applied uniformly so aspect ratio is preserved) — see
/// [`fit_svg_to_box`] for the logical half of that product.
///
/// Returns `None` only if tiny-skia refuses the pixmap allocation (zero
/// dimension); callers are expected to have already clamped both dimensions
/// to `>= 1` (e.g. via `view::ui_rtt::ui_rtt_texture_dims`), so this is a
/// defensive backstop, not an expected path — a bad SVG fails at the
/// `usvg::Tree::from_str` parse step (see `text::rich::try_parse_svg`), not
/// here.
pub fn rasterize_svg(
    tree: &usvg::Tree,
    target_width: u32,
    target_height: u32,
    content_scale: f64,
) -> Option<Vec<u8>> {
    let mut pixmap = tiny_skia::Pixmap::new(target_width, target_height)?;
    let transform = tiny_skia::Transform::from_scale(content_scale as f32, content_scale as f32);
    resvg::render(tree, transform, &mut pixmap.as_mut());
    Some(unpremultiply_to_straight_rgba(
        pixmap.data(),
        pixmap.width(),
        pixmap.height(),
    ))
}

/// Convert a tiny-skia premultiplied RGBA8 buffer (`Pixmap::data()`'s layout)
/// into straight (non-premultiplied) RGBA8 — the layout Bevy's
/// `TextureFormat::Rgba8UnormSrgb` expects. Skipping this conversion is the
/// classic vector-raster-into-a-game-engine bug: tiny-skia's premultiplied
/// values read as too-dark colors wherever alpha < 255, visible as a dark
/// halo/fringe around anti-aliased or semi-transparent SVG edges (fully
/// opaque and fully transparent pixels are numerically identical either way,
/// which is exactly why the bug hides during casual testing with solid-color
/// shapes that don't cross a transparent edge).
///
/// # Panics
///
/// If `premul.len() != width * height * 4` — a caller bug (mismatched
/// dimensions), not a data-dependent failure, so this asserts rather than
/// returning a `Result` a caller could forget to check.
pub fn unpremultiply_to_straight_rgba(premul: &[u8], width: u32, height: u32) -> Vec<u8> {
    let expected_len = width as usize * height as usize * 4;
    assert_eq!(
        premul.len(),
        expected_len,
        "premultiplied buffer length {} does not match {width}x{height}x4",
        premul.len(),
    );

    let mut out = Vec::with_capacity(expected_len);
    for px in premul.chunks_exact(4) {
        let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
        match a {
            0 => out.extend_from_slice(&[0, 0, 0, 0]),
            255 => out.extend_from_slice(&[r, g, b, 255]),
            _ => {
                // Straight = round(premultiplied * 255 / alpha). Premultiplied
                // invariant (c <= a) guarantees this never exceeds 255.
                let unmul = |c: u8| -> u8 { ((c as u32 * 255 + a as u32 / 2) / a as u32) as u8 };
                out.extend_from_slice(&[unmul(r), unmul(g), unmul(b), a]);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- fit_svg_to_box ------------------------------------------------

    #[test]
    fn fit_never_upscales_an_svg_that_already_fits() {
        // The live "giant cat" bug, with the real numbers off the running
        // app: a 200x200 SVG in a 948-logical-px-wide conversation column
        // was scaled 4.74x to 948x948 — wider than the whole window and
        // taller than the scroll viewport, painting through the text of
        // every neighbouring block. An SVG carries a real intrinsic size in
        // px; the box is a CEILING, not a target.
        let (scale, w, h) = fit_svg_to_box(200.0, 200.0, 948.0, 8192.0).unwrap();
        assert_eq!(scale, 1.0);
        assert_eq!(w, 200.0);
        assert_eq!(h, 200.0);
    }

    #[test]
    fn fit_never_upscales_when_only_the_height_has_room() {
        // Width-bound content is the case that already shrank correctly;
        // this pins the other half — a wide-but-short SVG must not grow
        // into the (deliberately enormous) SVG_MAX_HEIGHT ceiling either.
        let (scale, w, h) = fit_svg_to_box(300.0, 50.0, 300.0, 8192.0).unwrap();
        assert_eq!(scale, 1.0);
        assert_eq!(w, 300.0);
        assert_eq!(h, 50.0);
    }

    #[test]
    fn fit_result_is_always_contained_by_the_box() {
        // The containment law itself, swept over both orientations and both
        // over- and under-sized content: whatever comes back must fit
        // inside the box AND never exceed the SVG's own intrinsic size.
        for &(sw, sh) in &[(200.0f32, 200.0f32), (2000.0, 300.0), (60.0, 4000.0), (17.0, 3.0)] {
            for &(mw, mh) in &[(948.0f32, 8192.0f32), (100.0, 100.0), (5.0, 900.0)] {
                let (_, w, h) = fit_svg_to_box(sw, sh, mw, mh).unwrap();
                assert!(w <= mw + 1e-3 && h <= mh + 1e-3, "{sw}x{sh} in {mw}x{mh} -> {w}x{h} overflows the box");
                assert!(w <= sw + 1e-3 && h <= sh + 1e-3, "{sw}x{sh} in {mw}x{mh} -> {w}x{h} upscaled past intrinsic size");
            }
        }
    }

    #[test]
    fn fit_oversized_width_bound_fills_max_width_exactly() {
        let (scale, w, h) = fit_svg_to_box(200.0, 100.0, 100.0, 1000.0).unwrap();
        assert_eq!(scale, 0.5);
        assert_eq!(w, 100.0);
        assert_eq!(h, 50.0);
    }

    #[test]
    fn fit_oversized_height_bound_fills_max_height_exactly_and_is_narrower() {
        let (scale, w, h) = fit_svg_to_box(100.0, 400.0, 300.0, 200.0).unwrap();
        assert_eq!(scale, 0.5);
        assert_eq!(w, 50.0);
        assert_eq!(h, 200.0);
        assert!(w < 300.0, "height-bound content must not fill the full width");
    }

    #[test]
    fn fit_rejects_degenerate_svg_size() {
        assert!(fit_svg_to_box(0.0, 100.0, 100.0, 100.0).is_none());
        assert!(fit_svg_to_box(100.0, 0.0, 100.0, 100.0).is_none());
    }

    #[test]
    fn fit_rejects_degenerate_box() {
        assert!(fit_svg_to_box(100.0, 100.0, 0.0, 100.0).is_none());
        assert!(fit_svg_to_box(100.0, 100.0, 100.0, 0.0).is_none());
    }

    // -- unpremultiply_to_straight_rgba ---------------------------------

    #[test]
    fn unpremultiply_leaves_opaque_pixels_unchanged() {
        let premul = [10u8, 20, 30, 255];
        let out = unpremultiply_to_straight_rgba(&premul, 1, 1);
        assert_eq!(out, vec![10, 20, 30, 255]);
    }

    #[test]
    fn unpremultiply_leaves_fully_transparent_pixels_as_zero() {
        // A non-zero color with alpha=0 is nonsensical for premultiplied data,
        // but must not divide by zero or leak the color into a "transparent"
        // pixel — always resolves to (0,0,0,0).
        let premul = [200u8, 100, 50, 0];
        let out = unpremultiply_to_straight_rgba(&premul, 1, 1);
        assert_eq!(out, vec![0, 0, 0, 0]);
    }

    #[test]
    fn unpremultiply_recovers_exact_straight_values() {
        // Hand-picked so premultiplied*255/alpha divides evenly. This is the
        // actual bug fix under test: without unpremultiplying, this pixel
        // would stay (30, 60, 0, 90) — reading as a dark, dull red — instead
        // of a fully saturated (85, 170, 0) at 90/255 opacity.
        let premul = [30u8, 60, 0, 90];
        let out = unpremultiply_to_straight_rgba(&premul, 1, 1);
        assert_eq!(out, vec![85, 170, 0, 90]);
    }

    #[test]
    #[should_panic(expected = "does not match")]
    fn unpremultiply_panics_on_size_mismatch() {
        let premul = [0u8; 4];
        unpremultiply_to_straight_rgba(&premul, 2, 2); // claims 2x2x4=16 bytes, has 4
    }

    // -- rasterize_svg (round trip through real usvg/resvg) --------------

    #[test]
    fn rasterize_red_square_is_red_at_center_and_transparent_outside() {
        // A 10x10 red square inside a 40x40 canvas, offset from every edge —
        // deep enough inside/outside the shape that anti-aliasing at the
        // square's border can't touch either sample point.
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="40">
            <rect x="15" y="15" width="10" height="10" fill="#ff0000"/>
        </svg>"##;
        let options = usvg::Options::default();
        let tree = usvg::Tree::from_str(svg, &options).expect("valid SVG must parse");
        let size = tree.size();
        assert_eq!((size.width(), size.height()), (40.0, 40.0));

        let rgba = rasterize_svg(&tree, 40, 40, 1.0).expect("rasterize should succeed");
        assert_eq!(rgba.len(), 40 * 40 * 4);

        let pixel_at = |x: u32, y: u32| -> [u8; 4] {
            let i = ((y * 40 + x) * 4) as usize;
            [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
        };

        // Center of the square (20,20) — solid, fully inside the fill.
        assert_eq!(pixel_at(20, 20), [255, 0, 0, 255]);
        // Corner of the canvas — well outside the square, fully transparent.
        assert_eq!(pixel_at(2, 2), [0, 0, 0, 0]);
    }

    #[test]
    fn rasterize_scales_to_the_requested_physical_size() {
        // Rasterizing at 2x the SVG's native size must still land the fill
        // in the scaled-up center — this is the "physical pixel size, not
        // the SVG's own size" behavior `content_scale` exists for.
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">
            <rect x="5" y="5" width="10" height="10" fill="#00ff00"/>
        </svg>"##;
        let options = usvg::Options::default();
        let tree = usvg::Tree::from_str(svg, &options).expect("valid SVG must parse");

        let rgba = rasterize_svg(&tree, 40, 40, 2.0).expect("rasterize should succeed");
        let i = ((20 * 40 + 20) * 4) as usize; // center of the 40x40 target
        assert_eq!(&rgba[i..i + 4], &[0, 255, 0, 255]);
    }

    #[test]
    fn rasterize_mid_tone_fill_passes_through_unchanged() {
        // The red/green/transparent fixtures above are all values where sRGB
        // and linear encodings are numerically IDENTICAL (0 and 255 are
        // fixed points of any gamma curve) — they'd pass even if something
        // in this pipeline mismatched sRGB vs linear interpretation. A
        // genuine mid-tone exposes that: `resvg` paints a solid opaque fill
        // directly with no gamma step, and `unpremultiply_to_straight_rgba`
        // does none either for an opaque pixel (its `a == 255` fast path is
        // a straight copy) — so 0x80 must come out as exactly 128, not
        // shifted by the ~22 code points a linear<->sRGB round-trip would
        // introduce. Pins today's correct behavior so a future refactor that
        // adds an unneeded gamma conversion doesn't sail through unnoticed.
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">
            <rect x="5" y="5" width="10" height="10" fill="#808080"/>
        </svg>"##;
        let options = usvg::Options::default();
        let tree = usvg::Tree::from_str(svg, &options).expect("valid SVG must parse");

        let rgba = rasterize_svg(&tree, 20, 20, 1.0).expect("rasterize should succeed");
        let i = ((10 * 20 + 10) * 4) as usize; // center of the rect, deep inside the fill
        assert_eq!(&rgba[i..i + 4], &[128, 128, 128, 255]);
    }

    #[test]
    fn rasterize_rejects_malformed_svg_at_the_parse_step() {
        // Confirms the error path is at parse time, not here — matches
        // `text::rich::try_parse_svg`'s contract (bad markup never reaches
        // rasterize_svg at all).
        let options = usvg::Options::default();
        let result = usvg::Tree::from_str("not svg at all", &options);
        assert!(result.is_err(), "malformed SVG must fail to parse");
    }
}
