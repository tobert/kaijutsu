//! Chord material — the patch bay's emissive wire ribbons (`docs/scenes/patchbay.md`).
//!
//! Each observed subscription draws as one ribbon bowing around the open center.
//! A live wire glows in its fabric hue (crimson = MIDI), pushed **HDR** (>1.0) so
//! the app's single bloom pass halos it — "bright is live". The material carries a
//! per-pulse **timestamp** (`params.y`, in `globals.time` units): when the app's
//! render port sends MIDI, a bright packet travels the chord source→dest, animated
//! entirely on the GPU against `globals.time` — one CPU uniform write per pulse, no
//! per-frame material churn (the same event-driven material-lane trick the well's
//! track rays use for the beat packet). Unlit: the ribbon *is* light, so it needs
//! no scene lamp (the brass table + pegs went all-unlit too, 2026-07-10 — no
//! lit `StandardMaterial` remains in the scene family, `view::palette`'s
//! material discipline).

use bevy::prelude::*;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;

/// Material for one patch-bay chord (one ribbon per observed wire).
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct ChordMaterial {
    /// Wire hue: linear rgb in `.xyz` (HDR — a live wire blooms), `.w` unused
    /// (the ribbon is opaque). Crimson for the MIDI fabric.
    #[uniform(0)]
    pub color: Vec4,

    /// `[selected, pulse_time, _, _]`:
    /// - `selected` — 0/1, the inspected chord (brighter idle glow);
    /// - `pulse_time` — `globals.time` (`Time::elapsed_secs_wrapped`) stamped at
    ///   the render port's last send; the shader rides a packet src→dest over the
    ///   next `tune.x` seconds. A sentinel far in the past ⇒ no packet (solid-lit,
    ///   the resting state and the state of every wire the app can't observe).
    #[uniform(1)]
    pub params: Vec4,

    /// `[travel, band_width, pulse_gain, selected_gain]` — the Amy-tunable pulse
    /// shape, uploaded from the scene constants so the WGSL and the CPU mirror
    /// (`view::patch_bay::pulse_band`) read the same numbers.
    #[uniform(2)]
    pub tune: Vec4,
}

impl Material for ChordMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/chord.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        // Opaque: the ribbon is a solid body of light. The ribbon mesh is built
        // double-sided (`view::patch_bay::ribbon_mesh`), so the fixed
        // look-down-at-the-table camera sees it from either face without a
        // pipeline cull override.
        AlphaMode::Opaque
    }
}
