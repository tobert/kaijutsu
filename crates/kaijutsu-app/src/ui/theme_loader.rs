//! TOML-based theme loader for Kaijutsu.
//!
//! Loads theme configuration from `~/.config/kaijutsu/theme.toml`.
//! Missing file → `Theme::default()`; parse/IO errors → `Err` so the caller
//! can surface them (no silent fallback).
//!
//! Theme values are plain hex strings and numbers — no computed colors.
//! Users who want procedural themes can generate TOML with external tools.

use bevy::prelude::*;
use kaijutsu_types::theme::ThemeData;
use std::path::{Path, PathBuf};

use super::theme::Theme;

/// File-level failure loading theme.toml.
#[derive(Debug)]
pub enum ThemeConfigError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        message: String,
    },
}

impl std::fmt::Display for ThemeConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "failed to read theme at {}: {source}", path.display())
            }
            Self::Parse { path, message } => {
                write!(f, "failed to parse theme at {}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for ThemeConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { .. } => None,
        }
    }
}

/// Get the theme file path (~/.config/kaijutsu/theme.toml).
#[allow(dead_code)]
pub fn theme_file_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("kaijutsu").join("theme.toml"))
}

/// Load theme from the user's config file at the default path.
///
/// Missing file → `Ok(Theme::default())`. Read or parse failure → `Err` with
/// the path and message — no silent fallback to default.
#[allow(dead_code)]
pub fn load_theme() -> Result<Theme, ThemeConfigError> {
    let Some(path) = theme_file_path() else {
        info!("No config directory available, using default theme");
        return Ok(Theme::default());
    };
    load_theme_from_path(&path)
}

/// Load theme from an explicit path (testable).
pub fn load_theme_from_path(path: &Path) -> Result<Theme, ThemeConfigError> {
    if !path.exists() {
        info!("Theme not found at {:?}, using defaults", path);
        return Ok(Theme::default());
    }

    let content = std::fs::read_to_string(path).map_err(|e| ThemeConfigError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    let theme = parse_theme_toml(&content).map_err(|msg| ThemeConfigError::Parse {
        path: path.to_path_buf(),
        message: msg,
    })?;
    info!("Loaded theme from {:?}", path);
    Ok(theme)
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

    #[test]
    fn test_load_theme_missing_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("theme.toml");
        assert!(!path.exists());
        let theme = load_theme_from_path(&path).unwrap();
        // Default theme should come back; compare one known field.
        assert_eq!(theme.bg, Theme::default().bg);
    }

    #[test]
    fn test_load_theme_malformed_does_not_silently_fall_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("theme.toml");
        std::fs::write(&path, "[this is not ::: valid").unwrap();
        match load_theme_from_path(&path) {
            Ok(_) => panic!("malformed theme must not silently fall back to default"),
            Err(ThemeConfigError::Parse { path: p, .. }) => assert_eq!(p, path),
            Err(other) => panic!("expected Parse error, got {other}"),
        }
    }
}
