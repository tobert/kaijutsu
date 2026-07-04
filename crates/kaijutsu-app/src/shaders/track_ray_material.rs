//! Track ray material — a track's presence in the time well.
//!
//! Each track (a durable clock domain — docs/tracks.md) renders as one thin
//! beam lying on the funnel wall, from the vortex throat out through the magic
//! rings to the mouth, at an angle hashed stably from the track's name (see
//! `view::time_well::rays`). The ray is the *lane made visible*: contexts
//! churn, the ray persists. While the track's clock rolls, a bright pulse
//! rides the beam from the mouth down into the throat on every beat (energy
//! falling into the well), phase-driven by the app-local beat phasor
//! ([`crate::view::time_well::live::WellBeats`]) — *distribute tempo, not
//! pulses*. A stopped track is a dim, steady filament (LDR — passive
//! structure; the bright/blooming vocabulary stays reserved for live action).

use bevy::prelude::*;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;

/// Material for one track ray (one beam quad per track).
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct TrackRayMaterial {
    /// Track hue (linear rgb in `.xyz` — the same stable name-hashed hue the
    /// attached cards' borders use); `.w` = base alpha of the filament.
    #[uniform(0)]
    pub color: Vec4,

    /// `[beat_env, playing, beat_frac, activity]`:
    /// - `beat_env` — the beat envelope (1.0 on the beat, decaying through it);
    /// - `playing` — 0/1 transport gate for the pulse;
    /// - `beat_frac` — fractional position within the current beat (0..1),
    ///   which the shader maps to the pulse's position along the beam;
    /// - `activity` — decaying event energy of the track's attached contexts
    ///   (0..1), lifting the whole filament while its players talk.
    #[uniform(1)]
    pub params: Vec4,
}

impl TrackRayMaterial {
    /// A quiet ray in `color`: stopped, no pulse, no activity.
    pub fn new(color: Vec4) -> Self {
        Self { color, params: Vec4::ZERO }
    }
}

impl Material for TrackRayMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/track_ray.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        // A beam of light over the deck: additive-feeling blend, soft edges.
        AlphaMode::Blend
    }
}
