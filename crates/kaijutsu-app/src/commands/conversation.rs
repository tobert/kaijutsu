//! Conversation shortcuts.
//!
//! Quick keyboard shortcuts for conversation management:
//! - Ctrl+1/2/3 - Quick switch to conversation 1/2/3
//!
//! Note: `:conv` commands are now handled server-side by kaish via Shell mode.

use bevy::prelude::*;

use crate::input::FocusArea;
use crate::cell::{ContextSwitchRequested, DocumentCache};

/// Handle conversation quick-switch shortcuts (Ctrl+1/2/3).
pub fn handle_conversation_shortcuts(
    keys: Res<ButtonInput<KeyCode>>,
    focus_area: Res<FocusArea>,
    doc_cache: Res<DocumentCache>,
    mut switch_writer: MessageWriter<ContextSwitchRequested>,
) {
    // Only when navigating (not typing text)
    if focus_area.is_text_input() {
        return;
    }

    let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    if !ctrl {
        return;
    }

    let ids = doc_cache.mru_ids();

    let target = if keys.just_pressed(KeyCode::Digit1) && !ids.is_empty() {
        Some(ids[0])
    } else if keys.just_pressed(KeyCode::Digit2) && ids.len() > 1 {
        Some(ids[1])
    } else if keys.just_pressed(KeyCode::Digit3) && ids.len() > 2 {
        Some(ids[2])
    } else {
        None
    };

    if let Some(ctx_id) = target {
        switch_writer.write(ContextSwitchRequested { context_id: ctx_id });
        info!("Switched to context {}", ctx_id.short());
    }
}
