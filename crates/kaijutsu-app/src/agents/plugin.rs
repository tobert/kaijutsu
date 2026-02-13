//! Agent plugin for Bevy.

use bevy::prelude::*;

use super::registry::{AgentActivityMessage, AgentRegistry};
use super::systems;

/// Plugin that enables agent attachment and collaboration features.
pub struct AgentsPlugin;

impl Plugin for AgentsPlugin {
    fn build(&self, app: &mut App) {
        // Register resources
        app.init_resource::<AgentRegistry>();

        // Register messages
        app.add_message::<AgentActivityMessage>();

        // Add systems
        app.add_systems(
            Update,
            (
                systems::handle_agent_activity,
                systems::sync_agent_badges,
            ),
        );
    }
}
