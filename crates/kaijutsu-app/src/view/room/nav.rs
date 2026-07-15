//! Pure navigation primitives for the room level (`docs/scenes/shell.md`,
//! "Levels — the arrows continue"): the station carousel and the generic
//! double-tap used as the well-edge speedbump.
//!
//! No Bevy types here — unit-tested pure logic, same stance as
//! `view/time_well/card.rs`.

use std::time::Instant;

/// The stations the room carousel cycles with Left/Right. Order is the
/// carousel order; unbuilt stations ride along as dimmed nameplates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Station {
    TimeWell,
    PatchBay,
    Tracks,
    Vfs,
    Radiators,
}

impl Station {
    /// Engraved-nameplate label (blockout wording; naming candidates like
    /// "RHYTHM GATE" / "DATA HORIZON" are recorded in shell.md, undecided).
    pub fn label(self) -> &'static str {
        match self {
            Station::TimeWell => "TIME WELL",
            Station::PatchBay => "PATCH BAY",
            Station::Tracks => "TRACKER",
            Station::Vfs => "DATA HORIZON",
            Station::Radiators => "RADIATORS",
        }
    }

    /// Whether a dive target exists yet (well + patch bay + the FSN
    /// landscape, `view::fsn`, `docs/scenes/vfs.md` slice 0 + the tracker
    /// station, `view::tracker`, slice 0 of the plan `snazzy-jumping-hejlsberg.md`).
    pub fn built(self) -> bool {
        matches!(self, Station::TimeWell | Station::PatchBay | Station::Tracks | Station::Vfs)
    }

    /// Carousel order.
    pub const ALL: [Station; 5] = [
        Station::TimeWell,
        Station::PatchBay,
        Station::Tracks,
        Station::Vfs,
        Station::Radiators,
    ];
}

/// Left/Right station focus with wrap-around.
#[derive(Debug, Clone)]
pub struct StationCarousel {
    pub focused: usize,
}

impl StationCarousel {
    /// Start focused on `initial` (falls back to index 0 if absent).
    pub fn new(initial: Station) -> Self {
        let focused = Station::ALL.iter().position(|&s| s == initial).unwrap_or(0);
        Self { focused }
    }

    /// Step focus by `dir` (+1 right / -1 left), wrapping.
    pub fn step(&mut self, dir: i32) {
        let len = Station::ALL.len() as i32;
        let next = (self.focused as i32 + dir).rem_euclid(len);
        self.focused = next as usize;
    }

    pub fn focused_station(&self) -> Station {
        Station::ALL[self.focused]
    }
}

/// Generic double-tap window (the third copy of this shape in the app —
/// `input/interrupt.rs` and `input/vim/dismiss.rs` predate it; folding all
/// three onto this one is recorded in docs/issues.md, not done here).
///
/// `#[allow(dead_code)]`: unused as of the time-well/room integration plan's
/// Slice C, which retired this type's one consumer (`room::WellEdgeBump` —
/// the well's old Up-Up speedbump, gone now that leaving the well is just
/// `room.zoomed = None`). Kept rather than deleted: it's the still-open
/// consolidation target `docs/issues.md` already points `interrupt.rs`/
/// `dismiss.rs` at, not dead weight from an abandoned idea.
#[allow(dead_code)]
#[derive(Debug)]
pub struct DoubleTap {
    window_ms: u128,
    count: u8,
    last: Option<Instant>,
}

#[allow(dead_code)] // see the struct's own doc
impl DoubleTap {
    pub fn new(window_ms: u128) -> Self {
        Self {
            window_ms,
            count: 0,
            last: None,
        }
    }

    /// Register a press at `now`; returns the running count (1 = armed,
    /// 2 = fired). A press outside the window restarts at 1. Saturates at 2
    /// and self-resets when it fires.
    pub fn press_at(&mut self, now: Instant) -> u8 {
        let within_window = self
            .last
            .is_some_and(|last| now.duration_since(last).as_millis() < self.window_ms);
        self.count = if within_window {
            (self.count + 1).min(2)
        } else {
            1
        };
        self.last = Some(now);

        if self.count >= 2 {
            self.reset();
            2
        } else {
            self.count
        }
    }

    /// Convenience: `press_at(Instant::now())`.
    pub fn press(&mut self) -> u8 {
        self.press_at(Instant::now())
    }

    pub fn reset(&mut self) {
        self.count = 0;
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // -- Station::label / Station::built ------------------------------

    #[test]
    fn label_table() {
        assert_eq!(Station::TimeWell.label(), "TIME WELL");
        assert_eq!(Station::PatchBay.label(), "PATCH BAY");
        assert_eq!(Station::Tracks.label(), "TRACKER");
        assert_eq!(Station::Vfs.label(), "DATA HORIZON");
        assert_eq!(Station::Radiators.label(), "RADIATORS");
    }

    #[test]
    fn built_table() {
        assert!(Station::TimeWell.built());
        assert!(Station::PatchBay.built());
        assert!(Station::Tracks.built());
        assert!(Station::Vfs.built());
        assert!(!Station::Radiators.built());
    }

    // -- StationCarousel -------------------------------------------------

    #[test]
    fn new_finds_initial_in_all() {
        let c = StationCarousel::new(Station::Vfs);
        assert_eq!(c.focused, 3);
        assert_eq!(c.focused_station(), Station::Vfs);
    }

    #[test]
    fn new_starts_on_time_well() {
        let c = StationCarousel::new(Station::TimeWell);
        assert_eq!(c.focused, 0);
    }

    #[test]
    fn step_forward_wraps_past_end() {
        let mut c = StationCarousel::new(Station::Radiators); // index 4, last
        c.step(1);
        assert_eq!(c.focused, 0);
        assert_eq!(c.focused_station(), Station::TimeWell);
    }

    #[test]
    fn step_backward_from_zero_wraps_to_end() {
        let mut c = StationCarousel::new(Station::TimeWell); // index 0
        c.step(-1);
        assert_eq!(c.focused, Station::ALL.len() - 1);
        assert_eq!(c.focused_station(), Station::Radiators);
    }

    #[test]
    fn step_handles_large_positive_dir() {
        let mut c = StationCarousel::new(Station::TimeWell); // index 0
        // 5 stations: +7 should land on (0 + 7) % 5 == 2.
        c.step(7);
        assert_eq!(c.focused, 2);
    }

    #[test]
    fn step_handles_large_negative_dir() {
        let mut c = StationCarousel::new(Station::TimeWell); // index 0
        // -12 mod 5 (Euclidean) == 3.
        c.step(-12);
        assert_eq!(c.focused, 3);
    }

    #[test]
    fn step_sequence_round_trips() {
        let mut c = StationCarousel::new(Station::TimeWell);
        c.step(1);
        c.step(1);
        c.step(1);
        assert_eq!(c.focused, 3);
        c.step(-3);
        assert_eq!(c.focused, 0);
    }

    // -- DoubleTap ---------------------------------------------------------

    #[test]
    fn first_press_arms() {
        let mut d = DoubleTap::new(500);
        let t0 = Instant::now();
        assert_eq!(d.press_at(t0), 1);
    }

    #[test]
    fn second_press_within_window_fires() {
        let mut d = DoubleTap::new(500);
        let t0 = Instant::now();
        assert_eq!(d.press_at(t0), 1);
        assert_eq!(d.press_at(t0 + Duration::from_millis(100)), 2);
    }

    #[test]
    fn firing_self_resets_so_third_press_is_one() {
        let mut d = DoubleTap::new(500);
        let t0 = Instant::now();
        assert_eq!(d.press_at(t0), 1);
        assert_eq!(d.press_at(t0 + Duration::from_millis(100)), 2);
        // The gesture already fired and reset itself; the next press,
        // even immediately after, starts a fresh arm at 1.
        assert_eq!(d.press_at(t0 + Duration::from_millis(150)), 1);
    }

    #[test]
    fn press_outside_window_restarts_at_one() {
        let mut d = DoubleTap::new(500);
        let t0 = Instant::now();
        assert_eq!(d.press_at(t0), 1);
        // Well past the 500ms window.
        assert_eq!(d.press_at(t0 + Duration::from_millis(600)), 1);
    }

    #[test]
    fn press_exactly_at_window_boundary_restarts() {
        let mut d = DoubleTap::new(500);
        let t0 = Instant::now();
        assert_eq!(d.press_at(t0), 1);
        // duration_since == window_ms is NOT "< window_ms", so this counts
        // as a late (non-firing) press.
        assert_eq!(d.press_at(t0 + Duration::from_millis(500)), 1);
    }

    #[test]
    fn reset_clears_state() {
        let mut d = DoubleTap::new(500);
        let t0 = Instant::now();
        d.press_at(t0);
        d.reset();
        // A subsequent press is a fresh arm, not a fire, even though it's
        // "close" to the pre-reset press.
        assert_eq!(d.press_at(t0 + Duration::from_millis(10)), 1);
    }

    #[test]
    fn multiple_double_tap_cycles() {
        let mut d = DoubleTap::new(500);
        let t0 = Instant::now();
        assert_eq!(d.press_at(t0), 1);
        assert_eq!(d.press_at(t0 + Duration::from_millis(50)), 2);
        assert_eq!(d.press_at(t0 + Duration::from_millis(700)), 1);
        assert_eq!(d.press_at(t0 + Duration::from_millis(750)), 2);
    }
}
