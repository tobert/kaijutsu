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
//! # Shape
//!
//! 1. [`start_permission_pump`] subscribes to `LedgerEvents::onChanged` —
//!    a broadcast of bare generation numbers, no ask id, no content
//!    (`ActorHandle::subscribe_ledger_events`).
//! 2. Each bump (or a `Lagged` warning that changes were missed) triggers
//!    [`poll_ledger`], which runs `kj ledger list` in an arbitrary live
//!    session's context — the verb reads kernel-wide state, so which
//!    context it runs in doesn't matter — and diffs the returned ids
//!    against a `seen` set so each ask is only offered to the client once.
//! 3. For every unseen id, `kj ledger show <id>` names the ask's own
//!    `context_id`. If no ACP session here is bound to that context, the
//!    ask is **skipped, not denied** — see "Not ours to answer" below.
//! 4. Otherwise the round trip is spawned (`cx.spawn`): a
//!    `session/request_permission` call to the client, and on answer, `kj
//!    ledger allow|deny <id>` to write the decision back.
//!
//! # The kernel is the authority and the timeout
//!
//! There is no `PERMISSION_ASK_TIMEOUT` budget owned by this module anymore.
//! The gate expires the ask itself at `effective_gate_wait()`
//! (`kaijutsu-kernel`'s `kj::gate`) — if the ACP client never answers,
//! the ask expires kernel-side and `kj ledger allow|deny` on it fails
//! loudly on its own. [`REQUEST_PERMISSION_TIMEOUT`] below bounds only the
//! outgoing `session/request_permission` call itself, so a wedged ACP
//! client (stdio never reads the request) can't leave one of this pump's
//! spawned tasks parked forever; it is not a substitute for the kernel's
//! own budget and answering late (or not at all) here just means the
//! kernel's own expiry wins instead.
//!
//! # Racing is fine and expected
//!
//! A human can answer the same ask with `kj ledger allow` from a shell
//! while this pump's `session/request_permission` prompt is still on the
//! client's screen — the ledger's `claim`+`decide` transaction makes
//! exactly one answerer win (`approval-ledger`'s guarantee 5). The loser's
//! `kj ledger allow|deny` comes back `AlreadyDecided`; this is logged at
//! `debug!` and otherwise ignored — it is not a failure, it is two players
//! sharing one ledger.
//!
//! # Not ours to answer
//!
//! The old pump denied any ask whose context had no live ACP session,
//! because ACP was the only possible answerer. That is no longer true: the
//! ledger is answerable from any surface, so a context with no ACP session
//! here may still have a human at a shell, or another connected client,
//! about to answer it. This pump now **skips** such an ask rather than
//! denying it — the single biggest behavioral change in this rewrite.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SessionId, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Client, ConnectionTo};
use kaijutsu_types::ContextId;
use tokio::sync::broadcast;

use crate::bridge::KernelBridge;
use crate::rank;
use crate::session::SessionRegistry;
use crate::AcpBridge;

/// Bound on one outgoing `session/request_permission` call — NOT a budget
/// for the ledger ask itself (see module docs, "The kernel is the authority
/// and the timeout").
pub const REQUEST_PERMISSION_TIMEOUT: Duration = Duration::from_secs(30);

/// Subscribe to the kernel-wide ledger-change stream and drive the pump for
/// the life of the connection — the `.with_spawned` task
/// `lib.rs::serve_stdio` registers.
pub async fn start_permission_pump(bridge: &Arc<AcpBridge>, cx: ConnectionTo<Client>) {
    let generations = bridge.kernel.actor().subscribe_ledger_events();
    run_permission_pump(generations, bridge, cx).await;
}

/// Drain the ledger's generation-bump stream forever, polling `kj ledger
/// list` after each bump and offering every newly-seen, ours-to-answer ask
/// to the client. Never itself an error: a pump that failed should stop
/// pumping, not hang up the ACP connection.
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

/// One poll: list pending asks, prune `seen`, and spawn a round trip for
/// each unseen ask that belongs to a context this bridge has a live ACP
/// session for.
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

    let ids = match list_pending(kernel, admin_ctx).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %e, "kj ledger list failed; skipping this poll");
            return;
        }
    };

    // Prune `seen` down to the ids still pending, so it doesn't grow
    // forever as asks get answered and drop off the list.
    let still_pending: HashSet<&str> = ids.iter().map(String::as_str).collect();
    seen.retain(|id| still_pending.contains(id.as_str()));

    for id in ids {
        if seen.contains(&id) {
            continue;
        }

        let Some(ask) = show_ask(kernel, admin_ctx, &id).await else {
            // A failed read is a slow read: leave the id UNSEEN so the next
            // generation bump retries it. Marking it here would make one
            // transient `kj ledger show` failure suppress the ask for as
            // long as it stays pending.
            continue;
        };
        let session_id = rank::session_id_of(ask.context_id);
        if sessions.get(&session_id).is_none() {
            // Not ours to answer (see module docs) — some other surface
            // (a shell, another client) may be about to.
            //
            // Deliberately NOT marked seen. A session can attach to that
            // context after the ask was raised — ACP sessions come and go
            // while the ledger row waits out the whole gate budget — and an
            // ask suppressed here would then never be offered to the client
            // that just arrived to answer it. Re-showing a foreign ask on
            // each bump is one cheap read; silently never offering it is a
            // prompt the human never sees.
            tracing::debug!(
                request = %id,
                context = %ask.context_id.short(),
                "ledger ask belongs to a context with no live ACP session here; skipping"
            );
            continue;
        }

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

/// One pending ask's `kj ledger show` fields, decoded once so the caller
/// doesn't hand around a raw `serde_json::Value`.
struct AskInfo {
    context_id: ContextId,
    description: String,
}

async fn list_pending(kernel: &KernelBridge, ctx: ContextId) -> anyhow::Result<Vec<String>> {
    let result = kernel
        .execute_kj(ctx, vec!["ledger".to_string(), "list".to_string()])
        .await?;
    if result.exit_code != 0 {
        anyhow::bail!("kj ledger list exited {}: {}", result.exit_code, result.stderr);
    }
    let ids = match result.data {
        Some(serde_json::Value::Array(items)) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    Ok(ids)
}

async fn show_ask(kernel: &KernelBridge, ctx: ContextId, request_id: &str) -> Option<AskInfo> {
    let result = match kernel
        .execute_kj(
            ctx,
            vec!["ledger".to_string(), "show".to_string(), request_id.to_string()],
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(request = %request_id, error = %e, "kj ledger show errored");
            return None;
        }
    };
    if result.exit_code != 0 {
        tracing::warn!(
            request = %request_id,
            exit_code = result.exit_code,
            stderr = %result.stderr,
            "kj ledger show failed"
        );
        return None;
    }
    let data = result.data?;
    let context_id = match data
        .get("context_id")
        .and_then(|v| v.as_str())
        .and_then(|s| ContextId::parse(s).ok())
    {
        Some(id) => id,
        None => {
            tracing::warn!(request = %request_id, "ledger ask has no parseable context_id; skipping");
            return None;
        }
    };
    let description = data
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_default();
    Some(AskInfo {
        context_id,
        description,
    })
}

/// Run one `session/request_permission` round trip and write the answer
/// back through `kj ledger allow|deny`.
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
    let request = permission_request(session_id, request_id.clone(), title, options);

    let allow = match tokio::time::timeout(
        REQUEST_PERMISSION_TIMEOUT,
        cx.send_request(request).block_task(),
    )
    .await
    {
        Ok(Ok(response)) => map_response(&response, &kinds),
        Ok(Err(e)) => {
            tracing::warn!(request = %request_id, error = %e, "permission ask errored answering the client; denying");
            false
        }
        Err(_) => {
            tracing::warn!(
                request = %request_id,
                timeout = ?REQUEST_PERMISSION_TIMEOUT,
                "permission ask timed out waiting on the client; denying"
            );
            false
        }
    };

    let verb = if allow { "allow" } else { "deny" };
    // The decide verb doesn't care which context it runs in — the ask's
    // own context is as good as any.
    match kernel
        .execute_kj(
            ask.context_id,
            vec!["ledger".to_string(), verb.to_string(), request_id.clone()],
        )
        .await
    {
        Ok(result) if result.exit_code == 0 => {
            tracing::debug!(request = %request_id, verb, "ledger ask answered");
        }
        Ok(result) => {
            // A race with another answerer (AlreadyDecided) lands here too
            // — expected, not a failure (module docs, "Racing is fine").
            tracing::debug!(
                request = %request_id,
                verb,
                stderr = %result.stderr,
                "kj ledger {verb} did not apply (already decided, or expired)"
            );
        }
        Err(e) => {
            tracing::warn!(request = %request_id, verb, error = %e, "kj ledger {verb} errored");
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
