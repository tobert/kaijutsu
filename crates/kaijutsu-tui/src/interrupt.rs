//! `Ctrl+C`'s escalation ladder — the terminal client's port of the app's
//! `input::interrupt` (`kaijutsu-app/src/input/interrupt.rs`, `TapCounter`
//! with a 500ms window, max 3), reimplemented rather than pulled in as a
//! dependency: `kaijutsu-tui` does not otherwise depend on the app's Bevy
//! stack. `docs/tui.md`, "Ctrl+C reclaimed".
//!
//! Pure: no RPC, no terminal. [`Ladder::press`] turns one keystroke into a
//! [`Step`]; the caller makes the `interrupt_context` call and posts the
//! notice.

use std::time::Instant;

use crate::app::DOUBLE_TAP;

/// What one `Ctrl+C` press asks the caller to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// No turn is known running in the target context: call nothing.
    Nothing,
    /// 1st press: soft interrupt (`interrupt_context(ctx, immediate = false)`).
    Soft,
    /// 2nd press within the window: hard interrupt
    /// (`interrupt_context(ctx, immediate = true)`).
    Hard,
    /// 3rd+ press within the window: hard interrupt, and the caller also
    /// clears the draft.
    HardAndClear,
}

/// The escalation counter, one per client (not per context — `Ctrl+C`
/// always targets whichever context is on screen when it is pressed).
#[derive(Debug, Default)]
pub struct Ladder {
    count: u8,
    last: Option<Instant>,
}

impl Ladder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a press at `now` and turn it into a [`Step`]. `running` is
    /// whether this client currently believes a turn is running in the
    /// target context (`App::turns_running` — a partial signal,
    /// `docs/tui.md`, "Ctrl+C reclaimed"): a press that would **start** a
    /// fresh ladder (no prior press, or the prior one fell outside the
    /// window) with nothing known running is [`Step::Nothing`] rather than
    /// step 1 of a ladder with nothing to escalate. A press that continues
    /// an already-started ladder keeps escalating even if `running` has
    /// since gone false — the player is already mid-gesture.
    pub fn press(&mut self, now: Instant, running: bool) -> Step {
        let within_window = self.last.is_some_and(|prev| now.duration_since(prev) <= DOUBLE_TAP);
        if !within_window {
            if !running {
                self.count = 0;
                self.last = None;
                return Step::Nothing;
            }
            self.count = 1;
        } else {
            self.count = (self.count + 1).min(3);
        }
        self.last = Some(now);
        match self.count {
            1 => Step::Soft,
            2 => Step::Hard,
            _ => Step::HardAndClear,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn nothing_running_posts_nothing_to_interrupt() {
        let mut ladder = Ladder::new();
        assert_eq!(ladder.press(Instant::now(), false), Step::Nothing);
    }

    #[test]
    fn repeated_presses_with_nothing_running_stay_at_nothing() {
        let mut ladder = Ladder::new();
        let t0 = Instant::now();
        assert_eq!(ladder.press(t0, false), Step::Nothing);
        assert_eq!(ladder.press(t0 + Duration::from_millis(100), false), Step::Nothing);
    }

    #[test]
    fn first_press_with_something_running_is_soft() {
        let mut ladder = Ladder::new();
        assert_eq!(ladder.press(Instant::now(), true), Step::Soft);
    }

    /// The mutation this guards against: ignoring the window (treating every
    /// press as "within") would jump straight to `Hard` on the very first
    /// press whenever a stale `last` happened to be set.
    #[test]
    fn a_second_press_within_the_window_escalates_to_hard() {
        let mut ladder = Ladder::new();
        let t0 = Instant::now();
        assert_eq!(ladder.press(t0, true), Step::Soft);
        assert_eq!(ladder.press(t0 + Duration::from_millis(200), true), Step::Hard);
    }

    #[test]
    fn a_third_press_within_the_window_escalates_to_hard_and_clear() {
        let mut ladder = Ladder::new();
        let t0 = Instant::now();
        ladder.press(t0, true);
        ladder.press(t0 + Duration::from_millis(100), true);
        assert_eq!(ladder.press(t0 + Duration::from_millis(200), true), Step::HardAndClear);
    }

    #[test]
    fn presses_past_three_saturate_at_hard_and_clear() {
        let mut ladder = Ladder::new();
        let t0 = Instant::now();
        for n in 0..5 {
            let step = ladder.press(t0 + Duration::from_millis(n * 50), true);
            if n >= 2 {
                assert_eq!(step, Step::HardAndClear);
            }
        }
    }

    #[test]
    fn a_press_outside_the_window_restarts_the_ladder() {
        let mut ladder = Ladder::new();
        let t0 = Instant::now();
        ladder.press(t0, true);
        ladder.press(t0 + Duration::from_millis(100), true);
        // Outside DOUBLE_TAP (500ms): a fresh ladder, back to Soft.
        assert_eq!(ladder.press(t0 + Duration::from_secs(1), true), Step::Soft);
    }

    /// A ladder already in progress keeps escalating even if the turn ended
    /// between presses — the player is mid-gesture, not starting fresh.
    #[test]
    fn an_in_progress_ladder_keeps_escalating_after_running_goes_false() {
        let mut ladder = Ladder::new();
        let t0 = Instant::now();
        assert_eq!(ladder.press(t0, true), Step::Soft);
        assert_eq!(ladder.press(t0 + Duration::from_millis(100), false), Step::Hard);
    }

    /// A fresh ladder (outside the window) with nothing running resets
    /// state, so a later press with something running starts clean at Soft
    /// rather than inheriting a stale count.
    #[test]
    fn nothing_to_interrupt_resets_the_count() {
        let mut ladder = Ladder::new();
        let t0 = Instant::now();
        ladder.press(t0, true);
        ladder.press(t0 + Duration::from_millis(100), true);
        // Window expires; nothing running.
        ladder.press(t0 + Duration::from_secs(1), false);
        // A later press, still nothing running: stays at Nothing, never Hard.
        assert_eq!(ladder.press(t0 + Duration::from_millis(1_100), false), Step::Nothing);
    }
}
