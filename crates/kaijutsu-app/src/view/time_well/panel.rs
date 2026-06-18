//! In-scene MSDF panel primitive.
//!
//! An "MSDF panel" is the shared shape behind every in-scene text surface in the
//! well: a `Mesh3d` quad whose material samples a [`UiRttTexture`] that the MSDF
//! pass rasterizes glyphs into. Rim cards, the reading/focus card, and the edge
//! HUD all build on it — they differ only in mesh size, material params, and what
//! text they lay out, never in the texture/MSDF plumbing.
//!
//! The primitive owns exactly two things:
//! - allocation: [`create_msdf_panel`] makes the RTT texture and the component
//!   set the MSDF render pipeline keys on (a pure-MSDF surface — no
//!   [`UiVectorScene`](crate::view::ui_rtt::UiVectorScene), no vello);
//! - the rebuild signal: [`commit_panel_glyphs`] swaps a freshly-laid-out glyph
//!   set in and bumps the MSDF version so the render pass re-rasterizes.
//!
//! The caller owns the rest — the mesh, the material (`WellCardMaterial`), and
//! the glyph layout (cards carry fields + rings; the HUD lays out readout text).

use bevy::prelude::*;

use crate::text::msdf::{BlockRenderMethod, MsdfBlockGlyphs, PositionedGlyph};
use crate::view::ui_rtt::{UiRttTexture, create_ui_rtt_texture};

/// Allocate an RTT image sized `tex_w`×`tex_h` (physical == logical build space)
/// and return it alongside the component bundle every in-scene MSDF panel
/// carries. The caller adds `Mesh3d` + `MeshMaterial3d(material sampling the
/// returned image)` + `Transform` + `Visibility` + `Name`.
///
/// Pure MSDF: the bundle has no [`UiVectorScene`](crate::view::ui_rtt::UiVectorScene)
/// — the MSDF pass clears and owns the texture; the shader draws the body.
pub fn create_msdf_panel(
    images: &mut Assets<Image>,
    tex_w: u32,
    tex_h: u32,
) -> (Handle<Image>, impl Bundle) {
    let image = create_ui_rtt_texture(images, tex_w, tex_h);
    let bundle = (
        UiRttTexture {
            image: image.clone(),
            width: tex_w,
            height: tex_h,
            built_width: tex_w as f32,
            built_height: tex_h as f32,
        },
        MsdfBlockGlyphs::default(),
        BlockRenderMethod::Msdf,
    );
    (image, bundle)
}

/// Stamp a freshly-laid-out glyph set onto a panel and request a re-render. The
/// logical build size lives on the panel's [`UiRttTexture`] (set once at spawn
/// and constant), so this only swaps the glyphs and bumps the MSDF version —
/// the sole "re-rasterize me" signal the MSDF extract gates on.
pub fn commit_panel_glyphs(msdf: &mut MsdfBlockGlyphs, glyphs: Vec<PositionedGlyph>) {
    msdf.glyphs = glyphs;
    msdf.version = msdf.version.wrapping_add(1);
}
