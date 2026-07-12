//! Terrace-ring material — magic-circle glyph rings at the time well's terrace
//! boundaries (the Konosuba/"Explosion"-spell aesthetic — concentric glyph
//! rings, counter-rotating, receding into the funnel).
//!
//! `TerraceRingMaterial` draws a flat annulus quad, camera-facing like the base
//! ring deck (`WellRingsMaterial`), at each interior terrace boundary (see
//! `super::super::view::time_well::card::terrace_ring_geometry`). One material
//! instance per boundary, alternating spin direction and rate so the layers
//! counter-rotate. Bright values are **HDR** (>1.0) so they spill into the
//! app's single-camera bloom pass (`main::setup_camera`), same as the well
//! rings deck.

use bevy::prelude::*;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;

/// Material for one terrace-boundary magic-circle ring.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct TerraceRingMaterial {
    /// `[inner_radius_frac, outer_radius_frac, spin_rate, spin_dir]` — the
    /// annulus band as fractions of the quad's half-extent (0..1; the shader
    /// is transparent inside `inner_radius_frac` and outside
    /// `outer_radius_frac`, and at the quad's corners), the rotation speed
    /// (tune by eye), and the spin direction (`+1.0`/`-1.0` — the knob that
    /// makes adjacent layers counter-rotate). Animation reads `globals.time`.
    #[uniform(0)]
    pub params: Vec4,

    /// Glyph color: linear rgb in `.xyz` (HDR-scaled in-shader for bloom),
    /// `.w` = overall alpha/intensity multiplier.
    #[uniform(1)]
    pub color: Vec4,

    /// Ring identity: `[ring_index, ring_count, 0, 0]`. The shader derives the
    /// per-ring centerpiece variant (`ring_index % N_VARIANTS`) and every
    /// hash seed from this — deterministic per ring, stable across frames.
    #[uniform(2)]
    pub glyph: Vec4,

    /// Accent color (the room's gold, `palette::GOLD_HUE`): linear rgb in
    /// `.xyz`, `.w` reserved. The centerpiece's second tone — gold segments,
    /// rosette nodes, motes, and gem glints mix toward it so the terrace
    /// glyphs speak the room's gold-trim + violet-information language.
    #[uniform(3)]
    pub accent: Vec4,
}

impl TerraceRingMaterial {
    /// A ring band spanning `[inner_radius_frac, outer_radius_frac]` (fractions
    /// of the quad half-extent), spinning at `spin_rate` in direction
    /// `spin_dir` (`+1.0`/`-1.0`), themed `color` (linear rgb) at overall
    /// `alpha`. `ring_index`/`ring_count` are the ring's position in the
    /// funnel (mouth = 0), `accent` the centerpiece's second tone (gold).
    #[allow(clippy::too_many_arguments)] // flat uniform plumbing, one call site
    pub fn new(
        inner_radius_frac: f32,
        outer_radius_frac: f32,
        spin_rate: f32,
        spin_dir: f32,
        color: Vec3,
        alpha: f32,
        ring_index: usize,
        ring_count: usize,
        accent: Vec3,
    ) -> Self {
        Self {
            params: Vec4::new(inner_radius_frac, outer_radius_frac, spin_rate, spin_dir),
            color: Vec4::new(color.x, color.y, color.z, alpha),
            glyph: Vec4::new(ring_index as f32, ring_count as f32, 0.0, 0.0),
            accent: Vec4::new(accent.x, accent.y, accent.z, 0.0),
        }
    }
}

impl Material for TerraceRingMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/terrace_ring.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        // Transparent center + corners (the annulus band only), like the deck.
        AlphaMode::Blend
    }
}
