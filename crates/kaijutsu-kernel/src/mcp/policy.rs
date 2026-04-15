//! `InstancePolicy` — modest resource-limit seat (§5.5, D-27).
//!
//! Phase 1 ships trivial defaults enough to keep the kernel from OOMing on
//! pathological input. A real policy admin surface is a follow-up (§9).

use std::time::Duration;

/// Per-instance policy applied by the broker at `call_tool`.
#[derive(Clone, Debug)]
pub struct InstancePolicy {
    pub call_timeout: Duration,
    pub max_result_bytes: usize,
    pub max_concurrency: usize,
}

impl Default for InstancePolicy {
    fn default() -> Self {
        Self {
            call_timeout: Duration::from_secs(120),
            max_result_bytes: 64 * 1024 * 1024,
            max_concurrency: 16,
        }
    }
}
