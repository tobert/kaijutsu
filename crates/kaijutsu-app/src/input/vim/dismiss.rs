//! Multi-press Escape state for compose dismissal.
//!
//! Escape on a vim-modal compose surface is overloaded: in Insert mode it
//! switches to Normal mode (modalkit's default), and in Normal mode the
//! VimMachine produces no useful action. To give the user a way to
//! actually leave compose, we count rapid Escape presses — the second tap
//! within `WINDOW_MS` while vim is in Normal mode fires `Action::Unfocus`.
//!
//! The state shape mirrors `input::interrupt::InterruptState` (used for
//! Ctrl+C). When we add more double-tap dismiss gestures (e.g. screen-style
//! Ctrl+A Ctrl+A → start of line) we can lift this into a generic helper.

use bevy::prelude::*;

/// Time window for counting consecutive Escape presses (milliseconds).
const WINDOW_MS: u128 = 500;

/// Per-session state for the double-Escape dismiss gesture.
#[derive(Resource, Default)]
pub struct EscapeDismissState {
    count: u8,
    last_press: Option<std::time::Instant>,
}

impl EscapeDismissState {
    /// Record a new Escape press and return the current count (1 or 2).
    ///
    /// If the previous press was more than `WINDOW_MS` ago, the count
    /// resets to 1.
    pub fn press(&mut self) -> u8 {
        let now = std::time::Instant::now();
        if let Some(last) = self.last_press
            && now.duration_since(last).as_millis() < WINDOW_MS
        {
            self.count = (self.count + 1).min(2);
        } else {
            self.count = 1;
        }
        self.last_press = Some(now);
        self.count
    }

    /// Reset the press count and timestamp.
    ///
    /// Called after a successful dismiss, and on any non-Escape keypress
    /// so that `Esc x Esc` does not count as a double-tap.
    pub fn reset(&mut self) {
        self.count = 0;
        self.last_press = None;
    }

    /// Current press count without mutating state. Test/debug accessor.
    #[cfg(test)]
    pub fn count(&self) -> u8 {
        self.count
    }
}

/// Decide whether this Escape press should dismiss compose.
///
/// Bumps the press counter and returns true when:
///   - this is the second press within the window, AND
///   - vim is currently in Normal mode (no mode banner)
///
/// On a true return the state is already reset, so the caller can fire
/// `Action::Unfocus` and move on.
pub fn should_dismiss_after_escape(
    state: &mut EscapeDismissState,
    in_normal_mode: bool,
) -> bool {
    let count = state.press();
    if count >= 2 && in_normal_mode {
        state.reset();
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_press_is_one() {
        let mut s = EscapeDismissState::default();
        assert_eq!(s.press(), 1);
    }

    #[test]
    fn second_press_within_window_is_two() {
        let mut s = EscapeDismissState::default();
        s.press();
        assert_eq!(s.press(), 2);
    }

    #[test]
    fn third_press_saturates_at_two() {
        // We don't escalate beyond 2 — there's no third dismiss tier.
        let mut s = EscapeDismissState::default();
        s.press();
        s.press();
        assert_eq!(s.press(), 2);
    }

    #[test]
    fn reset_clears_count() {
        let mut s = EscapeDismissState::default();
        s.press();
        s.press();
        s.reset();
        assert_eq!(s.count(), 0);
        assert!(s.last_press.is_none());
        // Next press starts at 1 again.
        assert_eq!(s.press(), 1);
    }

    #[test]
    fn dismiss_helper_fires_on_second_press_in_normal_mode() {
        let mut s = EscapeDismissState::default();
        assert!(!should_dismiss_after_escape(&mut s, true));
        assert!(should_dismiss_after_escape(&mut s, true));
    }

    #[test]
    fn dismiss_helper_does_not_fire_outside_normal_mode() {
        // First Esc lands in Insert (non-Normal). It bumps count but
        // doesn't dismiss — Insert→Normal is the work for that press.
        let mut s = EscapeDismissState::default();
        assert!(!should_dismiss_after_escape(&mut s, false));
        // Second Esc — now in Normal mode (the first one took us there).
        assert!(should_dismiss_after_escape(&mut s, true));
    }

    #[test]
    fn dismiss_helper_resets_state_on_fire() {
        // After firing, the next press starts a fresh count of 1 — so we
        // don't accidentally dismiss on every subsequent Esc.
        let mut s = EscapeDismissState::default();
        should_dismiss_after_escape(&mut s, true);
        should_dismiss_after_escape(&mut s, true);
        // State is reset after the dismiss above; this should be a fresh 1.
        assert_eq!(s.press(), 1);
    }

    #[test]
    fn window_expiry_resets_count() {
        // Stale presses outside the window fall back to a fresh count.
        let mut s = EscapeDismissState::default();
        s.press();
        // Backdate the last press so the next one falls outside the window.
        s.last_press =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(WINDOW_MS as u64 + 50));
        assert_eq!(s.press(), 1);
    }
}
