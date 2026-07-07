//! MIDI clock-in — the M3 estimator (`docs/midi.md` "M3 — Drift-modeled
//! clock-in": observe an external clock master's pulses locally, learn its
//! tempo + phase + drift, emit low-rate `BeatRef`-shaped corrections; the
//! regenerate-locally-and-slew half is [`crate::LocalBeat`], already built).
//!
//! The gift in MIDI clock: **phase is exact, not estimated.** Pulse *n*
//! since the anchor is beat *n/24* by definition, so musical position is a
//! *count*, and the estimator's whole job is mapping that count onto
//! wall-clock predictions (tempo + time offset). Position only slips when
//! pulses drop — and a drop announces itself as an inter-pulse interval of
//! ~k periods, which we detect and re-count instead of absorbing as tempo.
//!
//! This is the EMA candidate (chosen 2026-07-06; the two-state Kalman is the
//! recorded upgrade path if real playing shows EMA lag on tempo ramps — the
//! consumer only ever sees [`ClockEstimate`]s, so it is a drop-in swap).
//! Each interval is classified by its **ratio to the learned period**:
//!
//! - ~1× → a normal pulse: count += 1, interval feeds the tempo EMA.
//! - ~integer 2–4× → dropped pulse(s): count += k (phase stays exact),
//!   interval/k feeds the EMA once.
//! - <0.75× → late-then-early jitter pair: count += 1, EMA unfed (the pair's
//!   second interval would drag tempo sharp).
//! - >4.5× → a discontinuity (stalled master): re-anchor timing at this
//!   pulse, count += 1, flag it — position vs a silently-stalled sender is
//!   unknowable, so we surface it rather than guess (`discontinuities`).
//!
//! Transport semantics follow the MIDI spec: `Start` re-anchors at beat 0,
//! `Continue` resumes the frozen count, `Stop` freezes position while the
//! tempo keeps learning (hardware keeps sending clock while stopped), and
//! Song Position sets the count outright (1 SPP unit = a 16th = 6 pulses).
//! Before any transport message the first pulse anchors beat 0 and runs —
//! the ambient case (a KeyStep free-running clock with no Start ever).

use crate::timebase::BeatRef;

/// MIDI clock pulses per quarter-note beat (the spec's 24 PPQN).
pub const PULSES_PER_BEAT: u32 = 24;

/// EMA weight per accepted interval. 0.05 ≈ a ~20-pulse (~0.8 beat) time
/// constant: fast enough to track a human tempo ramp within a couple of
/// beats, slow enough that single-interval jitter barely moves the tempo.
const EMA_ALPHA: f64 = 0.05;

/// Interval-ratio classification bounds (see the module doc).
const ACCEPT_LOW: f64 = 0.75;
const ACCEPT_HIGH: f64 = 1.33;
const DROPOUT_HIGH: f64 = 4.5;
/// A dropout interval must land within this of an integer period multiple.
const DROPOUT_TOLERANCE: f64 = 0.25;

/// Emit one estimate per this many counted pulses (24 = once per beat; at
/// 120 BPM that is a 2 Hz correction stream — comfortably "low-rate" for
/// the control plane, comfortably fast for a slewing phasor).
const PULSES_PER_EMIT: u64 = PULSES_PER_BEAT as u64;

/// One observed clock-relevant MIDI event, stamped at receipt (the capture
/// thread's clock tap feeds these; tests synthesize them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockEvent {
    /// `F8` timing clock.
    Pulse { epoch_ns: u64 },
    /// `FA` — play from the top: position re-anchors to beat 0.
    Start { epoch_ns: u64 },
    /// `FB` — resume from the frozen position.
    Continue { epoch_ns: u64 },
    /// `FC` — freeze position (tempo keeps learning from pulses).
    Stop { epoch_ns: u64 },
    /// `F2` — Song Position Pointer, in MIDI beats (16ths, 6 pulses each).
    SongPosition { epoch_ns: u64, sixteenths: u16 },
}

/// One correction toward the observed master: the [`BeatRef`] a downstream
/// phasor slews toward, plus the wall instant it was true at and the health
/// residual (predicted-vs-actual arrival of the pulse that produced it).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockEstimate {
    /// Beat coordinate + tempo at `epoch_ns` — [`crate::LocalBeat::observe`]
    /// shaped.
    pub reference: BeatRef,
    /// When the reference was true (receipt stamp of the producing event).
    pub epoch_ns: u64,
    /// Last pulse's arrival minus its prediction, ns. The health signal: a
    /// locked steady clock hovers near 0; sustained growth means the EMA is
    /// lagging a ramp (the recorded Kalman trigger).
    pub residual_ns: i64,
}

/// The EMA clock estimator for ONE observed master (one per source port).
#[derive(Debug, Clone, Default)]
pub struct ClockEstimator {
    /// Learned inter-pulse period, ns. `None` until two pulses have arrived.
    period_ema_ns: Option<f64>,
    /// Receipt stamp of the previous pulse (for the interval + prediction).
    last_pulse_ns: Option<u64>,
    /// Pulses counted since the anchor — the exact phase (`count / 24` beats).
    count: u64,
    /// Whether position advances on a pulse (MIDI transport gate). Starts
    /// true: the ambient no-transport master runs from its first pulse.
    running: bool,
    /// Whether any pulse has anchored yet (distinguishes the ambient
    /// first-pulse anchor from a resumed stream).
    anchored: bool,
    /// Counted pulses since the last emission.
    since_emit: u64,
    /// Last pulse's prediction residual (0 until predictable).
    residual_ns: i64,
    /// Discontinuities observed (stalls > ~4.5 periods). Monotonic; a
    /// consumer that sees this move knows position may have slipped.
    pub discontinuities: u64,
}

impl ClockEstimator {
    pub fn new() -> Self {
        Self { running: true, ..Self::default() }
    }

    /// Beat coordinate of the current count.
    fn beat(&self) -> f64 {
        self.count as f64 / PULSES_PER_BEAT as f64
    }

    /// Tempo in beats/sec from the learned period (`None` until learned).
    pub fn tempo_bps(&self) -> Option<f64> {
        self.period_ema_ns
            .map(|p| 1e9 / (p * PULSES_PER_BEAT as f64))
    }

    /// Last pulse's prediction residual, ns (health signal).
    pub fn residual_ns(&self) -> i64 {
        self.residual_ns
    }

    /// Fold one observed event in; returns an estimate when one is due
    /// (once per beat of counted pulses, and immediately on transport
    /// re-anchors so a downstream phasor never slews across a seek).
    pub fn observe(&mut self, event: ClockEvent) -> Option<ClockEstimate> {
        match event {
            ClockEvent::Pulse { epoch_ns } => self.observe_pulse(epoch_ns),
            ClockEvent::Start { epoch_ns } => {
                self.count = 0;
                self.running = true;
                self.anchored = true;
                self.last_pulse_ns = Some(epoch_ns);
                self.since_emit = 0;
                self.emit(epoch_ns)
            }
            ClockEvent::Continue { epoch_ns } => {
                self.running = true;
                // The next pulse advances from the frozen count; prediction
                // re-anchors here so the stop gap is not read as an interval.
                self.last_pulse_ns = Some(epoch_ns);
                self.emit(epoch_ns)
            }
            ClockEvent::Stop { epoch_ns: _ } => {
                self.running = false;
                None
            }
            ClockEvent::SongPosition { epoch_ns, sixteenths } => {
                self.count = sixteenths as u64 * 6;
                self.anchored = true;
                self.since_emit = 0;
                self.emit(epoch_ns)
            }
        }
    }

    fn observe_pulse(&mut self, epoch_ns: u64) -> Option<ClockEstimate> {
        let Some(prev) = self.last_pulse_ns else {
            // First pulse ever: the ambient anchor (beat 0, running).
            self.last_pulse_ns = Some(epoch_ns);
            self.anchored = true;
            return None;
        };
        let interval = epoch_ns.saturating_sub(prev) as f64;
        self.last_pulse_ns = Some(epoch_ns);

        let Some(period) = self.period_ema_ns else {
            // Second pulse: seed the period directly.
            if interval > 0.0 {
                self.period_ema_ns = Some(interval);
            }
            return self.count_pulse(1, epoch_ns);
        };

        let ratio = interval / period;
        self.residual_ns = (interval - period) as i64;
        if (ACCEPT_LOW..=ACCEPT_HIGH).contains(&ratio) {
            // Normal pulse: learn from it.
            self.period_ema_ns = Some(period + EMA_ALPHA * (interval - period));
            self.count_pulse(1, epoch_ns)
        } else if ratio > ACCEPT_HIGH && ratio <= DROPOUT_HIGH {
            let k = ratio.round();
            if (ratio - k).abs() <= DROPOUT_TOLERANCE && k >= 2.0 {
                // k-period gap ≈ k−1 dropped pulses: keep phase exact by
                // counting them, and learn from the per-period interval.
                let per = interval / k;
                self.period_ema_ns = Some(period + EMA_ALPHA * (per - period));
                self.residual_ns = (per - period) as i64;
                self.count_pulse(k as u64, epoch_ns)
            } else {
                // Off-grid long interval: count the pulse, don't learn.
                self.count_pulse(1, epoch_ns)
            }
        } else if ratio > DROPOUT_HIGH {
            // Stall: position vs the sender is unknowable — re-anchor
            // timing, count this pulse, and say so.
            self.discontinuities += 1;
            self.count_pulse(1, epoch_ns)
        } else {
            // Short interval (late-then-early jitter pair): count, no learn.
            self.count_pulse(1, epoch_ns)
        }
    }

    /// Advance the count by `n` pulses (transport-gated) and emit if a
    /// beat's worth has accumulated.
    fn count_pulse(&mut self, n: u64, epoch_ns: u64) -> Option<ClockEstimate> {
        if !self.running {
            return None; // stopped: tempo learned above, position frozen
        }
        self.count += n;
        self.since_emit += n;
        if self.since_emit >= PULSES_PER_EMIT {
            self.since_emit = 0;
            self.emit(epoch_ns)
        } else {
            None
        }
    }

    fn emit(&self, epoch_ns: u64) -> Option<ClockEstimate> {
        let tempo_bps = self.tempo_bps()?;
        if !self.anchored {
            return None;
        }
        Some(ClockEstimate {
            reference: BeatRef::new(self.beat(), tempo_bps),
            epoch_ns,
            residual_ns: self.residual_ns,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: u64 = 1_000_000_000;
    /// 120 BPM: 0.5 s/beat → 24 pulses → 20_833_333 ns between pulses.
    const P120: u64 = NS / 2 / PULSES_PER_BEAT as u64;

    /// Feed `n` pulses at fixed `period` starting from `t0`; returns the
    /// estimates emitted and the final timestamp.
    fn pulse_train(
        est: &mut ClockEstimator,
        t0: u64,
        period: u64,
        n: usize,
    ) -> (Vec<ClockEstimate>, u64) {
        let mut out = Vec::new();
        let mut t = t0;
        for _ in 0..n {
            if let Some(e) = est.observe(ClockEvent::Pulse { epoch_ns: t }) {
                out.push(e);
            }
            t += period;
        }
        (out, t)
    }

    #[test]
    fn a_steady_train_locks_tempo_and_counts_exact_beats() {
        let mut est = ClockEstimator::new();
        // 49 pulses = anchor + 48 counted = exactly 2 beats.
        let (emits, _) = pulse_train(&mut est, 0, P120, 49);
        let tempo = est.tempo_bps().expect("locked");
        assert!((tempo - 2.0).abs() < 0.001, "120 BPM == 2.0 bps, got {tempo}");
        assert_eq!(emits.len(), 2, "one estimate per beat");
        assert!((emits[0].reference.beat - 1.0).abs() < 1e-9, "phase is a count");
        assert!((emits[1].reference.beat - 2.0).abs() < 1e-9);
        assert!(est.residual_ns().abs() < 2, "steady clock ⇒ ~zero residual");
        assert_eq!(est.discontinuities, 0);
    }

    #[test]
    fn gaussian_ish_jitter_barely_moves_tempo_and_never_moves_phase() {
        let mut est = ClockEstimator::new();
        // Alternate ±2 ms around the true period (zero-mean jitter at 10% of
        // the 20.8 ms period — a nasty WiFi-ish stream).
        let mut t = 0u64;
        let mut emitted = Vec::new();
        for i in 0..97 {
            // 96 counted pulses = 4 beats
            if let Some(e) = est.observe(ClockEvent::Pulse { epoch_ns: t }) {
                emitted.push(e);
            }
            let jitter: i64 = if i % 2 == 0 { 2_000_000 } else { -2_000_000 };
            t = (t as i64 + P120 as i64 + jitter) as u64;
        }
        let tempo = est.tempo_bps().expect("locked");
        assert!((tempo - 2.0).abs() < 0.05, "tempo stays near 2.0 bps: {tempo}");
        // Phase never wavers: it is a count.
        let last = emitted.last().expect("emitted");
        assert!((last.reference.beat - 4.0).abs() < 1e-9, "beat 4 exactly");
    }

    #[test]
    fn a_single_burst_outlier_is_not_absorbed_as_tempo() {
        let mut est = ClockEstimator::new();
        let (_, t) = pulse_train(&mut est, 0, P120, 25);
        let before = est.tempo_bps().unwrap();
        // One pulse 30% late (ratio 1.3 — inside accept, worst case for EMA),
        // then back on grid.
        est.observe(ClockEvent::Pulse { epoch_ns: t + (P120 as f64 * 0.3) as u64 });
        let (_, _) = pulse_train(&mut est, t + P120 + (P120 as f64 * 0.3) as u64, P120, 24);
        let after = est.tempo_bps().unwrap();
        assert!(
            (after - before).abs() / before < 0.02,
            "one late pulse moves tempo <2%: {before} → {after}"
        );
    }

    #[test]
    fn a_tempo_ramp_is_tracked_within_the_ema_lag() {
        let mut est = ClockEstimator::new();
        let (_, mut t) = pulse_train(&mut est, 0, P120, 25);
        // Ramp 120 → 126 BPM over 4 beats (~5% faster ⇒ period ÷ 1.05).
        let p126 = (P120 as f64 / 1.05) as u64;
        for _ in 0..96 {
            est.observe(ClockEvent::Pulse { epoch_ns: t });
            t += p126;
        }
        let tempo = est.tempo_bps().unwrap();
        assert!(
            (tempo - 2.1).abs() < 0.02,
            "after 4 beats at 126 BPM the EMA has converged: {tempo} bps"
        );
    }

    #[test]
    fn dropped_pulses_are_recounted_so_phase_never_slips() {
        let mut est = ClockEstimator::new();
        let (_, t) = pulse_train(&mut est, 0, P120, 25); // anchor + 1 beat
        // Two pulses vanish: next arrival is 3 periods out.
        let t_after_gap = t + 2 * P120;
        est.observe(ClockEvent::Pulse { epoch_ns: t_after_gap });
        // Continue steady to the 2-beat boundary: 24+3=27 counted, need 48.
        let (emits, _) = pulse_train(&mut est, t_after_gap + P120, P120, 21);
        let last = emits.last().expect("an estimate at the beat boundary");
        assert!(
            (last.reference.beat - 2.0).abs() < 1e-9,
            "the gap was counted as 3 pulses — phase exact: {}",
            last.reference.beat
        );
        assert_eq!(est.discontinuities, 0, "a 3-period gap is a dropout, not a stall");
        let tempo = est.tempo_bps().unwrap();
        assert!((tempo - 2.0).abs() < 0.01, "per-period learning through the gap");
    }

    #[test]
    fn a_long_stall_is_a_loud_discontinuity_not_a_guess() {
        let mut est = ClockEstimator::new();
        let (_, t) = pulse_train(&mut est, 0, P120, 25);
        // 10 periods of silence — beyond dropout inference.
        est.observe(ClockEvent::Pulse { epoch_ns: t + 9 * P120 });
        assert_eq!(est.discontinuities, 1, "the stall is surfaced");
        // Tempo unpolluted by the gap.
        let tempo = est.tempo_bps().unwrap();
        assert!((tempo - 2.0).abs() < 0.01, "stall interval never fed the EMA: {tempo}");
    }

    #[test]
    fn transport_start_stop_continue_follow_the_midi_spec() {
        let mut est = ClockEstimator::new();
        let (_, t) = pulse_train(&mut est, 0, P120, 49); // 2 beats in
        // Stop: position freezes, tempo keeps learning.
        est.observe(ClockEvent::Stop { epoch_ns: t });
        let (emits, t2) = pulse_train(&mut est, t + P120, P120, 48);
        assert!(emits.is_empty(), "no estimates while stopped");
        let frozen = est.beat();
        assert!((frozen - 2.0).abs() < 1e-9, "position frozen at 2.0: {frozen}");
        assert!(est.tempo_bps().is_some(), "tempo still learned while stopped");

        // Continue: resumes the frozen count; next beat boundary is 3.0.
        let resumed = est.observe(ClockEvent::Continue { epoch_ns: t2 });
        assert!(
            (resumed.expect("continue emits").reference.beat - 2.0).abs() < 1e-9,
            "continue re-announces the frozen position"
        );
        let (emits, _) = pulse_train(&mut est, t2 + P120, P120, 24);
        assert!((emits.last().unwrap().reference.beat - 3.0).abs() < 1e-9);

        // Start: back to the top.
        let restarted = est.observe(ClockEvent::Start { epoch_ns: t2 + 30 * P120 });
        assert!((restarted.expect("start emits").reference.beat - 0.0).abs() < 1e-9);
    }

    #[test]
    fn song_position_sets_the_count_in_sixteenths() {
        let mut est = ClockEstimator::new();
        let (_, t) = pulse_train(&mut est, 0, P120, 25);
        // SPP 16 sixteenths = 4 beats = 96 pulses.
        let e = est
            .observe(ClockEvent::SongPosition { epoch_ns: t, sixteenths: 16 })
            .expect("SPP emits immediately");
        assert!((e.reference.beat - 4.0).abs() < 1e-9);
    }

    #[test]
    fn no_estimate_before_tempo_is_learnable() {
        let mut est = ClockEstimator::new();
        assert!(est.observe(ClockEvent::Pulse { epoch_ns: 0 }).is_none());
        assert!(est.tempo_bps().is_none(), "one pulse has no interval");
        // A Start before any period is learned emits nothing (a reference
        // needs a tempo for the phasor to free-run on).
        assert!(est.observe(ClockEvent::Start { epoch_ns: 10 }).is_none());
    }

    /// The estimate is shaped for the existing phasor: feeding a steady
    /// train's estimates into a `LocalBeat` keeps its position within a
    /// pulse of the true beat — the estimator and the slewing phasor agree.
    #[test]
    fn estimates_drive_a_local_beat_phasor_coherently() {
        use std::time::{Duration, Instant};

        let mut est = ClockEstimator::new();
        let t0 = Instant::now();
        let mut phasor: Option<crate::LocalBeat> = None;
        let mut t = 0u64;
        for _ in 0..97 {
            if let Some(e) = est.observe(ClockEvent::Pulse { epoch_ns: t }) {
                let at = t0 + Duration::from_nanos(e.epoch_ns);
                match &mut phasor {
                    Some(p) => p.observe(e.reference, at),
                    None => phasor = Some(crate::LocalBeat::new(e.reference, at)),
                }
            }
            t += P120;
        }
        let p = phasor.expect("anchored");
        let pos = p.position(t0 + Duration::from_nanos(t));
        // t is one period past the 96th counted pulse (beat 4).
        let expected = 4.0 + 1.0 / PULSES_PER_BEAT as f64;
        assert!(
            (pos - expected).abs() < 0.05,
            "phasor tracks the estimator: pos {pos}, expected ≈{expected}"
        );
    }
}
