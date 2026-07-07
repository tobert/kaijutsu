//! Translate Bevy keyboard input into the kernel's vi key-notation.
//!
//! The app is a *key forwarder*: it does not run a VimMachine (the kernel's
//! `EditorCore` does). Each keystroke becomes a notation string that
//! `EditorCore::apply_keys` / `parse_keys` understands — a literal char, or a
//! named/chorded token like `<Esc>`, `<CR>`, `<BS>`, `<Tab>`, `<C-w>`. We send
//! one keystroke per `editor_keys` call and rely on the editor push channel to
//! echo the resulting state back to the renderer.
//!
//! This is deliberately distinct from `input::vim::keyconv` (which builds a
//! modalkit `TerminalKey` for the *local* compose VimMachine): here the target
//! is the kernel's text notation, parsed by `kaijutsu-editor`.

use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;

/// The single character a key event resolves to, layout-aware via `logical_key`.
/// `None` for non-character keys (named keys, function keys) and for multi-char
/// dead-key compose sequences we can't forward as one keystroke.
fn pressed_char(event: &KeyboardInput) -> Option<char> {
    let Key::Character(s) = &event.logical_key else {
        return None;
    };
    let mut chars = s.chars();
    let c = chars.next()?;
    // Reject dead-key compose output ("fi", etc.) — not a single keystroke.
    if chars.next().is_some() {
        return None;
    }
    Some(c)
}

/// Whether `key_code` is a bare modifier key (Shift/Ctrl/Alt/Super). These are
/// never standalone vi keystrokes; the dispatcher skips them so a modifier press
/// can't cancel a pending operator (e.g. Shift re-asserted between `Z` and `Q`).
pub fn is_modifier_key(key_code: KeyCode) -> bool {
    matches!(
        key_code,
        KeyCode::ShiftLeft
            | KeyCode::ShiftRight
            | KeyCode::ControlLeft
            | KeyCode::ControlRight
            | KeyCode::AltLeft
            | KeyCode::AltRight
            | KeyCode::SuperLeft
            | KeyCode::SuperRight
    )
}

/// Translate a *pressed* Bevy keyboard event into kernel vi notation, or `None`
/// for keys the notation can't express (arrows / function keys — pass-1 vi
/// navigates with `hjkl`; `docs/issues.md` tracks widening this).
///
/// `ctrl` is the live control-modifier state (a chord becomes `<C-x>`). Note
/// `Z` is an ordinary keystroke here — the app forwards every key, including
/// `Z`; it's the kernel's `EditorCore` (the mode owner) that recognizes a real
/// `ZZ`/`ZQ` and closes the session, pushing `EditorClosed`.
pub fn bevy_to_vi_notation(event: &KeyboardInput, ctrl: bool) -> Option<String> {
    // Named keys resolve from the physical key_code (layout-independent) so a
    // synthetic event with `logical_key = Unidentified` (e.g. bevy_brp send_keys)
    // still maps. Space goes through here as a literal — `parse_keys` accepts a
    // bare space as a plain key.
    let named = match event.key_code {
        KeyCode::Escape => Some("<Esc>"),
        KeyCode::Enter => Some("<CR>"),
        KeyCode::Backspace => Some("<BS>"),
        KeyCode::Tab => Some("<Tab>"),
        KeyCode::Space => Some(" "),
        _ => None,
    };
    if let Some(n) = named {
        return Some(n.to_string());
    }

    let c = pressed_char(event)?;

    if ctrl {
        // Control chord: `<C-x>`, lowercased to match vim/kernel notation.
        return Some(format!("<C-{}>", c.to_ascii_lowercase()));
    }

    // A literal `<` would be read as the start of a `<...>` token by the kernel's
    // `parse_keys` (which has no `<lt>` escape and silently drops unknown
    // tokens), so we can't forward it faithfully yet. Guard it out rather than
    // corrupt the buffer. (docs/issues.md)
    if c == '<' {
        return None;
    }

    Some(c.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::entity::Entity;
    use bevy::input::ButtonState;

    fn ev(key_code: KeyCode, logical_key: Key) -> KeyboardInput {
        KeyboardInput {
            key_code,
            logical_key,
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window: Entity::PLACEHOLDER,
        }
    }

    fn char_ev(key_code: KeyCode, c: &str) -> KeyboardInput {
        ev(key_code, Key::Character(c.into()))
    }

    #[test]
    fn literal_char() {
        assert_eq!(
            bevy_to_vi_notation(&char_ev(KeyCode::KeyI, "i"), false).as_deref(),
            Some("i")
        );
    }

    #[test]
    fn uppercase_char_preserved() {
        // Shift+a arrives as logical Character("A").
        assert_eq!(
            bevy_to_vi_notation(&char_ev(KeyCode::KeyA, "A"), false).as_deref(),
            Some("A")
        );
    }

    #[test]
    fn named_keys() {
        for (kc, want) in [
            (KeyCode::Escape, "<Esc>"),
            (KeyCode::Enter, "<CR>"),
            (KeyCode::Backspace, "<BS>"),
            (KeyCode::Tab, "<Tab>"),
            (KeyCode::Space, " "),
        ] {
            // Named keys must resolve even with an Unidentified logical_key.
            let event = ev(
                kc,
                Key::Unidentified(bevy::input::keyboard::NativeKey::Unidentified),
            );
            assert_eq!(
                bevy_to_vi_notation(&event, false).as_deref(),
                Some(want),
                "key_code {kc:?}"
            );
        }
    }

    #[test]
    fn ctrl_chord_lowercased() {
        // Ctrl+W → <C-w> regardless of shift-derived case.
        assert_eq!(
            bevy_to_vi_notation(&char_ev(KeyCode::KeyW, "w"), true).as_deref(),
            Some("<C-w>")
        );
        assert_eq!(
            bevy_to_vi_notation(&char_ev(KeyCode::KeyW, "W"), true).as_deref(),
            Some("<C-w>")
        );
    }

    #[test]
    fn less_than_guarded_out() {
        // `<` would be mis-parsed as a token opener by the kernel — dropped.
        assert_eq!(bevy_to_vi_notation(&char_ev(KeyCode::Comma, "<"), false), None);
    }

    #[test]
    fn multi_char_rejected() {
        // Dead-key compose output is not a single keystroke.
        assert_eq!(bevy_to_vi_notation(&char_ev(KeyCode::KeyA, "fi"), false), None);
    }

    #[test]
    fn modifier_keys_detected() {
        for kc in [
            KeyCode::ShiftLeft,
            KeyCode::ShiftRight,
            KeyCode::ControlLeft,
            KeyCode::AltLeft,
            KeyCode::SuperLeft,
        ] {
            assert!(is_modifier_key(kc), "{kc:?} should be a modifier");
        }
        assert!(!is_modifier_key(KeyCode::KeyZ));
        assert!(!is_modifier_key(KeyCode::Escape));
    }

    #[test]
    fn pressed_char_detects_capital_z() {
        // ZZ/ZQ detection in the dispatcher keys off this.
        assert_eq!(pressed_char(&char_ev(KeyCode::KeyZ, "Z")), Some('Z'));
        assert_eq!(pressed_char(&char_ev(KeyCode::KeyZ, "z")), Some('z'));
        assert_eq!(
            pressed_char(&ev(
                KeyCode::Escape,
                Key::Unidentified(bevy::input::keyboard::NativeKey::Unidentified)
            )),
            None
        );
    }
}
