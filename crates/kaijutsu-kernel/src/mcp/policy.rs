//! `InstancePolicy` — modest resource-limit seat (§5.5, D-27).
//!
//! Phase 1 ships trivial defaults enough to keep the kernel from OOMing on
//! pathological input. A real policy admin surface is a follow-up (§9).
//!
//! ## `Default` is for tests only
//!
//! `Default` carries hard-coded constants that are *not* linked to
//! `kaijutsu_types::TimeoutPolicy`. Production registration must use
//! [`InstancePolicy::for_kernel`], which sources `call_timeout` from the
//! kernel's `TimeoutPolicy::mcp_call_timeout_default`. Keeping them
//! decoupled means a kernel with a non-default `TimeoutPolicy` (e.g.,
//! loaded from config CRDT) registers builtin servers with the *kernel's*
//! timeout rather than the `Default` constant — closing the silent-link
//! footgun called out in plan `lovely-hopping-pearl.md`.
//!
//! Per-instance overrides via the `policy_admin` MCP server keep working
//! either way.

use std::time::Duration;

/// Per-instance policy applied by the broker at `call_tool`.
#[derive(Clone, Debug)]
pub struct InstancePolicy {
    pub call_timeout: Duration,
    pub max_result_bytes: usize,
    pub max_concurrency: usize,
}

impl Default for InstancePolicy {
    /// **Test-only.** Production callers should use
    /// [`InstancePolicy::for_kernel`] so `call_timeout` reflects the
    /// kernel's `TimeoutPolicy`.
    fn default() -> Self {
        Self {
            call_timeout: Duration::from_secs(120),
            max_result_bytes: 64 * 1024 * 1024,
            max_concurrency: 16,
        }
    }
}

impl InstancePolicy {
    /// Production constructor. Sources `call_timeout` from the kernel's
    /// `TimeoutPolicy::mcp_call_timeout_default` so kernel-wide policy
    /// drives newly-registered instances without per-call-site fixups.
    pub fn for_kernel(kernel: &crate::Kernel) -> Self {
        Self {
            call_timeout: kernel.timeouts().mcp_call_timeout_default,
            max_result_bytes: 64 * 1024 * 1024,
            max_concurrency: 16,
        }
    }
}
