//! Rhai-based theme loader for Kaijutsu
//!
//! Loads theme configuration from `~/.config/kaijutsu/theme.rhai` using the
//! Rhai scripting language. Falls back to `Theme::default()` on any error.
//!
//! ## Config as CRDT (Phase 2)
//!
//! Config files are now managed by `ConfigCrdtBackend` on the server side.
//! The server:
//! 1. Loads config from disk into CRDT documents at kernel creation
//! 2. Watches for external edits and syncs them to CRDT
//! 3. Debounces CRDT changes and flushes to disk
//!
//! For Phase 2, the client loads directly from disk (which is synced by the server).
//! Future phases will add:
//! - Real-time CRDT sync for live updates
//! - Multi-seat config merging (base + seat overrides)
//!
//! ## Multi-Seat Architecture
//!
//! When multiple computers connect to the same kernel, each needs its own UI config:
//! - `~/.config/kaijutsu/theme.rhai` — Base theme (shared)
//! - `~/.config/kaijutsu/seats/{seat_id}.rhai` — Per-seat overrides
//!
//! Seat config contains only values to override from base (e.g., font_size for 4K).
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

/// Get the seat config file path (~/.config/kaijutsu/seats/{seat_id}.rhai).
pub fn seat_config_path(seat_id: &str) -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("kaijutsu").join("seats").join(format!("{}.rhai", seat_id)))
}

/// Get the current seat ID (hostname by default).
pub fn current_seat_id() -> String {
    // Try hostname
    if let Ok(name) = hostname::get() {
        if let Some(name_str) = name.to_str() {
            if !name_str.is_empty() {
                return name_str.to_string();
            }
        }
    }

    // Fallback
    "default".to_string()
}

/// Load theme from the user's config file.
///
/// If the file doesn't exist or has errors, returns `Theme::default()` and logs a warning.
pub fn load_theme() -> Theme {
    let seat_id = current_seat_id();
    load_theme_with_seat(Some(&seat_id))
}

/// Load theme with optional seat-specific overrides.
///
/// If `seat_id` is provided, loads base theme then applies seat overrides on top.
/// Seat configs only need to define values they want to change.
pub fn load_theme_with_seat(seat_id: Option<&str>) -> Theme {
    let Some(base_path) = theme_file_path() else {
        info!("No config directory available, using default theme");
        return Theme::default();
    };

    // Load base theme
    let base_script = if base_path.exists() {
        match std::fs::read_to_string(&base_path) {
            Ok(s) => {
                info!("Loaded base theme from {:?}", base_path);
                s
            }
            Err(e) => {
                warn!("Failed to read base theme {:?}: {}", base_path, e);
                String::new()
            }
        }
    } else {
        info!("Base theme not found at {:?}, using defaults", base_path);
        String::new()
    };

    // Load seat overrides if specified
    let seat_script = if let Some(seat_id) = seat_id {
        if let Some(seat_path) = seat_config_path(seat_id) {
            if seat_path.exists() {
                match std::fs::read_to_string(&seat_path) {
                    Ok(s) => {
                        info!("Loaded seat config for '{}' from {:?}", seat_id, seat_path);
                        s
                    }
                    Err(e) => {
                        warn!("Failed to read seat config {:?}: {}", seat_path, e);
                        String::new()
                    }
                }
            } else {
                debug!("No seat config found for '{}' at {:?}", seat_id, seat_path);
                String::new()
            }
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    // Merge: base first, then seat overrides (Rhai variable shadowing)
    let merged_script = if seat_script.is_empty() {
        base_script
    } else {
        format!(
            "// Base theme\n{}\n\n// Seat overrides\n{}",
            base_script, seat_script
        )
    };

    if merged_script.is_empty() {
        return Theme::default();
    }

    match parse_theme_script(&merged_script) {
        Ok(theme) => theme,
        Err(e) => {
            warn!("Failed to parse theme: {}", e);
            warn!("Falling back to default theme");
            Theme::default()
        }
    }
}

/// Load theme from a script string.
///
/// Used for loading from CRDT content or testing.
#[allow(dead_code)] // For future CRDT-based live reload
pub fn load_theme_from_script(script: &str) -> Result<Theme, String> {
    parse_theme_script(script)
}

/// Load and parse a theme file.
#[allow(dead_code)] // For future file-based reload
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
    // Support both mode_chat (new) and mode_insert (legacy) for backward compatibility
    if let Some(c) = get_color(&scope, "mode_chat").or_else(|| get_color(&scope, "mode_insert")) {
        theme.mode_chat = c;
    }
    if let Some(c) = get_color(&scope, "mode_shell") {
        theme.mode_shell = c;
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

    // ═══════════════════════════════════════════════════════════════════════
    // Input area configuration
    // ═══════════════════════════════════════════════════════════════════════
    if let Some(v) = get_float(&scope, "input_minimized_height") {
        theme.input_minimized_height = v;
    }
    if let Some(v) = get_float(&scope, "input_docked_height") {
        theme.input_docked_height = v;
    }
    if let Some(v) = get_float(&scope, "input_overlay_width_pct") {
        theme.input_overlay_width_pct = v;
    }
    if let Some(c) = get_color_with_alpha(&scope, "input_backdrop_color") {
        theme.input_backdrop_color = c;
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Font rendering quality (MSDF text)
    // ═══════════════════════════════════════════════════════════════════════
    if let Some(v) = get_float(&scope, "font_stem_darkening") {
        theme.font_stem_darkening = v;
    }
    if let Some(v) = get_float(&scope, "font_hint_amount") {
        theme.font_hint_amount = v;
    }
    if let Some(v) = get_bool(&scope, "font_taa_enabled") {
        theme.font_taa_enabled = v;
    }
    if let Some(v) = get_float(&scope, "font_horz_scale") {
        theme.font_horz_scale = v;
    }
    if let Some(v) = get_float(&scope, "font_vert_scale") {
        theme.font_vert_scale = v;
    }
    if let Some(v) = get_float(&scope, "font_text_bias") {
        theme.font_text_bias = v;
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Font effects (MSDF text)
    // ═══════════════════════════════════════════════════════════════════════
    if let Some(v) = get_float(&scope, "font_glow_intensity") {
        theme.font_glow_intensity = v;
    }
    if let Some(v) = get_float(&scope, "font_glow_spread") {
        theme.font_glow_spread = v;
    }
    if let Some(c) = get_color_with_alpha(&scope, "font_glow_color") {
        theme.font_glow_color = c;
    }
    if let Some(v) = get_bool(&scope, "font_rainbow") {
        theme.font_rainbow = v;
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Constellation configuration
    // ═══════════════════════════════════════════════════════════════════════
    if let Some(v) = get_float(&scope, "constellation_layout_radius") {
        theme.constellation_layout_radius = v;
    }
    if let Some(v) = get_float(&scope, "constellation_node_size") {
        theme.constellation_node_size = v;
    }
    if let Some(v) = get_float(&scope, "constellation_node_size_focused") {
        theme.constellation_node_size_focused = v;
    }
    if let Some(c) = get_color_with_alpha(&scope, "constellation_node_glow_idle") {
        theme.constellation_node_glow_idle = c;
    }
    if let Some(c) = get_color_with_alpha(&scope, "constellation_node_glow_active") {
        theme.constellation_node_glow_active = c;
    }
    if let Some(c) = get_color_with_alpha(&scope, "constellation_node_glow_streaming") {
        theme.constellation_node_glow_streaming = c;
    }
    if let Some(c) = get_color_with_alpha(&scope, "constellation_node_glow_error") {
        theme.constellation_node_glow_error = c;
    }
    if let Some(v) = get_float(&scope, "constellation_connection_glow") {
        theme.constellation_connection_glow = v;
    }
    if let Some(c) = get_color_with_alpha(&scope, "constellation_connection_color") {
        theme.constellation_connection_color = c;
    }
    if let Some(v) = get_int(&scope, "constellation_particle_budget") {
        theme.constellation_particle_budget = v;
    }
    if let Some(v) = get_float(&scope, "constellation_orbital_speed") {
        theme.constellation_orbital_speed = v;
    }

    // ═══════════════════════════════════════════════════════════════════════
    // HUD configuration
    // ═══════════════════════════════════════════════════════════════════════
    if let Some(c) = get_color_with_alpha(&scope, "hud_panel_bg") {
        theme.hud_panel_bg = c;
    }
    if let Some(c) = get_color_with_alpha(&scope, "hud_panel_glow") {
        theme.hud_panel_glow = c;
    }
    if let Some(v) = get_float(&scope, "hud_panel_glow_intensity") {
        theme.hud_panel_glow_intensity = v;
    }
    if let Some(v) = get_float(&scope, "hud_orbital_curve_radius") {
        theme.hud_orbital_curve_radius = v;
    }
    if let Some(c) = get_color(&scope, "hud_text_color") {
        theme.hud_text_color = c;
    }
    if let Some(v) = get_float(&scope, "hud_font_size") {
        theme.hud_font_size = v;
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

/// Extract an integer from scope (as u32).
fn get_int(scope: &Scope, name: &str) -> Option<u32> {
    scope.get_value::<i64>(name).map(|v| v as u32)
}

/// Extract a bool from scope.
fn get_bool(scope: &Scope, name: &str) -> Option<bool> {
    scope.get_value::<bool>(name)
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

    #[test]
    fn test_font_rendering_params() {
        let script = r##"
            let font_stem_darkening = 0.25;
            let font_hint_amount = 0.5;
            let font_taa_enabled = false;
            let font_horz_scale = 1.2;
            let font_vert_scale = 0.7;
            let font_text_bias = 0.48;
            let font_glow_intensity = 0.8;
            let font_glow_spread = 5.0;
            let font_glow_color = rgba(1.0, 0.0, 0.5, 0.8);
            let font_rainbow = true;
        "##;

        let theme = parse_theme_script(script).unwrap();
        assert!((theme.font_stem_darkening - 0.25).abs() < 0.01);
        assert!((theme.font_hint_amount - 0.5).abs() < 0.01);
        assert!(!theme.font_taa_enabled);
        assert!((theme.font_horz_scale - 1.2).abs() < 0.01);
        assert!((theme.font_vert_scale - 0.7).abs() < 0.01);
        assert!((theme.font_text_bias - 0.48).abs() < 0.01);
        assert!((theme.font_glow_intensity - 0.8).abs() < 0.01);
        assert!((theme.font_glow_spread - 5.0).abs() < 0.01);
        let glow_srgba = theme.font_glow_color.to_srgba();
        assert!((glow_srgba.red - 1.0).abs() < 0.01);
        assert!((glow_srgba.green - 0.0).abs() < 0.01);
        assert!((glow_srgba.blue - 0.5).abs() < 0.01);
        assert!((glow_srgba.alpha - 0.8).abs() < 0.01);
        assert!(theme.font_rainbow);
    }
}
