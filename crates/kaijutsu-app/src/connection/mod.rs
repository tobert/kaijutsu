//! Connection module — Bevy integration for kaijutsu-client via ActorHandle.
//!
//! The bootstrap thread owns a tokio runtime + LocalSet for Cap'n Proto's !Send
//! types. ActorPlugin polls broadcast channels each frame and provides resources
//! and messages for consumer systems.

pub mod actor_plugin;
pub mod bootstrap;

pub use actor_plugin::{
    ActorPlugin, RpcActor, RpcConnectionState, RpcResultChannel, RpcResultMessage,
    ServerEventMessage,
};
pub use bootstrap::{BootstrapChannel, BootstrapCommand};
