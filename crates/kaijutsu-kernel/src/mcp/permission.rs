//! Permission-ask plumbing for `HookAction::Ask` (D-57, docs/acp.md gap #2
//! "No Ask pathway for `session/request_permission`").
//!
//! Modeled on the `ElicitationEvents::onRequest` template
//! (`kaijutsu.capnp`): a blocking server→client call/response. Elicitation
//! is scoped per-connection (an MCP server's own request, answered by
//! whichever connection is driving that call); a permission Ask is
//! deliberately **kernel-wide** instead — `evaluate_phase` runs inside the
//! broker with no notion of "the connection that asked", because the hook
//! that fires it can be triggered from any call path (an autonomous turn,
//! a sibling context's tool call, a kaish script) and the client answering
//! it (the Bevy app, a CLI, a future ACP frontend) need not be the one that
//! triggered the call. `subscribeTurnEvents` is the closer precedent:
//! one kernel-wide bus, not a per-connection Vec.
//!
//! `kaijutsu-kernel` has no capnp dependency (codegen lives in
//! `kaijutsu-client`/`kaijutsu-server`), so this module defines the
//! plain-Rust request/response/trait shape only. The real
//! `PermissionEvents::Client` bridge lives in `kaijutsu-server` and is
//! wired onto `Broker::set_permission_asker` at kernel bootstrap, mirroring
//! `set_kernel`/`set_kj_dispatcher`.
//!
//! **No-subscriber / timeout policy (D-57):** shared-trust stance
//! (`CLAUDE.md` "Shared trust, crosstalk-as-feature") says capabilities are
//! ergonomic nudges, not security walls — but an `Ask` hook with nobody to
//! ask is not "nudge absent," it's a broken control: the operator declared
//! "a human must decide this" and no human is reachable. Silent fallbacks
//! are a mistake (`CLAUDE.md` user directives) — the fallback here is
//! **Deny, loud log, always** (fail closed but observable), for both a
//! missing subscriber and a subscriber that doesn't answer within the
//! timeout. This differs from `HookAction::Deny`'s tracing-only reason
//! channel: a no-subscriber/timeout Ask denial is logged at `warn!` (not
//! plain `debug!`) because it usually means an rc script or hook config is
//! misconfigured for the current session (Ask declared, nothing attached to
//! answer it), which the operator needs to notice.

use std::time::Duration;

use async_trait::async_trait;
use kaijutsu_types::ContextId;

/// Default round-trip budget for a permission ask before it resolves to the
/// fail-closed default. "Generous" per the design brief: long enough that a
/// human actually looking at a prompt has time to answer, short enough that
/// a turn doesn't hang indefinitely when nobody is watching.
pub const DEFAULT_PERMISSION_ASK_TIMEOUT: Duration = Duration::from_secs(30);

/// One selectable option offered to the answering client. ACP's
/// `request_permission` offers option *kinds* like allow-once/allow-always/
/// reject-once/reject-always; kept as free text here (not a closed enum) so
/// the wire shape isn't boxed into ACP's specific vocabulary. `HookAction::Ask`
/// itself doesn't populate this today (v1 rule syntax is a plain
/// description) — the field exists so a request can carry richer choices
/// once a caller wants them, without a wire change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionOption {
    pub id: String,
    pub label: String,
    pub kind: String,
}

/// A permission ask fired by `HookAction::Ask`. Plain data — no capnp
/// dependency in this crate.
#[derive(Clone, Debug)]
pub struct PermissionAskRequest {
    /// Unique id for this ask, for correlating logs and (eventually)
    /// remembered-scope lookups. Generated fresh per fire.
    pub request_id: String,
    /// The context the hooked call originated from.
    pub context_id: ContextId,
    /// Human-readable description of the action being asked about, shown
    /// to the answering client. Falls back to `"{instance}.{tool}"` at
    /// fire time when the hook's `AskSpec::description` is `None`.
    pub description: String,
    pub instance: String,
    pub tool: String,
    /// The `HookEntry` id that fired this Ask, so the answering side and
    /// the tracing log can both point at the same hook.
    pub hook_id: String,
    pub options: Vec<PermissionOption>,
}

/// The answering client's verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionAnswer {
    pub allow: bool,
    /// Which `options` entry (if any) the client selected.
    pub selected_option_id: Option<String>,
    /// Optional remembered scope the client wants applied to future asks
    /// (e.g. "session", "always"). Interpretation/storage of remembered
    /// scope is a follow-up — v1 only carries it on the wire so the answer
    /// path isn't lossy from day one.
    pub remember: Option<String>,
}

/// Outcome of a permission-ask round trip, including the two fail-closed
/// paths that never reach a real human.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionOutcome {
    /// A subscriber answered within the timeout.
    Answered(PermissionAnswer),
    /// No `PermissionEvents` subscriber was attached when the ask fired.
    NoSubscriber,
    /// A subscriber was attached but the round trip did not complete
    /// within the caller's timeout (or the bridge's own internal one).
    TimedOut,
}

/// Bridge to the server's `PermissionEvents` subscriber registry. Kept as a
/// trait so `kaijutsu-kernel` never needs a capnp dependency — the real
/// implementation lives in `kaijutsu-server` (holding
/// `PermissionEvents::Client`s) and is wired onto the broker via
/// `Broker::set_permission_asker`.
#[async_trait]
pub trait PermissionAsker: Send + Sync + 'static {
    async fn ask(&self, request: PermissionAskRequest) -> PermissionOutcome;
}
