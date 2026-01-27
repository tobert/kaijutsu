//! Commands module for Kaijutsu.
//!
//! This module provides keyboard shortcuts for common operations.
//! Note: `:` commands are now handled server-side by kaish via Shell mode.

mod conversation;

use bevy::prelude::*;

/// Plugin for command/shortcut handling.
pub struct CommandsPlugin;

impl Plugin for CommandsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, conversation::handle_conversation_shortcuts);
    }
}
