//! The continuous local timebase — a beat that free-runs and *slews* toward
//! low-rate references (the "good-enough shared hyoushigi", `docs/midi.md` "The
//! relative-lead timebase, analyzed").
//!
//! This COMPOSES alongside the per-cue [`RenderCue`](crate::RenderCue) trigger
//! path, it does not replace it: `RenderCue` owns *sound onset* (fire-and-forget
//! one-shots on the lead); [`LocalBeat`] owns *"where's the beat now"* — a
//! metronome click, a smooth playhead, beat-synced visuals. They are two
//! parallel renderings of the same kernel timeline; divergence between them is
//! *measured* (the metronome slice), never prevented by construction.
//!
//! The phasor never *hard-resyncs*: a fresh reference nudges it (a bounded,
//! slew-limited tempo correction that stays continuous in position), so one late
//! reference can't yank the beat — a little jitter buys resilience. FFI-free and
//! `Instant`-based: a sink drives it from its own local clock, exactly as it
//! anchors `RenderCue.lead` at `receipt + lead`.

use std::time::{Duration, Instant};

/// A low-rate beat reference the kernel ships to sinks: the fractional beat
/// coordinate at the instant of emission, plus the current tempo. Integer beat
/// values are onsets (the clicks). The phasor slews toward it.
///
/// Serializable because it is a wire payload. Unlike `RenderCue.lead` (a
/// relative offset that composes fine at receipt time), a low-rate reference
/// that's been queued behind a flood needs its *emission* instant to fold
/// correctly — so it carries `epoch_ns`, the sender's wallclock at emission
/// (mirrors `reportClockEstimate`'s `epochNs`, `beat.rs` `apply_clock_estimate`).
/// `epoch_ns == 0` means an unstamped reference (an old peer, or a synthetic
/// test `BeatRef`) — [`Self::backdated_at`] falls back to receipt time. A
/// receiver folds many buffered refs against ONE frame `now` by back-dating
/// each to its own emission instant first, rather than anchoring them all at
/// the same receipt instant (which walks the phasor several beats on a flood).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BeatRef {
    /// Fractional beat coordinate at emission. Integer values are beat onsets.
    pub beat: f64,
    /// Tempo in beats per second (120 BPM == `2.0`).
    pub tempo_bps: f64,
    /// Sender wallclock (ns since UNIX_EPOCH) at emission. `0` = unstamped
    /// (old peer or a test-constructed ref) → the sink falls back to receipt
    /// time. `#[serde(default)]` so old wire payloads without this field still
    /// deserialize (additive schema evolution).
    #[serde(default)]
    pub epoch_ns: u64,
}

/// A reference older than this is dropped rather than back-dated — the pipe is
/// backed up badly enough that "when this was true" is no longer useful; the
/// phasor free-runs on its last feedforward tempo instead (mirrors
/// `beat.rs::apply_clock_estimate`'s staleness drop).
pub const REF_STALE_MAX: Duration = Duration::from_secs(5);

/// A reference within this age is fresh enough to FOLD (back-date and hand to
/// [`LocalBeat::observe`] — it corrects phase/tempo). Older than this but
/// still within [`REF_STALE_MAX`] is TOUCH territory: too old to trust its
/// phase (folding it would step the beat toward stale data), but not so old
/// it should be treated as silence — a caller uses it only as a liveness
/// signal (e.g. resetting a "sender's gone quiet" timer). Distinct from
/// `REF_STALE_MAX` on purpose: fold-eligibility is a tighter bound than
/// drop-eligibility (Amy's ask — "trust the local clock, reject stale
/// adjustments harder").
pub const REF_FOLD_MAX: Duration = Duration::from_secs(1);

/// What a receiver should do with a [`BeatRef`], given its age. The
/// three-way split (vs. the old binary fold-or-drop) is the "reject stale
/// adjustments harder" stance: only a genuinely fresh reference gets to move
/// the phasor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RefDisposition {
    /// Fresh enough (age ≤ [`REF_FOLD_MAX`], or unstamped) — fold it into the
    /// phasor via [`LocalBeat::observe`] at the carried back-dated `Instant`.
    Fold(Instant),
    /// Too old to trust for phase (`REF_FOLD_MAX` < age ≤ `REF_STALE_MAX`) but
    /// not stale enough to ignore outright — liveness only, never `observe`.
    Touch,
    /// Older than `REF_STALE_MAX` — discard; the phasor free-runs.
    Drop,
}

/// The shared age computation behind [`BeatRef::backdated_at`] and
/// [`BeatRef::disposition`]: `None` for an unstamped reference (`epoch_ns ==
/// 0`, e.g. an old peer or a synthetic test `BeatRef`), `Some(age)` otherwise.
/// `saturating_sub` floors a future-stamped ref (clock skew, or `now_epoch_ns`
/// sampled a hair before `epoch_ns`) at age zero rather than underflowing.
pub fn stamp_age(epoch_ns: u64, now_epoch_ns: u64) -> Option<Duration> {
    if epoch_ns == 0 {
        return None;
    }
    Some(Duration::from_nanos(now_epoch_ns.saturating_sub(epoch_ns)))
}

impl BeatRef {
    pub fn new(beat: f64, tempo_bps: f64) -> Self {
        Self { beat, tempo_bps, epoch_ns: 0 }
    }

    /// Re-anchor this reference's emission instant into the receiver's local
    /// `Instant` domain: `now` is the receiver's `Instant::now()`, `now_epoch_ns`
    /// its `SystemTime::now()` at (as close as possible to) the same moment.
    ///
    /// - `epoch_ns == 0` (unstamped) → `Some(now)`, the old receipt-time
    ///   behavior.
    /// - Otherwise the age is computed in the u64-ns wallclock domain via
    ///   [`stamp_age`] (a future-stamped ref saturates to age `0` rather than
    ///   underflowing) and only the *final* subtraction crosses into the
    ///   `Instant` domain (mirrors `beat.rs::apply_clock_estimate`'s `at =
    ///   Instant::now() - Duration::from_nanos(age_ns)` — cross-reference, not
    ///   a refactor of it).
    /// - Age `> REF_STALE_MAX` → `None`: the ref is too old to trust: the
    ///   caller should drop it (never fold it) and let the phasor free-run.
    pub fn backdated_at(&self, now: Instant, now_epoch_ns: u64) -> Option<Instant> {
        let Some(age) = stamp_age(self.epoch_ns, now_epoch_ns) else {
            return Some(now);
        };
        if age > REF_STALE_MAX {
            return None;
        }
        // `checked_sub` guards an early-process `now` too close to program
        // start for `age` to subtract from; clamp to `now` rather than panic.
        Some(now.checked_sub(age).unwrap_or(now))
    }

    /// The three-way disposition (see [`RefDisposition`]): fresh/unstamped →
    /// `Fold` at the back-dated instant; `(REF_FOLD_MAX, REF_STALE_MAX]` →
    /// `Touch` (liveness only); beyond `REF_STALE_MAX` → `Drop`.
    pub fn disposition(&self, now: Instant, now_epoch_ns: u64) -> RefDisposition {
        let Some(age) = stamp_age(self.epoch_ns, now_epoch_ns) else {
            return RefDisposition::Fold(now); // unstamped → fold at receipt time
        };
        if age <= REF_FOLD_MAX {
            RefDisposition::Fold(now.checked_sub(age).unwrap_or(now))
        } else if age <= REF_STALE_MAX {
            RefDisposition::Touch
        } else {
            RefDisposition::Drop
        }
    }
}

/// A free-running local beat, corrected toward [`BeatRef`]s as they arrive.
///
/// The controller is **proportional-phase with feedforward tempo** (`docs/issues.md`
/// "Metronome phasor sloshes"). Because a reference carries the *exact* tempo, we
/// run the phasor at that tempo directly (feedforward) — never as a *persistent
/// rate bias*, which is what an earlier slew did and it wound up like an
/// integrator: a rate correction sized for a 1 s window kept driving until the
/// next (seconds-later) reference, overshooting ~3.8× and sloshing. Here the rate
/// is always exactly the reference's, so beats stay evenly spaced by construction;
/// only *phase* is corrected, by a small **fractional step** toward the reference
/// (gain `< 1` → always undershoots → never overshoots), bounded so an outlier
/// reference can't yank the beat. The step is tiny for normal jitter
/// (gain × a few-ms error) and the loop low-pass-filters reference jitter.
///
/// [`position`]: LocalBeat::position
#[derive(Debug, Clone)]
pub struct LocalBeat {
    /// Beat coordinate at `ref_at`.
    ref_beat: f64,
    /// Local clock instant the anchor was taken.
    ref_at: Instant,
    /// Extrapolation rate — always the *reference* tempo (feedforward, no bias).
    tempo_bps: f64,
    /// Proportional phase gain: the fraction of the phase error corrected per
    /// reference. `< 1` so the beat always undershoots the target (no overshoot).
    phase_gain: f64,
    /// Max phase step (beats) a single reference may apply — glitch protection so
    /// an outlier reference nudges the beat only a little, never yanks it.
    max_step: f64,
    /// Phase-error deadband (beats): once dialed in, a reference whose error
    /// falls at-or-inside this band applies NO step — the phasor trusts its own
    /// free-run line over chasing sub-deadband noise (Amy's ask: "once dialed
    /// in, trust the local clock; reject stale adjustments harder"). Tempo is
    /// still adopted even when deadbanded — inside the band the phasor already
    /// IS the local clock, exact feedforward is free and keeps it that way.
    phase_deadband: f64,
    /// EMA-smoothed magnitude of the raw phase error each `observe` sees — the
    /// health signal (mirrors `clockin.rs::ClockEstimator::residual_ns`): near
    /// zero once locked, shrinking across a persistent offset, otherwise a
    /// standing nonzero floor (the tuning loop for `phase_deadband`).
    residual: f64,
}

/// One `observe` call's report — what happened to the phasor, for the
/// telemetry rider (Slice 4) and for tests. Not `#[must_use]`: most callers
/// (the two production consumers, most tests here) only care about the
/// phasor's resulting state and are free to ignore it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slew {
    /// The raw phase error (beats) this reference reported, BEFORE the
    /// deadband/step decision — positive = the phasor was behind.
    pub error_beats: f64,
    /// The step actually applied to `ref_beat`. `0.0` when deadbanded.
    pub step_beats: f64,
    /// Whether `|error_beats|` fell at-or-inside `phase_deadband` (so
    /// `step_beats == 0.0` and position held the free-run line).
    pub deadbanded: bool,
    /// The EMA residual AFTER folding this observation in — see
    /// [`LocalBeat::residual_beats`].
    pub residual_beats: f64,
}

impl LocalBeat {
    /// Default: correct 20% of the phase error per reference, never more than a
    /// one-beat step (startup uses [`new`](Self::new), which locks fully).
    const DEFAULT_PHASE_GAIN: f64 = 0.20;
    const DEFAULT_MAX_STEP: f64 = 1.0;
    /// Default deadband (beats) — ≈10 ms at 120 BPM. Amy-tunable: the phasor
    /// slew metrics (Slice 4) are the empirical loop that dials this in from
    /// what's actually observed on the wire, not a guess held forever.
    const DEFAULT_PHASE_DEADBAND: f64 = 0.02;
    /// EMA weight for the residual health signal. Matches `clockin.rs`'s
    /// scale of "smooth over roughly a handful of observations" rather than
    /// reacting to any single one.
    const RESIDUAL_EMA_ALPHA: f64 = 0.25;

    /// Anchor a fresh phasor on its first reference (an instant lock — startup
    /// and post-gap re-entry snap to the reference; only *ongoing* corrections
    /// are gentle).
    pub fn new(initial: BeatRef, at: Instant) -> Self {
        Self {
            ref_beat: initial.beat,
            ref_at: at,
            tempo_bps: initial.tempo_bps,
            phase_gain: Self::DEFAULT_PHASE_GAIN,
            max_step: Self::DEFAULT_MAX_STEP,
            phase_deadband: Self::DEFAULT_PHASE_DEADBAND,
            residual: 0.0,
        }
    }

    /// Override the controller tuning (phase gain + max step). Chainable.
    pub fn with_tuning(mut self, phase_gain: f64, max_step: f64) -> Self {
        self.phase_gain = phase_gain;
        self.max_step = max_step;
        self
    }

    /// Override the phase-error deadband (beats). Chainable.
    pub fn with_deadband(mut self, phase_deadband: f64) -> Self {
        self.phase_deadband = phase_deadband;
        self
    }

    /// The current extrapolation tempo (beats/sec) — the last reference's tempo.
    pub fn tempo_bps(&self) -> f64 {
        self.tempo_bps
    }

    /// The EMA-smoothed phase-error magnitude — see the [`Slew::residual_beats`]
    /// doc on the struct field this mirrors.
    pub fn residual_beats(&self) -> f64 {
        self.residual
    }

    /// The fractional beat position at local instant `now` (free-run
    /// extrapolation from the anchor). `now` is expected to be at or after the
    /// last anchor; earlier instants clamp to the anchor (no negative dt).
    pub fn position(&self, now: Instant) -> f64 {
        let dt = now.saturating_duration_since(self.ref_at).as_secs_f64();
        self.ref_beat + self.tempo_bps * dt
    }

    /// Ingest a reference received at local instant `at`: adopt its (exact) tempo
    /// as feedforward — **no persistent rate bias** — and nudge *phase* a bounded
    /// fraction of the way toward it, UNLESS the error falls inside the deadband
    /// (then no step: position holds the free-run line, tempo still adopts).
    /// Re-anchoring at `at` keeps future extrapolation exact; the small phase
    /// step (not a rate change) is what locks the beat without sloshing.
    pub fn observe(&mut self, r: BeatRef, at: Instant) -> Slew {
        let current = self.position(at);
        let error = r.beat - current; // beats we're behind (+) or ahead (−)
        let deadbanded = error.abs() <= self.phase_deadband;
        let step = if deadbanded {
            0.0
        } else {
            (self.phase_gain * error).clamp(-self.max_step, self.max_step)
        };

        self.ref_beat = current + step;
        self.ref_at = at;
        self.tempo_bps = r.tempo_bps; // feedforward — the loop cannot wind up
        self.residual = Self::RESIDUAL_EMA_ALPHA * error.abs()
            + (1.0 - Self::RESIDUAL_EMA_ALPHA) * self.residual;

        Slew { error_beats: error, step_beats: step, deadbanded, residual_beats: self.residual }
    }
}

/// The integer beat onsets to click this frame: those strictly after `prev` and
/// at-or-before `cur` (half-open `(prev, cur]`), so a beat fired at a frame
/// boundary is never clicked twice. Expects `cur >= prev`; no forward progress
/// yields nothing.
pub fn beat_onsets_in(prev: f64, cur: f64) -> Vec<i64> {
    // Positive test (not `!(cur > prev)`) so a NaN on either side yields nothing
    // rather than tripping the negated-comparison lint or looping.
    if cur > prev {
        let mut onsets = Vec::new();
        let mut n = prev.floor() as i64 + 1;
        while (n as f64) <= cur {
            onsets.push(n);
            n += 1;
        }
        onsets
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // A phase error this small (in beats) reads as "no audible step".
    const EPS: f64 = 1e-9;

    #[test]
    fn free_runs_at_tempo_between_references() {
        let t0 = Instant::now();
        let beat = LocalBeat::new(BeatRef::new(0.0, 2.0), t0); // 120 BPM
        assert!((beat.position(t0) - 0.0).abs() < EPS);
        assert!((beat.position(t0 + Duration::from_secs_f64(0.5)) - 1.0).abs() < EPS);
        assert!((beat.position(t0 + Duration::from_secs(1)) - 2.0).abs() < EPS);
        assert!((beat.position(t0 + Duration::from_secs(2)) - 4.0).abs() < EPS);
    }

    #[test]
    fn position_before_anchor_clamps_to_anchor() {
        // Monotonic clocks never go backward, but a saturating dt must not panic
        // or extrapolate negative.
        let t0 = Instant::now() + Duration::from_secs(10);
        let beat = LocalBeat::new(BeatRef::new(3.0, 2.0), t0);
        let earlier = t0 - Duration::from_secs(5);
        assert!((beat.position(earlier) - 3.0).abs() < EPS, "clamps to the anchor");
    }

    #[test]
    fn a_consistent_reference_causes_no_correction() {
        let t0 = Instant::now();
        let mut beat = LocalBeat::new(BeatRef::new(0.0, 2.0), t0);
        // One second later the phasor reads exactly 2.0; a reference that agrees
        // must leave both position (continuous) and tempo untouched.
        let t1 = t0 + Duration::from_secs(1);
        beat.observe(BeatRef::new(2.0, 2.0), t1);
        assert!((beat.position(t1) - 2.0).abs() < EPS, "no step at the observe");
        assert!((beat.tempo_bps() - 2.0).abs() < EPS, "no spurious slew");
    }

    #[test]
    fn observe_uses_feedforward_tempo_never_a_rate_bias() {
        // THE anti-slosh invariant: a phase error must NOT bias the tempo. The
        // old slew set tempo = ref_tempo + error/window (a persistent rate bias
        // that wound up and sloshed); the feedforward controller keeps tempo
        // EXACTLY the reference's, whatever the phase error.
        let t0 = Instant::now();
        let mut beat = LocalBeat::new(BeatRef::new(0.0, 2.0), t0);
        let t2 = t0 + Duration::from_secs(2); // phasor reads 4.0
        beat.observe(BeatRef::new(5.5, 2.0), t2); // a big +1.5 phase error
        assert!(
            (beat.tempo_bps() - 2.0).abs() < EPS,
            "tempo stays exactly the reference tempo — no rate bias, got {}",
            beat.tempo_bps()
        );
    }

    #[test]
    fn a_reference_ahead_steps_phase_a_bounded_fraction() {
        let t0 = Instant::now();
        let mut beat = LocalBeat::new(BeatRef::new(0.0, 2.0), t0).with_tuning(0.25, 1.0);
        let t1 = t0 + Duration::from_secs(1); // phasor reads 2.0
        // Reference says 2.4 — 0.4 beat ahead. Gain 0.25 → step +0.1 → 2.1.
        beat.observe(BeatRef::new(2.4, 2.0), t1);
        assert!(
            (beat.position(t1) - 2.1).abs() < EPS,
            "phase nudged a quarter of the way (2.0 → 2.1), got {}",
            beat.position(t1)
        );
        // Undershoots the target (2.1 < 2.4) → never overshoots → cannot slosh.
        assert!(beat.position(t1) < 2.4, "always undershoots the reference");
        assert!((beat.tempo_bps() - 2.0).abs() < EPS, "tempo unchanged (feedforward)");
    }

    #[test]
    fn a_reference_behind_steps_phase_back_keeping_tempo() {
        let t0 = Instant::now();
        let mut beat = LocalBeat::new(BeatRef::new(0.0, 2.0), t0).with_tuning(0.25, 1.0);
        let t1 = t0 + Duration::from_secs(1); // reads 2.0
        beat.observe(BeatRef::new(1.6, 2.0), t1); // 0.4 behind → step −0.1 → 1.9
        assert!((beat.position(t1) - 1.9).abs() < EPS, "phase nudged back to 1.9");
        assert!(beat.position(t1) > 1.6, "undershoots (doesn't overshoot backward)");
        assert!((beat.tempo_bps() - 2.0).abs() < EPS, "tempo unchanged (feedforward)");
    }

    #[test]
    fn tempo_change_is_adopted_directly() {
        let t0 = Instant::now();
        let mut beat = LocalBeat::new(BeatRef::new(0.0, 2.0), t0);
        let t1 = t0 + Duration::from_secs(1); // reads 2.0
        // Consistent phase, new tempo (180 BPM = 3.0 bps) → rate adopts 3.0 at once
        // (feedforward: the reference tempo is trusted, not slewed toward).
        beat.observe(BeatRef::new(2.0, 3.0), t1);
        assert!((beat.tempo_bps() - 3.0).abs() < EPS, "adopted the new tempo");
        assert!(
            (beat.position(t1 + Duration::from_secs(1)) - 5.0).abs() < EPS,
            "extrapolates at 3 bps after the change"
        );
    }

    #[test]
    fn an_outlier_reference_is_step_bounded_not_a_jump() {
        let t0 = Instant::now();
        let mut beat = LocalBeat::new(BeatRef::new(0.0, 2.0), t0); // max_step 1.0
        let t1 = t0 + Duration::from_secs(1); // reads 2.0
        // A wild reference (100 beats ahead — a glitch, not a real section jump).
        // Gain 0.2 × 100 = 20 beats, but the step is capped at max_step (1.0).
        beat.observe(BeatRef::new(102.0, 2.0), t1);
        assert!(
            (beat.position(t1) - 3.0).abs() < EPS,
            "step capped at one beat (2.0 → 3.0), not a 20-beat lurch, got {}",
            beat.position(t1)
        );
        assert!((beat.tempo_bps() - 2.0).abs() < EPS, "tempo still exact");
    }

    #[test]
    fn repeated_consistent_references_stay_locked() {
        // A steady stream of agreeing references (the common case) must not drift
        // or oscillate: tempo stays put and position stays on the line.
        let t0 = Instant::now();
        let mut beat = LocalBeat::new(BeatRef::new(0.0, 2.0), t0);
        for phrase in 1..=8 {
            let at = t0 + Duration::from_secs(phrase);
            beat.observe(BeatRef::new(2.0 * phrase as f64, 2.0), at);
            assert!(
                (beat.tempo_bps() - 2.0).abs() < EPS,
                "no accumulated slew at phrase {phrase}"
            );
            assert!((beat.position(at) - 2.0 * phrase as f64).abs() < 1e-6);
        }
    }

    #[test]
    fn a_persistent_offset_converges_over_successive_references() {
        // If the phasor is consistently a bit behind, each reference should
        // shrink the error — it converges, it doesn't run away.
        let t0 = Instant::now();
        // Start it deliberately slow so it keeps falling behind a 2.0-bps truth.
        let mut beat = LocalBeat::new(BeatRef::new(0.0, 1.8), t0).with_tuning(0.5, 1.0);
        let mut prev_error = f64::INFINITY;
        for phrase in 1..=6 {
            let at = t0 + Duration::from_secs(phrase);
            let truth = 2.0 * phrase as f64;
            let error = (truth - beat.position(at)).abs();
            assert!(
                error <= prev_error + EPS,
                "error grew at phrase {phrase}: {error} > {prev_error}"
            );
            prev_error = error;
            beat.observe(BeatRef::new(truth, 2.0), at);
        }
        assert!(prev_error < 0.05, "converged close, residual {prev_error}");
    }

    // ── Slice 3 (phase-align): the deadband + disposition ladder ────────────

    #[test]
    fn deadband_holds_position_under_small_jitter() {
        let t0 = Instant::now();
        let make = || LocalBeat::new(BeatRef::new(0.0, 2.0), t0).with_tuning(0.25, 1.0);
        let t1 = t0 + Duration::from_secs(1); // free-run reads 2.0

        // +0.015 beats: inside the default 0.02 deadband.
        let mut ahead = make();
        let slew = ahead.observe(BeatRef::new(2.015, 2.0), t1);
        assert!(slew.deadbanded, "0.015 beat error is within the 0.02 deadband");
        assert_eq!(slew.step_beats, 0.0, "no step applied inside the deadband");
        assert!((slew.error_beats - 0.015).abs() < EPS, "raw error still reported");
        assert!((ahead.position(t1) - 2.0).abs() < EPS, "position holds the free-run line");
        assert!((ahead.tempo_bps() - 2.0).abs() < EPS, "tempo still adopted inside the deadband");

        // -0.015 beats: same on the other side of zero.
        let mut behind = make();
        let slew = behind.observe(BeatRef::new(1.985, 2.0), t1);
        assert!(slew.deadbanded, "-0.015 beat error is within the 0.02 deadband");
        assert_eq!(slew.step_beats, 0.0);
        assert!((behind.position(t1) - 2.0).abs() < EPS, "position holds on the other side too");
    }

    #[test]
    fn large_error_still_steps_bounded_beyond_the_deadband() {
        let t0 = Instant::now();
        let mut beat = LocalBeat::new(BeatRef::new(0.0, 2.0), t0).with_tuning(0.25, 1.0);
        let t1 = t0 + Duration::from_secs(1); // reads 2.0
        // A wild reference (100 beats ahead) — well outside the deadband, and
        // the gain × error would be a 20-beat step; the max_step cap still wins.
        let slew = beat.observe(BeatRef::new(102.0, 2.0), t1);
        assert!(!slew.deadbanded, "a 100-beat error is nowhere near the deadband");
        assert!((slew.step_beats - 1.0).abs() < EPS, "step capped at max_step, got {}", slew.step_beats);
        assert!((beat.position(t1) - 3.0).abs() < EPS);
        assert!((beat.tempo_bps() - 2.0).abs() < EPS, "tempo still exact (feedforward)");
    }

    #[test]
    fn disposition_ladder_fold_touch_drop() {
        let now = Instant::now();
        let now_epoch_ns: u64 = 10_000_000_000;

        // 0.5s old (≤ REF_FOLD_MAX) → Fold.
        let fresh = BeatRef { beat: 1.0, tempo_bps: 2.0, epoch_ns: now_epoch_ns - 500_000_000 };
        match fresh.disposition(now, now_epoch_ns) {
            RefDisposition::Fold(at) => assert!(at < now, "backdated into the past"),
            other => panic!("expected Fold for a 0.5s-old ref, got {other:?}"),
        }

        // 2s old (REF_FOLD_MAX, REF_STALE_MAX] → Touch.
        let stale_ish = BeatRef { beat: 1.0, tempo_bps: 2.0, epoch_ns: now_epoch_ns - 2_000_000_000 };
        assert_eq!(stale_ish.disposition(now, now_epoch_ns), RefDisposition::Touch);

        // 6s old (> REF_STALE_MAX) → Drop.
        let ancient = BeatRef { beat: 1.0, tempo_bps: 2.0, epoch_ns: now_epoch_ns - 6_000_000_000 };
        assert_eq!(ancient.disposition(now, now_epoch_ns), RefDisposition::Drop);

        // Unstamped → Fold at receipt time (the old fallback behavior).
        let unstamped = BeatRef::new(1.0, 2.0);
        assert_eq!(unstamped.disposition(now, now_epoch_ns), RefDisposition::Fold(now));
    }

    #[test]
    fn residual_decays_when_locked() {
        // The same persistent-offset scenario as
        // `a_persistent_offset_converges_over_successive_references`: the
        // phasor starts consistently behind (tempo 1.8 vs a 2.0-bps truth) and
        // each reference shrinks the error. The EMA residual has a one-phrase
        // warm-up bump (it starts at 0, so the first couple of samples pull it
        // UP before the shrinking error pulls it back down) — track that it
        // decays monotonically once past that warm-up, and settles small.
        let t0 = Instant::now();
        let mut beat = LocalBeat::new(BeatRef::new(0.0, 1.8), t0).with_tuning(0.5, 1.0);
        let mut residuals = Vec::new();
        for phrase in 1..=8 {
            let at = t0 + Duration::from_secs(phrase);
            let truth = 2.0 * phrase as f64;
            let slew = beat.observe(BeatRef::new(truth, 2.0), at);
            residuals.push(slew.residual_beats);
        }
        // Past the warm-up (index 1, phrase 2 — the EMA's peak), residual must
        // never grow again.
        for w in residuals[1..].windows(2) {
            assert!(
                w[1] <= w[0] + EPS,
                "residual grew after warm-up: {:?} → {}",
                w[0],
                w[1]
            );
        }
        let last = *residuals.last().unwrap();
        assert!(last < 0.05, "residual settles small once locked, got {last}");
    }

    #[test]
    fn beat_onsets_are_half_open_and_never_double_fire() {
        assert_eq!(beat_onsets_in(0.9, 1.2), vec![1]);
        assert_eq!(beat_onsets_in(1.9, 3.1), vec![2, 3]);
        // prev exactly on a beat → that beat already fired last frame, excluded.
        assert_eq!(beat_onsets_in(1.0, 2.0), vec![2]);
        // cur exactly on a beat → included.
        assert_eq!(beat_onsets_in(0.5, 1.0), vec![1]);
        // no forward progress → nothing.
        assert_eq!(beat_onsets_in(1.0, 1.0), Vec::<i64>::new());
        assert_eq!(beat_onsets_in(2.0, 1.5), Vec::<i64>::new());
        // beat zero fires when crossing up through it.
        assert_eq!(beat_onsets_in(-0.5, 0.5), vec![0]);
    }

    #[test]
    fn beat_ref_serde_round_trips() {
        let r = BeatRef { beat: 4.25, tempo_bps: 2.0, epoch_ns: 123_456_789 };
        let json = serde_json::to_string(&r).expect("serialize");
        let back: BeatRef = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, r);
    }

    #[test]
    fn beat_ref_without_epoch_ns_field_deserializes_to_zero() {
        // An old wire payload (pre-epoch_ns peer) has no `epoch_ns` key at all —
        // `#[serde(default)]` must fill it with 0 (unstamped), not fail to parse.
        let json = r#"{"beat":4.25,"tempo_bps":2.0}"#;
        let r: BeatRef = serde_json::from_str(json).expect("old payload still parses");
        assert_eq!(r.epoch_ns, 0);
    }

    // ========================================================================
    // BeatRef::backdated_at
    // ========================================================================

    #[test]
    fn backdated_at_falls_back_to_receipt_for_an_unstamped_ref() {
        let r = BeatRef::new(4.0, 2.0); // epoch_ns == 0
        let now = Instant::now();
        assert_eq!(r.backdated_at(now, 999), Some(now));
    }

    #[test]
    fn backdated_at_backdates_a_recent_stamp_by_its_age() {
        let now = Instant::now();
        let now_epoch_ns: u64 = 10_000_000_000; // arbitrary wallclock instant
        let age_ns = 200_000_000; // 200 ms old
        let r = BeatRef { beat: 1.0, tempo_bps: 2.0, epoch_ns: now_epoch_ns - age_ns };
        let at = r.backdated_at(now, now_epoch_ns).expect("recent, not stale");
        // `at` should read as `now - 200ms`: it must be strictly before `now`
        // and the gap must match the age within a hair (integer ns math).
        assert!(at < now, "backdated instant must be before receipt");
        let observed_age = now.duration_since(at);
        assert!(
            (observed_age.as_nanos() as i128 - age_ns as i128).abs() < 1_000,
            "backdated age should match the stamped age, got {observed_age:?}"
        );
    }

    #[test]
    fn backdated_at_drops_a_stale_reference() {
        let now = Instant::now();
        let now_epoch_ns: u64 = 10_000_000_000;
        // Exactly REF_STALE_MAX + 1ns old → dropped.
        let stale_ns = REF_STALE_MAX.as_nanos() as u64 + 1;
        let r = BeatRef { beat: 1.0, tempo_bps: 2.0, epoch_ns: now_epoch_ns - stale_ns };
        assert_eq!(r.backdated_at(now, now_epoch_ns), None, "older than REF_STALE_MAX drops");
    }

    #[test]
    fn backdated_at_accepts_a_reference_exactly_at_the_stale_boundary() {
        let now = Instant::now();
        let now_epoch_ns: u64 = 10_000_000_000;
        // Exactly REF_STALE_MAX old (not one ns past it) → still accepted.
        let boundary_ns = REF_STALE_MAX.as_nanos() as u64;
        let r = BeatRef { beat: 1.0, tempo_bps: 2.0, epoch_ns: now_epoch_ns - boundary_ns };
        assert!(
            r.backdated_at(now, now_epoch_ns).is_some(),
            "exactly REF_STALE_MAX old is still accepted (boundary is inclusive)"
        );
    }

    #[test]
    fn backdated_at_clamps_a_future_stamp_to_now() {
        // A ref stamped slightly ahead of the receiver's clock (skew, or
        // `now_epoch_ns` sampled a hair before `epoch_ns`) must not underflow
        // into a huge age — `saturating_sub` floors it at age 0 → `now`.
        let now = Instant::now();
        let now_epoch_ns: u64 = 10_000_000_000;
        let r = BeatRef { beat: 1.0, tempo_bps: 2.0, epoch_ns: now_epoch_ns + 5_000_000 };
        assert_eq!(r.backdated_at(now, now_epoch_ns), Some(now), "future stamp clamps to now");
    }

    #[test]
    fn a_flood_of_backdated_refs_folds_to_the_true_position_not_a_receipt_time_walk() {
        // THE pure regression for the burst bug: refs for beats 2/4/6/8 (2.0
        // bps ⇒ one every 8 beats == 4 s apart) arrive all bunched up (a
        // delivery-flood), stamped with their TRUE emission times spread over
        // the past ~3 s, but all *processed* at one receipt instant. Folding
        // each at its own back-dated instant (rather than all at the single
        // receipt `now`) must leave the phasor reading the newest ref's true
        // position, not walk several beats through the whole backlog.
        let now = Instant::now();
        let now_epoch_ns: u64 = 100_000_000_000; // arbitrary wallclock "now"
        let tempo = 2.0;
        // Each successive beat happened `1.0 / tempo` seconds after the last;
        // beat 8 is "now" (fresh), beat 2 happened 3s ago.
        let refs = [
            (2.0, 3_000_000_000u64),
            (4.0, 2_000_000_000u64),
            (6.0, 1_000_000_000u64),
            (8.0, 0u64),
        ];
        let mut beat: Option<LocalBeat> = None;
        for (b, age_ns) in refs {
            let r = BeatRef { beat: b, tempo_bps: tempo, epoch_ns: now_epoch_ns - age_ns };
            let at = r.backdated_at(now, now_epoch_ns).expect("all within staleness window");
            match &mut beat {
                Some(lb) => {
                    lb.observe(r, at);
                }
                None => beat = Some(LocalBeat::new(r, at)),
            }
        }
        let phasor = beat.expect("at least one ref folded");
        // Each ref agreed with the others in its own timeframe (beat 2 three
        // seconds ago, beat 8 now, all at the same 2.0 bps) — so after folding
        // all four, position(now) should read ≈ 8.0, not overshoot past it the
        // way a receipt-time fold (`observe` called at `now` for EVERY ref)
        // would by re-anchoring stale refs' beat values at the current instant.
        assert!(
            (phasor.position(now) - 8.0).abs() < 0.5,
            "flood of back-dated refs settles near beat 8.0, got {}",
            phasor.position(now)
        );
    }
}
