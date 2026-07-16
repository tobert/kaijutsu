//! The Ctrl+A prefix machine — screen/tmux muscle memory, kaijutsu verbs.
//!
//! Sits at the very front of the dispatcher's keyboard loop (docs/input.md
//! "The prefix table"): `Ctrl+A` arms it on any surface — conversation,
//! compose, the scenes, even the vi editor (prefix wins over vi; `Ctrl+A a`
//! is the literal-passthrough escape hatch). The next pressed key resolves
//! to an action; unbound keys are swallowed with a log flash, never leaked
//! to the surface below. The pending state times out after
//! [`PREFIX_TIMEOUT_MS`].
//!
//! The chord table is deliberately hardcoded here (not `bindings.toml`):
//! it's one screenful, its semantics are documented in docs/input.md, and a
//! prefix chord resolving differently per user would defeat the muscle
//! memory it exists to serve.

use bevy::prelude::*;
use std::time::Instant;

use super::action::Action;

/// How long an armed prefix waits for its second key (milliseconds).
pub const PREFIX_TIMEOUT_MS: u128 = 1000;

/// Pending-prefix state. Armed by Ctrl+A, cleared by the resolving key or
/// the timeout.
#[derive(Resource, Default)]
pub struct PrefixState {
    armed_at: Option<Instant>,
}

impl PrefixState {
    pub fn arm(&mut self) {
        self.armed_at = Some(Instant::now());
    }

    pub fn disarm(&mut self) {
        self.armed_at = None;
    }

    pub fn armed(&self) -> bool {
        self.armed_at.is_some()
    }

    /// Disarm if the window has lapsed; returns whether it just expired.
    pub fn tick_timeout(&mut self) -> bool {
        if let Some(t) = self.armed_at
            && t.elapsed().as_millis() >= PREFIX_TIMEOUT_MS
        {
            self.armed_at = None;
            return true;
        }
        false
    }
}

/// Bare modifier keys neither resolve nor cancel an armed prefix — the user
/// may still be holding (or re-pressing) Ctrl on the way to the second key.
pub fn is_bare_modifier(key: KeyCode) -> bool {
    matches!(
        key,
        KeyCode::ControlLeft
            | KeyCode::ControlRight
            | KeyCode::ShiftLeft
            | KeyCode::ShiftRight
            | KeyCode::AltLeft
            | KeyCode::AltRight
            | KeyCode::SuperLeft
            | KeyCode::SuperRight
    )
}

/// Resolve the second key of an armed prefix. `None` = unbound (swallow +
/// flash). Digits accept Ctrl held or released — screen users do both.
///
/// The table (docs/input.md; `'` switch-by-prompt and `A` rename are
/// deferred until the kj verbs they prefill exist — docs/issues.md):
pub fn resolve_chord(key: KeyCode, ctrl: bool, shift: bool) -> Option<Action> {
    match key {
        KeyCode::Digit0 => Some(Action::SwitchToActiveSeat(0)),
        KeyCode::Digit1 => Some(Action::SwitchToActiveSeat(1)),
        KeyCode::Digit2 => Some(Action::SwitchToActiveSeat(2)),
        KeyCode::Digit3 => Some(Action::SwitchToActiveSeat(3)),
        KeyCode::Digit4 => Some(Action::SwitchToActiveSeat(4)),
        KeyCode::Digit5 => Some(Action::SwitchToActiveSeat(5)),
        KeyCode::Digit6 => Some(Action::SwitchToActiveSeat(6)),
        KeyCode::Digit7 => Some(Action::SwitchToActiveSeat(7)),
        KeyCode::Digit8 => Some(Action::SwitchToActiveSeat(8)),
        KeyCode::Digit9 => Some(Action::SwitchToActiveSeat(9)),
        // Ctrl+A Ctrl+A — other window; Ctrl+A A (shift) — rename prompt
        // (screen's `A` title); Ctrl+A a — send a literal Ctrl+A through to
        // the focused vi surface (screen's `a` meta).
        KeyCode::KeyA if ctrl => Some(Action::SwitchToPreviousContext),
        KeyCode::KeyA if shift => Some(Action::PromptContextRename),
        KeyCode::KeyA => Some(Action::SendLiteralPrefix),
        // q — close-and-demote (Amy, 2026-07-16): demote on the ring ladder,
        // land on the MRU-previous context.
        KeyCode::KeyQ => Some(Action::CloseAndDemoteContext),
        // w and " — the well (the window list IS the well); ' — the
        // switch-by-prompt (screen's `'` select).
        KeyCode::KeyW => Some(Action::GoToWell),
        KeyCode::Quote if shift => Some(Action::GoToWell),
        KeyCode::Quote => Some(Action::PromptContextSwitch),
        // n / p — walk ring-0 seats.
        KeyCode::KeyN => Some(Action::ActiveSeatStep(1)),
        KeyCode::KeyP => Some(Action::ActiveSeatStep(-1)),
        // d — detach to the conversation view from any scene/editor.
        KeyCode::KeyD => Some(Action::DetachToConversation),
        // Esc cancels the pending prefix quietly.
        KeyCode::Escape => None,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_resolve_to_seats_with_or_without_ctrl() {
        // Screen fingers do both: Ctrl held through the digit, or released.
        assert_eq!(
            resolve_chord(KeyCode::Digit2, true, false),
            Some(Action::SwitchToActiveSeat(2))
        );
        assert_eq!(
            resolve_chord(KeyCode::Digit2, false, false),
            Some(Action::SwitchToActiveSeat(2))
        );
    }

    #[test]
    fn ctrl_a_ctrl_a_toggles_last_but_bare_a_is_literal() {
        assert_eq!(
            resolve_chord(KeyCode::KeyA, true, false),
            Some(Action::SwitchToPreviousContext)
        );
        assert_eq!(
            resolve_chord(KeyCode::KeyA, false, false),
            Some(Action::SendLiteralPrefix)
        );
    }

    #[test]
    fn well_chords() {
        assert_eq!(resolve_chord(KeyCode::KeyW, false, false), Some(Action::GoToWell));
        assert_eq!(resolve_chord(KeyCode::Quote, false, true), Some(Action::GoToWell));
    }

    #[test]
    fn prompt_chords() {
        // ' — switch-by-prompt; A (shift+a) — rename prompt.
        assert_eq!(
            resolve_chord(KeyCode::Quote, false, false),
            Some(Action::PromptContextSwitch)
        );
        assert_eq!(
            resolve_chord(KeyCode::KeyA, false, true),
            Some(Action::PromptContextRename)
        );
        // Ctrl wins over shift on the A family (Ctrl+A Ctrl+A with shift
        // accidentally down is still the toggle).
        assert_eq!(
            resolve_chord(KeyCode::KeyA, true, true),
            Some(Action::SwitchToPreviousContext)
        );
    }

    #[test]
    fn unbound_is_none() {
        assert_eq!(resolve_chord(KeyCode::KeyX, false, false), None);
        assert_eq!(resolve_chord(KeyCode::Escape, false, false), None);
    }

    #[test]
    fn timeout_disarms() {
        let mut s = PrefixState::default();
        s.arm();
        assert!(s.armed());
        // Backdate the arm so the next tick expires it.
        s.armed_at = Some(
            Instant::now() - std::time::Duration::from_millis(PREFIX_TIMEOUT_MS as u64 + 10),
        );
        assert!(s.tick_timeout());
        assert!(!s.armed());
    }

    #[test]
    fn modifiers_are_bare() {
        assert!(is_bare_modifier(KeyCode::ControlLeft));
        assert!(is_bare_modifier(KeyCode::ShiftRight));
        assert!(!is_bare_modifier(KeyCode::KeyA));
    }
}
