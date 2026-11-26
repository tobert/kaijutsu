//! Trace glow material — a faint, slowly moving glow for room decoration
//! (`docs/scenes/shell.md`; Amy, 2026-07-10 — "make the circuit patterns and
//! border glow faintly like the concepts... a lil bloom or some other
//! shader; something faintly moving might be interesting").
//!
//! Two modes, driven entirely by `globals.time` — one CPU uniform write per
//! spawned element (phase/rate/trough baked in at spawn time from
//! `bearing::hash01`), zero per-frame material churn: here nothing EVER
//! needs a second write.
//! - **mode 0 — traveling wave**. A crest follows the floor trace along
//!   its cumulative arc length in `uv.x`.
//! - **mode 1 — breathing** (terminal pads, the inscribed ring): a slow
//!   uniform sine breath, ignoring `uv` entirely — safe on primitives with
//!   their own UV convention (`Torus`, `Annulus`) that this material never
//!   has to match.
//!
//! `color` may carry brightness above 1.0 at the crest/breath's peak — the
//! app camera's threshold-1.0 bloom pass (`main::setup_camera`) haloes it
//! softly. Floor traces and terminal pads keep the theme's authored
//! brightness up to the scene crest limit. The inscribed ring
//! uses the full crest. Each material supplies its own trough fraction.

use bevy::prelude::*;
use bevy::material::AlphaMode;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;

/// Glow for the floor traces, terminal pads, inscribed ring and lineage drapes.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct TraceGlowMaterial {
    /// Linear RGB at the crest. Values above one can bloom.
    #[uniform(0)]
    pub color: Vec4,

    /// `[phase, rate, trough, mode]`:
    /// - `phase` — a per-element offset drawn from `bearing::hash01` so a
    ///   population of elements shimmers asynchronously rather than in
    ///   lockstep. Mode 0 reads it as a `[0, 1)` cycle-fraction (it only ever
    ///   feeds a `fract()`); mode 1 reads it as radians (it feeds `sin()`).
    /// - `rate` — mode 0: cycles/second the crest travels (`1 / period`, a
    ///   crest crossing the whole element every `period` seconds); mode 1:
    ///   angular rate (rad/s) of the breath.
    /// - `trough` — the resting brightness fraction of `color` (0..1) the
    ///   element sits at away from the crest/breath's peak.
    /// - `mode` — 0.0 = traveling wave (reads `uv.x`), 1.0 = breathing
    ///   (uniform, ignores `uv`).
    #[uniform(1)]
    pub params: Vec4,
}

impl Material for TraceGlowMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/trace_glow.wesl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Opaque
    }
}
