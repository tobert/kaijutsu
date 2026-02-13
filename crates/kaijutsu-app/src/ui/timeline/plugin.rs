//! Timeline plugin - registers resources and systems for temporal navigation.
//!
//! The visual scrubber UI has been removed. Timeline navigation is now
//! keyboard-only via `[`, `]`, `\`, and `f` keys.

use bevy::prelude::*;

use super::components::*;
use super::systems;
use crate::ui::state::AppScreen;

/// Plugin that enables timeline navigation.
pub struct TimelinePlugin;

impl Plugin for TimelinePlugin {
    fn build(&self, app: &mut App) {
        // Register types for BRP reflection
        app.register_type::<TimelineState>()
            .register_type::<TimelineViewMode>()
            .register_type::<TimelineVisibility>();

        // Initialize resources
        app.init_resource::<TimelineState>();

        // Register messages
        app.add_message::<ForkRequest>()
            .add_message::<ForkResult>()
            .add_message::<CherryPickRequest>()
            .add_message::<CherryPickResult>();

        // Core systems (only run in Conversation state)
        // Note: Timeline keyboard navigation moved to input::systems::handle_timeline
        app.add_systems(
            Update,
            (
                // Version sync first
                systems::sync_timeline_version,
                // Block visibility updates
                systems::update_block_visibility,
                // Request processing
                systems::process_fork_requests,
                systems::process_cherry_pick_requests,
                // Completion handlers
                systems::handle_fork_complete,
                systems::handle_cherry_pick_complete,
            )
                .run_if(in_state(AppScreen::Conversation)),
        );
    }
}
