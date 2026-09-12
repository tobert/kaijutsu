//! `session/request_permission` — driving the approval ledger from ACP.
//!
//! `HookAction::Ask` (and `shell_write`'s gate) leave a durable row in the
//! approval ledger and wait; this module is the ACP side of answering one.
//! It does not talk to a bespoke permission wire — that wire
//! (`PermissionEvents::onAsk`, `ActorHandle::take_permission_asks`) is gone
//! (`docs/gate-and-shell-split.md`, "The shared seam: one ledger, one
//! announcement, one write path"). The ledger is the one durable record and
//! `kj ledger` is the one write path, from any surface, ACP included.
//!
//! The ledger round trip itself — `kj ledger list`/`show`/`allow`/`deny` —
//! is `kaijutsu_client::ledger`, shared with every client. What is ACP-only
//! here is deciding *whether this bridge is the one to answer* an ask, and
//! the `session/request_permission` call/response mapping.
//!
//! # Shape
//!
//! 1. [`start_permission_pump`] subscribes to `LedgerEvents::onChanged` —
//!    a broadcast of bare generation numbers, no ask id, no content
//!    (`ActorHandle::subscribe_ledger_events`).
//! 2. Each bump (or a `Lagged` warning that changes were missed) triggers
//!    [`poll_ledger`], which calls `kaijutsu_client::ledger::poll_new_asks`
//!    in an arbitrary live session's context — the ledger reads kernel-wide
//!    state, so which context it runs in doesn't matter.
//! 3. For every ask that call returns, inspect its durable reviewer. Only an
//!    ask assigned to this connection's authenticated character is offered.
//! 4. Otherwise the round trip is spawned (`cx.spawn`): a
//!    `session/request_permission` call to the client, and on answer,
//!    `kaijutsu_client::ledger::decide_ask` to write the decision back.
//!
//! # The kernel is the authority, and nothing expires
//!
//! There is no `PERMISSION_ASK_TIMEOUT` budget owned by this module, and
//! there is no kernel-side one either: the gate records an ask and returns,
//! and an unanswered ask stays answerable indefinitely
//! (`docs/gate-resume.md`). An ask this pump offers and nobody answers is
//! not a leak — it is the open question it looks like, cleared by a human
//! marking it abandoned. [`REQUEST_PERMISSION_TIMEOUT`] below bounds only
//! the outgoing `session/request_permission` call, so a wedged ACP client
//! (stdio never reads the request) cannot leave one of this pump's spawned
//! tasks parked forever. Answering late still works; there is no deadline
//! to beat.
//!
//! # Racing is fine and expected
//!
//! A human can answer the same ask with `kj ledger allow` from a shell
//! while this pump's `session/request_permission` prompt is still on the
//! client's screen — the ledger's `claim`+`decide` transaction makes
//! exactly one answerer win (`approval-ledger`'s guarantee 5). The loser's
//! `kj ledger allow|deny` comes back nonzero; the returned reason is recorded
//! at `info!`, so a race and an ineligible reviewer are distinguishable.
//!
//! # Not ours to answer
//!
//! A lead may review a coder context without an ACP session attached to that
//! coder. The request uses an existing ACP session for the same reviewer;
//! an ask with another reviewer stays pending for that reviewer.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SessionId, SessionNotification, SessionUpdate, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Client, ConnectionTo};
use kaijutsu_client::ledger::{self, AskInfo, PendingAsk};
use kaijutsu_types::PrincipalId;
use tokio::sync::broadcast;

use crate::bridge::KernelBridge;
use crate::rank;
use crate::session::SessionRegistry;
use crate::AcpBridge;

fn decision_failure_message(request_id: &str, verb: &str, reason: &str) -> String {
    format!("Approval {verb} for {request_id} did not apply: {reason}")
}

fn permission_prompt_failure_message(request_id: &str, reason: &str) -> String {
    format!("Approval prompt for {request_id} failed: {reason}; denying")
}

fn notify_decision_failure(cx: &ConnectionTo<Client>, session_id: &SessionId, message: String) {
    let _ = cx.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AgentMessageChunk(crate::update::text_chunk(&message)),
    ));
}

/// Bound on one outgoing `session/request_permission` call — NOT a budget
/// for the ledger ask itself (see module docs, "The kernel is the authority
/// and the timeout").
pub const REQUEST_PERMISSION_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether the ask's durable reviewer assignment belongs to this connection.
/// Missing provenance is not authority.
fn is_assigned_reviewer(reviewer_id: Option<PrincipalId>, me: PrincipalId) -> bool {
    reviewer_id == Some(me)
}

/// Subscribe to the kernel-wide ledger-change stream and drive the pump for
/// the life of the connection — the `.with_spawned` task
/// `lib.rs::serve_stdio` registers.
pub async fn start_permission_pump(bridge: &Arc<AcpBridge>, cx: ConnectionTo<Client>) {
    let generations = bridge.kernel.actor().subscribe_ledger_events();
    run_permission_pump(generations, bridge, cx).await;
}

/// Drain the ledger's generation-bump stream forever, polling the ledger
/// after each bump and offering every newly-seen, ours-to-answer ask to the
/// client. Never itself an error: a pump that failed should stop pumping,
/// not hang up the ACP connection.
pub async fn run_permission_pump(
    mut generations: broadcast::Receiver<i64>,
    bridge: &Arc<AcpBridge>,
    cx: ConnectionTo<Client>,
) {
    let mut seen: HashSet<String> = HashSet::new();
    loop {
        match generations.recv().await {
            Ok(_generation) => {}
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                // Not an error: the ledger is the authority, not this
                // stream, and the poll below observes the current truth
                // regardless of how many intermediate bumps were dropped.
                tracing::debug!(
                    missed,
                    "ledger-changed stream lagged; polling current state anyway"
                );
            }
            Err(broadcast::error::RecvError::Closed) => {
                tracing::info!("ledger-changed stream closed; permission pump exiting");
                return;
            }
        }
        poll_ledger(&bridge.kernel, &bridge.sessions, &cx, &mut seen).await;
    }
}

/// One poll: read every newly-seen pending ask and spawn a round trip for
/// each one assigned to this ACP connection's authenticated character.
async fn poll_ledger(
    kernel: &KernelBridge,
    sessions: &SessionRegistry,
    cx: &ConnectionTo<Client>,
    seen: &mut HashSet<String>,
) {
    let Some(admin_ctx) = sessions.any_context_id() else {
        // No live session anywhere — nowhere to run `kj` this tick. The
        // next generation bump (typically arriving once a session exists)
        // tries again.
        return;
    };

    let new_asks = match ledger::poll_new_asks(kernel.actor(), admin_ctx, seen).await {
        Ok(asks) => asks,
        Err(e) => {
            tracing::warn!(error = %e, "kj ledger list failed; skipping this poll");
            return;
        }
    };
    let me = match kernel.actor().whoami().await {
        Ok(identity) => identity.principal_id,
        Err(error) => {
            tracing::warn!(error = %error, "cannot identify ACP reviewer; skipping ledger poll");
            return;
        }
    };

    for PendingAsk { request_id: id, info: ask } in new_asks {
        let detail = match ledger::show_ask_detail(kernel.actor(), admin_ctx, &id).await {
            Ok(Some(detail)) => detail,
            Ok(None) => {
                tracing::warn!(request = %id, "ledger ask could not be decoded; retrying on the next change");
                continue;
            }
            Err(error) => {
                tracing::warn!(request = %id, error = %error, "cannot inspect ledger ask; retrying on the next change");
                continue;
            }
        };
        if !is_assigned_reviewer(detail.reviewer_id, me) {
            tracing::debug!(
                request = %id,
                reviewer = ?detail.reviewer_id,
                "ledger ask is assigned to another reviewer; skipping"
            );
            continue;
        }

        // A lead can review a coder's ask without attaching ACP directly to
        // the coder context. Prefer that context's session, then send the
        // request through any live session owned by this connection.
        let session_id = rank::session_id_of(ask.context_id);
        let session_id = if sessions.get(&session_id).is_some() {
            session_id
        } else if let Some(session_id) = sessions.any_session_id() {
            session_id
        } else {
            continue;
        };

        // Ours, and about to be offered exactly once.
        seen.insert(id.clone());

        let kernel = kernel.clone();
        let cx_task = cx.clone();
        let id_task = id.clone();
        if let Err(e) = cx.spawn(async move {
            answer_ask(&kernel, &cx_task, session_id, id_task, ask).await;
            Ok(())
        }) {
            tracing::warn!(request = %id, error = %e, "failed to spawn permission round trip");
        }
    }
}

/// Run one `session/request_permission` round trip and write the answer
/// back through `kaijutsu_client::ledger::decide_ask`.
async fn answer_ask(
    kernel: &KernelBridge,
    cx: &ConnectionTo<Client>,
    session_id: SessionId,
    request_id: String,
    ask: AskInfo,
) {
    let (options, kinds) = build_options();
    let title = if ask.description.is_empty() {
        request_id.clone()
    } else {
        ask.description
    };
    let request = permission_request(session_id.clone(), request_id.clone(), title, options);

    let allow = match tokio::time::timeout(
        REQUEST_PERMISSION_TIMEOUT,
        cx.send_request(request).block_task(),
    )
    .await
    {
        Ok(Ok(response)) => map_response(&response, &kinds),
        Ok(Err(e)) => {
            tracing::warn!(request = %request_id, error = %e, "permission ask errored answering the client; denying");
            notify_decision_failure(cx, &session_id, permission_prompt_failure_message(&request_id, &e.to_string()));
            false
        }
        Err(_) => {
            tracing::warn!(
                request = %request_id,
                timeout = ?REQUEST_PERMISSION_TIMEOUT,
                "permission ask timed out waiting on the client; denying"
            );
            notify_decision_failure(cx, &session_id, permission_prompt_failure_message(&request_id, "timed out waiting for the client"));
            false
        }
    };

    let verb = if allow { "allow" } else { "deny" };
    // The decision is authored in the work context. The ledger validates the
    // reviewer actor, so a coder cannot approve its own ask from any context.
    match ledger::decide_ask(kernel.actor(), ask.context_id, &request_id, allow).await {
        Ok(result) if result.exit_code == 0 => {
            tracing::info!(request = %request_id, verb, "ledger ask answered");
        }
        Ok(result) => {
            // A nonzero result is visible operationally. `AlreadyDecided`
            // is a race; every other refusal (including an ineligible
            // reviewer) needs its returned reason to diagnose the contract.
            tracing::info!(
                request = %request_id,
                verb,
                stderr = %result.stderr,
                "kj ledger {verb} did not apply"
            );
            notify_decision_failure(cx, &session_id, decision_failure_message(&request_id, verb, result.stderr.trim()));
        }
        Err(e) => {
            tracing::warn!(request = %request_id, verb, error = %e, "kj ledger {verb} errored");
            notify_decision_failure(cx, &session_id, decision_failure_message(&request_id, verb, &e.to_string()));
        }
    }
}

/// Ids for the synthesized allow/deny pair this bridge always offers: the
/// kernel's ledger asks carry no `options` of their own today (unlike the
/// retired `HookAction::Ask` wire, which reserved a slot for them), so this
/// is the only shape a client is ever offered.
const OPT_ALLOW: &str = "allow";
const OPT_DENY: &str = "deny";

/// Build the fixed ACP allow/deny option pair, plus the id→kind map
/// [`map_response`] needs to turn a selected id back into a verdict.
fn build_options() -> (Vec<PermissionOption>, [(&'static str, PermissionOptionKind); 2]) {
    let kinds = [
        (OPT_ALLOW, PermissionOptionKind::AllowOnce),
        (OPT_DENY, PermissionOptionKind::RejectOnce),
    ];
    let options = vec![
        PermissionOption::new(OPT_ALLOW, "Allow", PermissionOptionKind::AllowOnce),
        PermissionOption::new(OPT_DENY, "Deny", PermissionOptionKind::RejectOnce),
    ];
    (options, kinds)
}

/// Shape the outgoing `session/request_permission`.
fn permission_request(
    session_id: SessionId,
    tool_call_id: impl Into<agent_client_protocol::schema::v1::ToolCallId>,
    title: impl Into<String>,
    options: Vec<PermissionOption>,
) -> RequestPermissionRequest {
    RequestPermissionRequest::new(
        session_id,
        ToolCallUpdate::new(tool_call_id, ToolCallUpdateFields::new().title(title.into())),
        options,
    )
}

/// Read a client's answer against the id→kind map [`build_options`] built
/// for this ask. Anything this cannot place — a cancelled prompt, an
/// unrecognised option id, or a future `PermissionOptionKind` variant the
/// `#[non_exhaustive]` wire type gains later — is a **deny**. A permission
/// prompt is the one place where failing open on an unparsed answer would
/// be indefensible.
fn map_response(
    response: &RequestPermissionResponse,
    kinds: &[(&'static str, PermissionOptionKind); 2],
) -> bool {
    let RequestPermissionOutcome::Selected(selected) = &response.outcome else {
        return false;
    };
    let id = selected.option_id.0.as_ref();
    match kinds.iter().find(|(k, _)| *k == id).map(|(_, kind)| kind) {
        Some(PermissionOptionKind::AllowOnce) | Some(PermissionOptionKind::AllowAlways) => true,
        Some(PermissionOptionKind::RejectOnce) | Some(PermissionOptionKind::RejectAlways) => {
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{PermissionOptionId, SelectedPermissionOutcome};

    fn selected(response: &str) -> RequestPermissionResponse {
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(PermissionOptionId::new(response.to_string())),
        ))
    }

    #[test]
    fn build_options_offers_exactly_allow_and_deny() {
        let (options, kinds) = build_options();
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].option_id, PermissionOptionId::new(OPT_ALLOW));
        assert_eq!(options[0].kind, PermissionOptionKind::AllowOnce);
        assert_eq!(options[1].option_id, PermissionOptionId::new(OPT_DENY));
        assert_eq!(options[1].kind, PermissionOptionKind::RejectOnce);
        assert_eq!(kinds.len(), 2);
    }

    #[test]
    fn the_request_names_the_session_and_carries_both_options() {
        let (options, _) = build_options();
        let req = permission_request(SessionId::new("s"), "req-1", "rm -rf /", options);
        assert_eq!(req.session_id, SessionId::new("s"));
        assert_eq!(req.tool_call.tool_call_id.0.as_ref(), "req-1");
        assert_eq!(req.tool_call.fields.title.as_deref(), Some("rm -rf /"));
        assert_eq!(req.options.len(), 2);
    }

    #[test]
    fn selecting_allow_maps_to_true() {
        let (_, kinds) = build_options();
        assert!(map_response(&selected(OPT_ALLOW), &kinds));
    }

    #[test]
    fn selecting_deny_maps_to_false() {
        let (_, kinds) = build_options();
        assert!(!map_response(&selected(OPT_DENY), &kinds));
    }

    #[test]
    fn only_the_assigned_reviewer_is_offered_an_ask() {
        let reviewer = PrincipalId::new();
        assert!(is_assigned_reviewer(Some(reviewer), reviewer));
        assert!(!is_assigned_reviewer(None, reviewer));
        assert!(!is_assigned_reviewer(Some(PrincipalId::new()), reviewer));
    }

    #[test]
    fn decision_failures_name_the_action_and_kernel_reason() {
        assert_eq!(
            decision_failure_message("req-1", "allow", "already decided"),
            "Approval allow for req-1 did not apply: already decided"
        );
    }

    #[test]
    fn prompt_failures_tell_the_client_that_a_deny_follows() {
        assert_eq!(
            permission_prompt_failure_message("req-1", "timed out waiting for the client"),
            "Approval prompt for req-1 failed: timed out waiting for the client; denying"
        );
    }

    #[test]
    fn a_cancelled_prompt_denies() {
        let r = RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled);
        let (_, kinds) = build_options();
        assert!(!map_response(&r, &kinds));
    }

    #[test]
    fn an_option_id_we_never_offered_denies_rather_than_running_it() {
        let (_, kinds) = build_options();
        assert!(!map_response(&selected("some-future-option"), &kinds));
    }
}
