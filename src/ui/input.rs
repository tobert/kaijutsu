use bevy::{
    input::keyboard::{Key, KeyboardInput},
    prelude::*,
    window::Ime,
};

use crate::state::{input::InputBuffer, mode::Mode};

use super::context::MessageEvent;

/// Marker for the input bar text display
#[derive(Component)]
pub struct InputDisplay;

/// Handle keyboard input in Insert mode
pub fn handle_keyboard_input(
    mut keyboard_events: MessageReader<KeyboardInput>,
    mut buffer: ResMut<InputBuffer>,
    mut message_events: MessageWriter<MessageEvent>,
    mode: Res<State<Mode>>,
) {
    // Only capture input in Insert mode
    if *mode.get() != Mode::Insert {
        return;
    }

    for event in keyboard_events.read() {
        // Only handle key press events
        if !event.state.is_pressed() {
            continue;
        }

        match (&event.logical_key, &event.text) {
            // Enter sends the message
            (Key::Enter, _) => {
                let text = buffer.clear();
                if !text.is_empty() {
                    message_events.write(MessageEvent {
                        sender: "amy".to_string(),
                        content: text,
                    });
                }
            }
            // Backspace removes last character
            (Key::Backspace, _) => {
                buffer.pop();
            }
            // Regular text input
            (_, Some(text)) => {
                if text.chars().all(is_printable) {
                    buffer.push(text);
                }
            }
            _ => {}
        }
    }
}

/// Handle IME (Input Method Editor) events for CJK input
pub fn handle_ime_input(
    mut ime_events: MessageReader<Ime>,
    mut buffer: ResMut<InputBuffer>,
    mode: Res<State<Mode>>,
) {
    // Only capture input in Insert mode
    if *mode.get() != Mode::Insert {
        return;
    }

    for event in ime_events.read() {
        match event {
            // Preedit text (composing, not yet committed)
            Ime::Preedit { value, cursor, .. } => {
                if cursor.is_some() {
                    buffer.set_preedit(value);
                } else {
                    buffer.clear_preedit();
                }
            }
            // Committed text from IME
            Ime::Commit { value, .. } => {
                buffer.clear_preedit();
                buffer.push(value);
            }
            _ => {}
        }
    }
}

/// Update the input bar display
pub fn update_input_display(
    buffer: Res<InputBuffer>,
    mut query: Query<&mut Text, With<InputDisplay>>,
) {
    if buffer.is_changed() {
        for mut text in &mut query {
            **text = buffer.display();
        }
    }
}

/// Check if a character is printable (not a control character)
fn is_printable(c: char) -> bool {
    let is_private_use = ('\u{e000}'..='\u{f8ff}').contains(&c)
        || ('\u{f0000}'..='\u{ffffd}').contains(&c)
        || ('\u{100000}'..='\u{10fffd}').contains(&c);

    !is_private_use && !c.is_ascii_control()
}
