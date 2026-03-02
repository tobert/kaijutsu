//! App state management
//!
//! The app starts directly in the tiling conversation view.
//! Connection + context join happens in background (ActorPlugin bootstrap).

use bevy::prelude::*;

/// Marker for the main content area
#[derive(Component, Reflect, Default)]
#[reflect(Component)]
pub struct ContentArea;

/// Marker for the conversation view root
#[derive(Component, Reflect, Default)]
#[reflect(Component)]
pub struct ConversationRoot;

/// Plugin for app state management
pub struct AppScreenPlugin;

impl Plugin for AppScreenPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<ContentArea>()
            .register_type::<ConversationRoot>()
            .add_systems(Update, handle_context_joined);
    }
}

/// Handle ContextJoined events — log for diagnostics.
///
/// Document cache management happens in `handle_block_events` (view/sync.rs).
fn handle_context_joined(
    mut result_events: MessageReader<crate::connection::RpcResultMessage>,
) {
    for result in result_events.read() {
        if let crate::connection::RpcResultMessage::ContextJoined { membership, .. } = result {
            info!("Context joined: {}", membership.context_id);
        }
    }
}
