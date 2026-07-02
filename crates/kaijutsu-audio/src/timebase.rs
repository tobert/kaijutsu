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
/// Serializable because it is a wire payload — and like `RenderCue.lead`, it
/// carries no absolute `Instant` (a process-local one can't cross the wire); the
/// sink stamps *receipt* against its own clock when it arrives.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BeatRef {
    /// Fractional beat coordinate at emission. Integer values are beat onsets.
    pub beat: f64,
    /// Tempo in beats per second (120 BPM == `2.0`).
    pub tempo_bps: f64,
}

impl BeatRef {
    pub fn new(beat: f64, tempo_bps: f64) -> Self {
        Self { beat, tempo_bps }
    }
}

/// A free-running local beat, corrected toward [`BeatRef`]s as they arrive.
///
/// Between references it extrapolates linearly at the current effective tempo.
/// On a reference it re-anchors *continuously* (no jump in [`position`]) and
/// folds the phase error into a **bounded** tempo correction that absorbs it
/// over `correction_window` — so the beat converges smoothly instead of
/// stepping, and an outlier reference moves it only by the slew cap.
///
/// [`position`]: LocalBeat::position
#[derive(Debug, Clone)]
pub struct LocalBeat {
    /// Beat coordinate at `ref_at` (kept continuous across corrections).
    ref_beat: f64,
    /// Local clock instant the anchor was taken.
    ref_at: Instant,
    /// Effective extrapolation rate = last reference tempo + slew correction.
    tempo_bps: f64,
    /// How long a phase error is spread over when correcting.
    correction_window: Duration,
    /// Slew cap as a fraction of the reference tempo — the max speed-up/slow-down
    /// a single correction may apply, so an outlier can't yank the beat.
    max_slew_fraction: f64,
}

impl LocalBeat {
    /// Default: absorb a phase error over ~1 s, never more than a 10% tempo nudge.
    const DEFAULT_CORRECTION_WINDOW: Duration = Duration::from_secs(1);
    const DEFAULT_MAX_SLEW_FRACTION: f64 = 0.10;

    /// Anchor a fresh phasor on its first reference (an instant lock — startup
    /// and post-gap re-entry snap to the reference; only *ongoing* corrections
    /// are gentle).
    pub fn new(initial: BeatRef, at: Instant) -> Self {
        Self {
            ref_beat: initial.beat,
            ref_at: at,
            tempo_bps: initial.tempo_bps,
            correction_window: Self::DEFAULT_CORRECTION_WINDOW,
            max_slew_fraction: Self::DEFAULT_MAX_SLEW_FRACTION,
        }
    }

    /// Override the correction tuning (window + slew cap). Chainable.
    pub fn with_tuning(mut self, correction_window: Duration, max_slew_fraction: f64) -> Self {
        self.correction_window = correction_window;
        self.max_slew_fraction = max_slew_fraction;
        self
    }

    /// The current effective tempo (beats/sec), including any active slew.
    pub fn tempo_bps(&self) -> f64 {
        self.tempo_bps
    }

    /// The fractional beat position at local instant `now` (free-run
    /// extrapolation from the anchor). `now` is expected to be at or after the
    /// last anchor; earlier instants clamp to the anchor (no negative dt).
    pub fn position(&self, now: Instant) -> f64 {
        let dt = now.saturating_duration_since(self.ref_at).as_secs_f64();
        self.ref_beat + self.tempo_bps * dt
    }

    /// Ingest a reference received at local instant `at`. Re-anchors so
    /// [`position`](Self::position) stays continuous at `at`, adopts the
    /// reference tempo, and folds the phase error into a bounded correction that
    /// absorbs it over `correction_window`. Never steps.
    pub fn observe(&mut self, r: BeatRef, at: Instant) {
        let current = self.position(at);
        let error = r.beat - current; // beats we're behind (+) or ahead (−)
        let window = self.correction_window.as_secs_f64();
        let raw_correction = if window > 0.0 { error / window } else { 0.0 };
        let cap = self.max_slew_fraction * r.tempo_bps.abs();
        let correction = raw_correction.clamp(-cap, cap);

        // Continuous re-anchor: position(at) is unchanged; only the forward rate
        // changes, so the beat glides toward the reference rather than jumping.
        self.ref_beat = current;
        self.ref_at = at;
        self.tempo_bps = r.tempo_bps + correction;
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
    fn a_reference_ahead_converges_without_a_step() {
        let t0 = Instant::now();
        let mut beat = LocalBeat::new(BeatRef::new(0.0, 2.0), t0);
        let t1 = t0 + Duration::from_secs(1); // phasor reads 2.0 here
        // Reference says we should be at 2.1 — 0.1 beat ahead.
        beat.observe(BeatRef::new(2.1, 2.0), t1);

        // No jump: position at the observe instant is exactly where we were.
        assert!(
            (beat.position(t1) - 2.0).abs() < EPS,
            "must be continuous at the correction, got {}",
            beat.position(t1)
        );
        // We now run slightly fast to close the 0.1-beat gap.
        assert!(beat.tempo_bps() > 2.0, "sped up to converge");
        // After the ~1 s correction window, we've closed the gap: position ≈ the
        // reference's own extrapolation (2.1 + 2.0·1 s = 4.1).
        let t2 = t1 + Duration::from_secs(1);
        assert!(
            (beat.position(t2) - 4.1).abs() < 1e-6,
            "converged to the reference line, got {}",
            beat.position(t2)
        );
    }

    #[test]
    fn a_reference_behind_slows_to_converge() {
        let t0 = Instant::now();
        let mut beat = LocalBeat::new(BeatRef::new(0.0, 2.0), t0);
        let t1 = t0 + Duration::from_secs(1); // reads 2.0
        beat.observe(BeatRef::new(1.95, 2.0), t1); // we're 0.05 ahead
        assert!((beat.position(t1) - 2.0).abs() < EPS, "continuous");
        assert!(beat.tempo_bps() < 2.0, "slowed to let the reference catch up");
    }

    #[test]
    fn tempo_change_updates_the_extrapolation_rate() {
        let t0 = Instant::now();
        let mut beat = LocalBeat::new(BeatRef::new(0.0, 2.0), t0);
        let t1 = t0 + Duration::from_secs(1); // reads 2.0
        // Consistent phase, new tempo (180 BPM = 3.0 bps) → rate adopts 3.0.
        beat.observe(BeatRef::new(2.0, 3.0), t1);
        assert!((beat.tempo_bps() - 3.0).abs() < EPS, "adopted the new tempo");
        assert!(
            (beat.position(t1 + Duration::from_secs(1)) - 5.0).abs() < EPS,
            "extrapolates at 3 bps after the change"
        );
    }

    #[test]
    fn an_outlier_reference_is_slew_bounded_not_a_jump() {
        let t0 = Instant::now();
        let mut beat = LocalBeat::new(BeatRef::new(0.0, 2.0), t0);
        let t1 = t0 + Duration::from_secs(1); // reads 2.0
        // A wild reference (100 beats ahead — a glitch, not a real section jump).
        beat.observe(BeatRef::new(102.0, 2.0), t1);
        assert!((beat.position(t1) - 2.0).abs() < EPS, "still no step");
        // Correction is capped at 10% of tempo: 2.0 → at most 2.2 bps, nowhere
        // near chasing a 100-beat error.
        assert!(
            beat.tempo_bps() <= 2.0 + 0.10 * 2.0 + EPS,
            "slew is capped, got {}",
            beat.tempo_bps()
        );
        assert!(beat.tempo_bps() > 2.0, "still nudging toward it, just gently");
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
        let mut beat =
            LocalBeat::new(BeatRef::new(0.0, 1.8), t0).with_tuning(Duration::from_secs(1), 0.5);
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
        let r = BeatRef::new(4.25, 2.0);
        let json = serde_json::to_string(&r).expect("serialize");
        let back: BeatRef = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, r);
    }
}
