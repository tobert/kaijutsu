//! The node's model of the kernel's clock (`docs/midi.md` "The one timebase").
//!
//! The kernel is the sole sequencer, so its wallclock IS the timebase. Every
//! wire timing artifact is minted with the kernel-domain wallclock and aged
//! against it; a node that reads its own `SystemTime` instead is reading a
//! different clock, and the difference shows up as a stamp from the future or
//! a stamp that looks seconds stale.
//!
//! A node learns its offset from the ping round trip rather than from NTP:
//! `offset = kernel_stamp − local midpoint of (send, receive)`, filtered
//! NTP-style by keeping the sample with the smallest round trip. **Model,
//! never chase**: the applied offset moves only when a new estimate differs
//! from it by more than the current uncertainty, and it moves as a step. No
//! musical scheduling depends on it — every phasor free-runs on `Instant` —
//! so a step is a correction to *labelling*, never to the beat.
//!
//! FFI-free and pure: `observe` takes the instants and stamps its caller
//! sampled, so the whole model is unit-testable with hand-picked values.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// One completed ping round trip, sampled by the caller around the call.
///
/// `sent`/`received` are the caller's monotonic clock (the round trip is
/// measured there, where a wallclock step cannot corrupt it); the three
/// `_epoch_ns` fields are wallclock ns since UNIX_EPOCH — the caller's own at
/// send and receive, and the kernel's as reported in the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSample {
    /// Local monotonic instant immediately before the request went out.
    pub sent: Instant,
    /// Local wallclock at `sent`.
    pub sent_epoch_ns: u64,
    /// The kernel's wallclock, as carried by the reply.
    pub kernel_epoch_ns: u64,
    /// Local monotonic instant immediately after the reply landed.
    pub received: Instant,
    /// Local wallclock at `received`.
    pub received_epoch_ns: u64,
}

/// How far a free-running crystal may drift between samples: 100 ppm, the
/// spec bound consumer oscillators are built to. It buys ~6 ms per minute of
/// uncertainty growth, which keeps a node whose ping stalls for a few minutes
/// honest about how well it still knows the kernel's clock.
pub const DRIFT_PPM: u64 = 100;

/// Samples kept in the filter window. Eight covers four minutes at the 30 s
/// liveness cadence — long enough that one congested round trip is outvoted,
/// short enough that a genuine step in the kernel's clock is adopted within
/// the window rather than held off by an old, lucky-rtt sample.
pub const FILTER_WINDOW: usize = 8;

/// Samples required before [`KernelClock::is_dialed_in`] can be true.
pub const DIALED_IN_SAMPLES: usize = 3;

/// Uncertainty bound for [`KernelClock::is_dialed_in`]. A LAN round trip to
/// the kernel is about a millisecond, so 25 ms tolerates a very sloppy link
/// and still sits an order of magnitude under
/// [`crate::timebase::STAMP_FUTURE_TOLERANCE`] — a dialed-in node never
/// manufactures a future stamp out of its own uncertainty.
pub const DIALED_IN_UNCERTAINTY: Duration = Duration::from_millis(25);

/// One accepted sample, reduced to what the filter needs.
#[derive(Debug, Clone, Copy)]
struct Observed {
    /// Kernel wallclock minus the local midpoint, in ns. Positive = the
    /// kernel is ahead of this node.
    offset_ns: i64,
    /// Round trip measured on the monotonic clock.
    rtt: Duration,
    /// Local monotonic instant the reply landed — the age reference for the
    /// drift allowance.
    at: Instant,
    /// Local wallclock at `at`, for [`ClockSnapshot::sampled_at_epoch_ns`].
    at_epoch_ns: u64,
}

/// A node's model of the kernel's clock, fed from ping round trips.
#[derive(Debug, Clone, Default)]
pub struct KernelClock {
    /// The filter window, oldest first.
    window: VecDeque<Observed>,
    /// The offset currently applied to stamps. `None` until the first
    /// accepted sample; then it only steps when a new estimate lands outside
    /// the current uncertainty.
    applied_ns: Option<i64>,
}

/// A readable summary of the model, for status output and for the provenance
/// a kept artifact carries: what offset was applied when it was written, and
/// how well the node knew it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClockSnapshot {
    /// Applied offset (kernel − local) in ns. `None` while unsampled.
    pub offset_ns: Option<i64>,
    /// Current uncertainty in ns. `None` while unsampled.
    pub uncertainty_ns: Option<u64>,
    /// Samples in the filter window.
    pub samples: usize,
    /// Whether the model meets [`DIALED_IN_SAMPLES`] and
    /// [`DIALED_IN_UNCERTAINTY`].
    pub dialed_in: bool,
    /// Local wallclock at the newest accepted sample. `0` while unsampled.
    pub sampled_at_epoch_ns: u64,
}

impl KernelClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one round trip into the model. Returns whether the sample was
    /// accepted.
    ///
    /// Rejected: a reply with no kernel stamp (`kernel_epoch_ns == 0` — an
    /// old kernel that carries neither the ns nor the ms field), a local
    /// wallclock of zero at either end, and a `received` that precedes
    /// `sent`. A rejected sample leaves the model exactly as it was.
    pub fn observe(&mut self, sample: ClockSample) -> bool {
        if sample.kernel_epoch_ns == 0 || sample.sent_epoch_ns == 0 || sample.received_epoch_ns == 0
        {
            return false;
        }
        if sample.received_epoch_ns < sample.sent_epoch_ns {
            return false;
        }
        let Some(rtt) = sample.received.checked_duration_since(sample.sent) else {
            return false;
        };
        // The local wallclock at the midpoint of the round trip is the best
        // guess at "when the kernel stamped its reply, in local time"; the
        // error is bounded by half the round trip, which is exactly what the
        // uncertainty reports.
        let midpoint = sample.sent_epoch_ns + (sample.received_epoch_ns - sample.sent_epoch_ns) / 2;
        let offset_ns = (sample.kernel_epoch_ns as i128 - midpoint as i128)
            .clamp(i64::MIN as i128, i64::MAX as i128) as i64;

        self.window.push_back(Observed {
            offset_ns,
            rtt,
            at: sample.received,
            at_epoch_ns: sample.received_epoch_ns,
        });
        while self.window.len() > FILTER_WINDOW {
            self.window.pop_front();
        }

        // Model, never chase: step only when the new estimate is further from
        // what we apply than the model can resolve.
        let Some(best) = self.best() else { return true };
        let estimate = best.offset_ns;
        match self.applied_ns {
            None => self.applied_ns = Some(estimate),
            Some(applied) => {
                let gap = estimate.abs_diff(applied);
                let uncertainty = self.uncertainty_ns_at(sample.received).unwrap_or(0);
                if gap > uncertainty {
                    self.applied_ns = Some(estimate);
                }
            }
        }
        true
    }

    /// The sample the estimate comes from: the smallest round trip in the
    /// window, newest first among ties (a fresher sample of equal quality has
    /// a smaller drift allowance).
    fn best(&self) -> Option<Observed> {
        self.window
            .iter()
            .copied()
            .reduce(|best, s| if s.rtt <= best.rtt { s } else { best })
    }

    /// Forget every sample. The caller resets when it reconnects to a
    /// *different* kernel; a restart of the same kernel keeps the same host
    /// clock, so the model still holds.
    pub fn reset(&mut self) {
        self.window.clear();
        self.applied_ns = None;
    }

    /// The applied offset (kernel − local) in ns, `None` while unsampled.
    pub fn offset_ns(&self) -> Option<i64> {
        self.applied_ns
    }

    /// Current uncertainty in ns, aged to `Instant::now()`.
    pub fn uncertainty_ns(&self) -> Option<u64> {
        self.uncertainty_ns_at(Instant::now())
    }

    /// Uncertainty in ns as of local instant `now`: half the best sample's
    /// round trip plus [`DRIFT_PPM`] of the time since that sample landed.
    pub fn uncertainty_ns_at(&self, now: Instant) -> Option<u64> {
        let best = self.best()?;
        let age_ns = now.saturating_duration_since(best.at).as_nanos() as u64;
        let drift_ns = age_ns / 1_000_000 * DRIFT_PPM;
        Some((best.rtt.as_nanos() as u64 / 2).saturating_add(drift_ns))
    }

    /// Samples in the filter window.
    pub fn samples(&self) -> usize {
        self.window.len()
    }

    /// Whether the model is trustworthy enough to age stamps against: at
    /// least [`DIALED_IN_SAMPLES`] samples and uncertainty under
    /// [`DIALED_IN_UNCERTAINTY`].
    pub fn is_dialed_in(&self) -> bool {
        self.is_dialed_in_at(Instant::now())
    }

    /// [`Self::is_dialed_in`] as of local instant `now`.
    pub fn is_dialed_in_at(&self, now: Instant) -> bool {
        self.window.len() >= DIALED_IN_SAMPLES
            && self
                .uncertainty_ns_at(now)
                .is_some_and(|u| u <= DIALED_IN_UNCERTAINTY.as_nanos() as u64)
    }

    /// Translate a local wallclock stamp into the kernel's domain. Identity
    /// while unsampled — a node that has never heard from the kernel stamps
    /// with its own clock, which is what it did before this model existed.
    pub fn to_kernel_ns(&self, local_epoch_ns: u64) -> u64 {
        match self.applied_ns {
            Some(offset) => local_epoch_ns.saturating_add_signed(offset),
            None => local_epoch_ns,
        }
    }

    /// A readable summary, aged to `Instant::now()`.
    pub fn snapshot(&self) -> ClockSnapshot {
        self.snapshot_at(Instant::now())
    }

    /// [`Self::snapshot`] as of local instant `now`.
    pub fn snapshot_at(&self, now: Instant) -> ClockSnapshot {
        ClockSnapshot {
            offset_ns: self.applied_ns,
            uncertainty_ns: self.uncertainty_ns_at(now),
            samples: self.window.len(),
            dialed_in: self.is_dialed_in_at(now),
            sampled_at_epoch_ns: self.window.back().map(|s| s.at_epoch_ns).unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;
    const S: u64 = 1_000_000_000;

    /// Build a sample as a node would: it left at local wallclock `sent_ns`,
    /// took `rtt`, and the kernel's reply claimed `kernel_ns` at the far end.
    fn sample(base: Instant, sent_ns: u64, rtt_ns: u64, kernel_ns: u64) -> ClockSample {
        ClockSample {
            sent: base,
            sent_epoch_ns: sent_ns,
            kernel_epoch_ns: kernel_ns,
            received: base + Duration::from_nanos(rtt_ns),
            received_epoch_ns: sent_ns + rtt_ns,
        }
    }

    #[test]
    fn one_sample_converges_across_a_hundred_second_skew() {
        // moltar's wallclock is 100.9 s behind zorak's; one ping is enough to
        // learn that, because the round trip bounds the error, not the skew.
        let base = Instant::now();
        let local = 1_000 * S;
        let skew = 100_900 * MS;
        let mut clock = KernelClock::new();
        assert!(clock.observe(sample(base, local, 2 * MS, local + skew + MS)));

        let offset = clock.offset_ns().expect("one sample is enough");
        // The kernel stamp was taken mid-flight, so the recovered offset is
        // the true skew to within half the round trip.
        assert!(
            (offset - skew as i64).abs() <= MS as i64,
            "offset {offset} should be within 1ms of the {skew}ns skew"
        );
        assert_eq!(clock.samples(), 1);
        // And a local stamp now reads in the kernel's domain.
        let kernel_now = clock.to_kernel_ns(local + 10 * MS);
        assert!(kernel_now > local + skew, "local stamps move into kernel time");
    }

    #[test]
    fn a_negative_offset_is_modeled_too() {
        // The other direction: this node's clock runs AHEAD of the kernel.
        let base = Instant::now();
        let local = 5_000 * S;
        let mut clock = KernelClock::new();
        assert!(clock.observe(sample(base, local, 2 * MS, local - 30 * S + MS)));
        let offset = clock.offset_ns().expect("sampled");
        assert!(offset < 0, "kernel behind local ⇒ negative offset, got {offset}");
        assert!(
            (offset + 30 * S as i64).abs() <= MS as i64,
            "recovered ≈ −30s, got {offset}"
        );
        assert_eq!(clock.to_kernel_ns(local), (local as i64 + offset) as u64);
    }

    #[test]
    fn an_unsampled_clock_is_the_identity() {
        let clock = KernelClock::new();
        assert_eq!(clock.offset_ns(), None);
        assert_eq!(clock.uncertainty_ns(), None);
        assert_eq!(clock.samples(), 0);
        assert!(!clock.is_dialed_in());
        assert_eq!(clock.to_kernel_ns(12_345), 12_345, "identity while unsampled");
        let snap = clock.snapshot();
        assert_eq!(snap.offset_ns, None);
        assert_eq!(snap.sampled_at_epoch_ns, 0);
        assert!(!snap.dialed_in);
    }

    #[test]
    fn a_reply_with_no_kernel_stamp_is_rejected() {
        // An old kernel returns 0 for both time fields; the caller passes
        // that through as kernel_epoch_ns == 0 and the model must not learn
        // an offset of "minus fifty-six years".
        let base = Instant::now();
        let local = 1_000 * S;
        let mut clock = KernelClock::new();
        assert!(!clock.observe(sample(base, local, 2 * MS, 0)), "0/0 sample rejected");
        assert_eq!(clock.samples(), 0);
        assert_eq!(clock.offset_ns(), None);

        // A reply that arrives before it was sent is a corrupt sample too.
        let backwards = ClockSample {
            sent: base + Duration::from_millis(5),
            sent_epoch_ns: local,
            kernel_epoch_ns: local,
            received: base,
            received_epoch_ns: local,
        };
        assert!(!clock.observe(backwards), "received-before-sent rejected");
        assert_eq!(clock.samples(), 0);
    }

    #[test]
    fn a_large_rtt_outlier_does_not_move_the_estimate() {
        // The min-rtt filter: one congested round trip carries a wildly wrong
        // midpoint, and the estimate must keep the clean sample's answer.
        let base = Instant::now();
        let local = 1_000 * S;
        let skew = 100 * S;
        let mut clock = KernelClock::new();
        clock.observe(sample(base, local, 2 * MS, local + skew + MS));
        let clean = clock.offset_ns().expect("sampled");

        // 400 ms round trip, and the kernel's stamp landed early in it, so a
        // naive midpoint would read ~200 ms of extra offset.
        clock.observe(sample(
            base + Duration::from_secs(30),
            local + 30 * S,
            400 * MS,
            local + 30 * S + skew + 10 * MS,
        ));
        assert_eq!(clock.samples(), 2, "the outlier is kept, just not believed");
        assert_eq!(
            clock.offset_ns(),
            Some(clean),
            "min-rtt filter ignores the congested sample"
        );
    }

    #[test]
    fn a_step_inside_uncertainty_is_ignored_and_one_outside_is_taken() {
        // Model, never chase: the applied offset holds through noise smaller
        // than what the model can actually resolve, and steps when the
        // evidence exceeds it.
        let base = Instant::now();
        let local = 1_000 * S;
        let skew = 100 * S;
        let mut clock = KernelClock::new();
        clock.observe(sample(base, local, 2 * MS, local + skew + MS));
        let first = clock.offset_ns().expect("sampled");
        let uncertainty = clock.uncertainty_ns_at(base + Duration::from_millis(2)).expect("sampled");
        assert!(uncertainty >= MS, "uncertainty is at least rtt/2, got {uncertainty}");

        // A 200 µs wobble — well inside 1 ms of uncertainty.
        clock.observe(sample(
            base + Duration::from_secs(30),
            local + 30 * S,
            2 * MS,
            local + 30 * S + skew + MS + 200_000,
        ));
        assert_eq!(clock.offset_ns(), Some(first), "sub-uncertainty wobble is ignored");

        // A 50 ms move — the kernel's clock genuinely stepped.
        clock.observe(sample(
            base + Duration::from_secs(60),
            local + 60 * S,
            2 * MS,
            local + 60 * S + skew + 50 * MS + MS,
        ));
        let stepped = clock.offset_ns().expect("sampled");
        assert!(
            (stepped - first - 50 * MS as i64).abs() <= MS as i64,
            "a 50ms move is adopted as a step: {first} → {stepped}"
        );
    }

    #[test]
    fn the_drift_allowance_grows_with_the_sample_age() {
        let base = Instant::now();
        let local = 1_000 * S;
        let mut clock = KernelClock::new();
        clock.observe(sample(base, local, 2 * MS, local + MS));
        let at_sample = clock
            .uncertainty_ns_at(base + Duration::from_millis(2))
            .expect("sampled");
        let a_minute_later = clock
            .uncertainty_ns_at(base + Duration::from_secs(60))
            .expect("sampled");
        assert!(
            a_minute_later > at_sample,
            "uncertainty grows with age: {at_sample} → {a_minute_later}"
        );
        // 100 ppm over 60 s is 6 ms.
        let grown = a_minute_later - at_sample;
        assert!(
            grown.abs_diff(6 * MS) < 100_000,
            "100 ppm over a minute is ~6ms, got {grown}"
        );
    }

    #[test]
    fn dialed_in_needs_samples_and_a_tight_uncertainty() {
        let base = Instant::now();
        let local = 1_000 * S;
        let mut clock = KernelClock::new();
        for n in 0..2 {
            let t = base + Duration::from_secs(n);
            clock.observe(sample(t, local + n * S, 2 * MS, local + n * S + MS));
        }
        assert!(!clock.is_dialed_in_at(base + Duration::from_secs(2)), "two samples is not enough");

        clock.observe(sample(
            base + Duration::from_secs(2),
            local + 2 * S,
            2 * MS,
            local + 2 * S + MS,
        ));
        assert!(clock.is_dialed_in_at(base + Duration::from_secs(2)), "three clean samples dial in");

        // Left alone long enough, drift alone pushes it back out: 25 ms at
        // 100 ppm is 250 s.
        assert!(
            !clock.is_dialed_in_at(base + Duration::from_secs(400)),
            "drift eventually un-dials a node that stops hearing from the kernel"
        );
    }

    #[test]
    fn the_window_keeps_only_the_most_recent_samples() {
        let base = Instant::now();
        let local = 1_000 * S;
        let mut clock = KernelClock::new();
        for n in 0..(FILTER_WINDOW as u64 + 4) {
            let t = base + Duration::from_secs(n);
            clock.observe(sample(t, local + n * S, 2 * MS, local + n * S + MS));
        }
        assert_eq!(clock.samples(), FILTER_WINDOW, "window is bounded");
    }

    #[test]
    fn reset_forgets_everything() {
        let base = Instant::now();
        let local = 1_000 * S;
        let mut clock = KernelClock::new();
        clock.observe(sample(base, local, 2 * MS, local + 50 * S));
        assert!(clock.offset_ns().is_some());
        clock.reset();
        assert_eq!(clock.offset_ns(), None);
        assert_eq!(clock.samples(), 0);
        assert_eq!(clock.to_kernel_ns(7), 7);
    }

    #[test]
    fn snapshot_round_trips_and_reports_the_sample_wallclock() {
        let base = Instant::now();
        let local = 1_000 * S;
        let mut clock = KernelClock::new();
        clock.observe(sample(base, local, 2 * MS, local + 100 * S + MS));
        let snap = clock.snapshot_at(base + Duration::from_millis(2));
        assert_eq!(snap.samples, 1);
        assert_eq!(snap.offset_ns, clock.offset_ns());
        assert_eq!(snap.sampled_at_epoch_ns, local + 2 * MS);
        let json = serde_json::to_string(&snap).expect("serialize");
        let back: ClockSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, snap);
    }
}
