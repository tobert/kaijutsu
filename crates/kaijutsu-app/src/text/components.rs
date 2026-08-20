//! Text effect markers and color/brush helpers for Kaijutsu.

use bevy::prelude::*;

/// Rainbow color cycling effect marker.
///
/// When present, the text brush uses a gradient instead of a solid color.
///
/// Unused: this and [`rainbow_brush`] were the deleted per-block-cell
/// path's plumbing for `Theme::font_rainbow` (default **on**). The
/// conversation surface (`view::surface::content`) never grew an equivalent
/// — `font_rainbow` is now a theme knob with no renderer behind it. Kept
/// (not deleted) as the reference implementation for whoever ports it;
/// tracked in `docs/issues.md`.
#[derive(Component, Default, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct KjTextEffects {
    pub rainbow: bool,
}

/// Convert a Bevy `Color` to a `peniko::Brush::Solid`.
///
/// `Brush` is the shared color currency for parley glyph runs on both text
/// paths (MSDF and vello) — most callers are MSDF-only (`collect_msdf_glyphs`
/// spans, border/role-divider labels), so this names `peniko` directly
/// rather than `vello::peniko` (the identical type; vello re-exports peniko
/// verbatim) to avoid making pure-MSDF code paths name the vello crate.
pub fn bevy_color_to_brush(color: Color) -> peniko::Brush {
    let srgba = color.to_srgba();
    peniko::Brush::Solid(peniko::Color::from_rgba8(
        (srgba.red * 255.0) as u8,
        (srgba.green * 255.0) as u8,
        (srgba.blue * 255.0) as u8,
        (srgba.alpha * 255.0) as u8,
    ))
}

/// Convert a Bevy `Color` to straight-alpha RGBA8 — the flat-geometry vertex
/// format (`text::msdf::geometry::GeometryVertex::color`, premultiplied later
/// in the geometry fragment shader). Component-wise truncating conversion,
/// shared so every geometry producer agrees bit-for-bit.
pub fn color_to_rgba8(color: Color) -> [u8; 4] {
    let c = color.to_srgba();
    [
        (c.red.clamp(0.0, 1.0) * 255.0) as u8,
        (c.green.clamp(0.0, 1.0) * 255.0) as u8,
        (c.blue.clamp(0.0, 1.0) * 255.0) as u8,
        (c.alpha.clamp(0.0, 1.0) * 255.0) as u8,
    ]
}

/// Build a scrolling rainbow gradient brush.
///
/// The rainbow flows spatially through the text: each character's color
/// is determined by its horizontal position. `offset` (0.0..1.0) shifts
/// the gradient start point over time, creating a smooth scrolling effect.
///
/// Uses `Extend::Repeat` so the palette tiles seamlessly across any text width.
///
/// Unused as of slice 5 — see [`KjTextEffects`]'s doc comment.
#[allow(dead_code)]
pub fn rainbow_brush(offset: f32, alpha: f32) -> peniko::Brush {
    use peniko::color::DynamicColor;
    use peniko::{Extend, Gradient};

    fn c(r: u8, g: u8, b: u8, a: f32) -> DynamicColor {
        peniko::Color::from_rgba8(r, g, b, (a * 255.0) as u8).into()
    }

    // Tokyo Night palette rainbow — vibrant but theme-cohesive.
    // 7 stops wrapping red→red for smooth cycling.
    let palette: [(f32, DynamicColor); 7] = [
        (0.00, c(247, 118, 142, alpha)), // #f7768e red
        (0.17, c(224, 175, 104, alpha)), // #e0af68 amber
        (0.33, c(158, 206, 106, alpha)), // #9ece6a green
        (0.50, c(125, 207, 255, alpha)), // #7dcfff cyan
        (0.67, c(122, 162, 247, alpha)), // #7aa2f7 blue
        (0.83, c(187, 154, 247, alpha)), // #bb9af7 purple
        (1.00, c(247, 118, 142, alpha)), // wrap back to red
    ];

    // One full rainbow cycle in pixels. Short enough that even a few
    // characters show the full spectrum.
    let cycle_px = 400.0_f64;

    // Shift the gradient origin by offset, creating the scroll effect.
    // Extend::Repeat tiles the palette seamlessly beyond cycle_px.
    let shift = (offset as f64) * cycle_px;

    Gradient::new_linear((-shift, 0.0), (cycle_px - shift, 0.0))
        .with_extend(Extend::Repeat)
        .with_stops(palette)
        .into()
}
