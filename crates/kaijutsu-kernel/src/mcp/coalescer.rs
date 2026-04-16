//! `NotificationCoalescer` (§5.3, D-24, D-39, D-40, D-45).
//!
//! Phase 2 wires per-instance broadcast streams through the broker pump; the
//! pump calls `observe()` on every incoming event. Events within the configured
//! window and under `max_in_window` pass through as individual blocks. Events
//! beyond the cap are tallied into a coalesced summary that is emitted once,
//! when the window elapses, via `flush()`.
//!
//! Phase 3 extends the key from `(InstanceId, NotifKind)` to
//! `(InstanceId, NotifKind, Option<String>)` (D-40). The `Option<String>` is
//! the resource URI for `ResourceUpdated` events; `None` for all other kinds.
//! Per-URI windows track independently so two URIs on the same instance do not
//! coalesce into each other.
//!
//! Phase 3 also introduces a per-kind `max_in_window` override
//! (`CoalescePolicy::per_kind_override`, D-45). `NotifKind::ResourceUpdated`
//! uses `max_in_window = 0` by default — every update opens a window
//! immediately and subsequent events fold into the summary. This matches §8
//! Phase 3 exit criterion #3 literally: "one coalesced child block per window,
//! not one per update."

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use super::types::{InstanceId, NotifKind};

/// Coalescing policy. `window` and `max_in_window` are the defaults applied
/// to every `(instance, kind, uri)` key unless `per_kind_override` names a
/// different cap for a particular `NotifKind`.
#[derive(Clone, Debug)]
pub struct CoalescePolicy {
    pub window: Duration,
    pub max_in_window: usize,
    pub hard_drop_after: Option<Duration>,
    /// Per-kind override for `max_in_window` (D-45). Default inserts
    /// `NotifKind::ResourceUpdated => 0` so resource updates coalesce
    /// immediately.
    pub per_kind_override: HashMap<NotifKind, usize>,
}

impl CoalescePolicy {
    /// Effective `max_in_window` for a given kind — the per-kind override
    /// if present, otherwise the default.
    fn max_in_window_for(&self, kind: NotifKind) -> usize {
        self.per_kind_override
            .get(&kind)
            .copied()
            .unwrap_or(self.max_in_window)
    }
}

impl Default for CoalescePolicy {
    fn default() -> Self {
        let mut per_kind_override = HashMap::new();
        // D-45: ResourceUpdated has zero pass-throughs; every update folds
        // into the flush-emitted summary child block.
        per_kind_override.insert(NotifKind::ResourceUpdated, 0);
        Self {
            window: Duration::from_millis(500),
            max_in_window: 20,
            hard_drop_after: None,
            per_kind_override,
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

/// Composite coalescer key (D-40). `uri` is `Some(...)` for
/// `NotifKind::ResourceUpdated`; `None` for all other kinds.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CoalesceKey {
    instance: InstanceId,
    kind: NotifKind,
    uri: Option<String>,
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
    windows: RwLock<HashMap<CoalesceKey, Window>>,
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

    /// Observe a notification arriving on `(instance, kind, uri)`. `uri` is
    /// meaningful only for `NotifKind::ResourceUpdated`; pass `None`
    /// otherwise. See `ObserveOutcome` for caller behavior.
    pub fn observe(
        &self,
        instance: &InstanceId,
        kind: NotifKind,
        uri: Option<&str>,
    ) -> ObserveOutcome {
        // §5.3: ToolsChanged is never coalesced. Tool-list changes drive
        // per-tool diffs downstream; dropping them would lose structural
        // state.
        if matches!(kind, NotifKind::ToolsChanged) {
            return ObserveOutcome::PassThrough;
        }

        let key = CoalesceKey {
            instance: instance.clone(),
            kind,
            uri: uri.map(|s| s.to_string()),
        };
        let cap = self.default_policy.max_in_window_for(kind);
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

        if window.count <= cap {
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

    /// Flush and clear the pending window for `(instance, kind, uri)`.
    /// Returns the coalesced count (>0) if a window was open and had folded
    /// events; `None` if no window existed or nothing was coalesced. The
    /// broker pump uses this to emit a single Coalesced summary block when
    /// the window timer elapses.
    pub fn flush(
        &self,
        instance: &InstanceId,
        kind: NotifKind,
        uri: Option<&str>,
    ) -> Option<usize> {
        let key = CoalesceKey {
            instance: instance.clone(),
            kind,
            uri: uri.map(|s| s.to_string()),
        };
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
            per_kind_override: HashMap::new(),
        }
    }

    #[test]
    fn observe_passes_through_within_cap() {
        let c = NotificationCoalescer::new(policy(500, 3));
        let inst = InstanceId::new("a");
        assert_eq!(c.observe(&inst, NotifKind::Log, None), ObserveOutcome::PassThrough);
        assert_eq!(c.observe(&inst, NotifKind::Log, None), ObserveOutcome::PassThrough);
        assert_eq!(c.observe(&inst, NotifKind::Log, None), ObserveOutcome::PassThrough);
    }

    #[test]
    fn observe_coalesces_beyond_cap() {
        let c = NotificationCoalescer::new(policy(500, 3));
        let inst = InstanceId::new("a");
        assert_eq!(c.observe(&inst, NotifKind::Log, None), ObserveOutcome::PassThrough);
        assert_eq!(c.observe(&inst, NotifKind::Log, None), ObserveOutcome::PassThrough);
        assert_eq!(c.observe(&inst, NotifKind::Log, None), ObserveOutcome::PassThrough);
        assert_eq!(
            c.observe(&inst, NotifKind::Log, None),
            ObserveOutcome::StartWindow,
            "fourth Log should signal StartWindow"
        );
        assert_eq!(
            c.observe(&inst, NotifKind::Log, None),
            ObserveOutcome::Coalesced { so_far: 2 },
            "fifth Log should be Coalesced with so_far=2"
        );
        assert_eq!(
            c.observe(&inst, NotifKind::Log, None),
            ObserveOutcome::Coalesced { so_far: 3 },
        );
    }

    #[test]
    fn independent_keys_do_not_interfere() {
        let c = NotificationCoalescer::new(policy(500, 3));
        let a = InstanceId::new("a");
        let b = InstanceId::new("b");

        for _ in 0..3 {
            assert_eq!(c.observe(&a, NotifKind::Log, None), ObserveOutcome::PassThrough);
        }
        // b's window is fresh.
        assert_eq!(c.observe(&b, NotifKind::Log, None), ObserveOutcome::PassThrough);
        // Different kind for a is also fresh.
        assert_eq!(
            c.observe(&a, NotifKind::PromptsChanged, None),
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
                c.observe(&inst, NotifKind::ToolsChanged, None),
                ObserveOutcome::PassThrough,
                "ToolsChanged was coalesced (§5.3 violation)"
            );
        }
    }

    #[test]
    fn window_resets_after_elapsed() {
        let c = NotificationCoalescer::new(policy(20, 1));
        let inst = InstanceId::new("a");
        assert_eq!(c.observe(&inst, NotifKind::Log, None), ObserveOutcome::PassThrough);
        assert_eq!(c.observe(&inst, NotifKind::Log, None), ObserveOutcome::StartWindow);
        // Simulate the timer firing and clearing the window.
        let count = c.flush(&inst, NotifKind::Log, None);
        assert_eq!(count, Some(1));
        thread::sleep(Duration::from_millis(30));
        assert_eq!(
            c.observe(&inst, NotifKind::Log, None),
            ObserveOutcome::PassThrough,
            "window did not reset after flush + elapsed duration"
        );
    }

    #[test]
    fn flush_returns_count_and_clears_window() {
        let c = NotificationCoalescer::new(policy(500, 2));
        let inst = InstanceId::new("a");
        assert_eq!(c.observe(&inst, NotifKind::Log, None), ObserveOutcome::PassThrough);
        assert_eq!(c.observe(&inst, NotifKind::Log, None), ObserveOutcome::PassThrough);
        assert_eq!(c.observe(&inst, NotifKind::Log, None), ObserveOutcome::StartWindow);
        assert_eq!(
            c.observe(&inst, NotifKind::Log, None),
            ObserveOutcome::Coalesced { so_far: 2 }
        );
        assert_eq!(
            c.observe(&inst, NotifKind::Log, None),
            ObserveOutcome::Coalesced { so_far: 3 }
        );
        assert_eq!(c.flush(&inst, NotifKind::Log, None), Some(3));
        // Second flush on the same key is a no-op (window was cleared).
        assert_eq!(c.flush(&inst, NotifKind::Log, None), None);
    }

    #[test]
    fn flush_on_passthrough_only_key_returns_none() {
        let c = NotificationCoalescer::new(policy(500, 5));
        let inst = InstanceId::new("a");
        // Two pass-through events — never crossed the cap, so no pending
        // coalesced summary exists.
        assert_eq!(c.observe(&inst, NotifKind::Log, None), ObserveOutcome::PassThrough);
        assert_eq!(c.observe(&inst, NotifKind::Log, None), ObserveOutcome::PassThrough);
        assert_eq!(c.flush(&inst, NotifKind::Log, None), None);
    }

    #[test]
    fn flush_on_untouched_key_returns_none() {
        let c = NotificationCoalescer::default();
        let inst = InstanceId::new("never-seen");
        assert_eq!(c.flush(&inst, NotifKind::Log, None), None);
    }

    // ── Phase 3: per-URI + per-kind override ───────────────────────────

    #[test]
    fn uri_windows_are_independent() {
        // D-40: two URIs on the same (instance, ResourceUpdated) must track
        // independent windows. A burst on URI-A does not spill into URI-B.
        let c = NotificationCoalescer::new(CoalescePolicy {
            window: Duration::from_millis(500),
            max_in_window: 2,
            hard_drop_after: None,
            per_kind_override: HashMap::new(),
        });
        let inst = InstanceId::new("a");

        // Fill URI-A past its cap.
        assert_eq!(
            c.observe(&inst, NotifKind::ResourceUpdated, Some("file:///a")),
            ObserveOutcome::PassThrough
        );
        assert_eq!(
            c.observe(&inst, NotifKind::ResourceUpdated, Some("file:///a")),
            ObserveOutcome::PassThrough
        );
        assert_eq!(
            c.observe(&inst, NotifKind::ResourceUpdated, Some("file:///a")),
            ObserveOutcome::StartWindow,
            "URI-A opens its own window",
        );

        // URI-B's window is fresh — first event passes through.
        assert_eq!(
            c.observe(&inst, NotifKind::ResourceUpdated, Some("file:///b")),
            ObserveOutcome::PassThrough,
            "URI-B must not inherit URI-A's count",
        );

        // Flushing URI-A must not clear URI-B.
        assert_eq!(
            c.flush(&inst, NotifKind::ResourceUpdated, Some("file:///a")),
            Some(1)
        );
        assert_eq!(
            c.flush(&inst, NotifKind::ResourceUpdated, Some("file:///b")),
            None,
            "URI-B has only a pass-through — no coalesced summary pending"
        );
    }

    #[test]
    fn uri_none_and_some_are_independent() {
        // Sanity: None and Some("x") are distinct keys. Prevents a
        // regression where Log/ResourceUpdated accidentally share a window.
        let c = NotificationCoalescer::new(policy(500, 0));
        let inst = InstanceId::new("a");
        assert_eq!(
            c.observe(&inst, NotifKind::Log, None),
            ObserveOutcome::StartWindow,
            "Log first event with cap=0 opens window"
        );
        assert_eq!(
            c.observe(&inst, NotifKind::Log, Some("x")),
            ObserveOutcome::StartWindow,
            "Log first event with uri=Some(x) opens a DIFFERENT window"
        );
        // Two distinct windows → two distinct flushes.
        assert_eq!(c.flush(&inst, NotifKind::Log, None), Some(1));
        assert_eq!(c.flush(&inst, NotifKind::Log, Some("x")), Some(1));
    }

    #[test]
    fn resource_updated_has_no_pass_throughs() {
        // D-45: default policy inserts ResourceUpdated => max_in_window=0.
        // First event already opens the window; there are zero pass-throughs.
        // Guards the pump's `unreachable!()` on PassThrough for ResourceUpdated.
        let c = NotificationCoalescer::default();
        let inst = InstanceId::new("a");
        assert_eq!(
            c.observe(&inst, NotifKind::ResourceUpdated, Some("file:///x")),
            ObserveOutcome::StartWindow,
            "default policy must route first ResourceUpdated to StartWindow"
        );
        // Subsequent events fold in.
        assert_eq!(
            c.observe(&inst, NotifKind::ResourceUpdated, Some("file:///x")),
            ObserveOutcome::Coalesced { so_far: 2 }
        );
        // Log remains pass-through under the same default policy.
        assert_eq!(
            c.observe(&inst, NotifKind::Log, None),
            ObserveOutcome::PassThrough,
            "Log is not affected by the ResourceUpdated override"
        );
    }
}
