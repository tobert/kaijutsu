//! InputMap resource — the binding table that Claude can inspect and mutate via BRP.

use bevy::prelude::*;

use super::binding::Binding;

/// The complete input binding configuration.
///
/// BRP-queryable: `world_get_resources("kaijutsu_app::input::map::InputMap")`
/// BRP-mutable: `world_mutate_resources("InputMap", ".bindings[42].action", ...)`
///
/// Claude can read all bindings, change individual ones, or replace the whole table.
#[derive(Resource, Reflect)]
#[reflect(Resource)]
pub struct InputMap {
    /// All active bindings, checked in order (first match wins per context priority).
    pub bindings: Vec<Binding>,
}

impl Default for InputMap {
    fn default() -> Self {
        // Default impl can't return Result; errors are already loudly logged
        // and surfaced via StartupConfigErrors in crate::config::load_app_config.
        // This path is hit only when InputMap is init'd via `init_resource`
        // before load_app_config has run — we prefer default bindings over
        // a crash so the app can still boot.
        let bindings = match super::bindings_config::load_bindings() {
            Ok(loaded) => {
                for err in &loaded.entry_errors {
                    warn!("bindings.toml: {err}");
                }
                loaded.bindings
            }
            Err(e) => {
                error!("{e}; falling back to default bindings");
                super::defaults::default_bindings()
            }
        };
        Self { bindings }
    }
}
