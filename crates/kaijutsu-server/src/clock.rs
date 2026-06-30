//! Clock sources — what drives a track's beat (docs/tracks.md, Stage 3).
//!
//! The firing schedule (the `BeatScheduler` heap + `run()`'s `sleep_until_opt`
//! arm) decides *when* a track beats. Stage 3 puts that decision behind a
//! [`ClockSource`]: the kernel always runs a tight LOCAL clock, generative and
//! heap-schedulable. A clock you don't own (MIDI) never streams realtime pulses
//! into the scheduler — an edge node models its tempo/phase/drift and ships
//! low-rate *estimates* over RPC, and the kernel's modeled clock phase-locks to
//! that estimate (docs/midi.md, "distribute tempo, not pulses").
//!
//! This stage ships [`SystemClock`] only (midi.md M1). The trait is shaped now
//! so M3's drift-modeled clock slots in without rework — but `ClockEstimate` and
//! the estimate-injection hook are NOT defined here yet (no dead-code theater;
//! they land with their producer at M3, per the Stage 3 review). [`ModeledClock`]
//! is an uninhabited placeholder: the variant exists in [`ClockSourceKind`] so
//! `clock_kind` persistence and the dispatch shape are real, but you cannot
//! construct one until M3 gives it a body.

use std::time::Duration;

use tokio::time::Instant;

/// What drives a track's beat — a *proxy* for a clock that may be remote and
/// drift-modeled (docs/midi.md). The kernel always runs a tight LOCAL clock; a
/// source answers "when is the next beat?" locally. We dispatch through the
/// [`ClockSourceKind`] enum rather than `Box<dyn ClockSource>` (Stage 3 review:
/// `TrackState` already isn't `Clone`/`Debug`, two known variants, hot loop), so
/// this trait documents the contract each variant fulfils.
pub trait ClockSource {
    /// The local instant of the next beat after `last`, consulting `now`.
    /// Heap-schedulable (generative). `last` is the `Instant` previously
    /// *returned* by `next_fire` (the scheduler's *scheduled* fire time), NOT
    /// the jittery `now` the heap popped at — a modeled clock measures residual
    /// drift against it apples-to-apples. SystemClock returns `now + period`,
    /// byte-for-byte today's re-push (absorbs scheduler jitter into the period).
    fn next_fire(&mut self, last: Instant, now: Instant) -> Instant;

    /// The current effective beat period — drives the BPM readback in
    /// `transport_vars`, the persisted tempo, and the derived speculation
    /// `TickClock`. Fixed for SystemClock; the modeled estimate for a drift clock.
    fn period(&self) -> Duration;

    /// Human/transport tempo override (`kj transport tempo`). SystemClock sets
    /// its period. A modeled clock (M3) MAY honour it as a manual nudge or ignore
    /// it while slaved. Slaving the new period down to the track's armed
    /// `Timeline` (the TickClock-desync fix) is the *caller's* job, not the
    /// clock's — see `BeatScheduler::set_tempo`.
    fn set_period(&mut self, period: Duration);
}

/// The wall-period system clock: one local timer at a fixed tempo. This is what
/// `beat.rs` already did before Stage 3 (`now + period`), now behind the trait.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    period: Duration,
}

impl SystemClock {
    pub fn new(period: Duration) -> Self {
        Self { period }
    }
}

impl ClockSource for SystemClock {
    fn next_fire(&mut self, _last: Instant, now: Instant) -> Instant {
        // `now + period`: preserves the pre-Stage-3 firing schedule exactly. The
        // system clock spaces beats off the actual pop time, folding scheduler
        // jitter into the period (decided, Stage 3 lock open-question 2).
        now + self.period
    }

    fn period(&self) -> Duration {
        self.period
    }

    fn set_period(&mut self, period: Duration) {
        self.period = period;
    }
}

/// The drift-modeled clock — **M3, not built yet.** Uninhabited on purpose: the
/// [`ClockSourceKind::Modeled`] variant is real (so `clock_kind` persistence and
/// the dispatch `match` are honest), but holding an uninhabited type makes the
/// variant unconstructable until M3 gives this a body — "unconstructable until
/// M3" enforced by the type system, not a runtime panic.
#[derive(Debug, Clone, Copy)]
pub enum ModeledClock {}

impl ClockSource for ModeledClock {
    fn next_fire(&mut self, _last: Instant, _now: Instant) -> Instant {
        match *self {}
    }
    fn period(&self) -> Duration {
        match *self {}
    }
    fn set_period(&mut self, _period: Duration) {
        match *self {}
    }
}

/// A track's clock source. Enum, not `Box<dyn>` (Stage 3 review): keeps
/// `TrackState` allocation-free in the hot loop and maps 1:1 to the `clock_kind`
/// persisted column. `Modeled` is unconstructable until M3.
#[derive(Debug, Clone, Copy)]
pub enum ClockSourceKind {
    System(SystemClock),
    Modeled(ModeledClock),
}

impl ClockSourceKind {
    /// Seed a fresh system clock at `period` — the only kind constructable in M1.
    pub fn system(period: Duration) -> Self {
        Self::System(SystemClock::new(period))
    }

    /// Reconstruct a persisted clock source from its `clock_kind` discriminator
    /// (the `tracks.clock_kind` column) and the persisted period. M1 can build
    /// only `"system"`; a `"modeled"` row names a driver this stage cannot
    /// construct ([`ModeledClock`] is uninhabited until M3), so we **crash loud**
    /// rather than silently downgrade the track's clock to the system one
    /// (CLAUDE.md: crash over corruption; "silent fallbacks are often a mistake").
    pub fn from_persisted(kind: &str, period: Duration) -> Result<Self, String> {
        match kind {
            "system" => Ok(Self::system(period)),
            other => Err(format!(
                "track clock_kind {other:?} is not constructable in M1 (only \
                 'system'); refusing to silently downgrade the clock"
            )),
        }
    }

    /// The persisted discriminator (`clock_kind` column). MUTABLE over a track's
    /// life — a track sketched on the system clock can later be slaved to MIDI
    /// (Stage 3 lock; unlike set-once `score_context_id`).
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::System(_) => "system",
            Self::Modeled(_) => "modeled",
        }
    }
}

impl ClockSource for ClockSourceKind {
    fn next_fire(&mut self, last: Instant, now: Instant) -> Instant {
        match self {
            Self::System(c) => c.next_fire(last, now),
            Self::Modeled(c) => c.next_fire(last, now),
        }
    }

    fn period(&self) -> Duration {
        match self {
            Self::System(c) => c.period(),
            Self::Modeled(c) => c.period(),
        }
    }

    fn set_period(&mut self, period: Duration) {
        match self {
            Self::System(c) => c.set_period(period),
            Self::Modeled(c) => c.set_period(period),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn system_clock_next_fire_is_now_plus_period() {
        // The behaviour-preserving contract: SystemClock fires exactly one period
        // after `now`, independent of `last` (today's `now + period` re-push).
        let period = Duration::from_millis(500);
        let mut clock = SystemClock::new(period);
        let now = Instant::now();
        let last = now - Duration::from_secs(10); // deliberately stale `last`
        assert_eq!(clock.next_fire(last, now), now + period);
        assert_eq!(clock.period(), period);
    }

    #[tokio::test]
    async fn set_period_changes_the_firing_interval_and_readback() {
        let mut clock = SystemClock::new(Duration::from_millis(500));
        clock.set_period(Duration::from_millis(250));
        assert_eq!(clock.period(), Duration::from_millis(250));
        let now = Instant::now();
        assert_eq!(clock.next_fire(now, now), now + Duration::from_millis(250));
    }

    #[tokio::test]
    async fn enum_dispatches_to_the_system_variant() {
        let mut clock = ClockSourceKind::system(Duration::from_millis(500));
        assert_eq!(clock.kind_str(), "system");
        assert_eq!(clock.period(), Duration::from_millis(500));
        let now = Instant::now();
        assert_eq!(clock.next_fire(now, now), now + Duration::from_millis(500));
        clock.set_period(Duration::from_secs(1));
        assert_eq!(clock.period(), Duration::from_secs(1));
    }
}
