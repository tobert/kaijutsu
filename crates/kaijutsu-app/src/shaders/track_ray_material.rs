//! Track ray material — a track's presence in the time well.
//!
//! Each track (a durable clock domain — docs/tracks.md) renders as one thin
//! beam lying on the funnel wall, from the vortex throat out through the magic
//! rings to the mouth, at an angle hashed stably from the track's name (see
//! `view::time_well::rays`). The ray is the *lane made visible*: contexts
//! churn, the ray persists. While the track's clock rolls, a bright pulse
//! rides the beam from the mouth down into the throat on every beat (energy
//! falling into the well), phase-driven by the app-local beat phasor
//! ([`kaijutsu_present::beats::WellBeats`]). A steady filament is dim when
//! stopped and brighter when the roster reports playing without a phasor.

use bevy::prelude::*;
use bevy::material::AlphaMode;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;

/// Material for one track ray (one beam quad per track).
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct TrackRayMaterial {
    /// Track hue (linear rgb in `.xyz` — the same stable name-hashed hue the
    /// attached cards' borders use); `.w` = base alpha of the filament.
    #[uniform(0)]
    pub color: Vec4,

    /// `[beat_env, clock, beat_frac, _]`:
    /// - `beat_env` — the beat envelope (1.0 on the beat, decaying through it);
    /// - `clock` — 0.0 stopped, 0.5 playing without a phasor, 1.0 live phasor;
    /// - `beat_frac` — fractional position within the current beat (0..1),
    ///   which the shader maps to the pulse's position along the beam.
    #[uniform(1)]
    pub params: Vec4,
}

impl TrackRayMaterial {
    /// A stopped ray in `color`.
    pub fn new(color: Vec4) -> Self {
        Self { color, params: Vec4::ZERO }
    }
}

impl Material for TrackRayMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/track_ray.wesl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        // A beam of light over the deck: additive-feeling blend, soft edges.
        AlphaMode::Blend
    }
}
