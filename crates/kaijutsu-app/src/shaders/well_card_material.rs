//! Well card material — the 3D card for the time well (rim cards + focus card).
//!
//! `WellCardMaterial` is a 3D `Material` that draws the *entire* card on the GPU:
//! the accent rounded-rect background, the selection/lineage rings (SDF, from
//! `params`), and the MSDF text composited on top (the `texture`, which the MSDF
//! pass renders text-on-transparent into). This is the vello-free well — vello no
//! longer touches card textures (it stays for SVG/ABC elsewhere). Glow is SDF in
//! the fragment shader (no HDR/bloom), matching `block_fx`.

use bevy::prelude::*;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;

/// Material for a single time-well card (rim card or focus card).
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct WellCardMaterial {
    /// MSDF text texture (text on transparent — the MSDF pass clears + renders it).
    #[texture(0)]
    #[sampler(1)]
    pub texture: Handle<Image>,

    /// Accent background color (linear rgba). Fills the rounded-rect body.
    #[uniform(2)]
    pub accent: Vec4,

    /// `[selected, in_lineage, status, time]` — drives the rings (and future FX).
    #[uniform(3)]
    pub params: Vec4,

    /// `[aspect (w/h), corner_radius, ring_width, inset]` in the shader's
    /// aspect-corrected UV space.
    #[uniform(4)]
    pub shape: Vec4,
}

impl Material for WellCardMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/well_card.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        // Masked alpha is order-independent (the rounded-rect body is opaque; only
        // outside the corners falls below the cutoff and is discarded).
        AlphaMode::Mask(0.5)
    }
}
