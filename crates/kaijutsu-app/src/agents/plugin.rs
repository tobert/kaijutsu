//! Agent plugin for Bevy.

use bevy::prelude::*;
use std::sync::Mutex;
use tokio::sync::mpsc;

use super::registry::{AgentActivityMessage, AgentRegistry};
use super::systems;

/// Channel for async agent invocation handler → Bevy systems.
///
/// Same pattern as `RpcResultChannel`: unbounded mpsc with `Mutex<Receiver>`
/// polled each frame. Invocations arrive from the kernel via the
/// `AgentCommands` callback and are dispatched by `poll_agent_invocations`.
#[derive(Resource)]
pub struct AgentInvocationChannel {
    pub tx: mpsc::UnboundedSender<kaijutsu_client::AgentInvocation>,
    pub rx: Mutex<mpsc::UnboundedReceiver<kaijutsu_client::AgentInvocation>>,
}

impl AgentInvocationChannel {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
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
