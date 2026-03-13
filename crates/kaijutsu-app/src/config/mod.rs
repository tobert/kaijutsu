//! Unified Rhai config engine for app-side configuration.
//!
//! Provides a shared `Engine` with all helpers (color + binding functions)
//! and a `FileModuleResolver` so scripts can `import "palette"` or other
//! shared modules from `~/.config/kaijutsu/`.
//!
//! ## Key types
//!
//! - [`AppConfig`] — holds both `Theme` and `Vec<Binding>` loaded at startup
//! - [`build_app_engine`] — one engine, all helpers, module resolver set
//! - [`load_app_config`] — evaluates theme.rhai then bindings.rhai in one scope
//! - [`write_default_configs_if_missing`] — writes both default files on first run

use bevy::prelude::*;
use rhai::module_resolvers::FileModuleResolver;
use rhai::{Dynamic, Engine, Scope};
use std::path::Path;

use crate::input::binding::Binding;
use crate::input::defaults::default_bindings;
use crate::input::rhai_config::{parse_bindings_from_dynamic, register_binding_fns};
use crate::ui::theme::Theme;

/// Combined output of app-side Rhai config evaluation.
pub struct AppConfig {
    pub theme: Theme,
    #[allow(dead_code)] // Phase N: pass pre-loaded bindings to InputMap instead of double-loading
    pub bindings: Vec<Binding>,
}

/// Build a Rhai engine with all app-side helpers registered.
///
/// Registers color functions (`hex`, `hexa`, `rgba`, `rgb`) and binding
/// functions (`binding`, `binding_mod`, `gamepad`, `default_bindings`).
/// Sets a `FileModuleResolver` pointing at `config_dir` so scripts can
/// `import "palette"` or other shared modules from the same directory.
pub fn build_app_engine(config_dir: &Path) -> Engine {
    let mut engine = Engine::new();
    // Register shared stdlib (math, color, format) — replaces old register_color_fns.
    // hex() now returns String instead of Array. ThemeData parsing handles both.
    kaijutsu_rhai::register_stdlib(&mut engine);
    register_binding_fns(&mut engine, default_bindings());
    engine.set_module_resolver(FileModuleResolver::new_with_path(config_dir));
    engine
}

/// Load theme and bindings from user config, evaluating both scripts in one scope.
///
/// theme.rhai runs first, populating the scope with color variables.
/// bindings.rhai runs second in the same scope, so it can reference those vars.
/// Falls back to defaults for each file independently if loading fails.
pub fn load_app_config() -> AppConfig {
    let Some(config_dir) = dirs::config_dir().map(|p| p.join("kaijutsu")) else {
        info!("No config directory available, using defaults for theme and bindings");
        return AppConfig { theme: Theme::default(), bindings: default_bindings() };
    };

    let engine = build_app_engine(&config_dir);
    let mut scope = Scope::new();

    // --- Theme (runs first, populates scope with color vars) ---
    let theme = load_theme_into_scope(&engine, &mut scope, &config_dir);

    // --- Bindings (runs second; scope has theme vars available) ---
    let bindings = load_bindings_from_scope(&engine, &mut scope, &config_dir);

    AppConfig { theme, bindings }
}

fn load_theme_into_scope(engine: &Engine, scope: &mut Scope, config_dir: &Path) -> Theme {
    let theme_path = config_dir.join("theme.rhai");
    if !theme_path.exists() {
        return Theme::default();
    }

    let script = match std::fs::read_to_string(&theme_path) {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to read theme {:?}: {}", theme_path, e);
            return Theme::default();
        }
    };

    match engine.run_with_scope(scope, &script) {
        Ok(()) => {
            info!("Loaded theme from {:?}", theme_path);
            let theme_data = kaijutsu_rhai::theme::parse_theme_data_from_scope(scope);
            Theme::from(theme_data)
        }
        Err(e) => {
            warn!("Failed to parse theme: {}", e);
            Theme::default()
        }
    }
}

fn load_bindings_from_scope(engine: &Engine, scope: &mut Scope, config_dir: &Path) -> Vec<Binding> {
    let bindings_path = config_dir.join("bindings.rhai");
    if !bindings_path.exists() {
        return default_bindings();
    }

    let script = match std::fs::read_to_string(&bindings_path) {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to read bindings {:?}: {}", bindings_path, e);
            return default_bindings();
        }
    };

    match engine.eval_with_scope::<Dynamic>(scope, &script) {
        Ok(result) => match parse_bindings_from_dynamic(result) {
            Ok(bindings) => {
                info!("Loaded bindings from {:?}", bindings_path);
                bindings
            }
            Err(e) => {
                warn!("Failed to parse bindings: {}", e);
                default_bindings()
            }
        },
        Err(e) => {
            warn!("Failed to eval bindings: {}", e);
            default_bindings()
        }
    }
}

/// Write default config files to the user config dir if they don't exist.
///
/// Writes `bindings.rhai` and `theme.rhai` from embedded defaults on first run.
/// These files are managed by Kaijutsu — do not edit them directly.
pub fn write_default_configs_if_missing() {
    let Some(config_dir) = dirs::config_dir().map(|p| p.join("kaijutsu")) else {
        return;
    };

    if let Err(e) = std::fs::create_dir_all(&config_dir) {
        warn!("Could not create config dir {:?}: {}", config_dir, e);
        return;
    }

    let bindings_path = config_dir.join("bindings.rhai");
    if !bindings_path.exists() {
        let content = include_str!("../../assets/defaults/bindings.rhai");
        match std::fs::write(&bindings_path, content) {
            Ok(()) => info!("Wrote default bindings to {:?}", bindings_path),
            Err(e) => warn!("Could not write bindings to {:?}: {}", bindings_path, e),
        }
    }

    let theme_path = config_dir.join("theme.rhai");
    if !theme_path.exists() {
        let content = include_str!("../../assets/defaults/theme.rhai");
        match std::fs::write(&theme_path, content) {
            Ok(()) => info!("Wrote default theme to {:?}", theme_path),
            Err(e) => warn!("Could not write theme to {:?}: {}", theme_path, e),
        }
    }
}
