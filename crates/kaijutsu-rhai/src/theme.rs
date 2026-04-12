//! Rhai scope parsing for theme data.
//!
//! Types (`ThemeData`, `AnsiColorsData`) live in `kaijutsu_types::theme`.
//! This module provides Rhai-specific extraction from a `Scope`.

use rhai::{Array, ImmutableString, Scope};

// Re-export the canonical types so existing consumers compile unchanged.
pub use kaijutsu_types::theme::{AnsiColorsData, ThemeData};

#[cfg(test)]
use crate::color::parse_hex;

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
    color!(mode_insert);
    // Support both mode_chat and mode_insert (legacy: mode_insert sets mode_chat)
    if let Some(c) =
        get_color_hex(scope, "mode_chat").or_else(|| get_color_hex(scope, "mode_insert"))
    {
        td.mode_chat = c;
    }
    color!(mode_shell);
    color!(mode_visual);

    // Mode labels (dock HUD text)
    macro_rules! string {
        ($field:ident) => {
            if let Some(v) = get_string(scope, stringify!($field)) {
                td.$field = v;
            }
        };
    }
    string!(mode_label_normal);
    string!(mode_label_insert);
    string!(mode_label_visual);
    string!(mode_label_shell);
    string!(mode_label_constellation);
    string!(mode_label_stack);
    string!(mode_label_input);

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
    float!(text_glow_radius);
    color!(text_glow_color);
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

    // Layout spacing
    float!(indent_width);
    float!(role_header_height);
    float!(role_header_spacing);
    float!(label_font_size);
    float!(label_inset);
    float!(label_pad);

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
