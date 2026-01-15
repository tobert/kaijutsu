//! Connection module for SSH + Cap'n Proto RPC
//!
//! Architecture:
//! - SSH handles auth and transport (russh)
//! - Cap'n Proto handles RPC over SSH channels (capnp-rpc)
//! - Bevy resources wrap the connection state

pub mod rpc;
pub mod ssh;

use bevy::prelude::*;

/// Connection state resource
#[derive(Resource, Default)]
pub struct ConnectionState {
    pub status: ConnectionStatus,
    pub server: Option<String>,
    pub username: Option<String>,
}

#[derive(Default, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

/// Event fired when connection status changes
#[derive(Message)]
pub struct ConnectionEvent {
    pub status: ConnectionStatus,
}

/// Plugin for connection management
pub struct ConnectionPlugin;

impl Plugin for ConnectionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ConnectionState>()
            .add_message::<ConnectionEvent>();
    }
}
