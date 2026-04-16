//! `NotificationCoalescer` (§5.3, D-24, D-39).
//!
//! Phase 2 wires per-instance broadcast streams through the broker pump; the
//! pump calls `observe()` on every incoming event. Events within the configured
//! window and under `max_in_window` pass through as individual blocks. Events
//! beyond the cap are tallied into a coalesced summary that is emitted once,
//! when the window elapses, via `flush()`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use super::types::{InstanceId, NotifKind};

/// Coalescing policy for a single `(instance, kind)` window.
#[derive(Clone, Debug)]
pub struct CoalescePolicy {
    pub window: Duration,
    pub max_in_window: usize,
    pub hard_drop_after: Option<Duration>,
}

impl Default for CoalescePolicy {
    fn default() -> Self {
        Self {
            window: Duration::from_millis(500),
            max_in_window: 20,
            hard_drop_after: None,
        }
    }
}

/// Outcome of a single `observe()` call (D-39).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObserveOutcome {
    /// Emit this notification as its own block.
    PassThrough,
    /// First event that exceeded `max_in_window`; caller schedules a flush
    /// timer and does not emit a block for this event.
    StartWindow,
    /// Subsequent coalesced event; counted into the pending summary.
    /// `so_far` is the number of events folded so far, including the
    /// `StartWindow` event that opened the coalesce state.
    Coalesced { so_far: usize },
}

#[derive(Debug)]
struct Window {
    opened_at: Instant,
    /// Total events observed in this window (includes passed-through).
    count: usize,
    /// Number of events beyond `max_in_window` — i.e., folded into the
    /// pending Coalesced summary. Zero while the window is still below cap.
    coalesced: usize,
}

pub struct NotificationCoalescer {
    windows: RwLock<HashMap<(InstanceId, NotifKind), Window>>,
    default_policy: CoalescePolicy,
}

impl Default for NotificationCoalescer {
    fn default() -> Self {
        Self::new(CoalescePolicy::default())
    }
}

impl NotificationCoalescer {
    pub fn new(default_policy: CoalescePolicy) -> Self {
        Self {
            windows: RwLock::new(HashMap::new()),
            default_policy,
        }
    }

    /// Observe a notification arriving on `(instance, kind)`. See
    /// `ObserveOutcome` for caller behavior.
    pub fn observe(&self, instance: &InstanceId, kind: NotifKind) -> ObserveOutcome {
        // §5.3: ToolsChanged is never coalesced. Tool-list changes drive
        // per-tool diffs downstream; dropping them would lose structural
        // state.
        if matches!(kind, NotifKind::ToolsChanged) {
            return ObserveOutcome::PassThrough;
        }

        let key = (instance.clone(), kind);
        let now = Instant::now();
        let mut windows = self.windows.write();

        let window = windows.entry(key).or_insert(Window {
            opened_at: now,
            count: 0,
            coalesced: 0,
        });

        if now.duration_since(window.opened_at) > self.default_policy.window {
            window.opened_at = now;
            window.count = 0;
            window.coalesced = 0;
        }

        window.count += 1;

        if window.count <= self.default_policy.max_in_window {
            return ObserveOutcome::PassThrough;
        }

        window.coalesced += 1;
        if window.coalesced == 1 {
            ObserveOutcome::StartWindow
        } else {
            ObserveOutcome::Coalesced {
                so_far: window.coalesced,
            }
        }
    }

    /// Flush and clear the pending window for `(instance, kind)`.
    /// Returns the coalesced count (>0) if a window was open and had folded
    /// events; `None` if no window existed or nothing was coalesced. The
    /// broker pump uses this to emit a single Coalesced summary block when
    /// the window timer elapses.
    pub fn flush(&self, instance: &InstanceId, kind: NotifKind) -> Option<usize> {
        let key = (instance.clone(), kind);
        let mut windows = self.windows.write();
        let window = windows.remove(&key)?;
        if window.coalesced > 0 {
            Some(window.coalesced)
        } else {
            None
        }
    }

    pub fn policy(&self) -> &CoalescePolicy {
        &self.default_policy
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    fn policy(window_ms: u64, max_in_window: usize) -> CoalescePolicy {
        CoalescePolicy {
            window: Duration::from_millis(window_ms),
            max_in_window,
            hard_drop_after: None,
        }
    }

    #[test]
    fn observe_passes_through_within_cap() {
        let c = NotificationCoalescer::new(policy(500, 3));
        let inst = InstanceId::new("a");
        assert_eq!(c.observe(&inst, NotifKind::Log), ObserveOutcome::PassThrough);
        assert_eq!(c.observe(&inst, NotifKind::Log), ObserveOutcome::PassThrough);
        assert_eq!(c.observe(&inst, NotifKind::Log), ObserveOutcome::PassThrough);
    }

    #[test]
    fn observe_coalesces_beyond_cap() {
        let c = NotificationCoalescer::new(policy(500, 3));
        let inst = InstanceId::new("a");
        assert_eq!(c.observe(&inst, NotifKind::Log), ObserveOutcome::PassThrough);
        assert_eq!(c.observe(&inst, NotifKind::Log), ObserveOutcome::PassThrough);
        assert_eq!(c.observe(&inst, NotifKind::Log), ObserveOutcome::PassThrough);
        assert_eq!(
            c.observe(&inst, NotifKind::Log),
            ObserveOutcome::StartWindow,
            "fourth Log should signal StartWindow"
        );
        assert_eq!(
            c.observe(&inst, NotifKind::Log),
            ObserveOutcome::Coalesced { so_far: 2 },
            "fifth Log should be Coalesced with so_far=2"
        );
        assert_eq!(
            c.observe(&inst, NotifKind::Log),
            ObserveOutcome::Coalesced { so_far: 3 },
        );
    }

    #[test]
    fn independent_keys_do_not_interfere() {
        let c = NotificationCoalescer::new(policy(500, 3));
        let a = InstanceId::new("a");
        let b = InstanceId::new("b");

        for _ in 0..3 {
            assert_eq!(c.observe(&a, NotifKind::Log), ObserveOutcome::PassThrough);
        }
        // b's window is fresh.
        assert_eq!(c.observe(&b, NotifKind::Log), ObserveOutcome::PassThrough);
        // Different kind for a is also fresh.
        assert_eq!(
            c.observe(&a, NotifKind::ResourceUpdated),
            ObserveOutcome::PassThrough
        );
    }

    #[test]
    fn tools_changed_never_coalesces() {
        // §5.3 rule: ToolsChanged must always pass through. Phase 2 diffs
        // per-tool so every tool-list-dirty event matters.
        let c = NotificationCoalescer::new(policy(500, 1));
        let inst = InstanceId::new("a");
        for _ in 0..10 {
            assert_eq!(
                c.observe(&inst, NotifKind::ToolsChanged),
                ObserveOutcome::PassThrough,
                "ToolsChanged was coalesced (§5.3 violation)"
            );
        }
    }

    #[test]
    fn window_resets_after_elapsed() {
        let c = NotificationCoalescer::new(policy(20, 1));
        let inst = InstanceId::new("a");
        assert_eq!(c.observe(&inst, NotifKind::Log), ObserveOutcome::PassThrough);
        assert_eq!(c.observe(&inst, NotifKind::Log), ObserveOutcome::StartWindow);
        // Simulate the timer firing and clearing the window.
        let count = c.flush(&inst, NotifKind::Log);
        assert_eq!(count, Some(1));
        thread::sleep(Duration::from_millis(30));
        assert_eq!(
            c.observe(&inst, NotifKind::Log),
            ObserveOutcome::PassThrough,
            "window did not reset after flush + elapsed duration"
        );
    }

    #[test]
    fn flush_returns_count_and_clears_window() {
        let c = NotificationCoalescer::new(policy(500, 2));
        let inst = InstanceId::new("a");
        assert_eq!(c.observe(&inst, NotifKind::Log), ObserveOutcome::PassThrough);
        assert_eq!(c.observe(&inst, NotifKind::Log), ObserveOutcome::PassThrough);
        assert_eq!(c.observe(&inst, NotifKind::Log), ObserveOutcome::StartWindow);
        assert_eq!(
            c.observe(&inst, NotifKind::Log),
            ObserveOutcome::Coalesced { so_far: 2 }
        );
        assert_eq!(
            c.observe(&inst, NotifKind::Log),
            ObserveOutcome::Coalesced { so_far: 3 }
        );
        assert_eq!(c.flush(&inst, NotifKind::Log), Some(3));
        // Second flush on the same key is a no-op (window was cleared).
        assert_eq!(c.flush(&inst, NotifKind::Log), None);
    }

    #[test]
    fn flush_on_passthrough_only_key_returns_none() {
        let c = NotificationCoalescer::new(policy(500, 5));
        let inst = InstanceId::new("a");
        // Two pass-through events — never crossed the cap, so no pending
        // coalesced summary exists.
        assert_eq!(c.observe(&inst, NotifKind::Log), ObserveOutcome::PassThrough);
        assert_eq!(c.observe(&inst, NotifKind::Log), ObserveOutcome::PassThrough);
        assert_eq!(c.flush(&inst, NotifKind::Log), None);
    }

    #[test]
    fn flush_on_untouched_key_returns_none() {
        let c = NotificationCoalescer::default();
        let inst = InstanceId::new("never-seen");
        assert_eq!(c.flush(&inst, NotifKind::Log), None);
    }
}
