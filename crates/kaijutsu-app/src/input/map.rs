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
    /// Timeout for multi-key sequences (g→t, g→T, g→g) in milliseconds.
    pub sequence_timeout_ms: u64,
}

impl Default for InputMap {
    fn default() -> Self {
        Self {
            bindings: super::defaults::default_bindings(),
            sequence_timeout_ms: 500,
        }
    }
}
