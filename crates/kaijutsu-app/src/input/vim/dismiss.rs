//! Multi-press Escape state for compose dismissal.
//!
//! Escape on a vim-modal compose surface is overloaded: in Insert mode it
//! switches to Normal mode (modalkit's default), and in Normal mode the
//! VimMachine produces no useful action. To give the user a way to
//! actually leave compose, we count rapid Escape presses — the second tap
//! within the window while vim is in Normal mode fires `Action::PopLevel`.
//!
//! Built on the shared [`TapCounter`] (`input/tap.rs`) — the same primitive
//! under the Ctrl+C interrupt ladder. The counter saturates at 2 and only
//! the dismiss consumes it, so an `Esc Esc` that lands outside Normal mode
//! stays armed for the next tap.

use bevy::prelude::*;

use crate::input::tap::TapCounter;

/// Time window for counting consecutive Escape presses (milliseconds).
const WINDOW_MS: u128 = 500;

/// Per-session state for the double-Escape dismiss gesture.
#[derive(Resource)]
pub struct EscapeDismissState(TapCounter);

impl Default for EscapeDismissState {
    fn default() -> Self {
        Self(TapCounter::new(WINDOW_MS, 2))
    }
}

impl EscapeDismissState {
    /// Reset the press count and timestamp.
    ///
    /// Called after a successful dismiss, and on any non-Escape keypress
    /// so that `Esc x Esc` does not count as a double-tap.
    pub fn reset(&mut self) {
        self.0.reset()
    }
}

/// Decide whether this Escape press should dismiss compose.
///
/// Bumps the press counter and returns true when:
///   - this is the second press within the window, AND
///   - vim is currently in Normal mode (no mode banner)
///
/// On a true return the state is already reset, so the caller can fire
/// `Action::PopLevel` and move on.
pub fn should_dismiss_after_escape(
    state: &mut EscapeDismissState,
    in_normal_mode: bool,
) -> bool {
    let count = state.0.press();
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
    fn saturated_count_stays_armed_until_normal_mode() {
        // Esc Esc both landing outside Normal (e.g. visual → normal
        // transitions still in flight): the counter saturates at 2 and
        // stays armed, so the NEXT rapid Esc in Normal mode dismisses.
        let mut s = EscapeDismissState::default();
        assert!(!should_dismiss_after_escape(&mut s, false));
        assert!(!should_dismiss_after_escape(&mut s, false));
        assert!(should_dismiss_after_escape(&mut s, true));
    }

    #[test]
    fn dismiss_helper_resets_state_on_fire() {
        // After firing, the next press starts a fresh count of 1 — so we
        // don't accidentally dismiss on every subsequent Esc.
        let mut s = EscapeDismissState::default();
        should_dismiss_after_escape(&mut s, true);
        should_dismiss_after_escape(&mut s, true);
        // State was reset by the dismiss above; this single press must not
        // fire even in Normal mode.
        assert!(!should_dismiss_after_escape(&mut s, true));
    }

    #[test]
    fn reset_breaks_the_gesture() {
        // A non-Escape key between taps calls reset() — `Esc x Esc` must
        // not dismiss.
        let mut s = EscapeDismissState::default();
        assert!(!should_dismiss_after_escape(&mut s, true));
        s.reset();
        assert!(!should_dismiss_after_escape(&mut s, true));
    }
}
