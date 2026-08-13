//! Client configuration constants.
//!
//! Centralizes hardcoded values for easier configuration and documentation.
//!
//! Transport timeouts (SSH, connect-phase budgets, liveness pinger,
//! per-RPC deadline, reconnect backoff, peer dispatch) are now a real,
//! configurable policy: [`kaijutsu_types::timeout::TransportTimeouts`]. The
//! `pub const`s below project a field out of `TransportTimeouts::DEFAULT`
//! at compile time so existing call sites don't churn — the values and doc
//! comments are unchanged, only their source of truth moved.

use std::time::Duration;

use kaijutsu_types::timeout::TransportTimeouts;

/// Default SSH host for local development.
pub const DEFAULT_SSH_HOST: &str = "localhost";

/// Default SSH port.
pub const DEFAULT_SSH_PORT: u16 = 2222;

/// The one source of truth for every transport timeout below. See
/// [`TransportTimeouts`] for per-field docs, tier assignments, and the
/// invariant tests that pin the relationships between them
/// (`kaijutsu-types/src/timeout.rs`) — the ones below that are specific to
/// this crate's reconnect FSM stay here instead.
const TRANSPORT: TransportTimeouts = TransportTimeouts::DEFAULT;

/// SSH inactivity timeout. russh closes the session if no I/O in either
/// direction for this long. Combined with the keepalive below, sets the
/// upper bound on how long a silently-dead peer keeps the socket open.
pub const SSH_INACTIVITY_TIMEOUT: Duration = TRANSPORT.ssh_inactivity_timeout;

/// SSH keep-alive interval. Client sends SSH_MSG_GLOBAL_REQUEST every
/// interval; the server's matching `keepalive_interval` echoes them. After
/// `SSH_KEEPALIVE_MAX` unanswered probes the client tears down the session.
pub const SSH_KEEPALIVE_INTERVAL: Duration = TRANSPORT.ssh_keepalive_interval;

/// SSH keep-alive max retries. 3 × 30s = ~90s upper bound on detecting a
/// silently-disconnected server at the SSH layer.
pub const SSH_KEEPALIVE_MAX: usize = TRANSPORT.ssh_keepalive_max;

// ── FSM connect-phase budgets ───────────────────────────────────────────────
//
// The reconnect state machine wraps the full handshake in `CONNECT_TOTAL_BUDGET`,
// and each phase carries its own per-phase deadline. The per-phase deadlines
// add up to the total so a single hung phase is reported by phase rather
// than swallowed into a generic "connect timeout".

/// SSH dial + auth + channel open. Generous because SSH agent enumeration
/// can be slow when multiple keys are present.
pub const SSH_DIAL_TIMEOUT: Duration = TRANSPORT.ssh_dial_timeout;

/// `bind_kernel` RPC. Pure capability handout; never does I/O.
pub const RPC_BIND_KERNEL_TIMEOUT: Duration = TRANSPORT.rpc_bind_kernel_timeout;

/// `join_context` RPC. May touch the kernel db; should be fast.
pub const RPC_JOIN_CONTEXT_TIMEOUT: Duration = TRANSPORT.rpc_join_context_timeout;

/// Subscribe handshake (block events + resource events combined, run in parallel).
/// Independent of the others so a wedged subscriber doesn't eat SSH budget.
pub const SUBSCRIBE_TIMEOUT: Duration = TRANSPORT.subscribe_timeout;

/// Total budget for the full Connecting transition. Should be ≥ the sum of
/// per-phase budgets above; serves as a safety net in case a phase forgets
/// to wrap itself.
pub const CONNECT_TOTAL_BUDGET: Duration = TRANSPORT.connect_total_budget;

// ── Liveness pinger ─────────────────────────────────────────────────────────

/// Interval between liveness `ping` RPCs while in `Connected`. Detects
/// wedged-but-not-dead RPC pipes faster than the SSH keepalive would
/// (because SSH transport can be alive while the kernel RPC system hangs).
pub const PING_INTERVAL: Duration = TRANSPORT.ping_interval;

/// Per-`ping` deadline. If a ping doesn't return within this window the
/// FSM transitions to Closing and reconnects. Generous to absorb a single
/// slow tick; the SSH keepalive is the backstop.
pub const PING_TIMEOUT: Duration = TRANSPORT.ping_timeout;

// ── Per-RPC deadline (dispatched commands) ──────────────────────────────────

/// Default deadline for a single dispatched RPC call. Commands that exceed
/// this trigger `CallError::Timeout`; the FSM does NOT tear down the
/// connection on per-call timeout (the call's recipient is the issue, not
/// the pipe). Override per-call if needed.
pub const RPC_CALL_TIMEOUT: Duration = TRANSPORT.rpc_call_timeout;

// ── Backoff policy ──────────────────────────────────────────────────────────

/// Base backoff between reconnect attempts (1s, doubles each attempt).
pub const BACKOFF_BASE: Duration = TRANSPORT.backoff_base;

/// Cap on reconnect backoff. After ~6 attempts we're at 32s capped to this.
pub const BACKOFF_MAX: Duration = TRANSPORT.backoff_max;

// ── Peer invocation ─────────────────────────────────────────────────────────

/// Timeout for agent invocation dispatch on the client side.
///
/// After receiving an invocation via Cap'n Proto callback, this is how long
/// we wait for the Bevy `poll_peer_invocations` system to pick it up and
/// reply. It must fire before the server and kernel hops so the client gives
/// the kernel a clean `Disconnected` instead of racing its `Timeout`.
///
/// That ordering is now a **tested** contract rather than a comment: the whole
/// three-hop ladder lives in `kaijutsu_types::timeout::peer`. It used to be
/// three separate hardcoded constants in three crates whose doc comments had
/// already drifted off the real values.
pub const PEER_INVOCATION_TIMEOUT: Duration = TRANSPORT.peer_dispatch;

// ── Legacy alias (deprecated) ───────────────────────────────────────────────

/// Legacy single-budget connect timeout. Kept as an alias for any code that
/// hasn't migrated to the per-phase budgets yet; equals `CONNECT_TOTAL_BUDGET`.
#[deprecated(note = "use CONNECT_TOTAL_BUDGET or the per-phase budgets")]
pub const CONNECT_TIMEOUT: Duration = CONNECT_TOTAL_BUDGET;

#[cfg(test)]
mod tests {
    use super::*;

    /// `CONNECT_TOTAL_BUDGET`'s own doc says it "should be ≥ the sum of
    /// per-phase budgets" — stated in prose since the FSM landed, enforced by
    /// nothing. If the sum ever exceeds the total, the safety net fires
    /// *before* the phase that is actually stuck, and the operator gets a
    /// generic "connect timeout" instead of the phase name the per-phase
    /// budgets exist to give them.
    ///
    /// Note SUBSCRIBE is deliberately excluded: its own doc says it runs
    /// independently so a wedged subscriber doesn't eat SSH budget.
    #[test]
    fn connect_phase_budgets_fit_inside_the_total() {
        let sequential_phases =
            SSH_DIAL_TIMEOUT + RPC_BIND_KERNEL_TIMEOUT + RPC_JOIN_CONTEXT_TIMEOUT;
        assert!(
            sequential_phases <= CONNECT_TOTAL_BUDGET,
            "per-phase budgets sum to {sequential_phases:?}, over the \
             {CONNECT_TOTAL_BUDGET:?} total — the net would fire before the \
             stuck phase and hide which one it was"
        );
    }

    /// The liveness pinger must get a full ping in before the next one is due,
    /// or a slow-but-alive pipe reads as dead and the FSM reconnects in a loop.
    #[test]
    fn ping_completes_within_its_interval() {
        assert!(PING_TIMEOUT < PING_INTERVAL);
    }

    /// SSH keepalive is documented as the backstop *behind* the RPC-level
    /// pinger, so the pinger has to notice first — otherwise the transport
    /// tears the session down and the FSM never learns the RPC pipe was the
    /// wedged part.
    #[test]
    fn rpc_pinger_detects_before_the_ssh_backstop() {
        let ssh_detect = SSH_KEEPALIVE_INTERVAL * SSH_KEEPALIVE_MAX as u32;
        let rpc_detect = PING_INTERVAL + PING_TIMEOUT;
        assert!(
            rpc_detect < ssh_detect,
            "RPC pinger detects in {rpc_detect:?} but SSH backstop fires at \
             {ssh_detect:?} — the backstop must be slower than what it backs"
        );
    }

    /// Backoff must not outlast the inactivity window it is retrying into.
    #[test]
    fn backoff_cap_stays_under_ssh_inactivity() {
        assert!(BACKOFF_BASE < BACKOFF_MAX);
        assert!(BACKOFF_MAX < SSH_INACTIVITY_TIMEOUT);
    }

    /// Every `pub const` here must still equal the field it now projects
    /// out of `TransportTimeouts::DEFAULT` — a compile-time identity in
    /// theory, but worth pinning as a test so a future refactor that
    /// breaks the projection (e.g. someone accidentally hand-writes a
    /// literal instead of `TRANSPORT.field`) fails loudly instead of
    /// silently reintroducing two sources of truth.
    #[test]
    fn constants_still_match_transport_timeouts_defaults() {
        let t = TransportTimeouts::default();
        assert_eq!(SSH_INACTIVITY_TIMEOUT, t.ssh_inactivity_timeout);
        assert_eq!(SSH_KEEPALIVE_INTERVAL, t.ssh_keepalive_interval);
        assert_eq!(SSH_KEEPALIVE_MAX, t.ssh_keepalive_max);
        assert_eq!(SSH_DIAL_TIMEOUT, t.ssh_dial_timeout);
        assert_eq!(RPC_BIND_KERNEL_TIMEOUT, t.rpc_bind_kernel_timeout);
        assert_eq!(RPC_JOIN_CONTEXT_TIMEOUT, t.rpc_join_context_timeout);
        assert_eq!(SUBSCRIBE_TIMEOUT, t.subscribe_timeout);
        assert_eq!(CONNECT_TOTAL_BUDGET, t.connect_total_budget);
        assert_eq!(PING_INTERVAL, t.ping_interval);
        assert_eq!(PING_TIMEOUT, t.ping_timeout);
        assert_eq!(RPC_CALL_TIMEOUT, t.rpc_call_timeout);
        assert_eq!(BACKOFF_BASE, t.backoff_base);
        assert_eq!(BACKOFF_MAX, t.backoff_max);
        assert_eq!(PEER_INVOCATION_TIMEOUT, t.peer_dispatch);
    }
}
