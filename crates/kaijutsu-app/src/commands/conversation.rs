//! Conversation shortcuts.
//!
//! Quick keyboard shortcuts for conversation management:
//! - Ctrl+1/2/3 - Quick switch to conversation 1/2/3
//!
//! Note: `:conv` commands are now handled server-side by kaish via Shell mode.

use bevy::prelude::*;

use crate::cell::EditorMode;
use crate::conversation::{ConversationRegistry, CurrentConversation};

/// Handle conversation quick-switch shortcuts (Ctrl+1/2/3).
pub fn handle_conversation_shortcuts(
    keys: Res<ButtonInput<KeyCode>>,
    mode: Res<crate::cell::CurrentMode>,
    mut registry: ResMut<ConversationRegistry>,
    mut current: ResMut<CurrentConversation>,
) {
    // Only in Normal mode
    if mode.0 != EditorMode::Normal {
        return;
    }

    let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    if !ctrl {
        return;
    }

    let ids = registry.ids().to_vec();

    if keys.just_pressed(KeyCode::Digit1) && !ids.is_empty() {
        current.0 = Some(ids[0].clone());
        registry.move_to_front(&ids[0]);
        info!("Switched to conversation 1");
    } else if keys.just_pressed(KeyCode::Digit2) && ids.len() > 1 {
        current.0 = Some(ids[1].clone());
        registry.move_to_front(&ids[1]);
        info!("Switched to conversation 2");
    } else if keys.just_pressed(KeyCode::Digit3) && ids.len() > 2 {
        current.0 = Some(ids[2].clone());
        registry.move_to_front(&ids[2]);
        info!("Switched to conversation 3");
    }
}
