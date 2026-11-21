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
use kaijutsu_types::theme::{
    HeroicData, HeroicDurationsData, HeroicFogData, HeroicFramingData, HeroicSizesData, SceneData,
};

/// The scene lane's colors, tiers, gains, and post chain. See module docs.
#[derive(Resource, Debug, Clone, Reflect)]
#[reflect(Resource)]
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
    /// The waiting tier: a context with an approval ask pending. Pale cream,
    /// bright enough to sustain HDR (`connection::ledger`).
    pub ask: LinearRgba,
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

    // ── Brightness tier ladder ──
    pub marker: f32,
    pub trim: f32,
    pub crest: f32,
    pub trough_wiring: f32,
    pub trough_wall_trim: f32,
    pub trough_pads: f32,
    pub trough_subtle: f32,

    // ── Live-signal gains (allowed to sustain HDR) ──
    pub gain_beat: f32,
    pub gain_active: f32,
    pub gain_focus_lift: f32,
    pub gain_reading_border: f32,
    pub gain_hud_border: f32,

    // ── Post chain (hot-applies) ──
    pub bloom_intensity: f32,
    pub bloom_low_frequency_boost: f32,
    pub tonemapper: Tonemapping,

    // ── Lit-chamber mode (OPTIONAL; off by default — docs/color.md) ──
    /// Light the chamber shell instead of the all-unlit room. Read at spawn
    /// time: off spawns no lights and builds every material as the unlit path.
    pub lit_chamber: bool,
    /// `perceptual_roughness`/`reflectance` for the lit shell surfaces.
    pub chamber_roughness: f32,
    pub chamber_reflectance: f32,
    /// The one warm key light at the well (color linear; intensity in lumens,
    /// range in world units — the room is not metric, so these are large).
    pub key_light: LinearRgba,
    pub key_light_intensity: f32,
    pub key_light_range: f32,
    /// Faint warm ambient fill so the far walls are not pure black.
    pub ambient: LinearRgba,
    pub ambient_brightness: f32,
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
        let l = &scene.lighting;
        Self {
            bg: hue("bg", &h.bg, d.bg),
            gold: hue("gold", &h.gold, d.gold),
            brass: hue("brass", &h.brass, d.brass),
            violet_glass: hue("violet_glass", &h.violet_glass, d.violet_glass),
            violet_thread: hue("violet_thread", &h.violet_thread, d.violet_thread),
            neon: hue("neon", &h.neon, d.neon),
            terrace: hue("terrace", &h.terrace, d.terrace),
            ask: hue("ask", &h.ask, d.ask),
            trace_crimson: hue("trace_crimson", &h.trace_crimson, d.trace_crimson),
            trace_cyan: hue("trace_cyan", &h.trace_cyan, d.trace_cyan),
            trace_green: hue("trace_green", &h.trace_green, d.trace_green),
            trace_gold: hue("trace_gold", &h.trace_gold, d.trace_gold),
            table: hue("table", &h.table, d.table),
            wall_base: hue("wall_base", &h.wall_base, d.wall_base),
            wall_mullion: hue("wall_mullion", &h.wall_mullion, d.wall_mullion),
            dark_surface: hue("dark_surface", &h.dark_surface, d.dark_surface),

            marker: t.marker,
            trim: t.trim,
            crest: t.crest,
            trough_wiring: t.trough_wiring,
            trough_wall_trim: t.trough_wall_trim,
            trough_pads: t.trough_pads,
            trough_subtle: t.trough_subtle,

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

            lit_chamber: l.lit_chamber,
            chamber_roughness: l.chamber_roughness,
            chamber_reflectance: l.chamber_reflectance,
            key_light: hue("key_light_color", &l.key_light_color, d.key_light),
            key_light_intensity: l.key_light_intensity,
            key_light_range: l.key_light_range,
            ambient: hue("ambient_color", &l.ambient_color, d.ambient),
            ambient_brightness: l.ambient_brightness,
        }
    }
}

impl Default for ScenePalette {
    /// Compiled defaults — the exact linear values the scene modules shipped
    /// with (scene_geometry.rs / room / time_well constants as of the color
    /// pass). A test pins these ≈ `SceneData::default()`'s hex.
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
            ask: lin(1.00, 0.913, 0.540),
            trace_crimson: lin(0.24, 0.055, 0.070),
            trace_cyan: lin(0.050, 0.170, 0.210),
            trace_green: lin(0.100, 0.260, 0.150),
            trace_gold: lin(0.300, 0.220, 0.100),
            table: lin(0.032, 0.036, 0.050),
            wall_base: lin(0.062, 0.060, 0.094),
            wall_mullion: lin(0.040, 0.040, 0.058),
            dark_surface: lin(0.012, 0.013, 0.019),

            marker: 0.42,
            trim: 0.50,
            crest: 1.25,
            trough_wiring: 0.55,
            trough_wall_trim: 0.60,
            trough_pads: 0.65,
            trough_subtle: 0.75,

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

            // Lit-chamber mode OFF by default (mirrors SceneLightingData::
            // default()); the light colors parse from the same hex as the
            // theme so the mirror test holds. Intensity/range are first
            // guesses against the room's large non-metric scale — tuned live.
            lit_chamber: false,
            chamber_roughness: 0.5,
            chamber_reflectance: 0.3,
            key_light: Srgba::hex("#ffc58f").unwrap().into(),
            key_light_intensity: 5.0e8,
            key_light_range: 2000.0,
            ambient: Srgba::hex("#ffe6cc").unwrap().into(),
            ambient_brightness: 150.0,
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
        "none" => Tonemapping::Linear,
        _ => return None,
    })
}

/// An unlit-material [`Color`] from a linear hue, scaled by a brightness
/// tier or gain (values may exceed 1.0 — the scene lane's HDR-capable
/// constructor) — the palette's hue × tier convention (docs/color.md's tier
/// ladder). Shared by every scene module that reads [`ScenePalette`] hues.
pub(crate) fn lin_scaled(c: LinearRgba, k: f32) -> Color {
    Color::LinearRgba(LinearRgba::rgb(c.red * k, c.green * k, c.blue * k))
}

/// Shared identity-hue rule (`docs/newui.md`, "Identity hues: one rule, separate keys").
///
/// Hashes `key` via FNV-1a + `bearing::hash01`, decorrelates with the golden ratio,
/// and maps onto the hue bands outside the excluded trouble-red (`[0, 22)`,
/// `[340, 360)`), world-cyan (`[170, 200)`), and world-magenta (`[285, 340)`) ranges.
/// Callers supply their own key (character, nick, or context-type bucket) and
/// their own brightness/chroma.
pub fn identity_hue_degrees(key: &str) -> f32 {
    const GOLDEN_RATIO: f32 = 1.618_034;
    let mut h: u32 = 2166136261;
    for b in key.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    let draw = crate::view::room::bearing::hash01(h);
    let t = (draw * GOLDEN_RATIO).fract();
    // Allowed intervals: [22, 170) width 148, [200, 285) width 85 => total 233.
    let deg = t * 233.0;
    if deg < 148.0 {
        22.0 + deg
    } else {
        200.0 + (deg - 148.0)
    }
}

/// Derive a linear-RGB identity color from `key` at the caller's chosen HSL
/// `saturation` and `lightness`.
pub fn identity_color_linear(key: &str, saturation: f32, lightness: f32) -> LinearRgba {
    let hue = identity_hue_degrees(key);
    Color::hsl(hue, saturation, lightness).to_linear()
}

/// Single shared heroic identity lightness/saturation pair so notes, cages,
/// panels, ring arcs, chips, and pad flashes agree for the same performer key.
pub fn heroic_identity_color(key: &str) -> LinearRgba {
    identity_color_linear(key, 0.78, 0.56)
}

/// OKLCH -> OKLab -> linear sRGB rainbow ramp (`docs/newui.md`, "A rainbow ramp").
/// Matches `rainbow(t, lightness, chroma)` in `assets/shaders/kaijutsu/color.wesl`.
#[cfg(test)]
pub fn rainbow_oklch(t: f32, lightness: f32, chroma: f32) -> Vec3 {
    let h = t.rem_euclid(1.0) * std::f32::consts::TAU;
    let a = chroma * h.cos();
    let b = chroma * h.sin();

    let l_ = lightness + 0.3963377774 * a + 0.2158037573 * b;
    let m_ = lightness - 0.1055613458 * a - 0.0638541728 * b;
    let s_ = lightness - 0.0894841775 * a - 1.2914855480 * b;

    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;

    let r = (4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s).max(0.0);
    let g = (-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s).max(0.0);
    let b_out = (-0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s).max(0.0);
    Vec3::new(r, g, b_out)
}

// End of rainbow_oklch helpers.

/// Linearized palette and live tunables for the heroic view (`[heroic]` in
/// `theme.toml`, `docs/newui.md`). Hot-reloaded via `ThemeReceived`.
#[derive(Resource, Debug, Clone, Reflect)]
#[reflect(Resource)]
pub struct HeroicPalette {
    pub intensity: f32,
    pub reduced_motion: bool,
    pub display_offset_ms: i64,
    pub alarm_beats: f32,
    pub lane_width: f32,
    pub road_seconds: f32,
    pub units_per_second: f32,
    pub unselected_opacity: f32,
    #[reflect(ignore)]
    pub framing: HeroicFramingData,
    #[reflect(ignore)]
    pub fog: HeroicFogData,
    #[reflect(ignore)]
    pub sizes: HeroicSizesData,
    #[reflect(ignore)]
    pub durations_ms: HeroicDurationsData,
    pub rainbow_lightness: f32,
    pub rainbow_chroma_min: f32,
    pub rainbow_chroma_max: f32,
    pub sky_top: LinearRgba,
    pub sky_mid: LinearRgba,
    pub sky_low: LinearRgba,
    pub horizon: LinearRgba,
    pub fog_color: LinearRgba,
    pub ground: LinearRgba,
    pub hill_fill: LinearRgba,
    pub rail: LinearRgba,
    pub beat_line: LinearRgba,
    pub sun: [LinearRgba; 3],
    pub trouble: LinearRgba,
    pub now: LinearRgba,
}

impl Default for HeroicPalette {
    fn default() -> Self {
        Self::from_heroic_data(&HeroicData::default())
    }
}

impl HeroicPalette {
    pub fn from_heroic_data(h: &HeroicData) -> Self {
        let parse_hex = |name: &str, hex: &str, fallback: &str| -> LinearRgba {
            match Srgba::hex(hex) {
                Ok(c) => c.into(),
                Err(e) => {
                    log::error!(
                        "[heroic.hues] {name} = {hex:?} is not a valid hex color ({e}); keeping default"
                    );
                    Srgba::hex(fallback).unwrap().into()
                }
            }
        };
        let d = HeroicData::default();
        Self {
            intensity: h.intensity,
            reduced_motion: h.reduced_motion,
            display_offset_ms: h.display_offset_ms,
            alarm_beats: h.alarm_beats,
            lane_width: h.lane_width,
            road_seconds: h.road_seconds,
            units_per_second: h.units_per_second,
            unselected_opacity: h.unselected_opacity,
            framing: h.framing.clone(),
            fog: h.fog.clone(),
            sizes: h.sizes.clone(),
            durations_ms: h.durations_ms.clone(),
            rainbow_lightness: h.rainbow.lightness,
            rainbow_chroma_min: h.rainbow.chroma_min,
            rainbow_chroma_max: h.rainbow.chroma_max,
            sky_top: parse_hex("sky_top", &h.hues.sky_top, &d.hues.sky_top),
            sky_mid: parse_hex("sky_mid", &h.hues.sky_mid, &d.hues.sky_mid),
            sky_low: parse_hex("sky_low", &h.hues.sky_low, &d.hues.sky_low),
            horizon: parse_hex("horizon", &h.hues.horizon, &d.hues.horizon),
            fog_color: parse_hex("fog", &h.hues.fog, &d.hues.fog),
            ground: parse_hex("ground", &h.hues.ground, &d.hues.ground),
            hill_fill: parse_hex("hill_fill", &h.hues.hill_fill, &d.hues.hill_fill),
            rail: parse_hex("rail", &h.hues.rail, &d.hues.rail),
            beat_line: parse_hex("beat_line", &h.hues.beat_line, &d.hues.beat_line),
            sun: [
                parse_hex("sun[0]", &h.hues.sun[0], &d.hues.sun[0]),
                parse_hex("sun[1]", &h.hues.sun[1], &d.hues.sun[1]),
                parse_hex("sun[2]", &h.hues.sun[2], &d.hues.sun[2]),
            ],
            trouble: parse_hex("trouble", &h.hues.trouble, &d.hues.trouble),
            now: parse_hex("now", &h.hues.now, &d.hues.now),
        }
    }
}

/// Hot-apply `[scene.post]` to the shared camera whenever the palette
/// resource changes (theme RPC, or a live mutation over BRP). Bloom threshold
/// is deliberately NOT driven from the palette — 1.0 is the HDR-tell
/// boundary contract (docs/color.md).
pub fn apply_scene_post_on_change(
    palette: Res<ScenePalette>,
    mut cameras: Query<(&mut Bloom, &mut Tonemapping), With<Camera3d>>,
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
        close(compiled.ask, parsed.ask, "ask");
        close(compiled.trace_crimson, parsed.trace_crimson, "trace_crimson");
        close(compiled.trace_cyan, parsed.trace_cyan, "trace_cyan");
        close(compiled.trace_green, parsed.trace_green, "trace_green");
        close(compiled.trace_gold, parsed.trace_gold, "trace_gold");
        close(compiled.table, parsed.table, "table");
        close(compiled.wall_base, parsed.wall_base, "wall_base");
        close(compiled.wall_mullion, parsed.wall_mullion, "wall_mullion");
        close(compiled.dark_surface, parsed.dark_surface, "dark_surface");
        close(compiled.key_light, parsed.key_light, "key_light");
        close(compiled.ambient, parsed.ambient, "ambient");
        assert_eq!(compiled.crest, parsed.crest);
        assert_eq!(compiled.gain_beat, parsed.gain_beat);
        assert_eq!(compiled.tonemapper, parsed.tonemapper);
        assert_eq!(compiled.bloom_intensity, parsed.bloom_intensity);
        assert_eq!(compiled.lit_chamber, parsed.lit_chamber);
        assert_eq!(compiled.chamber_roughness, parsed.chamber_roughness);
        assert_eq!(compiled.chamber_reflectance, parsed.chamber_reflectance);
        assert_eq!(compiled.key_light_intensity, parsed.key_light_intensity);
        assert_eq!(compiled.key_light_range, parsed.key_light_range);
        assert_eq!(compiled.ambient_brightness, parsed.ambient_brightness);
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
        assert_eq!(tonemapper_by_name("none"), Some(Tonemapping::Linear));
        assert_eq!(tonemapper_by_name("vhs"), None);
        let mut scene = SceneData::default();
        scene.post.tonemapper = "vhs".into();
        let p = ScenePalette::from_scene_data(&scene);
        assert_eq!(p.tonemapper, ScenePalette::default().tonemapper);
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

    #[test]
    fn identity_hue_never_returns_an_excluded_band() {
        for i in 0..1000 {
            let key = format!("performer-key-{i}");
            let h = identity_hue_degrees(&key);
            assert!(
                (22.0..170.0).contains(&h) || (200.0..285.0).contains(&h),
                "hue {h} for {key} landed in an excluded band"
            );
        }
    }

    #[test]
    fn identity_hue_first_eight_keys_are_pairwise_distinguishable() {
        let keys = ["bass", "drums", "lead", "vox", "synth", "perc", "pad", "arp"];
        let hues: Vec<f32> = keys.iter().map(|k| identity_hue_degrees(k)).collect();
        for i in 0..hues.len() {
            for j in (i + 1)..hues.len() {
                let d = (hues[i] - hues[j]).abs();
                let wrapped = d.min(360.0 - d);
                assert!(
                    wrapped > 5.0,
                    "keys {} ({}) and {} ({}) too close: {wrapped}°",
                    keys[i],
                    hues[i],
                    keys[j],
                    hues[j]
                );
            }
        }
    }

    #[test]
    fn rust_and_wesl_rainbow_agree_at_sample_points() {
        // Parse the constants from assets/shaders/kaijutsu/color.wesl to ensure
        // the shader's formula stays aligned with Rust's `rainbow_oklch`.
        let wesl = include_str!("../../../../assets/shaders/kaijutsu/color.wesl");
        assert!(wesl.contains("0.3963377774"), "WESL rainbow matrix present");
        assert!(wesl.contains("4.0767416621"), "WESL OKLab->sRGB matrix present");

        let eval_wesl_formula = |t: f32, lightness: f32, chroma: f32| -> Vec3 {
            let fract = t - t.floor();
            let h = fract * 6.28318530718_f32;
            let a = chroma * h.cos();
            let b = chroma * h.sin();
            let l_ = lightness + 0.3963377774 * a + 0.2158037573 * b;
            let m_ = lightness - 0.1055613458 * a - 0.0638541728 * b;
            let s_ = lightness - 0.0894841775 * a - 1.2914855480 * b;
            let (l, m, s) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
            Vec3::new(
                (4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s).max(0.0),
                (-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s).max(0.0),
                (-0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s).max(0.0),
            )
        };

        for t in [0.0, 0.125, 0.25, 0.5, 0.75, 0.99] {
            for chroma in [0.02, 0.12, 0.20] {
                let rust = rainbow_oklch(t, 0.78, chroma);
                let wesl_val = eval_wesl_formula(t, 0.78, chroma);
                assert!(
                    rust.distance(wesl_val) < 1e-5,
                    "t={t} chroma={chroma}: rust={rust:?} wesl={wesl_val:?}"
                );
            }
        }
    }
}
