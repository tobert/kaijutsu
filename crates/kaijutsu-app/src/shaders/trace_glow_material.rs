//! Trace glow material — a faint, slowly moving glow for room decoration
//! (`docs/scenes/shell.md`; Amy, 2026-07-10 — "make the circuit patterns and
//! border glow faintly like the concepts... a lil bloom or some other
//! shader; something faintly moving might be interesting").
//!
//! Two modes, driven entirely by `globals.time` — one CPU uniform write per
//! spawned element (phase/rate/trough baked in at spawn time from
//! `bearing::hash01`), zero per-frame material churn (the same
//! event-driven material-lane trick [`super::ChordMaterial`] uses for its
//! traffic pulse, taken one step further: here nothing EVER needs a second
//! write):
//! - **mode 0 — traveling wave** (floor traces, wall trim): one bright crest
//!   glides along `uv.x` (`bearing::ribbon_vertices`'s cumulative-arclength
//!   parametrization, or a length-tracking quad UV — `room::glow_quad_mesh`)
//!   while the rest of the element rests at `trough`.
//! - **mode 1 — breathing** (terminal pads, the inscribed ring): a slow
//!   uniform sine breath, ignoring `uv` entirely — safe on primitives with
//!   their own UV convention (`Torus`, `Annulus`) that this material never
//!   has to match. (The W dais bezel was a user until the wheel wall-mounted
//!   and the dais retired, 2026-07-10.)
//!
//! `color` may carry brightness above 1.0 at the crest/breath's peak — the
//! app camera's threshold-1.0 bloom pass (`main::setup_camera`) haloes it
//! softly. Every spawn site renormalizes its identity hue so the peak lands
//! at exactly [`crate::view::scene_palette::ScenePalette::crest`]
//! (`room::crest_color`), and picks a `trough` low enough that
//! `trough * crest < 1.0` — the element's resting state (and its
//! time-average) stays LDR even though the crest blooms (`view::palette`'s
//! amended material discipline).

use bevy::prelude::*;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;

/// Material for one glowing decoration element: a floor-trace ribbon, a
/// terminal pad disc, an inscribed-ring annulus, or a wall-trim strip.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct TraceGlowMaterial {
    /// Crest hue: linear rgb in `.xyz`, renormalized so its brightest channel
    /// is exactly [`crate::view::scene_palette::ScenePalette::crest`] (may
    /// exceed 1.0 — HDR, blooms); `.w` unused (every element this material
    /// draws is opaque).
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
        "shaders/trace_glow.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        // Opaque, DEFAULT cull (no `cull_mode` override here): the wall-trim
        // quads are single-sided, inward-facing, and the octagon's
        // dollhouse-cutaway read depends on the pipeline's default back-face
        // culling (`bearing`'s own module doc has the mechanics) — this
        // material must never defeat it. The floor ribbons face up and the
        // camera is always above, so the same default cull never hides them.
        AlphaMode::Opaque
    }
}
