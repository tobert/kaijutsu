//! The 3D scene lane's palette resource — the app-side face of `[scene]` in
//! theme.toml (docs/color.md).
//!
//! [`ScenePalette`] carries the scenes' identity hues (linearized for the HDR
//! pipeline), the brightness tier ladder, live-signal gains, and the camera
//! post chain. It is initialized from compiled defaults (mirroring
//! [`SceneData::default()`], tested below) so the app renders correctly
//! before the kernel answers, and replaced wholesale when a theme arrives
//! over RPC (`connection::actor_plugin::apply_theme_from_rpc`).
//!
//! Apply semantics: `[scene.post]` hot-applies to the live camera via
//! [`apply_scene_post_on_change`]; hues/tiers/gains are read at *spawn time*,
//! so a running room re-skins on the next room entry — a documented trade
//! (materials are built once at spawn), not an accident.
//!
//! Color-space contract: theme hues are sRGB hex in the file; everything in
//! this resource is **linear** (`LinearRgba`), ready to feed unlit/emissive
//! materials and shader uniforms directly. Don't hand these to the post-
//! tonemap UI shader lane — that lane wants sRGB (see docs/color.md).

use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::post_process::bloom::Bloom;
use bevy::prelude::*;
use kaijutsu_types::theme::SceneData;

/// The scene lane's colors, tiers, gains, and post chain. See module docs.
#[derive(Resource, Debug, Clone)]
pub struct ScenePalette {
    // ── Identity hues (linear) ──
    /// Room clear color (the octagon's void).
    pub bg: LinearRgba,
    /// The room's one metal: trim, console rings, etch, well core.
    pub gold: LinearRgba,
    /// Hardware: sockets, pegs, jacks.
    pub brass: LinearRgba,
    /// Information-violet: radiator glass backdrop.
    pub violet_glass: LinearRgba,
    /// Information-violet: thread/content strips.
    pub violet_thread: LinearRgba,
    /// The well's electric indigo-violet (ring deck + track rays).
    pub neon: LinearRgba,
    /// Terrace glyph rings: a paler tint of `neon`.
    pub terrace: LinearRgba,
    /// Patch-bay chord wire hue (normalized; resting HDR in `gains.wire`).
    pub wire: LinearRgba,
    /// Circuit-board floor trace fabrics.
    pub trace_crimson: LinearRgba,
    pub trace_cyan: LinearRgba,
    pub trace_green: LinearRgba,
    pub trace_gold: LinearRgba,
    /// Structural near-blacks (silhouettes, not brightness).
    pub table: LinearRgba,
    pub wall_base: LinearRgba,
    pub wall_mullion: LinearRgba,
    pub dark_surface: LinearRgba,
    /// FSN landscape (`docs/scenes/vfs.md` slice 0, `view::fsn`): prism
    /// wireframe edges — neon violet.
    pub fsn_edge: LinearRgba,
    /// FSN landscape: prism-top vertex points — magenta.
    pub fsn_vertex: LinearRgba,
    /// FSN landscape: quad-seam grid lines — faint violet, dimmer than
    /// `fsn_edge`.
    pub fsn_seam: LinearRgba,

    // ── Brightness tier ladder ──
    pub etch: f32,
    pub marker: f32,
    pub trim: f32,
    pub hardware: f32,
    pub crest: f32,
    pub trough_wiring: f32,
    pub trough_wall_trim: f32,
    pub trough_pads: f32,
    pub trough_subtle: f32,

    // ── Live-signal gains (allowed to sustain HDR) ──
    pub gain_pulse: f32,
    pub gain_chord_selected: f32,
    pub gain_wire: f32,
    pub gain_beat: f32,
    pub gain_active: f32,
    pub gain_focus_lift: f32,
    pub gain_reading_border: f32,
    pub gain_hud_border: f32,

    // ── Post chain (hot-applies) ──
    pub bloom_intensity: f32,
    pub bloom_low_frequency_boost: f32,
    pub tonemapper: Tonemapping,
}

impl ScenePalette {
    /// Convenience: a hue as `Vec3` for material uniforms.
    pub fn vec3(c: LinearRgba) -> Vec3 {
        Vec3::new(c.red, c.green, c.blue)
    }

    /// Build from parsed theme data. A malformed hex or unknown tonemapper
    /// name keeps that field's compiled default and logs an error naming the
    /// field — loud enough to notice, proportionate to a config typo (the
    /// rest of the theme still applies).
    pub fn from_scene_data(scene: &SceneData) -> Self {
        let d = Self::default();
        let hue = |name: &str, hex: &str, fallback: LinearRgba| -> LinearRgba {
            match Srgba::hex(hex) {
                Ok(c) => c.into(),
                Err(e) => {
                    log::error!("[scene.hues] {name} = {hex:?} is not a valid hex color ({e}); keeping default");
                    fallback
                }
            }
        };
        let h = &scene.hues;
        let t = &scene.tiers;
        let g = &scene.gains;
        let p = &scene.post;
        Self {
            bg: hue("bg", &h.bg, d.bg),
            gold: hue("gold", &h.gold, d.gold),
            brass: hue("brass", &h.brass, d.brass),
            violet_glass: hue("violet_glass", &h.violet_glass, d.violet_glass),
            violet_thread: hue("violet_thread", &h.violet_thread, d.violet_thread),
            neon: hue("neon", &h.neon, d.neon),
            terrace: hue("terrace", &h.terrace, d.terrace),
            wire: hue("wire", &h.wire, d.wire),
            trace_crimson: hue("trace_crimson", &h.trace_crimson, d.trace_crimson),
            trace_cyan: hue("trace_cyan", &h.trace_cyan, d.trace_cyan),
            trace_green: hue("trace_green", &h.trace_green, d.trace_green),
            trace_gold: hue("trace_gold", &h.trace_gold, d.trace_gold),
            table: hue("table", &h.table, d.table),
            wall_base: hue("wall_base", &h.wall_base, d.wall_base),
            wall_mullion: hue("wall_mullion", &h.wall_mullion, d.wall_mullion),
            dark_surface: hue("dark_surface", &h.dark_surface, d.dark_surface),
            fsn_edge: hue("fsn_edge", &h.fsn_edge, d.fsn_edge),
            fsn_vertex: hue("fsn_vertex", &h.fsn_vertex, d.fsn_vertex),
            fsn_seam: hue("fsn_seam", &h.fsn_seam, d.fsn_seam),

            etch: t.etch,
            marker: t.marker,
            trim: t.trim,
            hardware: t.hardware,
            crest: t.crest,
            trough_wiring: t.trough_wiring,
            trough_wall_trim: t.trough_wall_trim,
            trough_pads: t.trough_pads,
            trough_subtle: t.trough_subtle,

            gain_pulse: g.pulse,
            gain_chord_selected: g.chord_selected,
            gain_wire: g.wire,
            gain_beat: g.beat,
            gain_active: g.active,
            gain_focus_lift: g.focus_lift,
            gain_reading_border: g.reading_border,
            gain_hud_border: g.hud_border,

            bloom_intensity: p.bloom_intensity,
            bloom_low_frequency_boost: p.bloom_low_frequency_boost,
            tonemapper: tonemapper_by_name(&p.tonemapper).unwrap_or_else(|| {
                log::error!(
                    "[scene.post] tonemapper = {:?} is not a known tonemapper; keeping {:?}",
                    p.tonemapper,
                    d.tonemapper
                );
                d.tonemapper
            }),
        }
    }
}

impl Default for ScenePalette {
    /// Compiled defaults — the exact linear values the scene modules shipped
    /// with (palette.rs / room / time_well / patch_bay constants as of the
    /// color pass). A test pins these ≈ `SceneData::default()`'s hex.
    fn default() -> Self {
        let lin = |r: f32, g: f32, b: f32| LinearRgba::rgb(r, g, b);
        Self {
            // ROOM_BG shipped as Color::srgb(0.020, 0.026, 0.044) — sRGB
            // numbers, unlike every other scene constant (linear). Linearize
            // here so this resource keeps its all-linear contract. (The
            // module's mirror test caught exactly this on first run.)
            bg: Srgba::new(0.020, 0.026, 0.044, 1.0).into(),
            gold: lin(1.00, 0.78, 0.34),
            brass: lin(0.72, 0.55, 0.25),
            violet_glass: lin(0.090, 0.040, 0.150),
            violet_thread: lin(0.550, 0.180, 0.750),
            neon: lin(0.42, 0.30, 0.90),
            terrace: lin(0.55, 0.45, 0.95),
            wire: lin(1.0, 0.16 / 1.4, 0.24 / 1.4),
            trace_crimson: lin(0.24, 0.055, 0.070),
            trace_cyan: lin(0.050, 0.170, 0.210),
            trace_green: lin(0.100, 0.260, 0.150),
            trace_gold: lin(0.300, 0.220, 0.100),
            table: lin(0.032, 0.036, 0.050),
            wall_base: lin(0.062, 0.060, 0.094),
            wall_mullion: lin(0.040, 0.040, 0.058),
            dark_surface: lin(0.012, 0.013, 0.019),
            // Frame 45's edge-line/vertex-point hues (docs/scenes/vfs.md).
            fsn_edge: lin(0.254, 0.107, 1.000),
            fsn_vertex: lin(1.000, 0.028, 0.631),
            fsn_seam: lin(0.068, 0.036, 0.138),

            etch: 0.28,
            marker: 0.42,
            trim: 0.50,
            hardware: 0.55,
            crest: 1.25,
            trough_wiring: 0.55,
            trough_wall_trim: 0.60,
            trough_pads: 0.65,
            trough_subtle: 0.75,

            gain_pulse: 6.0,
            gain_chord_selected: 3.4,
            gain_wire: 1.4,
            gain_beat: 2.8,
            gain_active: 0.5,
            gain_focus_lift: 0.35,
            gain_reading_border: 1.6,
            gain_hud_border: 1.8,

            // ACES + raised bloom: the 2026-07-12 live A/B winner for the
            // synthwave target (mirrors ScenePostData::default()).
            bloom_intensity: 0.22,
            bloom_low_frequency_boost: 0.25,
            tonemapper: Tonemapping::AcesFitted,
        }
    }
}

/// Map a `[scene.post]` tonemapper name to Bevy's enum. `None` = unknown name
/// (caller decides the fallback and does the shouting).
pub fn tonemapper_by_name(name: &str) -> Option<Tonemapping> {
    Some(match name {
        "tony_mc_mapface" => Tonemapping::TonyMcMapface,
        "aces" => Tonemapping::AcesFitted,
        "agx" => Tonemapping::AgX,
        "blender_filmic" => Tonemapping::BlenderFilmic,
        "reinhard" => Tonemapping::Reinhard,
        "reinhard_luminance" => Tonemapping::ReinhardLuminance,
        "somewhat_boring" => Tonemapping::SomewhatBoringDisplayTransform,
        "none" => Tonemapping::None,
        _ => return None,
    })
}

/// An unlit-material [`Color`] from a linear hue (values may exceed 1.0 —
/// the scene lane's HDR-capable constructor). Shared by every scene module
/// that reads [`ScenePalette`] hues (previously duplicated per-module
/// `lin`/`lin_scaled` helpers in `room/mod.rs` and `patch_bay/mod.rs`).
pub(crate) fn lin(c: LinearRgba) -> Color {
    Color::LinearRgba(c)
}

/// [`lin`] scaled by a brightness tier or gain — the palette's hue × tier
/// convention (docs/color.md's tier ladder).
pub(crate) fn lin_scaled(c: LinearRgba, k: f32) -> Color {
    Color::LinearRgba(LinearRgba::rgb(c.red * k, c.green * k, c.blue * k))
}

/// FSN recency glow's baked vertex-color half of the color-composition law
/// (`view::fsn` slice 1, lane A): a per-channel tint that, multiplied into
/// `base` at render time by the material's own `base_color` (the
/// `apply_fsn_lod`-owned half), reproduces `lerp(base, gold, w)` exactly —
/// `tint_c = lerp(1.0, gold_c / base_c, w)`, so `tint_c × base_c =
/// lerp(base_c, gold_c, w)` for every channel. `w = 0` yields all-ones (no
/// tint at all — the untouched base color survives the multiply unchanged),
/// `w = 1` yields the full `gold / base` ratio (base × that ratio = gold
/// exactly). A near-zero `base` channel (`< 1e-6`) would blow the ratio up
/// toward infinity for no visible gain (a near-black channel has nothing to
/// tint), so that channel's tint holds at 1.0 regardless of `w` — the guard
/// this fn's own doc promises. Alpha is always 1.0 (recency never carries
/// transparency).
pub(crate) fn warmth_tint(base: LinearRgba, gold: LinearRgba, w: f32) -> [f32; 4] {
    let chan = |b: f32, g: f32| -> f32 { if b.abs() < 1e-6 { 1.0 } else { 1.0 + w * (g / b - 1.0) } };
    [chan(base.red, gold.red), chan(base.green, gold.green), chan(base.blue, gold.blue), 1.0]
}

/// Hot-apply `[scene.post]` to the shared camera whenever the palette
/// resource changes (theme RPC, or a live mutation over BRP). Bloom threshold
/// is deliberately NOT driven from the palette — 1.0 is the HDR-tell
/// boundary contract (docs/color.md).
pub fn apply_scene_post_on_change(
    palette: Res<ScenePalette>,
    // `Without<FsnBackdropCamera>`: defense-in-depth. The FSN backdrop's
    // off-screen RTT camera (`view::fsn::backdrop`) renders to an LDR
    // target with a deliberate `Tonemapping::None` and no Bloom — today it
    // can't match this query anyway (no `Bloom` component), but if someone
    // later adds Bloom to the backdrop this filter keeps the palette's
    // display post chain from silently retargeting a render texture that
    // was never meant to receive it.
    mut cameras: Query<
        (&mut Bloom, &mut Tonemapping),
        (With<Camera3d>, Without<crate::view::fsn::backdrop::FsnBackdropCamera>),
    >,
) {
    if !palette.is_changed() {
        return;
    }
    for (mut bloom, mut tonemapping) in cameras.iter_mut() {
        bloom.intensity = palette.bloom_intensity;
        bloom.low_frequency_boost = palette.bloom_low_frequency_boost;
        if *tonemapping != palette.tonemapper {
            *tonemapping = palette.tonemapper;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compiled defaults and `SceneData::default()`'s hex encodings are
    /// two writings of the same palette — hold them together (hex is 8-bit,
    /// so allow one quantization step ≈ 1/255 in sRGB ≈ ~0.004 linear at
    /// these magnitudes).
    #[test]
    fn compiled_defaults_mirror_scene_data_defaults() {
        let compiled = ScenePalette::default();
        let parsed = ScenePalette::from_scene_data(&SceneData::default());
        let close = |a: LinearRgba, b: LinearRgba, name: &str| {
            for (ca, cb, ch) in [
                (a.red, b.red, "r"),
                (a.green, b.green, "g"),
                (a.blue, b.blue, "b"),
            ] {
                assert!(
                    (ca - cb).abs() < 0.005,
                    "{name}.{ch}: compiled {ca} vs theme-default {cb}"
                );
            }
        };
        close(compiled.bg, parsed.bg, "bg");
        close(compiled.gold, parsed.gold, "gold");
        close(compiled.brass, parsed.brass, "brass");
        close(compiled.violet_glass, parsed.violet_glass, "violet_glass");
        close(compiled.violet_thread, parsed.violet_thread, "violet_thread");
        close(compiled.neon, parsed.neon, "neon");
        close(compiled.terrace, parsed.terrace, "terrace");
        close(compiled.wire, parsed.wire, "wire");
        close(compiled.trace_crimson, parsed.trace_crimson, "trace_crimson");
        close(compiled.trace_cyan, parsed.trace_cyan, "trace_cyan");
        close(compiled.trace_green, parsed.trace_green, "trace_green");
        close(compiled.trace_gold, parsed.trace_gold, "trace_gold");
        close(compiled.table, parsed.table, "table");
        close(compiled.wall_base, parsed.wall_base, "wall_base");
        close(compiled.wall_mullion, parsed.wall_mullion, "wall_mullion");
        close(compiled.dark_surface, parsed.dark_surface, "dark_surface");
        close(compiled.fsn_edge, parsed.fsn_edge, "fsn_edge");
        close(compiled.fsn_vertex, parsed.fsn_vertex, "fsn_vertex");
        close(compiled.fsn_seam, parsed.fsn_seam, "fsn_seam");
        assert_eq!(compiled.etch, parsed.etch);
        assert_eq!(compiled.crest, parsed.crest);
        assert_eq!(compiled.gain_pulse, parsed.gain_pulse);
        assert_eq!(compiled.tonemapper, parsed.tonemapper);
        assert_eq!(compiled.bloom_intensity, parsed.bloom_intensity);
    }

    #[test]
    fn malformed_hex_keeps_the_compiled_default_for_that_field_only() {
        let mut scene = SceneData::default();
        scene.hues.gold = "not-a-color".into();
        scene.hues.neon = "#ff00ff".into();
        let p = ScenePalette::from_scene_data(&scene);
        let d = ScenePalette::default();
        assert_eq!(p.gold, d.gold, "bad hex falls back to compiled default");
        let magenta: LinearRgba = Srgba::hex("#ff00ff").unwrap().into();
        assert_eq!(p.neon, magenta, "good fields still apply");
    }

    #[test]
    fn unknown_tonemapper_keeps_default_and_known_names_map() {
        assert_eq!(
            tonemapper_by_name("blender_filmic"),
            Some(Tonemapping::BlenderFilmic)
        );
        assert_eq!(tonemapper_by_name("none"), Some(Tonemapping::None));
        assert_eq!(tonemapper_by_name("vhs"), None);
        let mut scene = SceneData::default();
        scene.post.tonemapper = "vhs".into();
        let p = ScenePalette::from_scene_data(&scene);
        assert_eq!(p.tonemapper, ScenePalette::default().tonemapper);
    }

    // ── warmth_tint (FSN recency glow's color-composition law) ──

    #[test]
    fn warmth_tint_times_base_reproduces_the_lerp_toward_gold() {
        let base = LinearRgba::rgb(0.254, 0.107, 1.0); // fsn_edge
        let gold = LinearRgba::rgb(1.00, 0.78, 0.34);
        for w in [0.0_f32, 0.25, 0.5, 0.75, 1.0] {
            let tint = warmth_tint(base, gold, w);
            let want = [
                base.red + w * (gold.red - base.red),
                base.green + w * (gold.green - base.green),
                base.blue + w * (gold.blue - base.blue),
            ];
            let got = [tint[0] * base.red, tint[1] * base.green, tint[2] * base.blue];
            for (g, wa) in got.iter().zip(want.iter()) {
                assert!((g - wa).abs() < 1e-5, "w={w}: got {g} want {wa}");
            }
            assert_eq!(tint[3], 1.0, "alpha is always opaque");
        }
    }

    #[test]
    fn warmth_tint_at_zero_weight_is_all_ones() {
        let base = LinearRgba::rgb(0.254, 0.107, 1.0);
        let gold = LinearRgba::rgb(1.00, 0.78, 0.34);
        assert_eq!(warmth_tint(base, gold, 0.0), [1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn warmth_tint_guards_a_near_zero_base_channel() {
        // A base channel near zero would blow the gold/base ratio toward
        // infinity for a channel with nothing visible to tint — must hold
        // at 1.0 (no tint), not explode or divide-by-zero into NaN/inf.
        let base = LinearRgba::rgb(0.0, 0.5, 1.0);
        let gold = LinearRgba::rgb(1.0, 1.0, 1.0);
        let tint = warmth_tint(base, gold, 1.0);
        assert_eq!(tint[0], 1.0, "near-zero base channel must not blow up");
        assert!(tint[0].is_finite() && tint[1].is_finite() && tint[2].is_finite());
    }

    #[test]
    fn parsed_troughs_never_sustain_hdr() {
        let p = ScenePalette::from_scene_data(&SceneData::default());
        for (name, trough) in [
            ("wiring", p.trough_wiring),
            ("wall_trim", p.trough_wall_trim),
            ("pads", p.trough_pads),
            ("subtle", p.trough_subtle),
        ] {
            assert!(
                trough * p.crest < 1.0,
                "trough_{name} ({trough}) × crest ({}) must stay < 1.0",
                p.crest
            );
        }
    }
}
