//! Rhai-based theme loader for Kaijutsu
//!
//! Loads theme configuration from `~/.config/kaijutsu/theme.rhai` using the
//! Rhai scripting language. Falls back to `Theme::default()` on any error.
//!
//! Theme parsing routes through `kaijutsu_rhai::theme::ThemeData` for
//! framework-agnostic extraction, then converts to Bevy `Theme` via
//! `From<ThemeData>`.
//!
//! ## Rhai API
//!
//! Functions available in theme scripts (via kaijutsu-rhai stdlib):
//! - `hex("#rrggbb")` → `"#rrggbb"` (validates and normalizes)
//! - `hexa("#rrggbb", alpha)` → `"#rrggbbaa"`
//! - `oklch(l, c, h)` → `"#rrggbb"` (perceptually uniform)
//! - `hsl(h, s, l)` → `"#rrggbb"`
//! - `color_mix(hex1, hex2, t)` → `"#rrggbb"` (Oklab interpolation)
//! - `color_lighten(hex, amount)` / `color_darken(hex, amount)`
//! - `hue_shift(hex, degrees)`
//! - plus all math functions (sin, cos, lerp, clamp, etc.)
//!
//! Example theme.rhai:
//! ```rhai
//! let bg = hex("#1a1b26");
//! let fg = hex("#e5e5e5");
//! let accent = oklch(0.7, 0.15, 260.0);
//! ```

use bevy::prelude::*;
use rhai::{Engine, Scope};
use std::path::PathBuf;

use super::theme::Theme;

/// Get the theme file path (~/.config/kaijutsu/theme.rhai).
#[allow(dead_code)] // Kept as convenience wrapper; loading now goes through config::load_app_config
pub fn theme_file_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("kaijutsu").join("theme.rhai"))
}

/// Load theme from the user's config file.
///
/// Loads from `~/.config/kaijutsu/theme.rhai`. Falls back to `Theme::default()`
/// if the file doesn't exist or has errors.
#[allow(dead_code)] // Kept as convenience wrapper; loading now goes through config::load_app_config
pub fn load_theme() -> Theme {
    let Some(base_path) = theme_file_path() else {
        info!("No config directory available, using default theme");
        return Theme::default();
    };

    if !base_path.exists() {
        info!("Theme not found at {:?}, using defaults", base_path);
        return Theme::default();
    }

    let script = match std::fs::read_to_string(&base_path) {
        Ok(s) => {
            info!("Loaded theme from {:?}", base_path);
            s
        }
        Err(e) => {
            warn!("Failed to read theme {:?}: {}", base_path, e);
            return Theme::default();
        }
    };

    match parse_theme_script(&script) {
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
    let script =
        std::fs::read_to_string(path).map_err(|e| format!("Failed to read file: {}", e))?;

    parse_theme_script(&script)
}

/// Extract theme variables from a pre-populated Rhai scope.
///
/// Routes through `kaijutsu_rhai::theme::ThemeData` for framework-agnostic parsing,
/// then converts to Bevy `Theme` via `From<ThemeData>`.
pub fn parse_theme_from_scope(scope: &Scope) -> Theme {
    let theme_data = kaijutsu_rhai::theme::parse_theme_data_from_scope(scope);
    Theme::from(theme_data)
}

/// Parse a theme script and build a Theme struct.
fn parse_theme_script(script: &str) -> Result<Theme, String> {
    let mut engine = Engine::new();
    kaijutsu_rhai::register_stdlib(&mut engine);
    let mut scope = Scope::new();

    engine
        .run_with_scope(&mut scope, script)
        .map_err(|e| format!("Rhai parse error: {}", e))?;

    Ok(parse_theme_from_scope(&scope))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_font_rainbow_param() {
        let script = r##"
            let font_rainbow = true;
        "##;

        let theme = parse_theme_script(script).unwrap();
        assert!(theme.font_rainbow);
    }

    #[test]
    fn test_oklch_in_theme_script() {
        let script = r#"
            let accent = oklch(0.7, 0.15, 260.0);
        "#;

        let theme = parse_theme_script(script).unwrap();
        // Should have a valid color (not default)
        let srgba = theme.accent.to_srgba();
        // oklch(0.7, 0.15, 260) is a blue-ish color
        assert!(srgba.blue > srgba.red);
    }

    #[test]
    fn test_color_mix_in_theme_script() {
        let script = r##"
            let bg = color_mix("#000000", "#ffffff", 0.5);
        "##;

        let theme = parse_theme_script(script).unwrap();
        // 50% mix of black and white should be near gray
        let srgba = theme.bg.to_srgba();
        // In Oklab space, the midpoint may not be exactly 0.5 in sRGB
        assert!(srgba.red > 0.2 && srgba.red < 0.8);
    }
}
