//! `NotificationCoalescer` skeleton (§5.3, D-24).
//!
//! Phase 1 builds the type and injects it into the broker; **nothing emits
//! notifications yet** (D-32). Phase 2 subscribes external servers' streams
//! to the coalescer and wires `BlockKind::Notification` emission.

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

#[derive(Debug)]
struct Window {
    opened_at: Instant,
    count: usize,
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

    /// Record that a notification would be emitted. Returns `true` if the
    /// caller should pass the notification through, `false` if it should be
    /// folded into the current window summary.
    ///
    /// In Phase 1 no caller exercises this; Phase 2 hooks it up.
    pub fn observe(&self, instance: &InstanceId, kind: NotifKind) -> bool {
        let key = (instance.clone(), kind);
        let now = Instant::now();
        let mut windows = self.windows.write();

        let window = windows.entry(key).or_insert(Window {
            opened_at: now,
            count: 0,
        });

        if now.duration_since(window.opened_at) > self.default_policy.window {
            window.opened_at = now;
            window.count = 0;
        }

        window.count += 1;

        // ToolsChanged never coalesces (§5.3 rule).
        if matches!(kind, NotifKind::ToolsChanged) {
            return true;
        }

        window.count <= self.default_policy.max_in_window
    }

    pub fn policy(&self) -> &CoalescePolicy {
        &self.default_policy
    }
}
