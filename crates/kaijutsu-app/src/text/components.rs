//! Text effect markers and color/brush helpers for Kaijutsu.

use bevy::prelude::*;

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
    super::msdf::layout_bridge::rgba_unit_to_u8([c.red, c.green, c.blue, c.alpha])
}
