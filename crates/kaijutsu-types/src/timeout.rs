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
/// External bounds we do not own but must respect include Claude Code's
/// five-second hook timeout and Codex's three-second maximum for
/// `SessionEnd`. Anything on a hook critical path needs a budget below its
/// host's bound, or the host kills the hook while our side is still waiting.
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

    /// Codex's hard ceiling for `SessionEnd`. Unlike its other command hooks,
    /// this event defaults to one second and may be raised only to three.
    pub const CODEX_SESSION_END_DEADLINE: Duration = Duration::from_secs(3);

    /// Our own budget for everything on the CC hook critical path, held
    /// strictly under [`CC_HOOK_DEADLINE`] so **we** answer first.
    ///
    /// The distinction matters. If CC's deadline expires while we are still
    /// working, CC kills the hook: the user sees a hook timeout, and our side
    /// carries on doing work whose result nobody will read. If our budget
    /// expires first we return a permissive response with a note, which is
    /// the behaviour the hook path already commits to elsewhere — the mirror
    /// is ambient, and its slowness must not block the user's action any more
    /// than its failure does.
    ///
    /// This is not hypothetical headroom. `AuthorBlocks` queues FIFO in the
    /// doc task behind a resync whose fetch is bounded at the *request* tier,
    /// so a hook arriving mid-resync could wait far past CC's patience
    /// (docs/issues.md, the MCP latency entry). The budget converts that from
    /// "CC kills us" into "we degrade and say so".
    pub const HOOK_PATH: Duration = Duration::from_secs(4);

    /// Kaijutsu's reply budget for Codex hooks. Kept inside the shortest host
    /// ceiling (`SessionEnd`) and used for every event for consistent,
    /// quick-failing behavior.
    pub const CODEX_HOOK_PATH: Duration = Duration::from_millis(2500);
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

    /// How long a gated `kj` verb holds for a human answer from the
    /// approval ledger before expiring the ask and refusing (fail-closed).
    /// One shared number by design: the gate's poll deadline and the
    /// `ctx.patient(...)` hold that freezes the script clock around it
    /// (see `kj_builtin`) must read the SAME value — two numbers here would
    /// drift, and the drift is exactly the failure in docs/issues.md "Gate
    /// slice 1a — three findings", finding #1 (kaish's own watchdog killing
    /// a blocking gate long before the gate's timeout fires).
    pub gate_wait_timeout: Duration,
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
            // A human answering from another surface (a different shell,
            // another client) needs real minutes; a turn must not hang
            // indefinitely on an unanswered prompt either.
            gate_wait_timeout: Duration::from_secs(300),
        }
    }
}

/// Client-side transport timeout policy: SSH transport, the reconnect FSM's
/// connect-phase budgets, the liveness pinger, per-RPC call deadlines,
/// reconnect backoff, and the client hop of the peer-invocation ladder
/// (see [`peer`]).
///
/// Mirrors [`TimeoutPolicy`] on the client side of the wire: `Duration`
/// fields (plus one count, `ssh_keepalive_max`), serde, and a `Default` that
/// reproduces every value `kaijutsu-client::constants` shipped before this
/// struct existed — those `pub const`s now derive from
/// [`TransportTimeouts::DEFAULT`] rather than carrying independent literals,
/// so this struct is the one place the values live.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportTimeouts {
    /// SSH inactivity timeout. russh closes the session if no I/O in either
    /// direction for this long. Combined with the keepalive below, sets the
    /// upper bound on how long a silently-dead peer keeps the socket open.
    pub ssh_inactivity_timeout: Duration,

    /// SSH keep-alive interval. Client sends SSH_MSG_GLOBAL_REQUEST every
    /// interval; the server's matching `keepalive_interval` echoes them.
    /// After `ssh_keepalive_max` unanswered probes the client tears down
    /// the session.
    pub ssh_keepalive_interval: Duration,

    /// SSH keep-alive max retries. 3 × 30s = ~90s upper bound on detecting
    /// a silently-disconnected server at the SSH layer. A count, not a
    /// duration — russh multiplies it by `ssh_keepalive_interval` itself.
    pub ssh_keepalive_max: usize,

    // ── FSM connect-phase budgets ───────────────────────────────────────
    //
    // The reconnect state machine wraps the full handshake in
    // `connect_total_budget`, and each phase carries its own per-phase
    // deadline. The per-phase deadlines add up to the total so a single
    // hung phase is reported by phase rather than swallowed into a generic
    // "connect timeout".
    /// SSH dial + auth + channel open. Generous because SSH agent
    /// enumeration can be slow when multiple keys are present. Tier:
    /// [`tiers::HANDSHAKE`] by value.
    pub ssh_dial_timeout: Duration,

    /// `bind_kernel` RPC. Pure capability handout; never does I/O. Tier:
    /// [`tiers::HANDSHAKE`] — "one round trip that should not touch disk
    /// or network beyond the peer" describes this exactly.
    pub rpc_bind_kernel_timeout: Duration,

    /// `join_context` RPC. May touch the kernel db; should be fast. Tier:
    /// [`tiers::HANDSHAKE`] by value, though a db touch means it is not
    /// strictly the "never touch disk" case the tier describes.
    pub rpc_join_context_timeout: Duration,

    /// Subscribe handshake (block events + resource events combined, run
    /// in parallel). Independent of the others so a wedged subscriber
    /// doesn't eat SSH budget. Tier: [`tiers::HANDSHAKE`].
    pub subscribe_timeout: Duration,

    /// Total budget for the full Connecting transition. Should be ≥ the
    /// sum of the four phase budgets above; serves as a safety net in case
    /// a phase forgets to wrap itself. A composite over several
    /// handshake-tier phases — it does not map onto a single tier itself.
    pub connect_total_budget: Duration,

    // ── Liveness pinger ──────────────────────────────────────────────────
    /// Interval between liveness `ping` RPCs while in `Connected`. Detects
    /// wedged-but-not-dead RPC pipes faster than the SSH keepalive would
    /// (because SSH transport can be alive while the kernel RPC system
    /// hangs).
    pub ping_interval: Duration,

    /// Per-`ping` deadline. If a ping doesn't return within this window
    /// the FSM transitions to Closing and reconnects. Generous to absorb a
    /// single slow tick; the SSH keepalive is the backstop. Tier:
    /// [`tiers::HANDSHAKE`].
    pub ping_timeout: Duration,

    // ── Per-RPC deadline (dispatched commands) ──────────────────────────
    /// Default deadline for a single dispatched RPC call. Commands that
    /// exceed this trigger `CallError::Timeout`; the FSM does NOT tear
    /// down the connection on per-call timeout (the call's recipient is
    /// the issue, not the pipe). Override per-call if needed. Tier:
    /// [`tiers::REQUEST`] — "a single call that may do real work"
    /// describes this exactly.
    pub rpc_call_timeout: Duration,

    // ── Backoff policy ───────────────────────────────────────────────────
    /// Base backoff between reconnect attempts (1s, doubles each
    /// attempt).
    pub backoff_base: Duration,

    /// Cap on reconnect backoff. After ~6 attempts we're at 32s capped to
    /// this.
    pub backoff_max: Duration,

    // ── Peer invocation ──────────────────────────────────────────────────
    /// Client-side hop of the three-hop peer-invocation ladder — see
    /// [`peer`]. After receiving an invocation via Cap'n Proto callback,
    /// this is how long we wait for the Bevy `poll_peer_invocations`
    /// system to pick it up and reply. Must fire before the server
    /// (`peer::SERVER_FORWARD`) and kernel (`peer::KERNEL_WAIT`) hops.
    pub peer_dispatch: Duration,
}

impl TransportTimeouts {
    /// The values `kaijutsu-client::constants` hardcoded before this struct
    /// existed. `const` (not just a `Default` impl) so
    /// `kaijutsu-client::constants` can keep its `pub const` names by
    /// projecting a field straight out of this at compile time — no runtime
    /// initialization, no churn at call sites.
    pub const DEFAULT: Self = Self {
        ssh_inactivity_timeout: Duration::from_secs(300),
        ssh_keepalive_interval: Duration::from_secs(30),
        ssh_keepalive_max: 3,
        ssh_dial_timeout: Duration::from_secs(5),
        rpc_bind_kernel_timeout: Duration::from_secs(5),
        rpc_join_context_timeout: Duration::from_secs(5),
        subscribe_timeout: Duration::from_secs(5),
        connect_total_budget: Duration::from_secs(25),
        ping_interval: Duration::from_secs(30),
        ping_timeout: Duration::from_secs(5),
        rpc_call_timeout: Duration::from_secs(30),
        backoff_base: Duration::from_secs(1),
        backoff_max: Duration::from_secs(30),
        peer_dispatch: peer::CLIENT_DISPATCH,
    };
}

impl Default for TransportTimeouts {
    fn default() -> Self {
        Self::DEFAULT
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

    /// We must give up before Claude Code does, so the user gets our
    /// degraded-but-deliberate answer instead of CC's kill. Same
    /// caller-fires-first rule as the peer ladder, against an external peer
    /// whose deadline we do not control.
    #[test]
    fn hook_path_budget_expires_before_claude_code_gives_up() {
        assert!(
            tiers::HOOK_PATH < tiers::CC_HOOK_DEADLINE,
            "our hook budget ({:?}) must fire before CC's ({:?}), or CC kills \
             the hook while we are still working and the work is wasted",
            tiers::HOOK_PATH,
            tiers::CC_HOOK_DEADLINE
        );
    }

    /// The margin has to be usable, not nominal: a budget one millisecond
    /// under CC's leaves no room to build and write the degraded response
    /// before CC's own deadline lands. A full probe-tier of headroom is the
    /// floor.
    #[test]
    fn hook_path_budget_leaves_room_to_answer() {
        let margin = tiers::CC_HOOK_DEADLINE - tiers::HOOK_PATH;
        assert!(
            margin >= tiers::PROBE,
            "only {margin:?} of headroom to serialize and write the response"
        );
    }

    #[test]
    fn codex_session_end_budget_expires_before_codex_gives_up() {
        let margin = tiers::CODEX_SESSION_END_DEADLINE
            - tiers::CODEX_HOOK_PATH;
        assert!(
            tiers::CODEX_HOOK_PATH < tiers::CODEX_SESSION_END_DEADLINE
        );
        assert!(margin >= tiers::PROBE);
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

    /// Fields whose doc comments claim the handshake tier by value. If one
    /// of these drifts off `tiers::HANDSHAKE` the doc comment silently
    /// starts lying about which bucket the timeout belongs to — the same
    /// failure mode the tier ladder exists to catch.
    #[test]
    fn transport_handshake_tier_fields_match_the_handshake_tier() {
        let t = TransportTimeouts::default();
        assert_eq!(t.ssh_dial_timeout, tiers::HANDSHAKE);
        assert_eq!(t.rpc_bind_kernel_timeout, tiers::HANDSHAKE);
        assert_eq!(t.rpc_join_context_timeout, tiers::HANDSHAKE);
        assert_eq!(t.subscribe_timeout, tiers::HANDSHAKE);
        assert_eq!(t.ping_timeout, tiers::HANDSHAKE);
    }

    /// `rpc_call_timeout`'s doc claims the request tier ("a single call
    /// that may do real work") almost verbatim from [`tiers::REQUEST`]'s
    /// own doc. Pin the value so the claim stays true.
    #[test]
    fn transport_rpc_call_timeout_matches_the_request_tier() {
        assert_eq!(TransportTimeouts::default().rpc_call_timeout, tiers::REQUEST);
    }

    /// `TransportTimeouts::DEFAULT` must reproduce every value
    /// `kaijutsu-client::constants` hardcoded before this struct existed —
    /// that crate's `pub const`s now derive from these defaults, so a
    /// silent change here would silently change client transport behavior
    /// with no diff at any call site to review.
    #[test]
    fn transport_defaults_match_the_values_client_constants_used_to_hardcode() {
        let t = TransportTimeouts::default();
        assert_eq!(t.ssh_inactivity_timeout, Duration::from_secs(300));
        assert_eq!(t.ssh_keepalive_interval, Duration::from_secs(30));
        assert_eq!(t.ssh_keepalive_max, 3);
        assert_eq!(t.ssh_dial_timeout, Duration::from_secs(5));
        assert_eq!(t.rpc_bind_kernel_timeout, Duration::from_secs(5));
        assert_eq!(t.rpc_join_context_timeout, Duration::from_secs(5));
        assert_eq!(t.subscribe_timeout, Duration::from_secs(5));
        assert_eq!(t.connect_total_budget, Duration::from_secs(25));
        assert_eq!(t.ping_interval, Duration::from_secs(30));
        assert_eq!(t.ping_timeout, Duration::from_secs(5));
        assert_eq!(t.rpc_call_timeout, Duration::from_secs(30));
        assert_eq!(t.backoff_base, Duration::from_secs(1));
        assert_eq!(t.backoff_max, Duration::from_secs(30));
        assert_eq!(t.peer_dispatch, Duration::from_secs(15));
    }

    #[test]
    fn transport_json_roundtrip() {
        let t = TransportTimeouts::default();
        let j = serde_json::to_string(&t).unwrap();
        let back: TransportTimeouts = serde_json::from_str(&j).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn transport_cbor_roundtrip() {
        let t = TransportTimeouts::default();
        let bytes = crate::codec::encode(&t).unwrap();
        let back: TransportTimeouts = crate::codec::decode(&bytes).unwrap();
        assert_eq!(t, back);
    }
}
