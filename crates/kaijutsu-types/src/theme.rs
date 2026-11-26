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
        // Synthwave terminal palette (docs/newui.md).
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

/// Cyan, deliberately absent from the rest of the border palette (magenta
/// tool calls, violet results, red errors, lavender thinking, purple drift)
/// so the focus ring can't be mistaken for block-kind styling.
fn default_block_border_focus() -> String {
    "#00f0ffcc".into()
}

fn default_assistant_fg() -> String {
    "#a7c9df".into()
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
    /// Model reply foreground, independent of human and tool text.
    #[serde(default = "default_assistant_fg")]
    pub assistant_fg: String,
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
    /// Focus ring for the keyboard-focused conversation block (j/k). A
    /// serde default, not a required field: themes stored by the kernel
    /// before this knob existed must keep parsing, or the app silently
    /// reverts to compiled-in defaults.
    #[serde(default = "default_block_border_focus")]
    pub block_border_focus: String,

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
    /// parsing (the live kernel theme.toml predates it). See `docs/newui.md`.
    #[serde(default, deserialize_with = "read_scene")]
    pub scene: SceneData,

    /// The heroic view (`[heroic]` table): framing targets, hues, OKLCH rainbow,
    /// distance fog, element sizes, and motion durations. `serde(default)` so
    /// existing host `theme.toml` files without `[heroic]` keep parsing.
    #[serde(default)]
    pub heroic: HeroicData,
}

impl Default for ThemeData {
    fn default() -> Self {
        Self {
            // Base UI — Synthwave (docs/newui.md; Tokyo Night lives on in
            // contrib/themes/tokyo-night.toml)
            bg: "#17141f".into(),
            panel_bg: "#1d1926ff".into(),
            fg: "#cec8d8".into(),
            assistant_fg: default_assistant_fg(),
            fg_dim: "#a29aad".into(),
            accent: "#b58ba7".into(),
            accent2: "#8bbbc4".into(),
            border: "#393142".into(),
            selection_bg: "#9c8bb233".into(),

            // Row type colors
            row_tool: "#9c8bb2".into(),
            row_result: "#9c8bb2".into(),

            // Semantic
            error: "#e28c99".into(),
            warning: "#dfbd82".into(),
            success: "#8fb5a1".into(),

            // Mode colors
            mode_normal: "#9c8bb2".into(),
            mode_insert: "#8fb5a1".into(),
            mode_chat: "#8fb5a1".into(),
            mode_shell: "#dfbd82".into(),
            mode_visual: "#b58ba7".into(),

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
            frame_base: "#1d1926ff".into(),
            frame_focused: "#8bbbc414".into(),
            frame_insert: "#8fb5a114".into(),
            frame_visual: "#b58ba714".into(),
            frame_unfocused: "#17141fcc".into(),
            frame_edge: "#39314299".into(),

            // Frame shader params
            frame_params_base: [0.0, 0.0, 0.0, 0.0],
            frame_params_focused: [0.3, 0.0, 0.0, 0.0],
            frame_params_unfocused: [0.0, 0.0, 0.0, 0.0],

            // Edge dimming
            frame_edge_dim_unfocused: [0.5, 0.5, 0.5, 0.6],
            frame_edge_dim_focused: [1.0, 1.0, 1.0, 1.0],

            // Shader effects
            effect_glow_radius: 0.0,
            effect_glow_intensity: 0.0,
            effect_glow_falloff: 2.0,
            effect_sheen_speed: 0.5,
            effect_sheen_sparkle_threshold: 0.95,
            effect_breathe_speed: 1.0,
            effect_breathe_amplitude: 0.05,

            // Chasing border
            effect_chase_speed: 2.0,
            effect_chase_width: 0.15,
            effect_chase_glow_radius: 0.0,
            effect_chase_glow_intensity: 0.0,
            effect_chase_color_cycle: 0.0,

            // Input area
            input_minimized_height: 48.0,
            input_docked_height: 200.0,
            input_overlay_width_pct: 0.6,
            input_backdrop_color: "#17141fd9".into(),

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
            msdf_gamma_correction: 1.6,

            // Constellation
            constellation_base_radius: 500.0,
            constellation_ring_spacing: 550.0,

            // Block borders
            block_border_tool_call: "#b49aeb99".into(),
            block_border_tool_result: "#9c8bb233".into(),
            block_border_error: "#e28c99b3".into(),
            block_border_thinking: "#a29aad33".into(),
            block_border_drift: "#9c8bb259".into(),
            block_border_thickness: 1.0,
            block_border_corner_radius: 2.0,
            block_border_glow_radius: 0.0,
            block_border_glow_intensity: 0.0,
            text_glow_radius: 0.0,
            text_glow_color: "#cec8d800".into(),
            block_border_padding: 0.6,
            block_spacing: 12.0,

            // Compose
            compose_border: "#393142".into(),
            compose_bg: "#17141f".into(),

            // Modal
            modal_backdrop: "#00000099".into(),

            // User/assistant borders (transparent = disabled)
            block_border_user: "#00000000".into(),
            block_border_assistant: "#00000000".into(),
            block_border_focus: "#8bbbc499".into(),

            // Layout spacing
            indent_width: 24.0,
            role_header_height: 20.0,
            role_header_spacing: 4.0,
            label_font_size: 11.0,
            label_inset: 12.0,
            label_pad: 6.0,

            scene: SceneData::default(),
            heroic: HeroicData::default(),
        }
    }
}

// ─── Scene lane (`[scene]`) ─────────────────────────────────────────────────
//
// The 3D scenes' color contract (docs/newui.md): identity hues as sRGB hex
// (the app linearizes them for HDR materials), the brightness tier ladder,
// live-signal gains, and the camera post chain. Defaults mirror the values
// the scene modules shipped with, so a theme without `[scene]` renders
// exactly as before.

/// `[scene]` — the 3D scene lane. Every sub-table is `serde(default)` so a
/// partial `[scene]` section overrides only what it names.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SceneData {
    pub hues: SceneHuesData,
    pub tiers: SceneTiersData,
    pub gains: SceneGainsData,
    pub post: ScenePostData,
    pub lighting: SceneLightingData,
}

/// Load materialized themes that still contain settings for the room walls and pylons.
fn read_scene<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<SceneData, D::Error> {
    let mut scene = serde_json::Value::deserialize(deserializer)?;
    for (section, retired) in [
        ("hues", &["wall_base", "wall_mullion", "violet_thread"][..]),
        ("tiers", &["trough_wall_trim", "marker"]),
        ("gains", &["beat", "active", "focus_lift"]),
    ] {
        if let Some(table) = scene.get_mut(section).and_then(serde_json::Value::as_object_mut) {
            for key in retired {
                table.remove(*key);
            }
        }
    }
    serde_json::from_value(scene).map_err(serde::de::Error::custom)
}

/// `[scene.hues]` — identity hues, hex sRGB. Brightness lives in the tiers;
/// a hue's brightest channel should sit near full so tier math means what it
/// says.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SceneHuesData {
    /// Room clear color (the octagon's void).
    pub bg: String,
    /// The room's one metal: trim, console rings, etch, well core.
    pub gold: String,
    /// Hardware: sockets, pegs, jacks.
    pub brass: String,
    /// Violet floor traces.
    pub violet_glass: String,
    /// The well's electric indigo-violet (ring deck + track rays).
    pub neon: String,
    /// Terrace glyph rings: a paler tint of `neon`.
    pub terrace: String,
    /// The waiting tier: a context with an approval ask pending
    /// (`connection::ledger`). Pale cream, and allowed to sustain HDR — a
    /// question waiting on a player must read from across the room.
    pub ask: String,
    /// Circuit-board floor trace fabrics, one hue family per fabric.
    pub trace_crimson: String,
    pub trace_cyan: String,
    pub trace_green: String,
    pub trace_gold: String,
    /// Structural near-blacks (silhouette tiers, not brightness).
    pub table: String,
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
            neon: "#ad95f3".into(),
            terrace: "#c4b3f9".into(),
            ask: "#fff5c2".into(),
            trace_crimson: "#86424b".into(),
            trace_cyan: "#3f737e".into(),
            trace_green: "#598b6c".into(),
            trace_gold: "#958159".into(),
            table: "#32353f".into(),
            dark_surface: "#1d1e26".into(),
        }
    }
}

/// `[scene.tiers]` — the brightness ladder (multiply an identity hue).
/// LDR structure tiers + the decoration-glow band. Invariant the app tests:
/// every trough × `crest` < 1.0 (decoration never sustains HDR).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SceneTiersData {
    /// Gold table rims.
    pub trim: f32,
    /// Ceiling for ANY decoration crest (>1.0 = soft bloom halo).
    pub crest: f32,
    /// Floor traces at rest.
    pub trough_wiring: f32,
    /// Terminal pads breathing floor.
    pub trough_pads: f32,
    /// The calmest breathers (inscribed floor ring).
    pub trough_subtle: f32,
}

impl Default for SceneTiersData {
    fn default() -> Self {
        Self {
            trim: 0.50,
            crest: 1.25,
            trough_wiring: 0.55,
            trough_pads: 0.65,
            trough_subtle: 0.75,
        }
    }
}

/// `[scene.gains]` — live-signal HDR gains. These are allowed to sustain
/// >1.0 because they ARE the live-activity tell (docs/newui.md).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SceneGainsData {
    /// Well reading-card border gain.
    pub reading_border: f32,
    /// Well HUD border gain.
    pub hud_border: f32,
}

impl Default for SceneGainsData {
    fn default() -> Self {
        Self {
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
#[serde(default, deny_unknown_fields)]
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

/// `[scene.lighting]` controls the optional warm key light on the floor,
/// vault and console table. Traces, cards, lamps, gold trim and glyphs use
/// unlit materials. Light colors use sRGB hex and intensity uses lumens.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SceneLightingData {
    /// Light only the chamber shell instead of the all-unlit room.
    pub lit_chamber: bool,
    /// `perceptual_roughness` of the lit shell surfaces (wet-asphalt sheen).
    pub chamber_roughness: f32,
    /// `reflectance` of the lit shell surfaces (a small specular kick).
    pub chamber_reflectance: f32,
    /// Warm key light at the well center (~3200K).
    pub key_light_color: String,
    /// Key light luminous power (lumens). Large: the room is ~1300 units, not
    /// meters, so inverse-square falloff needs a big first guess.
    pub key_light_intensity: f32,
    /// Key light cutoff radius, covering the floor disc.
    pub key_light_range: f32,
    /// Faint warm ambient fill for the chamber.
    pub ambient_color: String,
    /// Ambient fill brightness (cd/m^2).
    pub ambient_brightness: f32,
}

impl Default for SceneLightingData {
    fn default() -> Self {
        Self {
            lit_chamber: false,
            chamber_roughness: 0.5,
            chamber_reflectance: 0.3,
            key_light_color: "#ffc58f".into(),
            key_light_intensity: 5.0e8,
            key_light_range: 2000.0,
            ambient_color: "#ffe6cc".into(),
            ambient_brightness: 150.0,
        }
    }
}

// ─── Heroic view (`[heroic]`) ────────────────────────────────────────────────

/// `[heroic]` — tunables for the full-window 3D heroic view (`docs/newui.md`).
/// Every field defaults to the shipped value so existing host themes without
/// `[heroic]` deserialize cleanly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeroicData {
    pub intensity: f32,
    pub reduced_motion: bool,
    pub display_offset_ms: i64,
    pub alarm_beats: f32,
    pub lane_width: f32,
    pub road_seconds: f32,
    pub units_per_second: f32,
    pub unselected_opacity: f32,
    pub framing: HeroicFramingData,
    pub hues: HeroicHuesData,
    pub rainbow: HeroicRainbowData,
    pub fog: HeroicFogData,
    pub sizes: HeroicSizesData,
    pub durations_ms: HeroicDurationsData,
}

impl Default for HeroicData {
    fn default() -> Self {
        Self {
            intensity: 0.6,
            reduced_motion: false,
            display_offset_ms: 0,
            alarm_beats: 8.0,
            lane_width: 1.0,
            road_seconds: 20.0,
            units_per_second: 1.8,
            unselected_opacity: 0.3,
            framing: HeroicFramingData::default(),
            hues: HeroicHuesData::default(),
            rainbow: HeroicRainbowData::default(),
            fog: HeroicFogData::default(),
            sizes: HeroicSizesData::default(),
            durations_ms: HeroicDurationsData::default(),
        }
    }
}

/// `[heroic.framing]` — screen-fraction targets that solve the camera pose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeroicFramingData {
    pub fov_degrees: f32,
    pub horizon: f32,
    pub strike_line: f32,
    pub strike_width: f32,
    pub orbit_radians: f32,
}

impl Default for HeroicFramingData {
    fn default() -> Self {
        Self {
            fov_degrees: 58.0,
            horizon: 0.67,
            strike_line: 0.11,
            strike_width: 0.55,
            orbit_radians: 1.0,
        }
    }
}

/// `[heroic.hues]` — sRGB hex colors for the heroic view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeroicHuesData {
    pub sky_top: String,
    pub sky_mid: String,
    pub sky_low: String,
    pub horizon: String,
    pub fog: String,
    pub ground: String,
    pub hill_fill: String,
    pub rail: String,
    pub beat_line: String,
    pub sun: [String; 3],
    pub trouble: String,
    pub now: String,
}

impl Default for HeroicHuesData {
    fn default() -> Self {
        Self {
            sky_top: "#06010f".into(),
            sky_mid: "#1d0638".into(),
            sky_low: "#5a0f6b".into(),
            horizon: "#ff2e88".into(),
            fog: "#2a0845".into(),
            ground: "#0d0221".into(),
            hill_fill: "#12041f".into(),
            rail: "#00f0ff".into(),
            beat_line: "#234a66".into(),
            sun: ["#ffe66d".into(), "#ff5e8a".into(), "#c800ff".into()],
            trouble: "#ff2020".into(),
            now: "#ffffff".into(),
        }
    }
}

/// `[heroic.rainbow]` — OKLCH rainbow ramp parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeroicRainbowData {
    pub lightness: f32,
    pub chroma_min: f32,
    pub chroma_max: f32,
}

impl Default for HeroicRainbowData {
    fn default() -> Self {
        Self {
            lightness: 0.78,
            chroma_min: 0.02,
            chroma_max: 0.20,
        }
    }
}

/// `[heroic.fog]` — distance fog bounds in world units.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeroicFogData {
    pub start: f32,
    pub end: f32,
}

impl Default for HeroicFogData {
    fn default() -> Self {
        Self {
            start: 18.0,
            end: 55.0,
        }
    }
}

/// `[heroic.sizes]` — world-unit dimensions for road, performers, and environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeroicSizesData {
    pub note_radius: f32,
    pub note_height: f32,
    pub pad_radius: f32,
    pub performer_radius: f32,
    pub cage_scale: f32,
    pub hover_height: f32,
    pub hop_arc: f32,
    pub fire_height: f32,
    pub fire_offset: f32,
    pub hill_width: f32,
    pub hill_length: f32,
    pub hill_inner_edge: f32,
    pub sun_diameter: f32,
    pub sun_distance: f32,
    pub ring_radius: f32,
    pub panel_size: [f32; 2],
    pub panel_yaw_degrees: f32,
}

impl Default for HeroicSizesData {
    fn default() -> Self {
        Self {
            note_radius: 0.34,
            note_height: 0.14,
            pad_radius: 0.38,
            performer_radius: 0.3,
            cage_scale: 1.24,
            hover_height: 0.34,
            hop_arc: 0.45,
            fire_height: 3.4,
            fire_offset: 0.35,
            hill_width: 26.0,
            hill_length: 64.0,
            hill_inner_edge: 6.0,
            sun_diameter: 20.0,
            sun_distance: 60.0,
            ring_radius: 4.5,
            panel_size: [2.35, 1.75],
            panel_yaw_degrees: 16.0,
        }
    }
}

/// `[heroic.durations_ms]` — animation durations in milliseconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeroicDurationsData {
    pub hop: u32,
    pub pad_pulse: u32,
    pub instruction_fall: u32,
    pub performer_flare: u32,
}

impl Default for HeroicDurationsData {
    fn default() -> Self {
        Self {
            hop: 150,
            pad_pulse: 300,
            instruction_fall: 700,
            performer_flare: 800,
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

        assert_eq!(
            toml::Value::try_from(td).unwrap(),
            toml::Value::try_from(ThemeData::default()).unwrap(),
        );
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
        assert_eq!(defaults.heroic, deserialized.heroic);
    }

    #[test]
    fn theme_without_heroic_section_still_parses() {
        let stripped = DEFAULT_THEME_TOML
            .split("\n[heroic]")
            .next()
            .unwrap()
            .to_string();
        let td: ThemeData = toml::from_str(&stripped)
            .expect("a theme.toml without [heroic] must deserialize");
        assert_eq!(td.heroic, HeroicData::default());
    }

    #[test]
    fn partial_heroic_section_overrides_only_named_keys() {
        let toml_src = format!(
            "{}\n[heroic]\nintensity = 0.85\n[heroic.framing]\nhorizon = 0.55\n",
            DEFAULT_THEME_TOML.split("\n[heroic]").next().unwrap()
        );
        let td: ThemeData = toml::from_str(&toml_src).unwrap();
        assert_eq!(td.heroic.intensity, 0.85);
        assert_eq!(td.heroic.framing.horizon, 0.55);
        assert_eq!(td.heroic.framing.fov_degrees, HeroicFramingData::default().fov_degrees);
        assert_eq!(td.heroic.hues, HeroicHuesData::default());
    }

    #[test]
    fn unknown_heroic_key_is_refused() {
        let base = DEFAULT_THEME_TOML.split("\n[heroic]").next().unwrap();
        for table in [
            "heroic",
            "heroic.framing",
            "heroic.hues",
            "heroic.rainbow",
            "heroic.fog",
            "heroic.sizes",
            "heroic.durations_ms",
        ] {
            let toml_src = format!("{base}\n[{table}]\nretired_key = 1\n");
            let err = toml::from_str::<ThemeData>(&toml_src)
                .expect_err(&format!("an unknown [{table}] key must not load"));
            assert!(
                err.to_string().contains("retired_key"),
                "[{table}] error names the key: {err}"
            );
        }
    }

    #[test]
    fn theme_without_scene_section_still_parses() {
        // The live kernel theme.toml predates `[scene]` — it MUST keep parsing,
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
    fn a_materialized_wall_theme_keeps_its_active_settings() {
        let base = DEFAULT_THEME_TOML.split("\n[scene.hues]").next().unwrap();
        let theme: ThemeData = toml::from_str(&format!(r##"{base}
[scene.hues]
wall_base = "#010203"
wall_mullion = "#ffffff"
violet_thread = "#888888"
violet_glass = "#123456"
[scene.tiers]
marker = 0.42
trough_wall_trim = 0.6
crest = 1.1
[scene.gains]
beat = 2.8
active = 0.5
focus_lift = 0.35
hud_border = 2.0
"##)).unwrap();
        assert_eq!(theme.scene.hues.violet_glass, "#123456");
        assert_eq!(theme.scene.tiers.crest, 1.1);
        assert_eq!(theme.scene.gains.hud_border, 2.0);
        let active = toml::Value::try_from(theme.scene).unwrap();
        for (section, retired) in [
            ("hues", &["wall_base", "wall_mullion", "violet_thread"][..]),
            ("tiers", &["trough_wall_trim", "marker"]),
            ("gains", &["beat", "active", "focus_lift"]),
        ] {
            for key in retired {
                assert!(active[section].get(*key).is_none(), "{section}.{key}");
            }
        }
    }

    #[test]
    fn unknown_scene_key_is_refused() {
        let base = DEFAULT_THEME_TOML.split("\n[scene.hues]").next().unwrap();
        for table in ["scene", "scene.hues", "scene.tiers", "scene.gains", "scene.post", "scene.lighting"] {
            let toml_src = format!("{base}\n[{table}]\nretired_key = 1\n");
            let err = toml::from_str::<ThemeData>(&toml_src)
                .expect_err(&format!("an unknown [{table}] key must not load"));
            assert!(
                err.to_string().contains("retired_key"),
                "[{table}] error names the key: {err}"
            );
        }
    }

    #[test]
    fn decoration_troughs_never_sustain_hdr() {
        // trough × crest < 1.0 — the trace-glow discipline, enforced at the
        // data layer too (the app re-tests it on the parsed palette).
        let t = SceneTiersData::default();
        for (name, trough) in [
            ("wiring", t.trough_wiring),
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
