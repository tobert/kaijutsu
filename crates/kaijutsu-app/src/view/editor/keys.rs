//! Translate Bevy keyboard input into the kernel's vi key-notation.
//!
//! The app is a *key forwarder*: it does not run a VimMachine (the kernel's
//! `EditorCore` does). Each keystroke becomes a notation string that
//! `EditorCore::apply_keys` / `parse_keys` understands — a literal char, or a
//! named/chorded token like `<Esc>`, `<CR>`, `<BS>`, `<Tab>`, `<Del>`, the
//! arrows, `<Home>`, `<End>`, `<PageUp>`, `<PageDown>`, `<lt>` for `<`, or
//! `<C-w>`. Keystrokes ride the ordered editor outbox
//! (`kaijutsu_client::editor_outbox`), and the editor push channel echoes the
//! resulting state back to the renderer.
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

/// The modifier keys held while a key event was pressed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Mods {
    /// Read the live modifier state from the keyboard.
    pub fn from_input(keys: &ButtonInput<KeyCode>) -> Self {
        Self {
            ctrl: keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight]),
            alt: keys.any_pressed([KeyCode::AltLeft, KeyCode::AltRight]),
            shift: keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]),
        }
    }

    #[cfg(test)]
    pub const CTRL: Self = Self { ctrl: true, alt: false, shift: false };
}

/// Is this key press the paste chord? Ctrl+Shift+V reads the clipboard; plain
/// Ctrl+V stays vi's visual-block key.
pub fn paste_chord(key_code: KeyCode, mods: Mods) -> bool {
    key_code == KeyCode::KeyV && mods.ctrl && mods.shift && !mods.alt
}

/// Translate a *pressed* Bevy keyboard event into kernel vi notation, or `None`
/// for keys the notation can't express: function keys, and a navigation key
/// held with Ctrl, Alt or Shift (the bare token would be a different key).
///
/// `mods` is the live modifier state (a Ctrl chord becomes `<C-x>`). Note
/// `Z` is an ordinary keystroke here — the app forwards every key, including
/// `Z`; it's the kernel's `EditorCore` (the mode owner) that recognizes a real
/// `ZZ`/`ZQ` and closes the session, pushing `EditorClosed`.
pub fn bevy_to_vi_notation(event: &KeyboardInput, mods: Mods) -> Option<String> {
    let ctrl = mods.ctrl;
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
    let navigation = match event.key_code {
        KeyCode::ArrowLeft => Some("<Left>"),
        KeyCode::ArrowRight => Some("<Right>"),
        KeyCode::ArrowUp => Some("<Up>"),
        KeyCode::ArrowDown => Some("<Down>"),
        KeyCode::Home => Some("<Home>"),
        KeyCode::End => Some("<End>"),
        KeyCode::PageUp => Some("<PageUp>"),
        KeyCode::PageDown => Some("<PageDown>"),
        KeyCode::Delete => Some("<Del>"),
        _ => None,
    };
    if let Some(n) = navigation {
        return (!(mods.ctrl || mods.alt || mods.shift)).then(|| n.to_string());
    }

    let c = pressed_char(event)?;

    if ctrl {
        // Control chord: `<C-x>`, lowercased to match vim/kernel notation.
        return Some(format!("<C-{}>", c.to_ascii_lowercase()));
    }

    // A bare `<` opens a `<...>` token in the kernel's `parse_keys`; `<lt>` is
    // the literal.
    if c == '<' {
        return Some("<lt>".to_string());
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
            bevy_to_vi_notation(&char_ev(KeyCode::KeyI, "i"), Mods::default()).as_deref(),
            Some("i")
        );
    }

    #[test]
    fn uppercase_char_preserved() {
        // Shift+a arrives as logical Character("A").
        assert_eq!(
            bevy_to_vi_notation(&char_ev(KeyCode::KeyA, "A"), Mods::default()).as_deref(),
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
                bevy_to_vi_notation(&event, Mods::default()).as_deref(),
                Some(want),
                "key_code {kc:?}"
            );
        }
    }

    #[test]
    fn ctrl_chord_lowercased() {
        // Ctrl+W → <C-w> regardless of shift-derived case.
        assert_eq!(
            bevy_to_vi_notation(&char_ev(KeyCode::KeyW, "w"), Mods::CTRL).as_deref(),
            Some("<C-w>")
        );
        assert_eq!(
            bevy_to_vi_notation(&char_ev(KeyCode::KeyW, "W"), Mods::CTRL).as_deref(),
            Some("<C-w>")
        );
    }

    #[test]
    fn less_than_is_the_lt_token() {
        // A bare `<` would open a `<...>` token in the kernel's `parse_keys`.
        assert_eq!(
            bevy_to_vi_notation(&char_ev(KeyCode::Comma, "<"), Mods::default()).as_deref(),
            Some("<lt>")
        );
    }

    fn unidentified(kc: KeyCode) -> KeyboardInput {
        ev(kc, Key::Unidentified(bevy::input::keyboard::NativeKey::Unidentified))
    }

    #[test]
    fn only_ctrl_shift_v_pastes() {
        let both = Mods { ctrl: true, shift: true, alt: false };
        assert!(paste_chord(KeyCode::KeyV, both));
        assert!(!paste_chord(KeyCode::KeyV, Mods::CTRL), "Ctrl+V is visual block");
        assert!(!paste_chord(KeyCode::KeyV, Mods::default()));
        assert!(!paste_chord(KeyCode::KeyC, both));
        assert!(!paste_chord(KeyCode::KeyV, Mods { alt: true, ..both }));
    }

    #[test]
    fn navigation_keys_become_tokens() {
        for (kc, want) in [
            (KeyCode::ArrowLeft, "<Left>"),
            (KeyCode::ArrowRight, "<Right>"),
            (KeyCode::ArrowUp, "<Up>"),
            (KeyCode::ArrowDown, "<Down>"),
            (KeyCode::Home, "<Home>"),
            (KeyCode::End, "<End>"),
            (KeyCode::PageUp, "<PageUp>"),
            (KeyCode::PageDown, "<PageDown>"),
            (KeyCode::Delete, "<Del>"),
        ] {
            assert_eq!(
                bevy_to_vi_notation(&unidentified(kc), Mods::default()).as_deref(),
                Some(want),
                "key_code {kc:?}"
            );
        }
    }

    #[test]
    fn navigation_key_with_a_modifier_is_refused() {
        // `Ctrl+Left` is not `<Left>`; the bare token would be a different key.
        for mods in [
            Mods::CTRL,
            Mods { alt: true, ..Mods::default() },
            Mods { shift: true, ..Mods::default() },
        ] {
            for kc in [KeyCode::ArrowLeft, KeyCode::Home, KeyCode::PageDown, KeyCode::Delete] {
                assert_eq!(bevy_to_vi_notation(&unidentified(kc), mods), None, "{kc:?} {mods:?}");
            }
        }
    }

    #[test]
    fn multi_char_rejected() {
        // Dead-key compose output is not a single keystroke.
        assert_eq!(bevy_to_vi_notation(&char_ev(KeyCode::KeyA, "fi"), Mods::default()), None);
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
        // `Z` is an ordinary character key: the kernel, not the app, sees ZZ/ZQ.
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
