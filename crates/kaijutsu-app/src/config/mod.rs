//! TOML config loader for app-side configuration.
//!
//! Loads theme from `theme.toml` and bindings from `bindings.toml`.
//! Falls back to compiled-in defaults if files don't exist or have errors.

use bevy::prelude::*;
use kaijutsu_types::theme::ThemeData;
use std::path::Path;

use crate::input::binding::Binding;
use crate::input::defaults::default_bindings;
use crate::input::rhai_config::{bindings_to_toml, parse_bindings_toml};
use crate::ui::theme::Theme;

/// Combined output of app-side config loading.
pub struct AppConfig {
    pub theme: Theme,
    #[allow(dead_code)]
    pub bindings: Vec<Binding>,
}

/// Load theme and bindings from user config.
///
/// Reads `theme.toml` and `bindings.toml` from `~/.config/kaijutsu/`.
/// Falls back to defaults for each file independently if loading fails.
pub fn load_app_config() -> AppConfig {
    let Some(config_dir) = dirs::config_dir().map(|p| p.join("kaijutsu")) else {
        info!("No config directory available, using defaults for theme and bindings");
        return AppConfig {
            theme: Theme::default(),
            bindings: default_bindings(),
        };
    };

    let theme = load_theme_toml(&config_dir);
    let bindings = load_bindings_toml(&config_dir);

    AppConfig { theme, bindings }
}

fn load_theme_toml(config_dir: &Path) -> Theme {
    let theme_path = config_dir.join("theme.toml");
    if !theme_path.exists() {
        return Theme::default();
    }

    let content = match std::fs::read_to_string(&theme_path) {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to read theme {:?}: {}", theme_path, e);
            return Theme::default();
        }
    };

    match toml::from_str::<ThemeData>(&content) {
        Ok(td) => {
            info!("Loaded theme from {:?}", theme_path);
            Theme::from(td)
        }
        Err(e) => {
            warn!("Failed to parse theme: {}", e);
            Theme::default()
        }
    }
}

fn load_bindings_toml(config_dir: &Path) -> Vec<Binding> {
    let bindings_path = config_dir.join("bindings.toml");
    if !bindings_path.exists() {
        return default_bindings();
    }

    let content = match std::fs::read_to_string(&bindings_path) {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to read bindings {:?}: {}", bindings_path, e);
            return default_bindings();
        }
    };

    match parse_bindings_toml(&content) {
        Ok(bindings) => {
            info!("Loaded bindings from {:?}", bindings_path);
            bindings
        }
        Err(e) => {
            warn!("Failed to parse bindings: {}", e);
            default_bindings()
        }
    }
}

/// Write default config files to the user config dir if they don't exist.
pub fn write_default_configs_if_missing() {
    let Some(config_dir) = dirs::config_dir().map(|p| p.join("kaijutsu")) else {
        return;
    };

    if let Err(e) = std::fs::create_dir_all(&config_dir) {
        warn!("Could not create config dir {:?}: {}", config_dir, e);
        return;
    }

    // Write default bindings as TOML
    let bindings_path = config_dir.join("bindings.toml");
    if !bindings_path.exists() {
        let content = bindings_to_toml(&default_bindings());
        match std::fs::write(&bindings_path, &content) {
            Ok(()) => info!("Wrote default bindings to {:?}", bindings_path),
            Err(e) => warn!("Could not write bindings to {:?}: {}", bindings_path, e),
        }
    }

    // Write default theme as TOML
    let theme_path = config_dir.join("theme.toml");
    if !theme_path.exists() {
        let content = include_str!("../../../../assets/defaults/theme.toml");
        match std::fs::write(&theme_path, content) {
            Ok(()) => info!("Wrote default theme to {:?}", theme_path),
            Err(e) => warn!("Could not write theme to {:?}: {}", theme_path, e),
        }
    }
}
