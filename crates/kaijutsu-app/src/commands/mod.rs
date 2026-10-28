//! Commands module for Kaijutsu.
//!
//! Consumers for the Ctrl+A prefix context verbs (docs/input.md).
//! Note: `:` commands are handled server-side by kaish via Shell mode.

mod conversation;
pub mod ctrl_z;
pub mod placement;

use bevy::prelude::*;

/// Plugin for prefix context-verb handling.
pub struct CommandsPlugin;

impl Plugin for CommandsPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<placement::PlacementRequested>()
            .init_resource::<placement::PlacementLatch>()
            .add_systems(
                Update,
                (
                    conversation::handle_prefix_context_verbs,
                    placement::run_placements,
                    placement::land_placements,
                )
                    .chain()
                    .after(crate::input::InputPhase::Dispatch),
            )
        .init_resource::<ctrl_z::ShellToggleInFlight>()
        .add_systems(
            Update,
            (ctrl_z::handle_toggle_shell_context, ctrl_z::handle_shell_toggle_result)
                .chain()
                .after(crate::input::InputPhase::Dispatch),
        );
    }
}
