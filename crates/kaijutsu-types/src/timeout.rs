//! Kernel-wide timeout policy.
//!
//! Bundles timeout knobs for kaish-script execution, LLM streaming, and MCP
//! server connect/handshake. Per-instance MCP `call_timeout` continues to live
//! in `kaijutsu-kernel::mcp::policy::InstancePolicy` (instance-scoped); this
//! struct provides the kernel-wide default that new instances start with.
//!
//! Wire-shareable: a parallel `TimeoutPolicy` struct exists in `kaijutsu.capnp`
//! (millisecond `UInt64` fields). Bridging lives wherever the RPC method sits,
//! not in this crate.
//!
//! # Per-call overrides
//!
//! `kaish_request_timeout` is the kernel-wide default applied to every
//! `EmbeddedKaish` instance via `kaish_kernel::KernelConfig::request_timeout`.
//! Specific call sites (rc lifecycle, hook bodies, init scripts) override per
//! call via `ExecuteOptions::with_timeout` using their own dedicated knobs:
//! `rc_script_timeout`, `hook_body_timeout`, `init_script_timeout`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Kernel-wide timeout policy for kaish, LLM, and MCP execution paths.
///
/// All fields are `Duration` in memory; wire/persisted form uses millis.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeoutPolicy {
    /// Default per-request timeout for every EmbeddedKaish kernel. Becomes
    /// `kaish_kernel::KernelConfig::request_timeout`. Per-call sites that
    /// supply `ExecuteOptions::with_timeout` override this value.
    pub kaish_request_timeout: Duration,

    /// Per-rc-script bound used by `KjDispatcher::run_kai_script`. Overrides
    /// `kaish_request_timeout` for `/etc/rc/<context_type>/<verb>/SXX-name.kai`
    /// scripts. On elapse, kaish returns exit 124 and the failure block lands
    /// via `insert_rc_failure_block` like any other non-zero exit.
    pub rc_script_timeout: Duration,

    /// Per-`HookBody::Kaish` bound used by `Broker::run_kaish_hook`. Closes
    /// the asymmetry where hook bodies could hang the gated tool call
    /// indefinitely while the call's own `policy.call_timeout` sat idle.
    pub hook_body_timeout: Duration,

    /// Per-context-init-script bound used by `EmbeddedKaish::run_init_script`.
    /// Init scripts are supposed to be quick — short bound by default.
    pub init_script_timeout: Duration,

    /// Total wall-clock bound on a single LLM streaming completion. Wraps the
    /// rig stream consumption loop; on elapse, the assistant turn ends with a
    /// `BlockKind::Error`.
    pub llm_request_timeout: Duration,

    /// No-progress guard between successive `stream.next()` chunks. Catches
    /// providers that open the connection but stop sending tokens. Distinct
    /// from `llm_request_timeout`, which is the total wall-clock cap.
    pub llm_idle_timeout: Duration,

    /// Bound on external MCP server spawn + handshake + initial `list_tools`.
    /// Applies to both initial connect and reconnect paths in
    /// `mcp::servers::external`.
    pub mcp_connect_timeout: Duration,

    /// Default `call_timeout` seeded into a fresh `InstancePolicy` at server
    /// registration. Per-instance overrides via the `policy_admin` MCP
    /// server continue to work.
    pub mcp_call_timeout_default: Duration,
}

impl Default for TimeoutPolicy {
    fn default() -> Self {
        Self {
            // Generous cap — interactive shell sessions can run cargo builds,
            // git clones, etc. without surprising the user. Catches true
            // wedges, not normal long-running commands.
            kaish_request_timeout: Duration::from_secs(1800),
            // Tighter per-call overrides for non-interactive paths:
            rc_script_timeout: Duration::from_secs(30),
            hook_body_timeout: Duration::from_secs(15),
            init_script_timeout: Duration::from_secs(10),
            llm_request_timeout: Duration::from_secs(300),
            llm_idle_timeout: Duration::from_secs(30),
            mcp_connect_timeout: Duration::from_secs(10),
            mcp_call_timeout_default: Duration::from_secs(120),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let p = TimeoutPolicy::default();
        assert!(p.init_script_timeout < p.rc_script_timeout);
        assert!(p.hook_body_timeout < p.rc_script_timeout);
        assert!(p.llm_idle_timeout < p.llm_request_timeout);
        assert!(p.mcp_connect_timeout < p.mcp_call_timeout_default);
    }

    #[test]
    fn json_roundtrip() {
        let p = TimeoutPolicy::default();
        let j = serde_json::to_string(&p).unwrap();
        let back: TimeoutPolicy = serde_json::from_str(&j).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn postcard_roundtrip() {
        let p = TimeoutPolicy::default();
        let bytes = postcard::to_stdvec(&p).unwrap();
        let back: TimeoutPolicy = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(p, back);
    }
}
