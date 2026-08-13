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

/// # The tier ladder
///
/// Timeout values here and in `kaijutsu-client::constants` accumulated during
/// the MCP-first period, before the app had much to it, and were never fully
/// swept. The result is not a wrong set of numbers — most are individually
/// reasonable and well-commented — it is that **the relationships between them
/// live in prose**, which rots silently. See [`peer`] for a receipt.
///
/// New knobs declare a tier, and the tier carries the rationale:
///
/// | Tier | Order | What it bounds |
/// |---|---|---|
/// | **probe** | ~200 ms | Is something listening? Never does work. |
/// | **handshake** | ~5 s | One round trip that should not touch disk or network beyond the peer. |
/// | **request** | ~30 s | A single call that may do real work. |
/// | **work** | ~300 s | A job with an unbounded-ish middle (LLM streaming). |
/// | **interactive** | ~1800 s | A human or agent is watching and may run a build. |
///
/// Two rules, both meant to be *tested* rather than described:
///
/// 1. **Nesting**: an inner deadline is strictly shorter than the outer budget
///    containing it, so the inner one fires first and names the real culprit.
/// 2. **Ladders across hops**: when one logical deadline is enforced at
///    several hops, the hop *closest to the caller* fires first, so the caller
///    reports a clean failure instead of racing a peer's timeout.
///
/// An external bound we do not own but must respect: **Claude Code's hook
/// timeout is 5 s.** Anything on the hook critical path needs a budget under
/// that, or CC kills the hook while our side is still patiently waiting.
pub mod tiers {
    use std::time::Duration;

    /// Liveness only — is something listening?
    pub const PROBE: Duration = Duration::from_millis(200);
    /// One round trip, no real work behind it.
    pub const HANDSHAKE: Duration = Duration::from_secs(5);
    /// A single call that may do real work.
    pub const REQUEST: Duration = Duration::from_secs(30);
    /// A job with an unbounded-ish middle.
    pub const WORK: Duration = Duration::from_secs(300);
    /// A human or agent is watching; may run a build.
    pub const INTERACTIVE: Duration = Duration::from_secs(1800);

    /// Claude Code's hook timeout. **Not ours to change** — an external
    /// constraint that binds anything on the hook critical path.
    pub const CC_HOOK_DEADLINE: Duration = Duration::from_secs(5);
}

/// The peer-invocation ladder — one logical deadline enforced at three hops.
///
/// The client dispatches an invocation, the server forwards it, and the kernel
/// waits for the reply. Each hop had its own hardcoded constant in its own
/// crate (two of them function-local and invisible), and the relationship
/// between them lived only in doc comments — which had already drifted:
/// `rpc.rs` said "matches the client-side bound (15s)" directly above a
/// constant reading **20s**, and the client said the kernel side was 30s,
/// which was true of one of the two server-side constants and not the other.
///
/// The intent survived by luck (15 < 20 < 30 still ordered correctly), which
/// is exactly the failure mode worth removing: nothing would have caught an
/// edit that inverted it. One source, one test.
///
/// Ordering is the contract: **the hop closest to the caller fires first**, so
/// the client reports a clean `Disconnected` rather than racing the kernel's
/// `Timeout`.
pub mod peer {
    use std::time::Duration;

    /// Client waits for its local consumer (e.g. the Bevy
    /// `poll_peer_invocations` system) to pick up and reply. Fires first.
    pub const CLIENT_DISPATCH: Duration = Duration::from_secs(15);
    /// Server forwards the invocation to the peer's callback.
    pub const SERVER_FORWARD: Duration = Duration::from_secs(20);
    /// Kernel waits for the whole round trip. The outermost bound.
    pub const KERNEL_WAIT: Duration = Duration::from_secs(30);
}

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

    /// The tier ladder must stay ordered, or "declare a tier" stops meaning
    /// anything — a knob could claim `handshake` while outlasting `request`.
    #[test]
    fn tiers_are_ordered() {
        use tiers::*;
        assert!(PROBE < HANDSHAKE);
        assert!(HANDSHAKE < REQUEST);
        assert!(REQUEST < WORK);
        assert!(WORK < INTERACTIVE);
    }

    /// The peer ladder's whole point: the hop closest to the caller fires
    /// first, so the caller reports a clean failure instead of racing a
    /// peer's timeout. This previously held by luck across three hardcoded
    /// constants in three crates, with two doc comments already drifted off
    /// the real values.
    #[test]
    fn peer_ladder_fires_caller_first() {
        assert!(
            peer::CLIENT_DISPATCH < peer::SERVER_FORWARD,
            "client must give up before the server does, or the client races \
             the server's timeout and reports the wrong cause"
        );
        assert!(
            peer::SERVER_FORWARD < peer::KERNEL_WAIT,
            "server must give up before the kernel does, for the same reason"
        );
    }

    /// Anything on the Claude Code hook critical path has to finish inside
    /// CC's own 5s hook timeout — a bound we do not own. A `request`-tier
    /// deadline is NOT safe there, and this pins the trap: the gap is what
    /// makes hook-path work need its own budget rather than the default.
    #[test]
    fn request_tier_does_not_fit_the_cc_hook_deadline() {
        assert!(
            tiers::REQUEST > tiers::CC_HOOK_DEADLINE,
            "if these ever cross, the hook-path carve-out below is obsolete \
             and this test should be replaced, not deleted"
        );
        assert!(
            tiers::HANDSHAKE <= tiers::CC_HOOK_DEADLINE,
            "handshake tier is the largest that may sit on the hook path"
        );
    }

    #[test]
    fn json_roundtrip() {
        let p = TimeoutPolicy::default();
        let j = serde_json::to_string(&p).unwrap();
        let back: TimeoutPolicy = serde_json::from_str(&j).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn cbor_roundtrip() {
        let p = TimeoutPolicy::default();
        let bytes = crate::codec::encode(&p).unwrap();
        let back: TimeoutPolicy = crate::codec::decode(&bytes).unwrap();
        assert_eq!(p, back);
    }
}
