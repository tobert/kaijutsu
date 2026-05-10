//! `InstancePolicy` — modest resource-limit seat (§5.5, D-27).
//!
//! Phase 1 ships trivial defaults enough to keep the kernel from OOMing on
//! pathological input. A real policy admin surface is a follow-up (§9).
//!
//! `Default` reads `call_timeout` from `TimeoutPolicy::mcp_call_timeout_default`
//! so the kernel-wide policy can move the floor without per-instance fixups.
//! The `policy_admin` MCP server can still override per-instance after
//! registration.

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
        // Pulls the call_timeout default from the kernel-wide TimeoutPolicy
        // so a single config change moves the floor for newly-registered
        // instances. Existing registered instances are unaffected — those
        // are mutated via the `policy_admin` MCP server.
        //
        // **Coupling to keep an eye on:** this `Default` calls
        // `TimeoutPolicy::default` directly, so the two `Default` impls are
        // linked silently. Architectural follow-up (see plan
        // `lovely-hopping-pearl.md`): drop `Default` on `InstancePolicy` for
        // production and have the broker seed `call_timeout` at register-time
        // from `kernel.timeouts().mcp_call_timeout_default`. Tests can keep a
        // local helper. Until that lands, keep the two defaults in sync if
        // either moves.
        let tp = kaijutsu_types::TimeoutPolicy::default();
        Self {
            call_timeout: tp.mcp_call_timeout_default,
            max_result_bytes: 64 * 1024 * 1024,
            max_concurrency: 16,
        }
    }
}
