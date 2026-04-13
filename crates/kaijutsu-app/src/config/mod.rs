//! TOML config loader for app-side configuration.
//!
//! Loads theme from `theme.toml` and bindings from `bindings.toml`.
//! Falls back to compiled-in defaults if files don't exist or have errors.

use bevy::prelude::*;
use std::path::Path;

use crate::input::binding::Binding;
use crate::input::bindings_config::{self, bindings_to_toml};
use crate::input::defaults::default_bindings;
use crate::ui::theme::Theme;
use crate::ui::theme_loader::{self, ThemeConfigError};
use crate::view::components::GlobalErrorQueue;

/// Errors collected while loading `theme.toml`/`bindings.toml` at startup.
///
/// Drained once by [`drain_startup_errors`] once `GlobalErrorQueue` is
/// available; after that the Vec is empty.
#[derive(Resource, Default)]
pub struct StartupConfigErrors(pub Vec<String>);

/// Combined output of app-side config loading.
///
/// `errors` contains human-readable messages for anything that went wrong
/// loading theme or bindings (missing-file is NOT an error). A startup
/// system drains this into the `GlobalErrorQueue` so the user sees the
/// problem without the app refusing to launch.
pub struct AppConfig {
    pub theme: Theme,
    #[allow(dead_code)]
    pub bindings: Vec<Binding>,
    pub errors: Vec<String>,
}

/// Load theme and bindings from user config.
///
/// Reads `theme.toml` and `bindings.toml` from `~/.config/kaijutsu/`.
/// File-level and per-entry errors are accumulated in `AppConfig::errors`;
/// good values still load and the app continues to boot.
pub fn load_app_config() -> AppConfig {
    let Some(config_dir) = dirs::config_dir().map(|p| p.join("kaijutsu")) else {
        info!("No config directory available, using defaults for theme and bindings");
        return AppConfig {
            theme: Theme::default(),
            bindings: default_bindings(),
            errors: Vec::new(),
        };
    };

    let mut errors = Vec::new();
    let theme = load_theme_toml(&config_dir, &mut errors);
    let bindings = load_bindings_toml(&config_dir, &mut errors);

    AppConfig {
        theme,
        bindings,
        errors,
    }
}

fn load_theme_toml(config_dir: &Path, errors: &mut Vec<String>) -> Theme {
    let theme_path = config_dir.join("theme.toml");
    match theme_loader::load_theme_from_path(&theme_path) {
        Ok(theme) => theme,
        Err(e) => {
            error!("{e}");
            errors.push(match &e {
                ThemeConfigError::Io { .. } | ThemeConfigError::Parse { .. } => e.to_string(),
            });
            Theme::default()
        }
    }
}

/// Drain `StartupConfigErrors` into `GlobalErrorQueue` so users see
/// config-load failures as dock HUD toasts.
pub fn drain_startup_errors(
    time: Res<Time>,
    mut errors: ResMut<StartupConfigErrors>,
    mut queue: ResMut<GlobalErrorQueue>,
) {
    if errors.0.is_empty() {
        return;
    }
    let now = time.elapsed_secs_f64();
    for msg in errors.0.drain(..) {
        queue.push("config", msg, now);
    }
}

fn load_bindings_toml(config_dir: &Path, errors: &mut Vec<String>) -> Vec<Binding> {
    let bindings_path = config_dir.join("bindings.toml");
    match bindings_config::load_bindings_from_path(&bindings_path) {
        Ok(loaded) => {
            for entry_err in &loaded.entry_errors {
                error!("bindings.toml: {entry_err}");
                errors.push(format!("bindings.toml: {entry_err}"));
            }
            loaded.bindings
        }
        Err(e) => {
            error!("{e}");
            errors.push(e.to_string());
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
