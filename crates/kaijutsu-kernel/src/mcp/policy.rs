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
//! loaded from config) registers builtin servers with the *kernel's*
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
            // ~16k tokens — generous for a normal tool result, but no longer
            // ~16M tokens (the old 64 MiB default), which was large enough to
            // let one `cargo build 2>&1` or stray `find /` blow the entire
            // context window with no recovery. Still raisable per-instance
            // via `kj policy set`. Oversized results truncate in place (see
            // `truncate_result_to_budget`) rather than erroring.
            max_result_bytes: 64 * 1024,
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
            // See the matching comment on `Default` above — 64 KiB, not 64 MiB.
            max_result_bytes: 64 * 1024,
            max_concurrency: 16,
        }
    }

    /// Production constructor for the ONE instance that can reach
    /// `kj::gate::run_gate` through `Broker::call_tool` — `builtin.shell_write`
    /// (the hot, mutating `ShellServer`; its read-only twin `builtin.shell`
    /// never gates, and no other builtin server calls `run_gate`).
    ///
    /// A gate-capable instance holds for a HUMAN to answer from another
    /// surface (`kj ledger`), not for a normal tool's own work, so it is
    /// bounded by the gate ladder (`kaijutsu_types::timeout::gate`) rather
    /// than the generic `mcp_call_timeout_default` every other instance
    /// uses. Using the generic default here reopens the exact defect the
    /// ladder exists to close: the broker would cancel the call out from
    /// under `run_gate` before the gate's own (longer) wait could ever
    /// resolve, silently truncating a human's answer window and replacing
    /// the gate's honest refusal reason with a generic broker timeout.
    ///
    /// Takes `_kernel` (unused today — the gate ladder's broker cap is a
    /// fixed constant, not derived from `TimeoutPolicy`; see the `gate`
    /// module's docs for why) so the signature matches [`Self::for_kernel`]
    /// and a future per-kernel override doesn't require touching every call
    /// site again.
    pub fn for_kernel_gated(_kernel: &crate::Kernel) -> Self {
        Self {
            call_timeout: kaijutsu_types::timeout::gate::BROKER_CALL,
            // See the matching comment on `Default` above — 64 KiB, not 64 MiB.
            max_result_bytes: 64 * 1024,
            max_concurrency: 16,
        }
    }
}
