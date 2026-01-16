//! Connection module - Bevy integration for kaijutsu-client
//!
//! Uses kaijutsu-client for SSH + RPC, provides Bevy bridge via channels.

pub mod bridge;

pub use bridge::{
    ConnectionBridgePlugin, ConnectionCommand, ConnectionCommands, ConnectionEvent,
    ConnectionEvents, ConnectionState,
};

// Re-export client types for convenience
pub use kaijutsu_client::{Identity, RoomConfig, RoomInfo, Row, RowType};
