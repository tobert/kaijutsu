//! TOML-based theme loader for Kaijutsu.
//!
//! Loads theme configuration from `~/.config/kaijutsu/theme.toml`.
//! Falls back to `Theme::default()` on any error.
//!
//! Theme values are plain hex strings and numbers — no computed colors.
//! Users who want procedural themes can generate TOML with external tools.

use bevy::prelude::*;
use kaijutsu_types::theme::ThemeData;
use std::path::PathBuf;

use super::theme::Theme;

/// Get the theme file path (~/.config/kaijutsu/theme.toml).
#[allow(dead_code)]
pub fn theme_file_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("kaijutsu").join("theme.toml"))
}

/// Load theme from the user's config file.
///
/// Loads from `~/.config/kaijutsu/theme.toml`. Falls back to `Theme::default()`
/// if the file doesn't exist or has errors.
#[allow(dead_code)]
pub fn load_theme() -> Theme {
    let Some(base_path) = theme_file_path() else {
        info!("No config directory available, using default theme");
        return Theme::default();
    };

    if !base_path.exists() {
        info!("Theme not found at {:?}, using defaults", base_path);
        return Theme::default();
    }

    let content = match std::fs::read_to_string(&base_path) {
        Ok(s) => {
            info!("Loaded theme from {:?}", base_path);
            s
        }
        Err(e) => {
            warn!("Failed to read theme {:?}: {}", base_path, e);
            return Theme::default();
        }
    };

    match parse_theme_toml(&content) {
        Ok(theme) => theme,
        Err(e) => {
            warn!("Failed to parse theme: {}", e);
            warn!("Falling back to default theme");
            Theme::default()
        }
    }
}

/// Load theme from a TOML content string.
#[allow(dead_code)]
pub fn load_theme_from_toml(content: &str) -> Result<Theme, String> {
    parse_theme_toml(content)
}

/// Parse a TOML string into a Theme.
pub fn parse_theme_toml(content: &str) -> Result<Theme, String> {
    let theme_data: ThemeData =
        toml::from_str(content).map_err(|e| format!("TOML parse error: {e}"))?;
    Ok(Theme::from(theme_data))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_THEME_TOML: &str = include_str!("../../../../assets/defaults/theme.toml");

    #[test]
    fn test_default_theme_toml_parses() {
        let theme = parse_theme_toml(DEFAULT_THEME_TOML).unwrap();
        // bg should be close to #1a1b26
        let srgba = theme.bg.to_srgba();
        assert!((srgba.red - 0.102).abs() < 0.01);
    }

    #[test]
    fn test_font_rainbow_param() {
        // Default is true
        let td: ThemeData = toml::from_str(DEFAULT_THEME_TOML).unwrap();
        assert!(td.font_rainbow);

        // Override to false
        let modified = DEFAULT_THEME_TOML.replace("font_rainbow = true", "font_rainbow = false");
        let theme = parse_theme_toml(&modified).unwrap();
        assert!(!theme.font_rainbow);
    }

    #[test]
    fn test_invalid_toml_returns_error() {
        let result = parse_theme_toml("[invalid");
        assert!(result.is_err());
    }
}
