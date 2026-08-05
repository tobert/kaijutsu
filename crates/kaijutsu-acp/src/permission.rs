//! `session/request_permission` — **STUB: auto-allow.**
//!
//! # What is stubbed and why
//!
//! ACP lets an agent ask its client before running a tool. kaijutsu currently
//! has nowhere to ask *from*: `HookAction` is `Invoke | Deny | Log` — there is
//! no `Ask` that suspends a tool and waits for an answer (docs/acp.md gap #2).
//! Until that lands, this bridge **never asks and always allows**, which is
//! honest about kaijutsu's shared-trust stance: every player is already inside
//! the trust boundary, and the kernel is not enforcing between them.
//!
//! It is *not* honest about an untrusted frontend. A phone on the open
//! internet driving this bridge has the kernel's full authority. Do not point
//! an untrusted client at a kaijutsu-acp bridge until the Ask pathway exists.
//!
//! # The seam
//!
//! A parallel lane is building `HookAction::Ask` + a `PermissionEvents`
//! server→client callback (the `ElicitationEvents::onRequest` blocking-call
//! shape is the template). When it lands, the wiring here is:
//!
//! 1. The kernel raises an Ask over the new callback; the bridge receives it
//!    with a tool-call identity and a reply channel.
//! 2. Build the outgoing ACP request with [`permission_request`].
//! 3. `cx.send_request(req)` from a spawned task (never from inside the
//!    dispatch loop — that deadlocks) and await it.
//! 4. Feed the response through [`interpret_outcome`] and answer the kernel.
//!
//! Everything in steps 2 and 4 is written and tested below. Only step 1's
//! transport is missing, and it is deliberately not built here — this crate
//! does not own the wire schema.

use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionId, ToolCallUpdate,
    ToolCallUpdateFields,
};

/// What the bridge tells the kernel to do with a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Run it.
    Allow,
    /// Refuse it. Also what a cancelled prompt means — a client that walked
    /// away has not consented.
    Deny,
}

/// Who decides. One implementation today; the Ask pathway adds a second.
pub trait PermissionPolicy: Send + Sync + std::fmt::Debug {
    /// Decide without a round trip, or `None` to mean "ask the client".
    ///
    /// `None` is unreachable today: nothing raises an Ask. It exists so the
    /// call site that will handle it is already written and typed.
    fn decide_locally(&self, tool_name: &str) -> Option<PermissionDecision>;
}

/// **STUB.** Allows everything without asking. See the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct AutoAllow;

impl PermissionPolicy for AutoAllow {
    fn decide_locally(&self, _tool_name: &str) -> Option<PermissionDecision> {
        Some(PermissionDecision::Allow)
    }
}

/// Option ids the bridge offers and understands. Stable strings, because the
/// client echoes one back.
pub const OPT_ALLOW_ONCE: &str = "allow-once";
pub const OPT_ALLOW_ALWAYS: &str = "allow-always";
pub const OPT_REJECT_ONCE: &str = "reject-once";
pub const OPT_REJECT_ALWAYS: &str = "reject-always";

/// The four choices offered for a tool call, in the order a client should
/// render them.
pub fn permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new(OPT_ALLOW_ONCE, "Allow once", PermissionOptionKind::AllowOnce),
        PermissionOption::new(
            OPT_ALLOW_ALWAYS,
            "Always allow",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(OPT_REJECT_ONCE, "Reject", PermissionOptionKind::RejectOnce),
        PermissionOption::new(
            OPT_REJECT_ALWAYS,
            "Always reject",
            PermissionOptionKind::RejectAlways,
        ),
    ]
}

/// Shape the outgoing `session/request_permission`. Ready for the Ask pathway;
/// nothing calls it yet.
pub fn permission_request(
    session_id: SessionId,
    tool_call_id: impl Into<agent_client_protocol::schema::v1::ToolCallId>,
    title: impl Into<String>,
) -> RequestPermissionRequest {
    RequestPermissionRequest::new(
        session_id,
        ToolCallUpdate::new(tool_call_id, ToolCallUpdateFields::new().title(title.into())),
        permission_options(),
    )
}

/// Read a client's answer.
///
/// Anything we do not recognise is a **Deny**. A permission prompt is the one
/// place where failing open on an unparsed answer would be indefensible — the
/// silent fallback that runs the command anyway is exactly the bug class we
/// refuse to ship.
pub fn interpret_outcome(response: &RequestPermissionResponse) -> PermissionDecision {
    match &response.outcome {
        RequestPermissionOutcome::Selected(sel) => match sel.option_id.0.as_ref() {
            OPT_ALLOW_ONCE | OPT_ALLOW_ALWAYS => PermissionDecision::Allow,
            _ => PermissionDecision::Deny,
        },
        // Cancelled, plus any variant added later — `RequestPermissionOutcome`
        // is `#[non_exhaustive]`.
        _ => PermissionDecision::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{PermissionOptionId, SelectedPermissionOutcome};

    fn selected(option: &str) -> RequestPermissionResponse {
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(PermissionOptionId::new(option.to_string())),
        ))
    }

    #[test]
    fn the_stub_allows_everything_without_asking() {
        // Pinning the stub's behaviour so replacing it is a deliberate,
        // test-breaking act rather than a quiet drift.
        assert_eq!(
            AutoAllow.decide_locally("rm"),
            Some(PermissionDecision::Allow)
        );
        assert_eq!(
            AutoAllow.decide_locally("anything at all"),
            Some(PermissionDecision::Allow)
        );
    }

    #[test]
    fn all_four_permission_kinds_are_offered() {
        let opts = permission_options();
        assert_eq!(opts.len(), 4);
        assert_eq!(opts[0].kind, PermissionOptionKind::AllowOnce);
        assert_eq!(opts[1].kind, PermissionOptionKind::AllowAlways);
        assert_eq!(opts[2].kind, PermissionOptionKind::RejectOnce);
        assert_eq!(opts[3].kind, PermissionOptionKind::RejectAlways);
    }

    #[test]
    fn the_request_names_the_tool_call_it_is_about() {
        let req = permission_request(SessionId::new("s"), "block-key", "rm -rf /");
        assert_eq!(req.session_id, SessionId::new("s"));
        assert_eq!(req.tool_call.tool_call_id.0.as_ref(), "block-key");
        assert_eq!(req.tool_call.fields.title.as_deref(), Some("rm -rf /"));
        assert_eq!(req.options.len(), 4);
    }

    #[test]
    fn allow_options_allow() {
        assert_eq!(
            interpret_outcome(&selected(OPT_ALLOW_ONCE)),
            PermissionDecision::Allow
        );
        assert_eq!(
            interpret_outcome(&selected(OPT_ALLOW_ALWAYS)),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn reject_options_deny() {
        assert_eq!(
            interpret_outcome(&selected(OPT_REJECT_ONCE)),
            PermissionDecision::Deny
        );
        assert_eq!(
            interpret_outcome(&selected(OPT_REJECT_ALWAYS)),
            PermissionDecision::Deny
        );
    }

    #[test]
    fn a_cancelled_prompt_denies() {
        let r = RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled);
        assert_eq!(interpret_outcome(&r), PermissionDecision::Deny);
    }

    #[test]
    fn an_unrecognised_option_denies_rather_than_running_it() {
        assert_eq!(
            interpret_outcome(&selected("some-future-option")),
            PermissionDecision::Deny
        );
    }
}
