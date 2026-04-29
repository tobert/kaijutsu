//! Client-side peer transport.
//!
//! Receives invocations from the kernel via the `PeerCommands` callback and
//! dispatches them via Bevy systems. The app registers itself as a peer
//! (`nick = "kaijutsu-app"`) on connect; other agents can then call into the
//! app — most notably `switch_context` and `active_context` for drift
//! navigation.

mod plugin;
mod systems;

pub use plugin::{PeerInvocationChannel, PeersPlugin};
