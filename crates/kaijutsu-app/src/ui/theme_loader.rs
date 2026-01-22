//! Rhai-based theme loader for Kaijutsu
//!
//! Loads theme configuration from `~/.config/kaijutsu/theme.rhai` using the
//! Rhai scripting language. Falls back to `Theme::default()` on any error.
//!
//! ## Rhai API
//!
//! Functions available in theme scripts:
//! - `hex("#rrggbb")` → `[r, g, b, 1.0]`
//! - `hexa("#rrggbb", alpha)` → `[r, g, b, alpha]`
//! - `rgba(r, g, b, a)` → `[r, g, b, a]`
//! - `rgb(r, g, b)` → `[r, g, b, 1.0]`
//!
//! Example theme.rhai:
//! ```rhai
//! let bg = hex("#1a1b26");
//! let fg = hex("#e5e5e5");
//! let accent = hex("#7aa2f7");
//! ```

use bevy::math::Vec4;
use bevy::prelude::*;
use rhai::{Array, Dynamic, Engine, Scope};
use std::path::PathBuf;

use super::theme::{AnsiColors, Theme};

/// Get the theme file path (~/.config/kaijutsu/theme.rhai).
pub fn theme_file_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("kaijutsu").join("theme.rhai"))
}

/// Load theme from the user's config file.
///
/// If the file doesn't exist or has errors, returns `Theme::default()` and logs a warning.
pub fn load_theme() -> Theme {
    let Some(path) = theme_file_path() else {
        info!("No config directory available, using default theme");
        return Theme::default();
    };

    if !path.exists() {
        info!("Theme file not found at {:?}, using defaults", path);
        return Theme::default();
    }

    match load_theme_from_file(&path) {
        Ok(theme) => {
            info!("Loaded theme from {:?}", path);
            theme
        }
        Err(e) => {
            warn!("Failed to load theme from {:?}: {}", path, e);
            warn!("Falling back to default theme");
            Theme::default()
        }
    }
}

/// Load and parse a theme file.
fn load_theme_from_file(path: &PathBuf) -> Result<Theme, String> {
    let script = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read file: {}", e))?;

    parse_theme_script(&script)
}

/// Create a Rhai engine with theme functions registered.
fn create_engine() -> Engine {
    let mut engine = Engine::new();

    // hex("#rrggbb") → [r, g, b, 1.0]
    engine.register_fn("hex", |s: &str| -> Array {
        parse_hex_color(s, 1.0)
    });

    // hexa("#rrggbb", alpha) → [r, g, b, alpha]
    engine.register_fn("hexa", |s: &str, alpha: f64| -> Array {
        parse_hex_color(s, alpha as f32)
    });

    // rgba(r, g, b, a) → [r, g, b, a] (values 0.0-1.0)
    engine.register_fn("rgba", |r: f64, g: f64, b: f64, a: f64| -> Array {
        vec![
            Dynamic::from_float(r),
            Dynamic::from_float(g),
            Dynamic::from_float(b),
            Dynamic::from_float(a),
        ]
    });

    // rgb(r, g, b) → [r, g, b, 1.0] (values 0.0-1.0)
    engine.register_fn("rgb", |r: f64, g: f64, b: f64| -> Array {
        vec![
            Dynamic::from_float(r),
            Dynamic::from_float(g),
            Dynamic::from_float(b),
            Dynamic::from_float(1.0),
        ]
    });

    engine
}

/// Parse a hex color string to an RGBA array.
fn parse_hex_color(s: &str, alpha: f32) -> Array {
    let s = s.trim_start_matches('#');

    let (r, g, b) = if s.len() == 6 {
        let r = u8::from_str_radix(&s[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&s[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&s[4..6], 16).unwrap_or(0);
        (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0)
    } else if s.len() == 3 {
        // Short form #rgb → #rrggbb
        let r = u8::from_str_radix(&s[0..1], 16).unwrap_or(0);
        let g = u8::from_str_radix(&s[1..2], 16).unwrap_or(0);
        let b = u8::from_str_radix(&s[2..3], 16).unwrap_or(0);
        (
            (r * 17) as f32 / 255.0,
            (g * 17) as f32 / 255.0,
            (b * 17) as f32 / 255.0,
        )
    } else {
        warn!("Invalid hex color: #{}", s);
        (0.0, 0.0, 0.0)
    };

    vec![
        Dynamic::from_float(r as f64),
        Dynamic::from_float(g as f64),
        Dynamic::from_float(b as f64),
        Dynamic::from_float(alpha as f64),
    ]
}

/// Parse a theme script and build a Theme struct.
fn parse_theme_script(script: &str) -> Result<Theme, String> {
    let engine = create_engine();
    let mut scope = Scope::new();

    engine
        .run_with_scope(&mut scope, script)
        .map_err(|e| format!("Rhai parse error: {}", e))?;

    // Start with default theme
    let mut theme = Theme::default();

    // Extract variables from scope and apply to theme
    // Base UI colors
    if let Some(c) = get_color(&scope, "bg") {
        theme.bg = c;
    }
    if let Some(c) = get_color_with_alpha(&scope, "panel_bg") {
        theme.panel_bg = c;
    }
    if let Some(c) = get_color(&scope, "fg") {
        theme.fg = c;
    }
    if let Some(c) = get_color(&scope, "fg_dim") {
        theme.fg_dim = c;
    }
    if let Some(c) = get_color(&scope, "accent") {
        theme.accent = c;
    }
    if let Some(c) = get_color(&scope, "accent2") {
        theme.accent2 = c;
    }
    if let Some(c) = get_color(&scope, "border") {
        theme.border = c;
    }
    if let Some(c) = get_color_with_alpha(&scope, "selection_bg") {
        theme.selection_bg = c;
    }

    // Row colors
    if let Some(c) = get_color(&scope, "row_tool") {
        theme.row_tool = c;
    }
    if let Some(c) = get_color(&scope, "row_result") {
        theme.row_result = c;
    }

    // Semantic colors
    if let Some(c) = get_color(&scope, "error") {
        theme.error = c;
    }
    if let Some(c) = get_color(&scope, "warning") {
        theme.warning = c;
    }
    if let Some(c) = get_color(&scope, "success") {
        theme.success = c;
    }

    // Mode colors
    if let Some(c) = get_color(&scope, "mode_normal") {
        theme.mode_normal = c;
    }
    if let Some(c) = get_color(&scope, "mode_insert") {
        theme.mode_insert = c;
    }
    if let Some(c) = get_color(&scope, "mode_command") {
        theme.mode_command = c;
    }
    if let Some(c) = get_color(&scope, "mode_visual") {
        theme.mode_visual = c;
    }

    // Cursor colors (Vec4)
    if let Some(v) = get_vec4(&scope, "cursor_normal") {
        theme.cursor_normal = v;
    }
    if let Some(v) = get_vec4(&scope, "cursor_insert") {
        theme.cursor_insert = v;
    }
    if let Some(v) = get_vec4(&scope, "cursor_command") {
        theme.cursor_command = v;
    }
    if let Some(v) = get_vec4(&scope, "cursor_visual") {
        theme.cursor_visual = v;
    }

    // ANSI colors
    theme.ansi = extract_ansi_colors(&scope);

    // ═══════════════════════════════════════════════════════════════════════
    // Frame configuration
    // ═══════════════════════════════════════════════════════════════════════

    // Frame structure
    if let Some(v) = get_float(&scope, "frame_corner_size") {
        theme.frame_corner_size = v;
    }
    if let Some(v) = get_float(&scope, "frame_edge_thickness") {
        theme.frame_edge_thickness = v;
    }
    if let Some(v) = get_float(&scope, "frame_content_padding") {
        theme.frame_content_padding = v;
    }

    // Frame colors
    if let Some(c) = get_color_with_alpha(&scope, "frame_base") {
        theme.frame_base = c;
    }
    if let Some(c) = get_color_with_alpha(&scope, "frame_focused") {
        theme.frame_focused = c;
    }
    if let Some(c) = get_color_with_alpha(&scope, "frame_insert") {
        theme.frame_insert = c;
    }
    if let Some(c) = get_color_with_alpha(&scope, "frame_command") {
        theme.frame_command = c;
    }
    if let Some(c) = get_color_with_alpha(&scope, "frame_visual") {
        theme.frame_visual = c;
    }
    if let Some(c) = get_color_with_alpha(&scope, "frame_unfocused") {
        theme.frame_unfocused = c;
    }
    if let Some(c) = get_color_with_alpha(&scope, "frame_edge") {
        theme.frame_edge = c;
    }

    // Frame shader params
    if let Some(v) = get_vec4(&scope, "frame_params_base") {
        theme.frame_params_base = v;
    }
    if let Some(v) = get_vec4(&scope, "frame_params_focused") {
        theme.frame_params_focused = v;
    }
    if let Some(v) = get_vec4(&scope, "frame_params_unfocused") {
        theme.frame_params_unfocused = v;
    }

    // Edge dimming multipliers
    if let Some(v) = get_vec4(&scope, "frame_edge_dim_unfocused") {
        theme.frame_edge_dim_unfocused = v;
    }
    if let Some(v) = get_vec4(&scope, "frame_edge_dim_focused") {
        theme.frame_edge_dim_focused = v;
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Shader effect parameters (GPU-reactive)
    // ═══════════════════════════════════════════════════════════════════════
    if let Some(v) = get_float(&scope, "effect_glow_radius") {
        theme.effect_glow_radius = v;
    }
    if let Some(v) = get_float(&scope, "effect_glow_intensity") {
        theme.effect_glow_intensity = v;
    }
    if let Some(v) = get_float(&scope, "effect_glow_falloff") {
        theme.effect_glow_falloff = v;
    }
    if let Some(v) = get_float(&scope, "effect_sheen_speed") {
        theme.effect_sheen_speed = v;
    }
    if let Some(v) = get_float(&scope, "effect_sheen_sparkle_threshold") {
        theme.effect_sheen_sparkle_threshold = v;
    }
    if let Some(v) = get_float(&scope, "effect_breathe_speed") {
        theme.effect_breathe_speed = v;
    }
    if let Some(v) = get_float(&scope, "effect_breathe_amplitude") {
        theme.effect_breathe_amplitude = v;
    }

    // Chasing border effect parameters
    if let Some(v) = get_float(&scope, "effect_chase_speed") {
        theme.effect_chase_speed = v;
    }
    if let Some(v) = get_float(&scope, "effect_chase_width") {
        theme.effect_chase_width = v;
    }
    if let Some(v) = get_float(&scope, "effect_chase_glow_radius") {
        theme.effect_chase_glow_radius = v;
    }
    if let Some(v) = get_float(&scope, "effect_chase_glow_intensity") {
        theme.effect_chase_glow_intensity = v;
    }
    if let Some(v) = get_float(&scope, "effect_chase_color_cycle") {
        theme.effect_chase_color_cycle = v;
    }

    Ok(theme)
}

/// Extract a color from scope (ignores alpha, sets to 1.0).
fn get_color(scope: &Scope, name: &str) -> Option<Color> {
    let arr = scope.get_value::<Array>(name)?;
    if arr.len() < 3 {
        return None;
    }
    let r = arr[0].as_float().ok()? as f32;
    let g = arr[1].as_float().ok()? as f32;
    let b = arr[2].as_float().ok()? as f32;
    Some(Color::srgb(r, g, b))
}

/// Extract a color from scope (preserves alpha).
fn get_color_with_alpha(scope: &Scope, name: &str) -> Option<Color> {
    let arr = scope.get_value::<Array>(name)?;
    if arr.len() < 4 {
        return get_color(scope, name);
    }
    let r = arr[0].as_float().ok()? as f32;
    let g = arr[1].as_float().ok()? as f32;
    let b = arr[2].as_float().ok()? as f32;
    let a = arr[3].as_float().ok()? as f32;
    Some(Color::srgba(r, g, b, a))
}

/// Extract a Vec4 from scope.
fn get_vec4(scope: &Scope, name: &str) -> Option<Vec4> {
    let arr = scope.get_value::<Array>(name)?;
    if arr.len() < 4 {
        return None;
    }
    let x = arr[0].as_float().ok()? as f32;
    let y = arr[1].as_float().ok()? as f32;
    let z = arr[2].as_float().ok()? as f32;
    let w = arr[3].as_float().ok()? as f32;
    Some(Vec4::new(x, y, z, w))
}

/// Extract a float from scope.
fn get_float(scope: &Scope, name: &str) -> Option<f32> {
    scope.get_value::<f64>(name).map(|v| v as f32)
}

/// Extract ANSI colors from scope.
fn extract_ansi_colors(scope: &Scope) -> AnsiColors {
    let mut ansi = AnsiColors::default();

    // Helper to try to get a color or use default
    let get = |name: &str, default: Color| -> Color {
        get_color(scope, name).unwrap_or(default)
    };

    // Standard colors
    ansi.black = get("ansi_black", ansi.black);
    ansi.red = get("ansi_red", ansi.red);
    ansi.green = get("ansi_green", ansi.green);
    ansi.yellow = get("ansi_yellow", ansi.yellow);
    ansi.blue = get("ansi_blue", ansi.blue);
    ansi.magenta = get("ansi_magenta", ansi.magenta);
    ansi.cyan = get("ansi_cyan", ansi.cyan);
    ansi.white = get("ansi_white", ansi.white);

    // Bright variants
    ansi.bright_black = get("ansi_bright_black", ansi.bright_black);
    ansi.bright_red = get("ansi_bright_red", ansi.bright_red);
    ansi.bright_green = get("ansi_bright_green", ansi.bright_green);
    ansi.bright_yellow = get("ansi_bright_yellow", ansi.bright_yellow);
    ansi.bright_blue = get("ansi_bright_blue", ansi.bright_blue);
    ansi.bright_magenta = get("ansi_bright_magenta", ansi.bright_magenta);
    ansi.bright_cyan = get("ansi_bright_cyan", ansi.bright_cyan);
    ansi.bright_white = get("ansi_bright_white", ansi.bright_white);

    ansi
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_parsing() {
        let arr = parse_hex_color("#1a1b26", 1.0);
        assert_eq!(arr.len(), 4);
        // #1a = 26/255 ≈ 0.102
        let r = arr[0].as_float().unwrap();
        assert!((r - 0.102).abs() < 0.01);
    }

    #[test]
    fn test_simple_script() {
        let script = r##"
            let bg = hex("#1a1b26");
            let fg = hex("#e5e5e5");
        "##;

        let theme = parse_theme_script(script).unwrap();
        // bg should be close to #1a1b26
        let srgba = theme.bg.to_srgba();
        assert!((srgba.red - 0.102).abs() < 0.01);
    }

    #[test]
    fn test_rgba_function() {
        let script = r##"
            let panel_bg = rgba(0.1, 0.2, 0.3, 0.9);
        "##;

        let theme = parse_theme_script(script).unwrap();
        let srgba = theme.panel_bg.to_srgba();
        assert!((srgba.red - 0.1).abs() < 0.01);
        assert!((srgba.alpha - 0.9).abs() < 0.01);
    }
}
