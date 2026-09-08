//! The shared handle around this client's [`KernelClock`] model
//! (`docs/midi.md` "The one timebase").
//!
//! The actor feeds the model from every liveness ping; everything that stamps
//! or ages a wire timing artifact reads it through this handle. Reads are
//! synchronous and lock-only, so an ALSA capture thread can take one per
//! event without an async context or a channel hop.

use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use kaijutsu_audio::{ClockSample, ClockSnapshot, KernelClock};

/// A cheap, cloneable, `Send + Sync` reader/writer of one client's kernel
/// clock model. Clones share one model.
#[derive(Clone, Debug, Default)]
pub struct KernelClockHandle {
    clock: Arc<RwLock<KernelClock>>,
}

impl KernelClockHandle {
    /// A handle over a fresh, unsampled model — every translation is the
    /// identity until a ping lands.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one ping round trip in. Returns whether the sample was accepted
    /// (see [`KernelClock::observe`]).
    pub fn observe(&self, sample: ClockSample) -> bool {
        self.clock.write().expect("kernel clock lock poisoned").observe(sample)
    }

    /// Forget every sample. The caller resets only when it reconnects to a
    /// *different* kernel: the same kernel restarting keeps the same host
    /// clock, so the model still holds.
    pub fn reset(&self) {
        self.clock.write().expect("kernel clock lock poisoned").reset();
    }

    /// This node's wallclock, translated into the kernel's domain — what
    /// every stamp and every age computation should use.
    pub fn now_ns(&self) -> u64 {
        self.to_kernel_ns(local_epoch_ns())
    }

    /// Translate a local wallclock stamp into the kernel's domain.
    pub fn to_kernel_ns(&self, local_epoch_ns: u64) -> u64 {
        self.clock.read().expect("kernel clock lock poisoned").to_kernel_ns(local_epoch_ns)
    }

    /// The model's current state, for status output and artifact provenance.
    pub fn snapshot(&self) -> ClockSnapshot {
        self.clock.read().expect("kernel clock lock poisoned").snapshot()
    }
}

/// This node's own wallclock in ns since UNIX_EPOCH. `0` before the epoch,
/// which no host reaches.
pub fn local_epoch_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn an_unsampled_handle_is_the_identity() {
        let handle = KernelClockHandle::new();
        assert_eq!(handle.to_kernel_ns(1_234), 1_234);
        assert_eq!(handle.snapshot().samples, 0);
        assert!(handle.now_ns() > 0, "a live host is well past the epoch");
    }

    #[test]
    fn clones_share_one_model() {
        let handle = KernelClockHandle::new();
        let other = handle.clone();
        let sent = Instant::now();
        assert!(handle.observe(ClockSample {
            sent,
            sent_epoch_ns: 1_000_000_000_000,
            kernel_epoch_ns: 1_000_000_000_000 + 100_000_000_000,
            received: sent + Duration::from_millis(2),
            received_epoch_ns: 1_000_000_000_000 + 2_000_000,
        }));
        assert_eq!(other.snapshot().samples, 1, "the clone sees the sample");
        assert!(other.snapshot().offset_ns.is_some_and(|o| o > 99 * 1_000_000_000));

        other.reset();
        assert_eq!(handle.snapshot().samples, 0, "reset is shared too");
        assert_eq!(handle.to_kernel_ns(7), 7);
    }
}
