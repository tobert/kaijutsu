//! When a newly shown ask starts taking answer keys.
//!
//! An ask can appear while a player is typing into a draft. A keystroke meant
//! for the draft must not answer it, so every client surface that shows an
//! ask starts it disarmed: its answer keys do nothing until
//! [`ASK_ARM_DELAY`] has passed since it appeared, and each keystroke while
//! it is still disarmed moves arming to [`ASK_ARM_TYPING_HOLD`] after that
//! keystroke. Once armed, it stays armed.
//!
//! This is client presentation only. The kernel does not change an ask's
//! state on input; the ask is pending the whole time.
//!
//! The clock is the caller's: any monotonic stamp that adds a [`Duration`]
//! works — `std::time::Instant` in the terminal client, the elapsed
//! [`Duration`] of Bevy's `Time` in the app — so tests pass explicit stamps.

use std::ops::Add;
use std::time::Duration;

/// How long a newly shown ask stays disarmed when nobody is typing.
pub const ASK_ARM_DELAY: Duration = Duration::from_millis(100);

/// How long after a keystroke a still-disarmed ask waits before it arms.
pub const ASK_ARM_TYPING_HOLD: Duration = Duration::from_millis(200);

/// One shown ask's arming clock. See the module docs for the rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AskArming<T> {
    arms_at: T,
}

impl<T> AskArming<T>
where
    T: Copy + Ord + Add<Duration, Output = T>,
{
    /// An ask that appeared at `now`: disarmed until [`ASK_ARM_DELAY`] later.
    pub fn shown(now: T) -> Self {
        Self { arms_at: now + ASK_ARM_DELAY }
    }

    /// Whether answer keys act on the ask at `now`.
    pub fn armed(&self, now: T) -> bool {
        now >= self.arms_at
    }

    /// A keystroke at `now`. While disarmed it pushes arming out to
    /// [`ASK_ARM_TYPING_HOLD`] after `now`, never earlier than it already
    /// was. Once armed, a keystroke changes nothing.
    pub fn keystroke(&mut self, now: T) {
        if !self.armed(now) {
            self.arms_at = self.arms_at.max(now + ASK_ARM_TYPING_HOLD);
        }
    }

    /// When the ask arms, as things stand.
    pub fn arms_at(&self) -> T {
        self.arms_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn an_ask_arms_one_delay_after_it_appears() {
        let arming = AskArming::shown(ms(0));
        assert!(!arming.armed(ms(0)));
        assert!(!arming.armed(ms(99)));
        assert!(arming.armed(ms(100)));
    }

    #[test]
    fn typing_while_disarmed_pushes_arming_out() {
        let mut arming = AskArming::shown(ms(0));
        arming.keystroke(ms(80));
        assert_eq!(arming.arms_at(), ms(280));
        assert!(!arming.armed(ms(250)), "250 ms is inside the hold");
        assert!(arming.armed(ms(290)));
    }

    #[test]
    fn every_disarmed_keystroke_restarts_the_hold() {
        let mut arming = AskArming::shown(ms(0));
        arming.keystroke(ms(80));
        arming.keystroke(ms(250));
        assert!(!arming.armed(ms(290)), "the key at 250 ms moved arming to 450 ms");
        assert!(arming.armed(ms(450)));
    }

    #[test]
    fn a_keystroke_never_moves_arming_earlier() {
        let mut arming = AskArming::shown(ms(1_000));
        arming.keystroke(ms(0));
        assert_eq!(arming.arms_at(), ms(1_100));
    }

    #[test]
    fn an_armed_ask_stays_armed_through_typing() {
        let mut arming = AskArming::shown(ms(0));
        arming.keystroke(ms(150));
        assert!(arming.armed(ms(150)));
        assert_eq!(arming.arms_at(), ms(100));
    }

    #[test]
    fn instants_work_as_the_clock() {
        let t0 = std::time::Instant::now();
        let mut arming = AskArming::shown(t0);
        arming.keystroke(t0 + ms(80));
        assert!(!arming.armed(t0 + ms(279)));
        assert!(arming.armed(t0 + ms(280)));
    }
}
