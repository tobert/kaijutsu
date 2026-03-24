//! Framework-agnostic theme data.
//!
//! `ThemeData` holds all theme values as hex strings (colors) and primitives (floats, bools).
//! It can be parsed from a Rhai scope and then converted to framework-specific types
//! (e.g. Bevy's `Color`, `Vec4`) by downstream crates.

use rhai::{Array, ImmutableString, Scope};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use crate::color::parse_hex;

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
        Self {
            black: "#15161e".into(),
            red: "#f7768e".into(),
            green: "#9ece6a".into(),
            yellow: "#e0af68".into(),
            blue: "#7aa2f7".into(),
            magenta: "#bb9af7".into(),
            cyan: "#7dcfff".into(),
            white: "#a9b1d6".into(),
            bright_black: "#414868".into(),
            bright_red: "#f7768e".into(),
            bright_green: "#9ece6a".into(),
            bright_yellow: "#e0af68".into(),
            bright_blue: "#7aa2f7".into(),
            bright_magenta: "#bb9af7".into(),
            bright_cyan: "#7dcfff".into(),
            bright_white: "#c0caf5".into(),
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
    pub mode_chat: String,
    pub mode_shell: String,
    pub mode_visual: String,

    // Cursor colors
    pub cursor_normal: [f32; 4],
    pub cursor_insert: [f32; 4],
    pub cursor_visual: [f32; 4],

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
}

impl Default for ThemeData {
    fn default() -> Self {
        Self {
            // Base UI — Tokyo Night
            bg: "#1a1b26".into(),
            panel_bg: "#1a1b26f2".into(),
            fg: "#c0caf5".into(),
            fg_dim: "#565f89".into(),
            accent: "#7aa2f7".into(),
            accent2: "#9ece6a".into(),
            border: "#3b4261".into(),
            selection_bg: "#7aa2f74d".into(),

            // Row type colors
            row_tool: "#bb9af7".into(),
            row_result: "#e0af68".into(),

            // Semantic
            error: "#f7768e".into(),
            warning: "#e0af68".into(),
            success: "#9ece6a".into(),

            // Mode colors
            mode_normal: "#7aa2f7".into(),
            mode_chat: "#9ece6a".into(),
            mode_shell: "#e0af68".into(),
            mode_visual: "#bb9af7".into(),

            // Cursor colors
            cursor_normal: [0.478, 0.635, 0.969, 0.8],
            cursor_insert: [0.620, 0.808, 0.416, 0.9],
            cursor_visual: [0.733, 0.604, 0.969, 0.7],

            // ANSI
            ansi: AnsiColorsData::default(),

            // Frame config
            frame_corner_size: 16.0,
            frame_edge_thickness: 2.0,
            frame_content_padding: 12.0,

            // Frame colors
            frame_base: "#1a1b26f2".into(),
            frame_focused: "#7aa2f726".into(),
            frame_insert: "#9ece6a1f".into(),
            frame_visual: "#bb9af71f".into(),
            frame_unfocused: "#1a1b26cc".into(),
            frame_edge: "#3b426199".into(),

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
            input_backdrop_color: "#1a1b26d9".into(),

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
            block_border_tool_call: "#ffaa0099".into(),
            block_border_tool_result: "#00ff8866".into(),
            block_border_error: "#ff2060cc".into(),
            block_border_thinking: "#6080a04d".into(),
            block_border_drift: "#00aaff80".into(),
            block_border_thickness: 1.5,
            block_border_corner_radius: 4.0,
            block_border_glow_radius: 0.15,
            block_border_glow_intensity: 0.6,
            block_border_padding: 0.4,
            block_spacing: 12.0,

            // Compose
            compose_border: "#3b4261".into(),
            compose_bg: "#1a1b26".into(),

            // Modal
            modal_backdrop: "#00000099".into(),

            // User/assistant borders (transparent = disabled)
            block_border_user: "#00000000".into(),
            block_border_assistant: "#00000000".into(),
        }
    }
}

/// Extract a color hex string from a Rhai scope.
///
/// Accepts BOTH:
/// - Hex strings (new stdlib path, returned by `hex()`, `oklch()`, etc.)
/// - `[r, g, b, a]` arrays with 0.0-1.0 floats (old app convention)
///
/// This dual-accept provides backward compatibility with existing theme.rhai files.
pub fn get_color_hex(scope: &Scope, name: &str) -> Option<String> {
    // Try string first (new stdlib convention)
    if let Some(s) = scope.get_value::<ImmutableString>(name) {
        let s = s.to_string();
        if s.starts_with('#') || s.starts_with('!') {
            return Some(s);
        }
    }

    // Fallback: try [r, g, b, a] array (old app convention, 0.0-1.0 floats)
    if let Some(arr) = scope.get_value::<Array>(name).filter(|a| a.len() >= 3) {
        let r = arr[0].as_float().ok()? as f32;
        let g = arr[1].as_float().ok()? as f32;
        let b = arr[2].as_float().ok()? as f32;
        let a = if arr.len() >= 4 {
            arr[3].as_float().ok().unwrap_or(1.0) as f32
        } else {
            1.0
        };
        let ri = (r.clamp(0.0, 1.0) * 255.0).round() as u8;
        let gi = (g.clamp(0.0, 1.0) * 255.0).round() as u8;
        let bi = (b.clamp(0.0, 1.0) * 255.0).round() as u8;
        if a >= 1.0 {
            return Some(format!("#{ri:02x}{gi:02x}{bi:02x}"));
        } else {
            let ai = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
            return Some(format!("#{ri:02x}{gi:02x}{bi:02x}{ai:02x}"));
        }
    }

    None
}

/// Extract a `[f32; 4]` from a Rhai scope.
fn get_vec4(scope: &Scope, name: &str) -> Option<[f32; 4]> {
    let arr = scope.get_value::<Array>(name)?;
    if arr.len() < 4 {
        return None;
    }
    let x = arr[0].as_float().ok()? as f32;
    let y = arr[1].as_float().ok()? as f32;
    let z = arr[2].as_float().ok()? as f32;
    let w = arr[3].as_float().ok()? as f32;
    Some([x, y, z, w])
}

/// Extract an f32 from a Rhai scope.
fn get_float(scope: &Scope, name: &str) -> Option<f32> {
    scope.get_value::<f64>(name).map(|v| v as f32)
}

/// Extract a bool from a Rhai scope.
fn get_bool(scope: &Scope, name: &str) -> Option<bool> {
    scope.get_value::<bool>(name)
}

/// Extract a String from a Rhai scope.
fn get_string(scope: &Scope, name: &str) -> Option<String> {
    scope
        .get_value::<rhai::ImmutableString>(name)
        .map(|s| s.to_string())
}

/// Parse a `ThemeData` from a Rhai scope.
///
/// Starts from `ThemeData::default()` and overlays any variables found.
pub fn parse_theme_data_from_scope(scope: &Scope) -> ThemeData {
    let mut td = ThemeData::default();

    // Macro for color fields
    macro_rules! color {
        ($field:ident) => {
            if let Some(c) = get_color_hex(scope, stringify!($field)) {
                td.$field = c;
            }
        };
        ($field:ident, $name:expr) => {
            if let Some(c) = get_color_hex(scope, $name) {
                td.$field = c;
            }
        };
    }

    macro_rules! float {
        ($field:ident) => {
            if let Some(v) = get_float(scope, stringify!($field)) {
                td.$field = v;
            }
        };
    }

    macro_rules! vec4 {
        ($field:ident) => {
            if let Some(v) = get_vec4(scope, stringify!($field)) {
                td.$field = v;
            }
        };
    }

    // Base UI
    color!(bg);
    color!(panel_bg);
    color!(fg);
    color!(fg_dim);
    color!(accent);
    color!(accent2);
    color!(border);
    color!(selection_bg);

    // Row colors
    color!(row_tool);
    color!(row_result);

    // Semantic
    color!(error);
    color!(warning);
    color!(success);

    // Mode colors
    color!(mode_normal);
    // Support both mode_chat and mode_insert (legacy)
    if let Some(c) =
        get_color_hex(scope, "mode_chat").or_else(|| get_color_hex(scope, "mode_insert"))
    {
        td.mode_chat = c;
    }
    color!(mode_shell);
    color!(mode_visual);

    // Cursor colors
    vec4!(cursor_normal);
    vec4!(cursor_insert);
    vec4!(cursor_visual);

    // ANSI colors
    let ansi_names = [
        ("ansi_black", &mut td.ansi.black),
        ("ansi_red", &mut td.ansi.red),
        ("ansi_green", &mut td.ansi.green),
        ("ansi_yellow", &mut td.ansi.yellow),
        ("ansi_blue", &mut td.ansi.blue),
        ("ansi_magenta", &mut td.ansi.magenta),
        ("ansi_cyan", &mut td.ansi.cyan),
        ("ansi_white", &mut td.ansi.white),
        ("ansi_bright_black", &mut td.ansi.bright_black),
        ("ansi_bright_red", &mut td.ansi.bright_red),
        ("ansi_bright_green", &mut td.ansi.bright_green),
        ("ansi_bright_yellow", &mut td.ansi.bright_yellow),
        ("ansi_bright_blue", &mut td.ansi.bright_blue),
        ("ansi_bright_magenta", &mut td.ansi.bright_magenta),
        ("ansi_bright_cyan", &mut td.ansi.bright_cyan),
        ("ansi_bright_white", &mut td.ansi.bright_white),
    ];
    for (name, field) in ansi_names {
        if let Some(c) = get_color_hex(scope, name) {
            *field = c;
        }
    }

    // Frame structure
    float!(frame_corner_size);
    float!(frame_edge_thickness);
    float!(frame_content_padding);

    // Frame colors
    color!(frame_base);
    color!(frame_focused);
    color!(frame_insert);
    color!(frame_visual);
    color!(frame_unfocused);
    color!(frame_edge);

    // Frame shader params
    vec4!(frame_params_base);
    vec4!(frame_params_focused);
    vec4!(frame_params_unfocused);

    // Edge dimming
    vec4!(frame_edge_dim_unfocused);
    vec4!(frame_edge_dim_focused);

    // Shader effects
    float!(effect_glow_radius);
    float!(effect_glow_intensity);
    float!(effect_glow_falloff);
    float!(effect_sheen_speed);
    float!(effect_sheen_sparkle_threshold);
    float!(effect_breathe_speed);
    float!(effect_breathe_amplitude);

    // Chasing border
    float!(effect_chase_speed);
    float!(effect_chase_width);
    float!(effect_chase_glow_radius);
    float!(effect_chase_glow_intensity);
    float!(effect_chase_color_cycle);

    // Input area
    float!(input_minimized_height);
    float!(input_docked_height);
    float!(input_overlay_width_pct);
    color!(input_backdrop_color);

    // Font configuration
    if let Some(v) = get_bool(scope, "font_rainbow") {
        td.font_rainbow = v;
    }
    if let Some(v) = get_string(scope, "font_mono") {
        td.font_mono = v;
    }
    if let Some(v) = get_string(scope, "font_serif") {
        td.font_serif = v;
    }
    if let Some(v) = get_string(scope, "font_sans") {
        td.font_sans = v;
    }

    // MSDF text rendering quality
    float!(msdf_hint_amount);
    float!(msdf_stem_darkening);
    float!(msdf_horz_scale);
    float!(msdf_vert_scale);
    float!(msdf_text_bias);
    float!(msdf_gamma_correction);

    // Constellation
    float!(constellation_base_radius);
    float!(constellation_ring_spacing);

    // Block borders
    color!(block_border_tool_call);
    color!(block_border_tool_result);
    color!(block_border_error);
    color!(block_border_thinking);
    color!(block_border_drift);
    float!(block_border_thickness);
    float!(block_border_corner_radius);
    float!(block_border_glow_radius);
    float!(block_border_glow_intensity);
    float!(block_border_padding);
    float!(block_spacing);

    // Compose
    color!(compose_border);
    color!(compose_bg);

    // Modal
    color!(modal_backdrop);

    // User/assistant text borders
    color!(block_border_user);
    color!(block_border_assistant);

    td
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhai::Engine;

    #[test]
    fn default_theme_data_has_valid_hex() {
        let td = ThemeData::default();
        assert!(parse_hex(&td.bg).is_some());
        assert!(parse_hex(&td.fg).is_some());
        assert!(parse_hex(&td.accent).is_some());
        assert!(parse_hex(&td.panel_bg).is_some()); // 8-char hex
    }

    #[test]
    fn parse_from_scope_with_hex_strings() {
        let mut engine = Engine::new();
        crate::register_stdlib(&mut engine);
        let mut scope = Scope::new();

        engine
            .run_with_scope(
                &mut scope,
                r##"
                    let bg = hex("#ff0000");
                    let font_rainbow = false;
                "##,
            )
            .unwrap();

        let td = parse_theme_data_from_scope(&scope);
        assert_eq!(td.bg, "#ff0000");
        assert!(!td.font_rainbow);
    }

    #[test]
    fn parse_from_scope_with_array_compat() {
        // Simulate old-style hex() that returns [r, g, b, a] array
        let mut engine = Engine::new();
        engine.register_fn("hex", |_s: &str| -> rhai::Array {
            vec![
                rhai::Dynamic::from_float(1.0),
                rhai::Dynamic::from_float(0.0),
                rhai::Dynamic::from_float(0.0),
                rhai::Dynamic::from_float(1.0),
            ]
        });

        let mut scope = Scope::new();
        engine
            .run_with_scope(
                &mut scope,
                r##"
                    let bg = hex("#ff0000");
                "##,
            )
            .unwrap();

        let td = parse_theme_data_from_scope(&scope);
        assert_eq!(td.bg, "#ff0000"); // Array [1.0, 0.0, 0.0, 1.0] → #ff0000
    }

    #[test]
    fn mode_insert_alias() {
        let mut engine = Engine::new();
        crate::register_stdlib(&mut engine);
        let mut scope = Scope::new();

        engine
            .run_with_scope(
                &mut scope,
                r##"
                    let mode_insert = hex("#00ff00");
                "##,
            )
            .unwrap();

        let td = parse_theme_data_from_scope(&scope);
        assert_eq!(td.mode_chat, "#00ff00");
    }

    #[test]
    fn oklch_in_theme() {
        let mut engine = Engine::new();
        crate::register_stdlib(&mut engine);
        let mut scope = Scope::new();

        engine
            .run_with_scope(
                &mut scope,
                r#"
                    let bg = oklch(0.2, 0.05, 260.0);
                "#,
            )
            .unwrap();

        let td = parse_theme_data_from_scope(&scope);
        assert!(td.bg.starts_with('#'));
        assert!(parse_hex(&td.bg).is_some());
    }
}
