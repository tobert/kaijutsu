//! `Ctrl+Z`'s double tap, which suspends the process (`docs/tui.md`, "Keys").
//! What the single press does, the toggle between a person's root console and
//! a context the person performs, is `kaijutsu_client::shell_toggle`, shared
//! with the GUI.
//!
//! Pure: no terminal, no clock of its own; [`Tap::press`] takes the instant
//! in. The window is [`crate::app::DOUBLE_TAP`], the window `Ctrl+C` and
//! `Ctrl+A Ctrl+A` use.

use std::time::Instant;

use crate::app::DOUBLE_TAP;

/// The `Ctrl+Z` double tap: the second press within [`crate::app::DOUBLE_TAP`]
/// of the first. Mirrors `interrupt::Ladder`'s window, without its escalation.
#[derive(Debug, Default, Clone, Copy)]
pub struct Tap {
    last: Option<Instant>,
}

impl Tap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a press at `now`. Returns `true` when it completes a double
    /// tap. A completed double tap clears the pending first press, so a
    /// third press starts a fresh tap rather than suspending again.
    pub fn press(&mut self, now: Instant) -> bool {
        let second = self.last.is_some_and(|prev| now.duration_since(prev) <= DOUBLE_TAP);
        self.last = if second { None } else { Some(now) };
        second
    }

    /// Forget any pending first press, e.g. after the process has been
    /// suspended and resumed.
    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::DOUBLE_TAP;
    use std::time::Duration;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_first_press_is_never_a_double_tap() {
        let mut tap = Tap::new();
        assert!(!tap.press(t0()));
    }

    #[test]
    fn a_second_press_within_the_window_completes_the_double_tap() {
        let mut tap = Tap::new();
        let start = t0();
        assert!(!tap.press(start));
        assert!(tap.press(start + Duration::from_millis(200)));
    }

    /// Same boundary as `Ladder`: a press exactly at the window still counts.
    #[test]
    fn a_second_press_at_the_window_edge_counts() {
        let mut tap = Tap::new();
        let start = t0();
        tap.press(start);
        assert!(tap.press(start + DOUBLE_TAP));
    }

    /// The window mutation: a stale first press must not pair with a late
    /// second one. The late press starts a new tap, and the one after it
    /// completes that.
    #[test]
    fn a_press_outside_the_window_starts_over() {
        let mut tap = Tap::new();
        let start = t0();
        assert!(!tap.press(start));
        assert!(!tap.press(start + Duration::from_millis(600)));
        assert!(tap.press(start + Duration::from_millis(800)));
    }

    /// A completed double tap leaves nothing pending: the next press in the
    /// same window is a first press, not a third that suspends again.
    #[test]
    fn a_completed_double_tap_leaves_no_pending_first_press() {
        let mut tap = Tap::new();
        let start = t0();
        tap.press(start);
        assert!(tap.press(start + Duration::from_millis(100)));
        assert!(!tap.press(start + Duration::from_millis(200)));
    }

    #[test]
    fn reset_forgets_a_pending_first_press() {
        let mut tap = Tap::new();
        let start = t0();
        tap.press(start);
        tap.reset();
        assert!(!tap.press(start + Duration::from_millis(100)));
    }
}
