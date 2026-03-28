//! Convert Bevy keyboard events to modalkit's TerminalKey.
//!
//! Maps Bevy's layout-aware `logical_key` to crossterm's `KeyCode`,
//! and Bevy modifier state to crossterm's `KeyModifiers`.

use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;

use modalkit::crossterm::event::{KeyCode as CtKeyCode, KeyEvent, KeyModifiers};
use modalkit::key::TerminalKey;

/// Convert a Bevy KeyboardInput event to a modalkit TerminalKey.
///
/// Returns `None` for keys we can't map (media keys, unidentified, etc.).
pub fn bevy_to_terminal_key(
    event: &KeyboardInput,
    keys: &ButtonInput<KeyCode>,
) -> Option<TerminalKey> {
    let code = logical_key_to_crossterm(&event.logical_key)?;
    let modifiers = bevy_modifiers(keys);

    // TerminalKey::new() is pub(crate), so construct via From<KeyEvent>
    Some(TerminalKey::from(KeyEvent::new(code, modifiers)))
}

/// Map Bevy's logical key to crossterm KeyCode.
fn logical_key_to_crossterm(key: &Key) -> Option<CtKeyCode> {
    match key {
        // Named keys
        Key::Enter => Some(CtKeyCode::Enter),
        Key::Tab => Some(CtKeyCode::Tab),
        Key::Backspace => Some(CtKeyCode::Backspace),
        Key::Escape => Some(CtKeyCode::Esc),
        Key::Delete => Some(CtKeyCode::Delete),
        Key::Home => Some(CtKeyCode::Home),
        Key::End => Some(CtKeyCode::End),
        Key::PageUp => Some(CtKeyCode::PageUp),
        Key::PageDown => Some(CtKeyCode::PageDown),
        Key::ArrowLeft => Some(CtKeyCode::Left),
        Key::ArrowRight => Some(CtKeyCode::Right),
        Key::ArrowUp => Some(CtKeyCode::Up),
        Key::ArrowDown => Some(CtKeyCode::Down),
        Key::Insert => Some(CtKeyCode::Insert),
        Key::Space => Some(CtKeyCode::Char(' ')),

        // Function keys
        Key::F1 => Some(CtKeyCode::F(1)),
        Key::F2 => Some(CtKeyCode::F(2)),
        Key::F3 => Some(CtKeyCode::F(3)),
        Key::F4 => Some(CtKeyCode::F(4)),
        Key::F5 => Some(CtKeyCode::F(5)),
        Key::F6 => Some(CtKeyCode::F(6)),
        Key::F7 => Some(CtKeyCode::F(7)),
        Key::F8 => Some(CtKeyCode::F(8)),
        Key::F9 => Some(CtKeyCode::F(9)),
        Key::F10 => Some(CtKeyCode::F(10)),
        Key::F11 => Some(CtKeyCode::F(11)),
        Key::F12 => Some(CtKeyCode::F(12)),

        // Character keys — layout-aware via logical_key
        Key::Character(s) => {
            let mut chars = s.chars();
            let c = chars.next()?;
            // Single character only
            if chars.next().is_some() {
                return None;
            }
            Some(CtKeyCode::Char(c))
        }

        // Keys we don't map
        _ => None,
    }
}

/// Extract crossterm-compatible modifier flags from Bevy's key state.
fn bevy_modifiers(keys: &ButtonInput<KeyCode>) -> KeyModifiers {
    let mut mods = KeyModifiers::empty();

    if keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight) {
        mods |= KeyModifiers::CONTROL;
    }
    if keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight) {
        mods |= KeyModifiers::SHIFT;
    }
    if keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight) {
        mods |= KeyModifiers::ALT;
    }
    // Note: crossterm has SUPER/HYPER/META but we don't map SuperLeft/SuperRight
    // since those are typically consumed by the window manager.

    mods
}
