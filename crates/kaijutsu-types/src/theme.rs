//! Framework-agnostic theme data.
//!
//! `ThemeData` holds all theme values as hex strings (colors) and primitives (floats, bools).
//! It can be parsed from TOML config and then converted to framework-specific types
//! (e.g. Bevy's `Color`, `Vec4`) by downstream crates.

use serde::{Deserialize, Serialize};

/// ANSI 16-color palette (hex strings).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnsiColorsData {
    pub black: String,
    pub red: String,
    pub green: String,
    pub yellow: String,
    pub blue: String,
    pub magenta: String,
    pub cyan: String,
    pub white: String,
    pub bright_black: String,
    pub bright_red: String,
    pub bright_green: String,
    pub bright_yellow: String,
    pub bright_blue: String,
    pub bright_magenta: String,
    pub bright_cyan: String,
    pub bright_white: String,
}

impl Default for AnsiColorsData {
    fn default() -> Self {
        // Synthwave terminal palette (docs/color.md).
        Self {
            black: "#16121f".into(),
            red: "#ff5c8a".into(),
            green: "#4ce0b3".into(),
            yellow: "#ffcf7d".into(),
            blue: "#a487ff".into(),
            magenta: "#ff7ad9".into(),
            cyan: "#7de0ff".into(),
            white: "#cdc6e6".into(),
            bright_black: "#453a66".into(),
            bright_red: "#ff7aa2".into(),
            bright_green: "#6bf0c9".into(),
            bright_yellow: "#ffe0a1".into(),
            bright_blue: "#bda4ff".into(),
            bright_magenta: "#ff9ae4".into(),
            bright_cyan: "#a5ecff".into(),
            bright_white: "#ece7fa".into(),
        }
    }
}

/// Framework-agnostic theme data.
///
/// Colors are hex strings (`"#rrggbb"` or `"#rrggbbaa"`).
/// Numeric values are `f32`. Boolean flags are `bool`.
/// Vec4-like values are `[f32; 4]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeData {
    // Base UI
    pub bg: String,
    pub panel_bg: String,
    pub fg: String,
    pub fg_dim: String,
    pub accent: String,
    pub accent2: String,
    pub border: String,
    pub selection_bg: String,

    // Row type colors
    pub row_tool: String,
    pub row_result: String,

    // Semantic
    pub error: String,
    pub warning: String,
    pub success: String,

    // Mode colors
    pub mode_normal: String,
    pub mode_insert: String,
    pub mode_chat: String,
    pub mode_shell: String,
    pub mode_visual: String,

    // Mode labels (dock HUD text, scriptable)
    pub mode_label_normal: String,
    pub mode_label_insert: String,
    pub mode_label_visual: String,
    pub mode_label_shell: String,
    pub mode_label_constellation: String,
    pub mode_label_stack: String,
    pub mode_label_input: String,

    // Cursor colors
    pub cursor_normal: [f32; 4],
    pub cursor_insert: [f32; 4],
    pub cursor_visual: [f32; 4],
    pub cursor_replace: [f32; 4],

    // ANSI palette
    pub ansi: AnsiColorsData,

    // Frame configuration
    pub frame_corner_size: f32,
    pub frame_edge_thickness: f32,
    pub frame_content_padding: f32,

    // Frame colors
    pub frame_base: String,
    pub frame_focused: String,
    pub frame_insert: String,
    pub frame_visual: String,
    pub frame_unfocused: String,
    pub frame_edge: String,

    // Frame shader params
    pub frame_params_base: [f32; 4],
    pub frame_params_focused: [f32; 4],
    pub frame_params_unfocused: [f32; 4],

    // Edge dimming
    pub frame_edge_dim_unfocused: [f32; 4],
    pub frame_edge_dim_focused: [f32; 4],

    // Shader effects
    pub effect_glow_radius: f32,
    pub effect_glow_intensity: f32,
    pub effect_glow_falloff: f32,
    pub effect_sheen_speed: f32,
    pub effect_sheen_sparkle_threshold: f32,
    pub effect_breathe_speed: f32,
    pub effect_breathe_amplitude: f32,

    // Chasing border
    pub effect_chase_speed: f32,
    pub effect_chase_width: f32,
    pub effect_chase_glow_radius: f32,
    pub effect_chase_glow_intensity: f32,
    pub effect_chase_color_cycle: f32,

    // Input area
    pub input_minimized_height: f32,
    pub input_docked_height: f32,
    pub input_overlay_width_pct: f32,
    pub input_backdrop_color: String,

    // Font configuration
    pub font_rainbow: bool,
    /// Font family for monospace (code, blocks). Used for both Vello text and SVG fallback.
    pub font_mono: String,
    /// Font family for serif text.
    pub font_serif: String,
    /// Font family for sans-serif text (SVG generic family fallback).
    pub font_sans: String,

    // MSDF text rendering quality
    /// Hinting strength (0.0 = off, 1.0 = full). Default 0.8.
    pub msdf_hint_amount: f32,
    /// Stem darkening (0.0 = off, ~0.15 = ClearType-like). Default 0.15.
    pub msdf_stem_darkening: f32,
    /// Horizontal stroke AA scale (1.0-1.3). Default 1.1.
    pub msdf_horz_scale: f32,
    /// Vertical stroke AA scale (0.5-0.8). Default 0.6.
    pub msdf_vert_scale: f32,
    /// SDF threshold (0.45-0.55). Default 0.5.
    pub msdf_text_bias: f32,
    /// Alpha gamma correction. Default 0.85.
    pub msdf_gamma_correction: f32,

    // Constellation
    pub constellation_base_radius: f32,
    pub constellation_ring_spacing: f32,

    // Block borders
    pub block_border_tool_call: String,
    pub block_border_tool_result: String,
    pub block_border_error: String,
    pub block_border_thinking: String,
    pub block_border_drift: String,
    pub block_border_thickness: f32,
    pub block_border_corner_radius: f32,
    pub block_border_glow_radius: f32,
    pub block_border_glow_intensity: f32,
    /// Text glow halo radius in pixels (0 = disabled).
    pub text_glow_radius: f32,
    /// Text glow halo color (hex, independent of border color).
    pub text_glow_color: String,
    pub block_border_padding: f32,
    pub block_spacing: f32,

    // Compose
    pub compose_border: String,
    pub compose_bg: String,

    // Modal
    pub modal_backdrop: String,

    // User/assistant text borders
    pub block_border_user: String,
    pub block_border_assistant: String,

    // Layout spacing
    pub indent_width: f32,
    pub role_header_height: f32,
    pub role_header_spacing: f32,
    pub label_font_size: f32,
    pub label_inset: f32,
    pub label_pad: f32,

    /// The 3D scene lane (`[scene]` table): identity hues, the brightness tier
    /// ladder, live-signal gains, and the camera post chain. `serde(default)`
    /// on purpose — themes written before this section existed must keep
    /// parsing (the live CRDT theme.toml predates it). See `docs/color.md`.
    #[serde(default)]
    pub scene: SceneData,
}

impl Default for ThemeData {
    fn default() -> Self {
        Self {
            // Base UI — Synthwave (docs/color.md; Tokyo Night lives on in
            // contrib/themes/tokyo-night.toml)
            bg: "#131020".into(),
            panel_bg: "#131020f2".into(),
            fg: "#d8d2ee".into(),
            fg_dim: "#6f6592".into(),
            accent: "#a487ff".into(),
            accent2: "#ffcf7d".into(),
            border: "#3d3260".into(),
            selection_bg: "#a487ff4d".into(),

            // Row type colors
            row_tool: "#d18bff".into(),
            row_result: "#e8b45e".into(),

            // Semantic
            error: "#ff5c8a".into(),
            warning: "#ffb45e".into(),
            success: "#4ce0b3".into(),

            // Mode colors
            mode_normal: "#a487ff".into(),
            mode_insert: "#4ce0b3".into(),
            mode_chat: "#4ce0b3".into(),
            mode_shell: "#e8b45e".into(),
            mode_visual: "#ff7ad9".into(),

            // Mode labels
            mode_label_normal: "NORMAL".into(),
            mode_label_insert: "INSERT".into(),
            mode_label_visual: "VISUAL".into(),
            mode_label_shell: "SHELL".into(),
            mode_label_constellation: "CONSTELLATION".into(),
            mode_label_stack: "STACK".into(),
            mode_label_input: "INPUT".into(),

            // Cursor colors
            cursor_normal: [0.643, 0.529, 1.000, 0.8],
            cursor_insert: [0.298, 0.878, 0.702, 0.9],
            cursor_visual: [1.000, 0.478, 0.851, 0.7],
            cursor_replace: [1.000, 0.700, 0.360, 0.85], // warm gold

            // ANSI
            ansi: AnsiColorsData::default(),

            // Frame config
            frame_corner_size: 16.0,
            frame_edge_thickness: 2.0,
            frame_content_padding: 12.0,

            // Frame colors
            frame_base: "#131020f2".into(),
            frame_focused: "#a487ff26".into(),
            frame_insert: "#4ce0b31f".into(),
            frame_visual: "#ff7ad91f".into(),
            frame_unfocused: "#131020cc".into(),
            frame_edge: "#3d326099".into(),

            // Frame shader params
            frame_params_base: [0.0, 0.0, 0.0, 0.0],
            frame_params_focused: [0.3, 0.0, 0.0, 0.0],
            frame_params_unfocused: [0.0, 0.0, 0.0, 0.0],

            // Edge dimming
            frame_edge_dim_unfocused: [0.5, 0.5, 0.5, 0.6],
            frame_edge_dim_focused: [1.0, 1.0, 1.0, 1.0],

            // Shader effects
            effect_glow_radius: 4.0,
            effect_glow_intensity: 0.3,
            effect_glow_falloff: 2.0,
            effect_sheen_speed: 0.5,
            effect_sheen_sparkle_threshold: 0.95,
            effect_breathe_speed: 1.0,
            effect_breathe_amplitude: 0.05,

            // Chasing border
            effect_chase_speed: 2.0,
            effect_chase_width: 0.15,
            effect_chase_glow_radius: 8.0,
            effect_chase_glow_intensity: 0.5,
            effect_chase_color_cycle: 0.0,

            // Input area
            input_minimized_height: 48.0,
            input_docked_height: 200.0,
            input_overlay_width_pct: 0.6,
            input_backdrop_color: "#131020d9".into(),

            // Font configuration
            font_rainbow: true,
            font_mono: "Cascadia Code NF".into(),
            font_serif: "Noto Serif".into(),
            font_sans: "Noto Sans CJK JP".into(),

            // MSDF text rendering quality
            msdf_hint_amount: 0.8,
            msdf_stem_darkening: 0.15,
            msdf_horz_scale: 1.1,
            msdf_vert_scale: 0.6,
            msdf_text_bias: 0.5,
            msdf_gamma_correction: 0.85,

            // Constellation
            constellation_base_radius: 500.0,
            constellation_ring_spacing: 550.0,

            // Block borders
            block_border_tool_call: "#e8b45e99".into(),
            block_border_tool_result: "#4ce0b366".into(),
            block_border_error: "#ff2d6ecc".into(),
            block_border_thinking: "#7a6aa04d".into(),
            block_border_drift: "#c476e180".into(),
            block_border_thickness: 1.5,
            block_border_corner_radius: 4.0,
            block_border_glow_radius: 6.0,
            block_border_glow_intensity: 0.25,
            text_glow_radius: 2.5,
            text_glow_color: "#cbb8ff59".into(), // soft violet-white, 35% alpha
            block_border_padding: 0.6,
            block_spacing: 12.0,

            // Compose
            compose_border: "#3d3260".into(),
            compose_bg: "#131020".into(),

            // Modal
            modal_backdrop: "#00000099".into(),

            // User/assistant borders (transparent = disabled)
            block_border_user: "#00000000".into(),
            block_border_assistant: "#00000000".into(),

            // Layout spacing
            indent_width: 24.0,
            role_header_height: 20.0,
            role_header_spacing: 4.0,
            label_font_size: 11.0,
            label_inset: 12.0,
            label_pad: 6.0,

            scene: SceneData::default(),
        }
    }
}

// ─── Scene lane (`[scene]`) ─────────────────────────────────────────────────
//
// The 3D scenes' color contract (docs/color.md): identity hues as sRGB hex
// (the app linearizes them for HDR materials), the brightness tier ladder,
// live-signal gains, and the camera post chain. Defaults mirror the values
// the scene modules shipped with, so a theme without `[scene]` renders
// exactly as before.

/// `[scene]` — the 3D scene lane. Every sub-table is `serde(default)` so a
/// partial `[scene]` section overrides only what it names.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SceneData {
    pub hues: SceneHuesData,
    pub tiers: SceneTiersData,
    pub gains: SceneGainsData,
    pub post: ScenePostData,
}

/// `[scene.hues]` — identity hues, hex sRGB. Brightness lives in the tiers;
/// a hue's brightest channel should sit near full so tier math means what it
/// says. (`wire` was stored pre-multiplied at 1.4 before this system; it is
/// now normalized here with the 1.4 in `gains.wire`.)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SceneHuesData {
    /// Room clear color (the octagon's void).
    pub bg: String,
    /// The room's one metal: trim, console rings, etch, well core.
    pub gold: String,
    /// Hardware: sockets, pegs, jacks.
    pub brass: String,
    /// Information-violet: radiator glass backdrop.
    pub violet_glass: String,
    /// Information-violet: thread/content strips.
    pub violet_thread: String,
    /// The well's electric indigo-violet (ring deck + track rays).
    pub neon: String,
    /// Terrace glyph rings: a paler tint of `neon`.
    pub terrace: String,
    /// Patch-bay chord wire (normalized; HDR gain in `gains.wire`).
    pub wire: String,
    /// Circuit-board floor trace fabrics, one hue family per fabric.
    pub trace_crimson: String,
    pub trace_cyan: String,
    pub trace_green: String,
    pub trace_gold: String,
    /// Structural near-blacks (silhouette tiers, not brightness).
    pub table: String,
    pub wall_base: String,
    pub wall_mullion: String,
    pub dark_surface: String,
}

impl Default for SceneHuesData {
    fn default() -> Self {
        // Hex = exact sRGB encoding of the linear constants the scene modules
        // shipped with (≤1/255 per-channel rounding).
        Self {
            bg: "#05070b".into(),
            gold: "#ffe59e".into(),
            brass: "#ddc489".into(),
            violet_glass: "#55386c".into(),
            violet_thread: "#c476e1".into(),
            neon: "#ad95f3".into(),
            terrace: "#c4b3f9".into(),
            wire: "#ff5f73".into(),
            trace_crimson: "#86424b".into(),
            trace_cyan: "#3f737e".into(),
            trace_green: "#598b6c".into(),
            trace_gold: "#958159".into(),
            table: "#32353f".into(),
            wall_base: "#464556".into(),
            wall_mullion: "#383844".into(),
            dark_surface: "#1d1e26".into(),
        }
    }
}

/// `[scene.tiers]` — the brightness ladder (multiply an identity hue).
/// LDR structure tiers + the decoration-glow band. Invariant the app tests:
/// every trough × `crest` < 1.0 (decoration never sustains HDR).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SceneTiersData {
    /// Engraved detail: guide rings, ticks.
    pub etch: f32,
    /// Station markers at rest.
    pub marker: f32,
    /// Gold architectural trim (table rims, pylon caps).
    pub trim: f32,
    /// Brass hardware.
    pub hardware: f32,
    /// Ceiling for ANY decoration crest (>1.0 = soft bloom halo).
    pub crest: f32,
    /// Floor traces at rest.
    pub trough_wiring: f32,
    /// Wall trim breathing floor.
    pub trough_wall_trim: f32,
    /// Terminal pads breathing floor.
    pub trough_pads: f32,
    /// The calmest breathers (inscribed floor ring).
    pub trough_subtle: f32,
}

impl Default for SceneTiersData {
    fn default() -> Self {
        Self {
            etch: 0.28,
            marker: 0.42,
            trim: 0.50,
            hardware: 0.55,
            crest: 1.25,
            trough_wiring: 0.55,
            trough_wall_trim: 0.60,
            trough_pads: 0.65,
            trough_subtle: 0.75,
        }
    }
}

/// `[scene.gains]` — live-signal HDR gains. These are allowed to sustain
/// >1.0 because they ARE the live-activity tell (docs/color.md).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SceneGainsData {
    /// Patch-bay traffic packet peak.
    pub pulse: f32,
    /// Selected chord idle gain.
    pub chord_selected: f32,
    /// Chord wire resting HDR (hue is normalized in `hues.wire`).
    pub wire: f32,
    /// Tracker marker beat thump.
    pub beat: f32,
    /// Tracker marker active lift.
    pub active: f32,
    /// Station focus lift.
    pub focus_lift: f32,
    /// Well reading-card border gain.
    pub reading_border: f32,
    /// Well HUD border gain.
    pub hud_border: f32,
}

impl Default for SceneGainsData {
    fn default() -> Self {
        Self {
            pulse: 6.0,
            chord_selected: 3.4,
            wire: 1.4,
            beat: 2.8,
            active: 0.5,
            focus_lift: 0.35,
            reading_border: 1.6,
            hud_border: 1.8,
        }
    }
}

/// `[scene.post]` — the shared HDR camera's post chain. Applied LIVE on theme
/// change (unlike hues/tiers, which apply at spawn) — `kj config set` on the
/// theme is a live color-management console. The bloom THRESHOLD is not here
/// on purpose: 1.0 is the HDR-tell boundary, a contract rather than a style
/// knob.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScenePostData {
    pub bloom_intensity: f32,
    pub bloom_low_frequency_boost: f32,
    /// One of: "tony_mc_mapface", "aces", "agx", "blender_filmic",
    /// "reinhard", "reinhard_luminance", "somewhat_boring", "none".
    pub tonemapper: String,
}

impl Default for ScenePostData {
    fn default() -> Self {
        // ACES + raised bloom won the 2026-07-12 live A/B against the
        // synthwave target (vs TonyMcMapface = the pre-pass muted look,
        // BlenderFilmic = halfway): deep ground, saturated neon, confident
        // glow. TonyMcMapface remains one theme-edit away.
        Self {
            bloom_intensity: 0.22,
            bloom_low_frequency_boost: 0.25,
            tonemapper: "aces".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_THEME_TOML: &str = include_str!("../../../assets/defaults/theme.toml");

    #[test]
    fn default_theme_toml_deserializes() {
        let td: ThemeData = toml::from_str(DEFAULT_THEME_TOML)
            .expect("theme.toml should deserialize into ThemeData");

        // Spot-check against Default impl
        let defaults = ThemeData::default();
        assert_eq!(td.bg, defaults.bg);
        assert_eq!(td.fg, defaults.fg);
        assert_eq!(td.accent, defaults.accent);
        assert_eq!(td.panel_bg, defaults.panel_bg);
        assert_eq!(td.cursor_normal, defaults.cursor_normal);
        assert_eq!(td.ansi.red, defaults.ansi.red);
        assert_eq!(td.ansi.bright_cyan, defaults.ansi.bright_cyan);
        assert_eq!(td.frame_corner_size, defaults.frame_corner_size);
        assert_eq!(td.effect_glow_radius, defaults.effect_glow_radius);
        assert_eq!(td.font_rainbow, defaults.font_rainbow);
        assert_eq!(td.font_mono, defaults.font_mono);
        assert_eq!(td.modal_backdrop, defaults.modal_backdrop);
        assert_eq!(td.block_spacing, defaults.block_spacing);
    }

    #[test]
    fn default_roundtrip() {
        // Serialize defaults to TOML and back
        let defaults = ThemeData::default();
        let serialized = toml::to_string_pretty(&defaults).unwrap();
        let deserialized: ThemeData = toml::from_str(&serialized).unwrap();
        assert_eq!(defaults.bg, deserialized.bg);
        assert_eq!(defaults.cursor_normal, deserialized.cursor_normal);
        assert_eq!(defaults.ansi.magenta, deserialized.ansi.magenta);
        assert_eq!(defaults.scene.hues.gold, deserialized.scene.hues.gold);
        assert_eq!(defaults.scene.tiers.crest, deserialized.scene.tiers.crest);
        assert_eq!(defaults.scene.post.tonemapper, deserialized.scene.post.tonemapper);
    }

    #[test]
    fn theme_without_scene_section_still_parses() {
        // The live CRDT theme.toml predates `[scene]` — it MUST keep parsing,
        // yielding the compiled scene defaults.
        let stripped = DEFAULT_THEME_TOML
            .split("\n[scene.hues]")
            .next()
            .unwrap()
            .to_string();
        let td: ThemeData = toml::from_str(&stripped)
            .expect("a theme.toml without [scene] must deserialize");
        assert_eq!(td.scene.hues.gold, SceneData::default().hues.gold);
        assert_eq!(
            td.scene.post.bloom_intensity,
            ScenePostData::default().bloom_intensity
        );
    }

    #[test]
    fn partial_scene_section_overrides_only_named_keys() {
        let toml_src = format!(
            "{}\n[scene.post]\nbloom_intensity = 0.3\n",
            DEFAULT_THEME_TOML.split("\n[scene.hues]").next().unwrap()
        );
        let td: ThemeData = toml::from_str(&toml_src).unwrap();
        assert_eq!(td.scene.post.bloom_intensity, 0.3, "named key overrides");
        assert_eq!(
            td.scene.post.tonemapper,
            ScenePostData::default().tonemapper,
            "unnamed keys keep defaults"
        );
        assert_eq!(td.scene.tiers.crest, 1.25, "untouched tables keep defaults");
    }

    #[test]
    fn decoration_troughs_never_sustain_hdr() {
        // trough × crest < 1.0 — the trace-glow discipline, enforced at the
        // data layer too (the app re-tests it on the parsed palette).
        let t = SceneTiersData::default();
        for (name, trough) in [
            ("wiring", t.trough_wiring),
            ("wall_trim", t.trough_wall_trim),
            ("pads", t.trough_pads),
            ("subtle", t.trough_subtle),
        ] {
            assert!(
                trough * t.crest < 1.0,
                "trough_{name} ({trough}) × crest ({}) must stay < 1.0",
                t.crest
            );
        }
    }
}
