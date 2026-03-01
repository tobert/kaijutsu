//! App state management
//!
//! The app starts directly in the tiling conversation view.
//! Connection + context join happens in background (ActorPlugin bootstrap).

use bevy::prelude::*;

/// Marker for the main content area
#[derive(Component)]
pub struct ContentArea;

/// Marker for the conversation view root
#[derive(Component)]
pub struct ConversationRoot;

/// Plugin for app state management
pub struct AppScreenPlugin;

impl Plugin for AppScreenPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, handle_context_joined);
    }
}

/// Handle ContextJoined events to create conversation metadata.
fn handle_context_joined(
    mut result_events: MessageReader<crate::connection::RpcResultMessage>,
    mut context_order: ResMut<crate::conversation::ContextOrder>,
    mut active_ctx: ResMut<crate::conversation::ActiveContext>,
) {
    for result in result_events.read() {
        if let crate::connection::RpcResultMessage::ContextJoined { membership, .. } = result {
            let ctx_id = membership.context_id;
            context_order.add(ctx_id);
            active_ctx.0 = Some(ctx_id);
            info!("Context joined: {}", ctx_id);
        }
    }
}
