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
/// Prefers the layout-aware `logical_key` (so AZERTY users get the right
/// chars for `KeyA`) and falls back to `key_code` for synthetic events
/// where `logical_key` is `Unidentified` — bevy_brp's send_keys ships
/// non-character keys this way, and it's a reasonable fallback for any
/// embedded UI sending KeyboardInput without setting both fields.
///
/// Returns `None` for keys we can't map (media keys, dead keys with no
/// resolution, etc.).
pub fn bevy_to_terminal_key(
    event: &KeyboardInput,
    keys: &ButtonInput<KeyCode>,
) -> Option<TerminalKey> {
    let code = logical_key_to_crossterm(&event.logical_key)
        .or_else(|| key_code_to_crossterm(event.key_code))?;
    let modifiers = bevy_modifiers(keys);

    // TerminalKey::new() is pub(crate), so construct via From<KeyEvent>
    Some(TerminalKey::from(KeyEvent::new(code, modifiers)))
}

/// Fallback: map Bevy `KeyCode` (physical key) directly to a crossterm
/// keycode. Used when `logical_key` is `Unidentified` — typically
/// synthetic input from bevy_brp_extras send_keys for non-character keys.
///
/// Only covers keys where the physical→logical mapping is unambiguous
/// across layouts (named keys, function keys). Letter/digit keys still
/// require the logical_key path so that QWERTY vs AZERTY produces the
/// expected char.
fn key_code_to_crossterm(code: KeyCode) -> Option<CtKeyCode> {
    Some(match code {
        KeyCode::Enter => CtKeyCode::Enter,
        KeyCode::Tab => CtKeyCode::Tab,
        KeyCode::Backspace => CtKeyCode::Backspace,
        KeyCode::Escape => CtKeyCode::Esc,
        KeyCode::Delete => CtKeyCode::Delete,
        KeyCode::Home => CtKeyCode::Home,
        KeyCode::End => CtKeyCode::End,
        KeyCode::PageUp => CtKeyCode::PageUp,
        KeyCode::PageDown => CtKeyCode::PageDown,
        KeyCode::ArrowLeft => CtKeyCode::Left,
        KeyCode::ArrowRight => CtKeyCode::Right,
        KeyCode::ArrowUp => CtKeyCode::Up,
        KeyCode::ArrowDown => CtKeyCode::Down,
        KeyCode::Insert => CtKeyCode::Insert,
        KeyCode::Space => CtKeyCode::Char(' '),
        KeyCode::F1 => CtKeyCode::F(1),
        KeyCode::F2 => CtKeyCode::F(2),
        KeyCode::F3 => CtKeyCode::F(3),
        KeyCode::F4 => CtKeyCode::F(4),
        KeyCode::F5 => CtKeyCode::F(5),
        KeyCode::F6 => CtKeyCode::F(6),
        KeyCode::F7 => CtKeyCode::F(7),
        KeyCode::F8 => CtKeyCode::F(8),
        KeyCode::F9 => CtKeyCode::F(9),
        KeyCode::F10 => CtKeyCode::F(10),
        KeyCode::F11 => CtKeyCode::F(11),
        KeyCode::F12 => CtKeyCode::F(12),
        _ => return None,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::input::ButtonState;
    use bevy::ecs::entity::Entity;

    fn make_event(key_code: KeyCode, logical_key: Key) -> KeyboardInput {
        KeyboardInput {
            key_code,
            logical_key,
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window: Entity::PLACEHOLDER,
        }
    }

    fn no_modifiers() -> ButtonInput<KeyCode> {
        ButtonInput::default()
    }

    fn with_modifier(modifier: KeyCode) -> ButtonInput<KeyCode> {
        let mut keys = ButtonInput::default();
        keys.press(modifier);
        keys
    }

    // ── Character keys ──

    #[test]
    fn char_a() {
        let event = make_event(KeyCode::KeyA, Key::Character("a".into()));
        let result = bevy_to_terminal_key(&event, &no_modifiers()).unwrap();
        let expected = TerminalKey::from(KeyEvent::new(CtKeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(result, expected);
    }

    #[test]
    fn char_space() {
        let event = make_event(KeyCode::Space, Key::Space);
        let result = bevy_to_terminal_key(&event, &no_modifiers()).unwrap();
        let expected = TerminalKey::from(KeyEvent::new(CtKeyCode::Char(' '), KeyModifiers::NONE));
        assert_eq!(result, expected);
    }

    // ── Named keys ──

    #[test]
    fn named_enter() {
        let event = make_event(KeyCode::Enter, Key::Enter);
        let result = bevy_to_terminal_key(&event, &no_modifiers()).unwrap();
        let expected = TerminalKey::from(KeyEvent::new(CtKeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(result, expected);
    }

    #[test]
    fn named_escape() {
        let event = make_event(KeyCode::Escape, Key::Escape);
        let result = bevy_to_terminal_key(&event, &no_modifiers()).unwrap();
        let expected = TerminalKey::from(KeyEvent::new(CtKeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(result, expected);
    }

    #[test]
    fn named_backspace() {
        let event = make_event(KeyCode::Backspace, Key::Backspace);
        let result = bevy_to_terminal_key(&event, &no_modifiers()).unwrap();
        let expected = TerminalKey::from(KeyEvent::new(CtKeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(result, expected);
    }

    #[test]
    fn named_tab() {
        let event = make_event(KeyCode::Tab, Key::Tab);
        let result = bevy_to_terminal_key(&event, &no_modifiers()).unwrap();
        let expected = TerminalKey::from(KeyEvent::new(CtKeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(result, expected);
    }

    #[test]
    fn named_arrows() {
        for (kc, key, ct) in [
            (KeyCode::ArrowLeft, Key::ArrowLeft, CtKeyCode::Left),
            (KeyCode::ArrowRight, Key::ArrowRight, CtKeyCode::Right),
            (KeyCode::ArrowUp, Key::ArrowUp, CtKeyCode::Up),
            (KeyCode::ArrowDown, Key::ArrowDown, CtKeyCode::Down),
        ] {
            let event = make_event(kc, key);
            let result = bevy_to_terminal_key(&event, &no_modifiers()).unwrap();
            let expected = TerminalKey::from(KeyEvent::new(ct, KeyModifiers::NONE));
            assert_eq!(result, expected);
        }
    }

    // ── Function keys ──

    #[test]
    fn function_keys() {
        let event = make_event(KeyCode::F1, Key::F1);
        let result = bevy_to_terminal_key(&event, &no_modifiers()).unwrap();
        let expected = TerminalKey::from(KeyEvent::new(CtKeyCode::F(1), KeyModifiers::NONE));
        assert_eq!(result, expected);

        let event = make_event(KeyCode::F12, Key::F12);
        let result = bevy_to_terminal_key(&event, &no_modifiers()).unwrap();
        let expected = TerminalKey::from(KeyEvent::new(CtKeyCode::F(12), KeyModifiers::NONE));
        assert_eq!(result, expected);
    }

    // ── Modifiers ──

    #[test]
    fn ctrl_c() {
        let event = make_event(KeyCode::KeyC, Key::Character("c".into()));
        let keys = with_modifier(KeyCode::ControlLeft);
        let result = bevy_to_terminal_key(&event, &keys).unwrap();
        let expected = TerminalKey::from(KeyEvent::new(CtKeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(result, expected);
    }

    #[test]
    fn shift_a_uppercase() {
        // Bevy sends logical_key = Character("A") when Shift+A is pressed
        let event = make_event(KeyCode::KeyA, Key::Character("A".into()));
        let keys = with_modifier(KeyCode::ShiftLeft);
        let result = bevy_to_terminal_key(&event, &keys).unwrap();
        // Result: Char('A') + SHIFT — modalkit should handle this
        let expected = TerminalKey::from(KeyEvent::new(CtKeyCode::Char('A'), KeyModifiers::SHIFT));
        assert_eq!(result, expected);
    }

    #[test]
    fn alt_x() {
        let event = make_event(KeyCode::KeyX, Key::Character("x".into()));
        let keys = with_modifier(KeyCode::AltLeft);
        let result = bevy_to_terminal_key(&event, &keys).unwrap();
        let expected = TerminalKey::from(KeyEvent::new(CtKeyCode::Char('x'), KeyModifiers::ALT));
        assert_eq!(result, expected);
    }

    // ── Rejection cases ──

    #[test]
    fn multi_char_rejected() {
        // Dead key compose sequences produce multi-char strings
        let event = make_event(KeyCode::KeyA, Key::Character("fi".into()));
        let result = bevy_to_terminal_key(&event, &no_modifiers());
        assert!(result.is_none());
    }

    #[test]
    fn dead_key_rejected() {
        let event = make_event(KeyCode::KeyA, Key::Dead(Some('`')));
        let result = bevy_to_terminal_key(&event, &no_modifiers());
        assert!(result.is_none());
    }

    #[test]
    fn unidentified_letter_rejected() {
        // Letter keys still require logical_key — physical fallback would
        // produce wrong chars on non-QWERTY layouts.
        let event = make_event(KeyCode::KeyA, Key::Unidentified(bevy::input::keyboard::NativeKey::Unidentified));
        let result = bevy_to_terminal_key(&event, &no_modifiers());
        assert!(result.is_none());
    }

    #[test]
    fn unidentified_escape_falls_back_to_key_code() {
        // bevy_brp's send_keys ships named keys with logical_key=Unidentified.
        // The fallback path maps the physical key_code to crossterm so vim
        // still receives the press.
        let event = make_event(
            KeyCode::Escape,
            Key::Unidentified(bevy::input::keyboard::NativeKey::Unidentified),
        );
        let result = bevy_to_terminal_key(&event, &no_modifiers()).unwrap();
        let expected = TerminalKey::from(KeyEvent::new(CtKeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(result, expected);
    }

    #[test]
    fn unidentified_arrow_falls_back_to_key_code() {
        let event = make_event(
            KeyCode::ArrowLeft,
            Key::Unidentified(bevy::input::keyboard::NativeKey::Unidentified),
        );
        let result = bevy_to_terminal_key(&event, &no_modifiers()).unwrap();
        let expected = TerminalKey::from(KeyEvent::new(CtKeyCode::Left, KeyModifiers::NONE));
        assert_eq!(result, expected);
    }
}
