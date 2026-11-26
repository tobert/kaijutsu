//! Well deck with concentric rings, a core lit by the loudest track beat,
//! and ripples at the bearings of contexts whose turns ended.

use bevy::prelude::*;
use bevy::material::AlphaMode;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;

use crate::view::time_well::activity::MAX_RIPPLES;

/// Material for the well's base ring deck (one disc mesh).
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct WellRingsMaterial {
    /// Envelope of the loudest track beat (0..1).
    #[uniform(0)]
    pub beat: f32,

    /// Spiral-core color (linear rgb in `.xyz`; `.w` unused). HDR-scaled in-shader.
    #[uniform(1)]
    pub core_color: Vec4,

    /// Concentric-ring color (linear rgb in `.xyz`; `.w` unused).
    #[uniform(2)]
    pub ring_color: Vec4,

    /// Each ripple packs its direction, normalized radius, and fading amplitude.
    /// Unused slots have zero amplitude. The array matches `well_rings.wesl`.
    #[uniform(3)]
    pub ripples: [Vec4; MAX_RIPPLES],
}

impl WellRingsMaterial {
    /// All-quiet material: zero beat, no ripples, themed colors.
    pub fn new(core_color: Vec4, ring_color: Vec4) -> Self {
        Self {
            beat: 0.0,
            core_color,
            ring_color,
            ripples: [Vec4::ZERO; MAX_RIPPLES],
        }
    }
}

impl Material for WellRingsMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/well_rings.wesl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        // The deck is a translucent floor: it blends over the well background and
        // fades to nothing at the disc rim (the square quad's corners vanish).
        AlphaMode::Blend
    }
}
