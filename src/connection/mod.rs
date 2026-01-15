//! Connection module for SSH + Cap'n Proto RPC
//!
//! Architecture:
//! - SSH handles auth and transport (russh)
//! - Cap'n Proto handles RPC over SSH channels (capnp-rpc)
//! - Bridge module connects async RPC to Bevy ECS via channels

pub mod bridge;
pub mod rpc;
pub mod ssh;

pub use bridge::{
    ConnectionBridgePlugin, ConnectionCommand, ConnectionCommandSender, ConnectionEvent,
    ConnectionEventReceiver,
};

use bevy::prelude::*;

/// Connection state resource (updated by systems responding to ConnectionEvent)
#[derive(Resource, Default)]
pub struct ConnectionState {
    pub status: ConnectionStatus,
    pub server: Option<String>,
    pub username: Option<String>,
}

#[derive(Default, Clone, PartialEq, Eq, Debug)]
pub enum ConnectionStatus {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}
