//! Agent plugin for Bevy.

use bevy::prelude::*;
use std::sync::Mutex;

use super::registry::{AgentActivityMessage, AgentRegistry};
use super::systems;

/// Channel for async agent invocation handler → Bevy systems.
///
/// Uses `std::sync::mpsc` (not tokio) because the sender lives inside a
/// capnp server dispatch on a tokio LocalSet, while the receiver is polled
/// by a Bevy system. `std::sync::mpsc::Sender` works from any thread/executor.
#[derive(Resource)]
pub struct AgentInvocationChannel {
    pub tx: std::sync::mpsc::Sender<kaijutsu_client::AgentInvocation>,
    pub rx: Mutex<std::sync::mpsc::Receiver<kaijutsu_client::AgentInvocation>>,
}

impl AgentInvocationChannel {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            tx,
            rx: Mutex::new(rx),
        }
    }
}

/// Plugin that enables agent attachment and collaboration features.
pub struct AgentsPlugin;

impl Plugin for AgentsPlugin {
    fn build(&self, app: &mut App) {
        // Register resources
        app.init_resource::<AgentRegistry>();
        app.insert_resource(AgentInvocationChannel::new());

        // Register messages
        app.add_message::<AgentActivityMessage>();

        // Add systems
        app.add_systems(
            Update,
            (
                systems::handle_agent_activity,
                systems::poll_agent_invocations,
            ),
        );
    }
}
