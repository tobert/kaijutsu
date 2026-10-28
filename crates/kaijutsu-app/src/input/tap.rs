//! Shared multi-press window counter — the one primitive under every
//! rapid-repeat gesture (docs/input.md; consolidated 2026-07-16 from three
//! copies: `interrupt.rs`, `vim/dismiss.rs`, and the room's retired
//! `DoubleTap` speedbump type, now deleted).
//!
//! Semantics: `press()` returns the running count, restarting at 1 when the
//! previous press fell outside the window and **saturating at `max`** — it
//! never self-resets; the caller decides when a count consumes the gesture
//! and calls `reset()`. That caller-owned reset is load-bearing: the escape
//! dismiss stays armed at 2 until vim is actually in Normal mode, and the
//! interrupt ladder holds at 3 while the user mashes.

use std::time::Instant;

#[derive(Debug)]
pub struct TapCounter {
    window_ms: u128,
    max: u8,
    count: u8,
    last: Option<Instant>,
}

impl TapCounter {
    pub fn new(window_ms: u128, max: u8) -> Self {
        Self {
            window_ms,
            max,
            count: 0,
            last: None,
        }
    }

    /// Register a press at `now`; returns the running count (1..=max).
    pub fn press_at(&mut self, now: Instant) -> u8 {
        let within_window = self
            .last
            .is_some_and(|last| now.duration_since(last).as_millis() < self.window_ms);
        self.count = if within_window {
            (self.count + 1).min(self.max)
        } else {
            1
        };
        self.last = Some(now);
        self.count
    }

    /// Convenience: `press_at(Instant::now())`.
    pub fn press(&mut self) -> u8 {
        self.press_at(Instant::now())
    }

    /// Clear the count and timestamp. The caller owns gesture consumption.
    pub fn reset(&mut self) {
        self.count = 0;
        self.last = None;
    }

    #[cfg(test)]
    pub fn count(&self) -> u8 {
        self.count
    }

    /// Test helper: backdate the last press so the next one falls outside
    /// the window.
    #[cfg(test)]
    pub fn backdate(&mut self, by_ms: u64) {
        if let Some(last) = self.last {
            self.last = Some(last - std::time::Duration::from_millis(by_ms));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_press_is_one() {
        let mut t = TapCounter::new(500, 3);
        assert_eq!(t.press(), 1);
    }

    #[test]
    fn rapid_presses_count_up_and_saturate() {
        let mut t = TapCounter::new(500, 3);
        assert_eq!(t.press(), 1);
        assert_eq!(t.press(), 2);
        assert_eq!(t.press(), 3);
        // Saturates — never wraps, never self-resets.
        assert_eq!(t.press(), 3);
    }

    #[test]
    fn saturation_respects_max_two() {
        let mut t = TapCounter::new(500, 2);
        t.press();
        t.press();
        assert_eq!(t.press(), 2);
    }

    #[test]
    fn window_expiry_restarts_at_one() {
        let mut t = TapCounter::new(500, 3);
        t.press();
        t.backdate(600);
        assert_eq!(t.press(), 1);
    }

    #[test]
    fn reset_clears() {
        let mut t = TapCounter::new(500, 3);
        t.press();
        t.press();
        t.reset();
        assert_eq!(t.count(), 0);
        assert_eq!(t.press(), 1);
    }

    #[test]
    fn press_at_is_deterministic() {
        let mut t = TapCounter::new(500, 2);
        let base = Instant::now();
        assert_eq!(t.press_at(base), 1);
        assert_eq!(t.press_at(base + std::time::Duration::from_millis(100)), 2);
        // Outside the window from the SECOND press.
        assert_eq!(t.press_at(base + std::time::Duration::from_millis(700)), 1);
    }
}
