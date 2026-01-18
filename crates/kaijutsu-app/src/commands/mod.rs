//! Command system for Kaijutsu.
//!
//! Handles vim-style `:` commands including:
//! - `:conv list` - list all conversations
//! - `:conv new [name]` - create a new conversation
//! - `:conv switch <id>` - switch to a conversation
//! - `:q` - quit (future)
//! - `:w` - save (future)

mod conversation;

use bevy::prelude::*;

/// Plugin for command handling.
pub struct CommandsPlugin;

impl Plugin for CommandsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CommandBuffer>()
            .init_resource::<CommandOutput>()
            .add_systems(Update, (
                handle_command_input,
                conversation::handle_conversation_commands,
            ));
    }
}

/// Buffer for the current command being typed.
#[derive(Resource, Default)]
pub struct CommandBuffer {
    /// The command text (without leading ':').
    pub text: String,
    /// Whether command mode is active.
    pub active: bool,
}

impl CommandBuffer {
    /// Clear the command buffer.
    pub fn clear(&mut self) {
        self.text.clear();
        self.active = false;
    }

    /// Start command mode.
    pub fn start(&mut self) {
        self.text.clear();
        self.active = true;
    }

    /// Parse the command into parts.
    pub fn parts(&self) -> Vec<&str> {
        self.text.split_whitespace().collect()
    }

    /// Get the command name (first word).
    pub fn command(&self) -> Option<&str> {
        self.parts().first().copied()
    }

    /// Get command arguments (everything after the first word).
    pub fn args(&self) -> Vec<&str> {
        let parts = self.parts();
        if parts.len() > 1 {
            parts[1..].to_vec()
        } else {
            vec![]
        }
    }
}

/// Output from command execution (shown in UI).
#[derive(Resource, Default)]
pub struct CommandOutput {
    /// The output message.
    pub message: String,
    /// Whether it's an error.
    pub is_error: bool,
    /// When to hide the output (frames remaining).
    pub hide_after: u32,
}

impl CommandOutput {
    /// Set a success message.
    pub fn success(&mut self, msg: impl Into<String>) {
        self.message = msg.into();
        self.is_error = false;
        self.hide_after = 180; // ~3 seconds at 60fps
    }

    /// Set an error message.
    pub fn error(&mut self, msg: impl Into<String>) {
        self.message = msg.into();
        self.is_error = true;
        self.hide_after = 300; // ~5 seconds
    }

    /// Clear the output.
    pub fn clear(&mut self) {
        self.message.clear();
        self.hide_after = 0;
    }
}

/// Handle command input in command mode.
fn handle_command_input(
    mut key_events: MessageReader<bevy::input::keyboard::KeyboardInput>,
    mode: Res<crate::cell::CurrentMode>,
    mut command_buffer: ResMut<CommandBuffer>,
    mut command_output: ResMut<CommandOutput>,
) {
    use crate::cell::EditorMode;

    // Only handle input in command mode
    if mode.0 != EditorMode::Command {
        if command_buffer.active {
            command_buffer.clear();
        }
        return;
    }

    // Activate command buffer when entering command mode
    if !command_buffer.active {
        command_buffer.start();
    }

    // Handle key input
    for event in key_events.read() {
        if !event.state.is_pressed() {
            continue;
        }

        match event.key_code {
            // Escape handled by mode switching
            KeyCode::Escape => {
                command_buffer.clear();
            }
            // Enter executes the command
            KeyCode::Enter => {
                // Command will be handled by specific command handlers
                // Don't clear here - let the handlers process it
            }
            // Backspace deletes last character
            KeyCode::Backspace => {
                command_buffer.text.pop();
            }
            // Other keys: add to buffer if they produce text
            _ => {
                if let Some(text) = &event.text {
                    // Skip special characters
                    if text.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
                        command_buffer.text.push_str(text);
                    }
                }
            }
        }
    }

    // Tick down output visibility
    if command_output.hide_after > 0 {
        command_output.hide_after -= 1;
        if command_output.hide_after == 0 {
            command_output.clear();
        }
    }
}
