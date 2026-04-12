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
            mode_insert: "#9ece6a".into(),
            mode_chat: "#9ece6a".into(),
            mode_shell: "#e0af68".into(),
            mode_visual: "#bb9af7".into(),

            // Mode labels
            mode_label_normal: "NORMAL".into(),
            mode_label_insert: "INSERT".into(),
            mode_label_visual: "VISUAL".into(),
            mode_label_shell: "SHELL".into(),
            mode_label_constellation: "CONSTELLATION".into(),
            mode_label_stack: "STACK".into(),
            mode_label_input: "INPUT".into(),

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
            block_border_glow_radius: 6.0,
            block_border_glow_intensity: 0.25,
            text_glow_radius: 2.5,
            text_glow_color: "#bfd1f259".into(), // soft cool white, 35% alpha
            block_border_padding: 0.6,
            block_spacing: 12.0,

            // Compose
            compose_border: "#3b4261".into(),
            compose_bg: "#1a1b26".into(),

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
    }
}
