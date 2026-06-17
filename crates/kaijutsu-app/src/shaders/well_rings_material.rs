//! Well rings material — the base "deck" of the time well.
//!
//! `WellRingsMaterial` draws a flat disc that sits behind the cards in the well's
//! XY plane (the camera looks down the −Z axis, so the disc reads face-on as the
//! concentric-ring deck + spiral core from the concept art, mockups 27/33). It is
//! the well's **pulse**: the rings brighten and quicken, the core spins faster,
//! and localized **ripples** expand from the angle of whichever context just did
//! something — all driven by the live kernel-event rate (see
//! [`super::super::view::time_well::activity`]). Bright values are **HDR** (>1.0)
//! so they spill into the app's single-camera bloom pass (`main::setup_camera`).

use bevy::prelude::*;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;

use crate::view::time_well::activity::MAX_RIPPLES;

/// Material for the well's base ring deck (one disc mesh).
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct WellRingsMaterial {
    /// `[energy, _, _, _]` — global activity level (0..~4). Drives ring
    /// brightness, flow speed, and core spin. Animation reads `globals.time`.
    #[uniform(0)]
    pub energy: Vec4,

    /// Spiral-core color (linear rgb in `.xyz`; `.w` unused). HDR-scaled in-shader.
    #[uniform(1)]
    pub core_color: Vec4,

    /// Concentric-ring color (linear rgb in `.xyz`; `.w` unused).
    #[uniform(2)]
    pub ring_color: Vec4,

    /// Live ripples: each `[cos(angle), sin(angle), age_norm (0..1), intensity]`.
    /// Unused slots carry `intensity = 0`. Length must match [`MAX_RIPPLES`] and
    /// the array in `well_rings.wgsl`.
    #[uniform(3)]
    pub ripples: [Vec4; MAX_RIPPLES],
}

impl WellRingsMaterial {
    /// All-quiet material: zero energy, no ripples, themed colors.
    pub fn new(core_color: Vec4, ring_color: Vec4) -> Self {
        Self {
            energy: Vec4::ZERO,
            core_color,
            ring_color,
            ripples: [Vec4::ZERO; MAX_RIPPLES],
        }
    }
}

impl Material for WellRingsMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/well_rings.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        // The deck is a translucent floor: it blends over the well background and
        // fades to nothing at the disc rim (the square quad's corners vanish).
        AlphaMode::Blend
    }
}
