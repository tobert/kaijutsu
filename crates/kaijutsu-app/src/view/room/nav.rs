//! Room stations in controller navigation order.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Station {
    TimeWell,
    Switchboard,
}

impl Station {
    pub fn label(self) -> &'static str {
        match self {
            Station::TimeWell => "TIME WELL",
            Station::Switchboard => "SWITCHBOARD",
        }
    }

    /// Carousel order.
    pub const ALL: [Station; 2] = [Station::TimeWell, Station::Switchboard];
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_table() {
        assert_eq!(Station::TimeWell.label(), "TIME WELL");
        assert_eq!(Station::Switchboard.label(), "SWITCHBOARD");
    }

    #[test]
    fn new_finds_initial_in_all() {
        let c = StationCarousel::new(Station::Switchboard);
        assert_eq!(c.focused, 1);
        assert_eq!(c.focused_station(), Station::Switchboard);
    }

    #[test]
    fn new_starts_on_time_well() {
        let c = StationCarousel::new(Station::TimeWell);
        assert_eq!(c.focused, 0);
    }

    #[test]
    fn step_forward_wraps_past_end() {
        let mut c = StationCarousel::new(Station::Switchboard);
        c.step(1);
        assert_eq!(c.focused, 0);
        assert_eq!(c.focused_station(), Station::TimeWell);
    }

    #[test]
    fn step_backward_from_zero_wraps_to_end() {
        let mut c = StationCarousel::new(Station::TimeWell); // index 0
        c.step(-1);
        assert_eq!(c.focused, Station::ALL.len() - 1);
        assert_eq!(c.focused_station(), Station::Switchboard);
    }

    #[test]
    fn step_handles_large_positive_dir() {
        let mut c = StationCarousel::new(Station::TimeWell); // index 0
        c.step(5);
        assert_eq!(c.focused, 1);
    }

    #[test]
    fn step_handles_large_negative_dir() {
        let mut c = StationCarousel::new(Station::TimeWell); // index 0
        c.step(-7);
        assert_eq!(c.focused, 1);
    }

    #[test]
    fn step_sequence_round_trips() {
        let mut c = StationCarousel::new(Station::TimeWell);
        c.step(1);
        c.step(1);
        assert_eq!(c.focused, 0);
        c.step(-1);
        assert_eq!(c.focused, 1);
    }
}
