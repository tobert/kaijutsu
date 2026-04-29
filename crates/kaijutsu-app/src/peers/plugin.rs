//! Peer plugin for Bevy.

use bevy::prelude::*;
use std::sync::Mutex;

use super::systems;

/// Channel for async peer invocation handler → Bevy systems.
///
/// Uses `std::sync::mpsc` (not tokio) because the sender lives inside a
/// capnp server dispatch on a tokio LocalSet, while the receiver is polled
/// by a Bevy system. `std::sync::mpsc::Sender` works from any thread/executor.
#[derive(Resource)]
pub struct PeerInvocationChannel {
    pub tx: std::sync::mpsc::Sender<kaijutsu_client::PeerInvocation>,
    pub rx: Mutex<std::sync::mpsc::Receiver<kaijutsu_client::PeerInvocation>>,
}

impl PeerInvocationChannel {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            tx,
            rx: Mutex::new(rx),
        }
    }
}

/// Plugin that wires the peer invocation transport into Bevy.
pub struct PeersPlugin;

impl Plugin for PeersPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(PeerInvocationChannel::new());
        app.add_systems(Update, systems::poll_peer_invocations);
    }
}
