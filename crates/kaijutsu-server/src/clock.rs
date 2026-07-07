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

/// Max tempo move per applied estimate (fraction of the current period). A
/// bad estimate can nudge, never yank — the tempo-step slew limit from the
/// PLL failure-mode list (docs/midi.md "relative-lead timebase, analyzed").
const TEMPO_STEP_LIMIT: f64 = 0.05;

/// Max phase correction per applied estimate, in beats. Small errors slew
/// (phase-slew-not-step); an error beyond [`PHASE_SEEK_THRESHOLD`] is a
/// musical seek (Start / Song Position) and re-anchors outright.
const PHASE_SLEW_LIMIT: f64 = 0.05;
const PHASE_SEEK_THRESHOLD: f64 = 0.5;

/// References older than this mean the observer went quiet (unplugged,
/// stopped app): the clock free-runs at its last tempo — bounded drift is
/// the design — but says so once (starvation, PLL guard list).
const STARVATION: Duration = Duration::from_secs(10);

/// The drift-modeled clock — M3's body (built 2026-07-06; the variant sat
/// uninhabited from Stage 3 until its producer existed). Phase-locks the
/// track's beat to an observed external master via low-rate
/// `{beat, tempo}` references from the edge's estimator
/// (`kaijutsu_audio::clockin`), per "distribute tempo, not pulses": the
/// kernel never sees a pulse, only the model.
///
/// Until the first reference arrives it free-runs exactly like
/// [`SystemClock`] (a `"modeled"` track is immediately usable; slaving
/// begins when the observer speaks). With an anchor, `next_fire` returns
/// the wall instant of the next *integer master beat* — the track's grid
/// lands on the master's grid, not merely at its tempo.
#[derive(Debug, Clone, Copy)]
pub struct ModeledClock {
    /// Effective period (readback, persistence, speculation TickClock).
    period: Duration,
    /// The phase anchor: master beat `beat` was true at local `at`.
    anchor: Option<ModelAnchor>,
    /// Latched once per starvation episode so the warn isn't per-beat spam.
    warned_starving: bool,
}

#[derive(Debug, Clone, Copy)]
struct ModelAnchor {
    at: Instant,
    beat: f64,
    /// When the last reference was applied (starvation detection).
    applied_at: Instant,
}

impl ModeledClock {
    /// Free-running at `period` until the first reference anchors it.
    pub fn new(period: Duration) -> Self {
        Self { period, anchor: None, warned_starving: false }
    }

    /// The master's beat coordinate at local instant `t` (anchor + tempo
    /// extrapolation).
    fn beat_at(&self, anchor: &ModelAnchor, t: Instant) -> f64 {
        let tempo_bps = 1.0 / self.period.as_secs_f64();
        if t >= anchor.at {
            anchor.beat + (t - anchor.at).as_secs_f64() * tempo_bps
        } else {
            anchor.beat - (anchor.at - t).as_secs_f64() * tempo_bps
        }
    }

    /// Fold one observed reference in: master beat `beat` (with tempo
    /// `tempo_bps`) was true at local instant `at`. Tempo moves at most
    /// [`TEMPO_STEP_LIMIT`] per call; phase slews within
    /// [`PHASE_SLEW_LIMIT`] beats unless the error is a seek
    /// (≥ [`PHASE_SEEK_THRESHOLD`]), which re-anchors outright.
    pub fn apply_estimate(&mut self, beat: f64, tempo_bps: f64, at: Instant) {
        if !(tempo_bps.is_finite() && tempo_bps > 0.0) {
            log::warn!("modeled clock: ignoring non-positive tempo estimate {tempo_bps}");
            return;
        }
        // Tempo, slew-limited.
        let target = 1.0 / tempo_bps;
        let current = self.period.as_secs_f64();
        let step = (target - current).clamp(-current * TEMPO_STEP_LIMIT, current * TEMPO_STEP_LIMIT);
        self.period = Duration::from_secs_f64(current + step);

        // Phase: slew toward the reference, or step on a seek / first anchor.
        let anchored_beat = match self.anchor {
            None => beat,
            Some(a) => {
                let predicted = self.beat_at(&a, at);
                let err = beat - predicted;
                if err.abs() >= PHASE_SEEK_THRESHOLD {
                    beat // a musical seek: land on it, don't crawl to it
                } else {
                    predicted + err.clamp(-PHASE_SLEW_LIMIT, PHASE_SLEW_LIMIT)
                }
            }
        };
        self.anchor = Some(ModelAnchor { at, beat: anchored_beat, applied_at: at });
        self.warned_starving = false;
    }

    /// True when references have gone quiet past [`STARVATION`].
    pub fn starving(&self, now: Instant) -> bool {
        self.anchor
            .map(|a| now.saturating_duration_since(a.applied_at) > STARVATION)
            .unwrap_or(false)
    }
}

impl ClockSource for ModeledClock {
    fn next_fire(&mut self, last: Instant, now: Instant) -> Instant {
        let Some(anchor) = self.anchor else {
            // No reference yet: free-run like SystemClock.
            return now + self.period;
        };
        if self.starving(now) && !self.warned_starving {
            log::warn!(
                "modeled clock: no reference for >{STARVATION:?}; free-running at the \
                 last tempo ({:?}/beat)",
                self.period
            );
            self.warned_starving = true;
        }
        // The next integer master beat strictly after max(last, now): `last`
        // keeps a slewed grid from double-firing the same beat; `now` keeps
        // us from scheduling into the past.
        let from = if last > now { last } else { now };
        let beat_now = self.beat_at(&anchor, from);
        let mut next_beat = beat_now.floor() + 1.0;
        let tempo_bps = 1.0 / self.period.as_secs_f64();
        let mut fire = anchor.at + Duration::from_secs_f64(((next_beat - anchor.beat) / tempo_bps).max(0.0));
        // Guard float rounding right at a boundary.
        while fire <= from {
            next_beat += 1.0;
            fire = anchor.at + Duration::from_secs_f64(((next_beat - anchor.beat) / tempo_bps).max(0.0));
        }
        fire
    }

    fn period(&self) -> Duration {
        self.period
    }

    /// `kj transport tempo` while slaved: honored as a manual nudge — the
    /// next applied reference re-corrects (slew-limited). Honest, not silent:
    /// the caller's tempo takes effect immediately and the master then wins.
    fn set_period(&mut self, period: Duration) {
        self.period = period;
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

    /// Seed a fresh modeled clock at `period` (free-running until the first
    /// reference; see [`ModeledClock`]).
    pub fn modeled(period: Duration) -> Self {
        Self::Modeled(ModeledClock::new(period))
    }

    /// Reconstruct a persisted clock source from its `clock_kind` discriminator
    /// (the `tracks.clock_kind` column) and the persisted period. A `"modeled"`
    /// row restarts free-running at the persisted tempo and re-locks when its
    /// observer speaks again (the anchor is process-local and cannot persist).
    /// An unknown kind still **crashes loud** rather than silently downgrading
    /// the track's clock (CLAUDE.md: silent fallbacks are often a mistake).
    pub fn from_persisted(kind: &str, period: Duration) -> Result<Self, String> {
        match kind {
            "system" => Ok(Self::system(period)),
            "modeled" => Ok(Self::modeled(period)),
            other => Err(format!(
                "unknown track clock_kind {other:?} (this build knows 'system' \
                 and 'modeled'); refusing to silently downgrade the clock"
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
    async fn modeled_free_runs_like_system_until_anchored() {
        let period = Duration::from_millis(500);
        let mut clock = ModeledClock::new(period);
        let now = Instant::now();
        assert_eq!(clock.next_fire(now, now), now + period, "unanchored == system");
        assert_eq!(clock.period(), period);
        assert!(!clock.starving(now), "no anchor, no starvation");
    }

    #[tokio::test]
    async fn modeled_fires_on_the_masters_integer_beats() {
        // Anchor: master beat 4.25 at t0, 2 bps (120 BPM). The next integer
        // beats are 5, 6, … at t0 + 0.375 s, t0 + 0.875 s, …
        let mut clock = ModeledClock::new(Duration::from_millis(500));
        let t0 = Instant::now();
        clock.apply_estimate(4.25, 2.0, t0);
        let fire1 = clock.next_fire(t0, t0);
        assert_eq!(fire1, t0 + Duration::from_millis(375), "lands ON the grid");
        // With `last` at the previous fire, the next is one full beat on.
        let fire2 = clock.next_fire(fire1, fire1);
        assert_eq!(fire2, t0 + Duration::from_millis(875), "no double-fire");
    }

    #[tokio::test]
    async fn modeled_tempo_moves_at_most_the_slew_limit_per_estimate() {
        let mut clock = ModeledClock::new(Duration::from_millis(500));
        let t0 = Instant::now();
        // A wild estimate: 4 bps (double time). One step may only move 5%.
        clock.apply_estimate(0.0, 4.0, t0);
        let p = clock.period().as_secs_f64();
        assert!((p - 0.475).abs() < 1e-9, "500 ms −5% = 475 ms, got {p}");
        // Repeated estimates keep converging (never yank).
        clock.apply_estimate(0.0, 4.0, t0 + Duration::from_millis(500));
        assert!(clock.period().as_secs_f64() < 0.475);
    }

    #[tokio::test]
    async fn modeled_phase_slews_small_errors_and_steps_seeks() {
        let mut clock = ModeledClock::new(Duration::from_millis(500));
        let t0 = Instant::now();
        clock.apply_estimate(0.0, 2.0, t0);
        // Half a period later the model predicts beat 1.0 at t0+500ms; a
        // reference saying 1.1 (0.1 beat ahead) may only pull 0.05.
        let t1 = t0 + Duration::from_millis(500);
        clock.apply_estimate(1.1, 2.0, t1);
        let fire = clock.next_fire(t1, t1);
        // Anchored at ~1.05: next integer beat 2 is 0.95 beats out ≈ 475 ms.
        let expect = t1 + Duration::from_millis(475);
        let delta = if fire > expect { fire - expect } else { expect - fire };
        assert!(delta < Duration::from_millis(2), "slewed, not stepped: {delta:?}");

        // A seek (error ≥ 0.5 beat) re-anchors outright.
        let t2 = t1 + Duration::from_millis(500);
        clock.apply_estimate(32.0, 2.0, t2);
        let fire = clock.next_fire(t2, t2);
        assert_eq!(fire, t2 + Duration::from_millis(500), "beat 33 is one beat out");
    }

    #[tokio::test]
    async fn modeled_starves_loudly_but_keeps_free_running() {
        let mut clock = ModeledClock::new(Duration::from_millis(500));
        let t0 = Instant::now();
        clock.apply_estimate(0.0, 2.0, t0);
        assert!(!clock.starving(t0 + Duration::from_secs(5)));
        let late = t0 + Duration::from_secs(30);
        assert!(clock.starving(late), "silent observer detected");
        // Still generative: fires on the extrapolated grid (beat 60 next).
        let fire = clock.next_fire(late, late);
        assert!(fire > late && fire <= late + Duration::from_millis(500));
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
