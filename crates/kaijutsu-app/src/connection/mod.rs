//! Connection module - Bevy integration for kaijutsu-client
//!
//! Uses kaijutsu-client for SSH + RPC, provides Bevy bridge via channels.

pub mod bridge;

pub use bridge::{
    ConnectionBridgePlugin, ConnectionCommand, ConnectionCommands, ConnectionEvent,
};

