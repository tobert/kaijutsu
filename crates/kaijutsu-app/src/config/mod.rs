//! TOML config loader for app-side configuration.
//!
//! Loads **bindings** from the host `bindings.toml` (app-only config). The
//! **theme** is no longer a host file: it is CRDT-owned by the kernel (slice 2,
//! `docs/config-crdt-ownership.md`) and fetched over RPC on connect
//! (`apply_theme_from_rpc`), so the app starts on `Theme::default()` and the
//! real theme arrives once connected. Bindings stay host-side because they are
//! purely client-local and the kernel has no notion of them.

use bevy::prelude::*;
use std::path::Path;

use crate::input::binding::Binding;
use crate::input::bindings_config;
use crate::input::defaults::default_bindings;
use crate::ui::theme::Theme;
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

/// Load bindings from user config.
///
/// Reads `bindings.toml` from `~/.config/kaijutsu/` (merged over the
/// built-in defaults). The theme never touches host disk: it starts on
/// `Theme::default()` and the CRDT-owned `theme.toml` arrives over RPC on
/// connect. File-level and per-entry errors are accumulated in
/// `AppConfig::errors`; good values still load and the app continues to boot.
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
    // Theme is CRDT-owned and fetched over RPC on connect; start on the default.
    let theme = Theme::default();
    let bindings = load_bindings_toml(&config_dir, &mut errors);

    AppConfig {
        theme,
        bindings,
        errors,
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

/// Write default app-only config files to the user config dir if they don't
/// exist. Only `bindings.toml` lives on host disk now — the theme is CRDT-owned
/// by the kernel (slice 2) and seeded there, so the app no longer writes a host
/// `theme.toml`.
pub fn write_default_configs_if_missing() {
    let Some(config_dir) = dirs::config_dir().map(|p| p.join("kaijutsu")) else {
        return;
    };

    if let Err(e) = std::fs::create_dir_all(&config_dir) {
        warn!("Could not create config dir {:?}: {}", config_dir, e);
        return;
    }

    // Write an overrides TEMPLATE, not a dump of the default table. The file
    // MERGES over the built-in defaults at load (`load_bindings_from_path`),
    // so it should contain only what the user actually changed — a full dump
    // freezes the install at whatever the defaults were the day it was
    // written (the 2026-04 snapshot trap, found 2026-07-16).
    let bindings_path = config_dir.join("bindings.toml");
    if !bindings_path.exists() {
        let content = "\
# kaijutsu key binding overrides.
#
# Entries here MERGE over the built-in default table: an entry with the same
# key + modifiers + context replaces that default; new combinations are added.
# Defaults you don't mention stay live, and evolve with the app.
# The full current table is BRP-visible (`InputMap`) and documented in
# docs/input.md.
#
# [[bindings]]
# key = \"KeyJ\"
# modifiers = \"CTRL\"        # empty, or +-joined: CTRL, SHIFT, ALT, SUPER
# context = \"Navigation\"    # Global, Navigation, TextInput, Dialog,
#                            # RoomNav, WellZoomed, PatchBayZoomed,
#                            # StationZoomed, FsnFly
# action = \"FocusNextBlock\"
# label = \"Next block\"
";
        match std::fs::write(&bindings_path, content) {
            Ok(()) => info!("Wrote bindings override template to {:?}", bindings_path),
            Err(e) => warn!("Could not write bindings to {:?}: {}", bindings_path, e),
        }
    }
}
