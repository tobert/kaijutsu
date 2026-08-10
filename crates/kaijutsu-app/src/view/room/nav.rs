//! Pure navigation primitives for the room level (`docs/scenes/shell.md`,
//! "Levels — the arrows continue"): the station carousel.
//!
//! No Bevy types here — unit-tested pure logic, same stance as
//! `view/time_well/card.rs`.

/// The stations the room carousel cycles with Left/Right. Order is the
/// carousel order; unbuilt stations ride along as dimmed nameplates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Station {
    TimeWell,
    PatchBay,
    Tracks,
    Vfs,
    /// The south wall's presence-lamp switchboard (`view::room::switchboard`)
    /// — a real station identity (nameplate, marker pylon, gold cap) for what
    /// used to be the grey reserved South stub. Deliberately **not** in
    /// [`built`](Station::built)'s dive-target set: there is no dive scene,
    /// only the ambient wall of lamps at room scale (`station_is_zoomable`'s
    /// table in `room/mod.rs` carries the "no dive" half of this call).
    Switchboard,
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
            Station::Switchboard => "SWITCHBOARD",
            Station::Radiators => "RADIATORS",
        }
    }

    /// Whether a dive target exists yet (well + patch bay + the FSN
    /// landscape, `view::fsn`, `docs/scenes/vfs.md` slice 0 + the tracker
    /// station, `view::tracker`, slice 0 of the plan `snazzy-jumping-hejlsberg.md`).
    /// `Switchboard` is deliberately excluded — it has a nameplate and a
    /// marker pylon (a real station identity) but no dive scene, only the
    /// ambient lamp wall at room scale.
    pub fn built(self) -> bool {
        matches!(self, Station::TimeWell | Station::PatchBay | Station::Tracks | Station::Vfs)
    }

    /// Carousel order.
    pub const ALL: [Station; 6] = [
        Station::TimeWell,
        Station::PatchBay,
        Station::Tracks,
        Station::Vfs,
        Station::Switchboard,
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

// The generic double-tap window that lived here (the retired well-edge
// speedbump's timer) was consolidated with `input/interrupt.rs` and
// `input/vim/dismiss.rs` onto `input/tap.rs::TapCounter` (2026-07-16).

#[cfg(test)]
mod tests {
    use super::*;

    // -- Station::label / Station::built ------------------------------

    #[test]
    fn label_table() {
        assert_eq!(Station::TimeWell.label(), "TIME WELL");
        assert_eq!(Station::PatchBay.label(), "PATCH BAY");
        assert_eq!(Station::Tracks.label(), "TRACKER");
        assert_eq!(Station::Vfs.label(), "DATA HORIZON");
        assert_eq!(Station::Switchboard.label(), "SWITCHBOARD");
        assert_eq!(Station::Radiators.label(), "RADIATORS");
    }

    #[test]
    fn built_table() {
        assert!(Station::TimeWell.built());
        assert!(Station::PatchBay.built());
        assert!(Station::Tracks.built());
        assert!(Station::Vfs.built());
        assert!(!Station::Switchboard.built(), "a real station identity, but no dive scene");
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
        let mut c = StationCarousel::new(Station::Radiators); // index 5, last
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
        // 6 stations: +7 should land on (0 + 7) % 6 == 1.
        c.step(7);
        assert_eq!(c.focused, 1);
    }

    #[test]
    fn step_handles_large_negative_dir() {
        let mut c = StationCarousel::new(Station::TimeWell); // index 0
        // -13 mod 6 (Euclidean) == 5.
        c.step(-13);
        assert_eq!(c.focused, 5);
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
}
