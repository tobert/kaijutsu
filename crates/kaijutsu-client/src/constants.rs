//! Client configuration constants.
//!
//! Centralizes hardcoded values for easier configuration and documentation.

use std::time::Duration;

/// Default SSH host for local development.
pub const DEFAULT_SSH_HOST: &str = "localhost";

/// Default SSH port.
pub const DEFAULT_SSH_PORT: u16 = 2222;

/// SSH inactivity timeout. russh closes the session if no I/O in either
/// direction for this long. Combined with the keepalive below, sets the
/// upper bound on how long a silently-dead peer keeps the socket open.
pub const SSH_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(300);

/// SSH keep-alive interval. Client sends SSH_MSG_GLOBAL_REQUEST every
/// interval; the server's matching `keepalive_interval` echoes them. After
/// `SSH_KEEPALIVE_MAX` unanswered probes the client tears down the session.
pub const SSH_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// SSH keep-alive max retries. 3 × 30s = ~90s upper bound on detecting a
/// silently-disconnected server at the SSH layer.
pub const SSH_KEEPALIVE_MAX: usize = 3;

// ── FSM connect-phase budgets ───────────────────────────────────────────────
//
// The reconnect state machine wraps the full handshake in `CONNECT_TOTAL_BUDGET`,
// and each phase carries its own per-phase deadline. The per-phase deadlines
// add up to the total so a single hung phase is reported by phase rather
// than swallowed into a generic "connect timeout".

/// SSH dial + auth + channel open. Generous because SSH agent enumeration
/// can be slow when multiple keys are present.
pub const SSH_DIAL_TIMEOUT: Duration = Duration::from_secs(5);

/// `bind_kernel` RPC. Pure capability handout; never does I/O.
pub const RPC_BIND_KERNEL_TIMEOUT: Duration = Duration::from_secs(5);

/// `join_context` RPC. May touch the kernel db; should be fast.
pub const RPC_JOIN_CONTEXT_TIMEOUT: Duration = Duration::from_secs(5);

/// Subscribe handshake (block events + resource events combined, run in parallel).
/// Independent of the others so a wedged subscriber doesn't eat SSH budget.
pub const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Total budget for the full Connecting transition. Should be ≥ the sum of
/// per-phase budgets above; serves as a safety net in case a phase forgets
/// to wrap itself.
pub const CONNECT_TOTAL_BUDGET: Duration = Duration::from_secs(25);

// ── Liveness pinger ─────────────────────────────────────────────────────────

/// Interval between liveness `ping` RPCs while in `Connected`. Detects
/// wedged-but-not-dead RPC pipes faster than the SSH keepalive would
/// (because SSH transport can be alive while the kernel RPC system hangs).
pub const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Per-`ping` deadline. If a ping doesn't return within this window the
/// FSM transitions to Closing and reconnects. Generous to absorb a single
/// slow tick; the SSH keepalive is the backstop.
pub const PING_TIMEOUT: Duration = Duration::from_secs(5);

// ── Per-RPC deadline (dispatched commands) ──────────────────────────────────

/// Default deadline for a single dispatched RPC call. Commands that exceed
/// this trigger `CallError::Timeout`; the FSM does NOT tear down the
/// connection on per-call timeout (the call's recipient is the issue, not
/// the pipe). Override per-call if needed.
pub const RPC_CALL_TIMEOUT: Duration = Duration::from_secs(30);

// ── Backoff policy ──────────────────────────────────────────────────────────

/// Base backoff between reconnect attempts (1s, doubles each attempt).
pub const BACKOFF_BASE: Duration = Duration::from_secs(1);

/// Cap on reconnect backoff. After ~6 attempts we're at 32s capped to this.
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);

// ── Peer invocation ─────────────────────────────────────────────────────────

/// Timeout for agent invocation dispatch on the client side.
///
/// After receiving an invocation via Cap'n Proto callback, this is how long
/// we wait for the Bevy `poll_peer_invocations` system to pick it up and
/// reply. Must be shorter than the kernel-side timeout (30s) so the client
/// fires first, giving the kernel a clean `Disconnected` instead of `Timeout`.
pub const PEER_INVOCATION_TIMEOUT: Duration = Duration::from_secs(15);

// ── Legacy alias (deprecated) ───────────────────────────────────────────────

/// Legacy single-budget connect timeout. Kept as an alias for any code that
/// hasn't migrated to the per-phase budgets yet; equals `CONNECT_TOTAL_BUDGET`.
#[deprecated(note = "use CONNECT_TOTAL_BUDGET or the per-phase budgets")]
pub const CONNECT_TIMEOUT: Duration = CONNECT_TOTAL_BUDGET;
