//! `session/request_permission` — the kernel-side Ask pathway wired live.
//!
//! `HookAction::Ask` (D-57, docs/acp.md gap #2) is real: a hook can suspend a
//! tool call and block on a `PermissionEvents::onAsk` round trip to whichever
//! client subscribed. `kaijutsu-client::ActorHandle::take_permission_asks`
//! hands out the kernel-wide envelope stream (re-armed on every reconnect —
//! see `actor.rs`'s `connect_handshake`); this module turns each envelope
//! into an ACP `session/request_permission` call against the matching live
//! session and turns the client's answer back into a
//! `kaijutsu_client::PermissionAskAnswer`.
//!
//! # Fail-closed, end to end
//!
//! The kernel already denies an `Ask` when nobody subscribed or nobody
//! answered in time (`mcp::permission`'s no-subscriber/timeout policy,
//! `warn!`-logged). This module extends the same posture to every failure
//! mode ON THIS SIDE of the wire:
//!
//! - no live ACP session for the ask's `contextId` → deny, `warn!`
//! - the ACP client errors answering `session/request_permission` → deny, `warn!`
//! - the ACP client never answers within [`PERMISSION_ASK_TIMEOUT`] → deny, `warn!`
//! - a selected option whose kind we cannot place → deny (see [`map_response`])
//!
//! Nothing here ever runs a tool call because the surrounding machinery
//! failed quietly. CLAUDE.md: "Silent fallbacks are often a mistake."
//!
//! # The seam, now closed
//!
//! 1. [`run_permission_pump`] drains `ActorHandle::take_permission_asks()`
//!    forever, as the bridge's `.with_spawned` task (`lib.rs::serve_stdio`) —
//!    NOT inside the ACP dispatch loop, so blocking on
//!    `cx.send_request(...).block_task()` per ask is safe here (the dispatch
//!    loop this module used to warn about is a different task entirely).
//! 2. `rank::session_id_of` resolves the envelope's `contextId` to an ACP
//!    session id — no side table, same mapping `session/load` uses.
//! 3. [`build_options`] shapes the outgoing options ([`permission_request`]),
//!    [`map_response`] shapes the answer.
//!
//! Asks are processed one at a time off the shared channel: a slow or
//! unanswered prompt delays whatever queues up behind it rather than fanning
//! out concurrent `session/request_permission` calls. Acceptable for a
//! prototype — permission asks are rare and interactive — but worth revisiting
//! if that stops being true (docs/issues.md).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SessionId, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Client, ConnectionTo};
use kaijutsu_client::{PermissionAskAnswer, PermissionAskEnvelope, PermissionOptionInfo};
use tokio::sync::mpsc;

use crate::rank;
use crate::session::SessionRegistry;
use crate::AcpBridge;

/// Round-trip budget for one `session/request_permission` call, mirrored
/// from the kernel's own `DEFAULT_PERMISSION_ASK_TIMEOUT` (`kaijutsu-kernel`'s
/// `mcp::permission`). Defense in depth, not the authority: the kernel's
/// timeout is what actually bounds the hooked call; this one exists so a
/// wedged ACP client (stdio never reads the request, say) doesn't leave this
/// bridge's pump stuck past the point the kernel already gave up.
pub const PERMISSION_ASK_TIMEOUT: Duration = Duration::from_secs(30);

/// Take the bridge's permission-ask stream and drive it for the life of the
/// connection — the `.with_spawned` task `lib.rs::serve_stdio` registers.
///
/// `take_permission_asks()` returning `None` means either this is a second
/// call on the same actor (shouldn't happen — `serve_stdio` calls it once)
/// or the actor's own subscribe attempt never landed a working channel in
/// the first place (it always creates the channel; only the kernel-side
/// subscribe underneath can silently fail, best-effort, per `actor.rs`).
/// Either way there is nothing to drain, so this logs loudly and returns
/// rather than busy-looping — every `Ask` for this bridge's whole
/// connection then falls through to the kernel's own no-subscriber
/// fail-closed default (`mcp::permission`), same as before this seam
/// existed at all.
pub async fn start_permission_pump(bridge: &Arc<AcpBridge>, cx: ConnectionTo<Client>) {
    match bridge.kernel.actor().take_permission_asks() {
        Some(asks) => run_permission_pump(asks, &bridge.sessions, cx).await,
        None => tracing::error!(
            "no permission-ask receiver available for this connection — every \
             Ask will fail closed on the kernel's no-subscriber default"
        ),
    }
}

/// Drain the kernel's permission-ask stream forever, answering each one
/// against the matching live ACP session (or denying — see the module docs).
///
/// Returns when the stream closes (the actor shut down, or nobody ever
/// subscribed — `ActorHandle::take_permission_asks` returned `None`, which
/// callers should log loudly since it means every Ask will fail closed for
/// this bridge's whole life). Never itself an error: same reasoning as
/// `session::run_pump` — a pump that failed should stop pumping, not hang up
/// the ACP connection.
pub async fn run_permission_pump(
    asks: mpsc::Receiver<PermissionAskEnvelope>,
    sessions: &SessionRegistry,
    cx: ConnectionTo<Client>,
) {
    run_permission_pump_with_timeout(asks, sessions, cx, PERMISSION_ASK_TIMEOUT).await
}

/// [`run_permission_pump`] with an injectable timeout — test-overridable
/// (mirrors the kernel's own `DEFAULT_PERMISSION_ASK_TIMEOUT`, docs/acp.md)
/// so the timeout-denies path can be exercised without an actual 30s wait.
pub async fn run_permission_pump_with_timeout(
    mut asks: mpsc::Receiver<PermissionAskEnvelope>,
    sessions: &SessionRegistry,
    cx: ConnectionTo<Client>,
    timeout: Duration,
) {
    while let Some(envelope) = asks.recv().await {
        let answer = resolve_and_ask(sessions, &cx, &envelope, timeout).await;
        // A dropped-without-sending answer channel resolves the kernel's ask
        // as a denial too (`PermissionAskEnvelope`'s doc comment in
        // kaijutsu-client) — so a failed send here (the kernel already gave
        // up waiting) needs no special handling.
        let _ = envelope.answer.send(answer);
    }
    tracing::info!("permission-ask stream closed; pump exiting");
}

/// Resolve one envelope's session and, if live, run the ACP round trip.
async fn resolve_and_ask(
    sessions: &SessionRegistry,
    cx: &ConnectionTo<Client>,
    envelope: &PermissionAskEnvelope,
    timeout: Duration,
) -> PermissionAskAnswer {
    let session_id = rank::session_id_of(envelope.context_id);
    if sessions.get(&session_id).is_none() {
        tracing::warn!(
            context = %envelope.context_id.short(),
            request_id = %envelope.request_id,
            tool = %envelope.tool,
            hook = %envelope.hook_id,
            "permission ask for a context with no live ACP session — denying"
        );
        return deny();
    }

    let (options, kinds) = build_options(&envelope.options);
    // The kernel already falls back to "{instance}.{tool}" when a hook's
    // `AskSpec::description` is `None` (`hook_table.rs`'s `AskSpec` doc), so
    // `description` should never actually be empty by the time it reaches
    // here — this is a defensive second line, not the primary fallback.
    let title = if envelope.description.is_empty() {
        format!("{}.{}", envelope.instance, envelope.tool)
    } else {
        envelope.description.clone()
    };
    // `request_id` is the one identifier the kernel guarantees is unique
    // per-ask; there is no natural ACP tool-call id to reuse (the hook fires
    // before any `BlockKind::ToolCall` exists for a PreCall Ask).
    let request = permission_request(session_id, envelope.request_id.clone(), title, options);

    match tokio::time::timeout(timeout, cx.send_request(request).block_task()).await {
        Ok(Ok(response)) => map_response(&response, &kinds),
        Ok(Err(e)) => {
            tracing::warn!(
                request_id = %envelope.request_id,
                error = %e,
                "permission ask errored answering the client — denying"
            );
            deny()
        }
        Err(_) => {
            tracing::warn!(
                request_id = %envelope.request_id,
                timeout = ?timeout,
                "permission ask timed out waiting on the client — denying"
            );
            deny()
        }
    }
}

fn deny() -> PermissionAskAnswer {
    PermissionAskAnswer {
        allow: false,
        selected_option_id: None,
        remember: None,
    }
}

/// Ids the bridge synthesizes when the kernel sends no options at all — v1's
/// `AskSpec` carries a description only (`hook_table.rs`), so this is the
/// path every real ask takes today. Stable strings only in the sense that
/// they must round-trip through [`map_response`] within a single ask; they
/// have no meaning to the kernel, which only sees `allow`/`remember`.
const OPT_ALLOW: &str = "allow";
const OPT_DENY: &str = "deny";

/// Kernel option kinds are free text shaped to fit ACP's four kinds
/// (`kaijutsu.capnp`'s `PermissionOption.kind` doc: "allow_once" /
/// "allow_always" / "reject_once" / "reject_always"). Unrecognised text maps
/// to `RejectOnce` — the SAFEST reading, not the most permissive: if a hook
/// author's option kind can't be placed, picking it must not look like
/// consent to run something.
fn map_kind(kind: &str) -> PermissionOptionKind {
    match kind {
        "allow_once" => PermissionOptionKind::AllowOnce,
        "allow_always" => PermissionOptionKind::AllowAlways,
        "reject_once" => PermissionOptionKind::RejectOnce,
        "reject_always" => PermissionOptionKind::RejectAlways,
        _ => PermissionOptionKind::RejectOnce,
    }
}

/// Build the ACP option list for an ask, plus the id→kind map
/// [`map_response`] needs to turn a selected id back into an allow/deny
/// verdict. Empty kernel options (see [`OPT_ALLOW`]'s doc) synthesize a
/// plain allow/deny pair instead of offering the client nothing to pick.
fn build_options(
    kernel_options: &[PermissionOptionInfo],
) -> (Vec<PermissionOption>, HashMap<String, PermissionOptionKind>) {
    if kernel_options.is_empty() {
        let kinds = HashMap::from([
            (OPT_ALLOW.to_string(), PermissionOptionKind::AllowOnce),
            (OPT_DENY.to_string(), PermissionOptionKind::RejectOnce),
        ]);
        let options = vec![
            PermissionOption::new(OPT_ALLOW, "Allow", PermissionOptionKind::AllowOnce),
            PermissionOption::new(OPT_DENY, "Deny", PermissionOptionKind::RejectOnce),
        ];
        return (options, kinds);
    }

    let mut kinds = HashMap::with_capacity(kernel_options.len());
    let options = kernel_options
        .iter()
        .map(|o| {
            let kind = map_kind(&o.kind);
            kinds.insert(o.id.clone(), kind);
            PermissionOption::new(o.id.clone(), o.label.clone(), kind)
        })
        .collect();
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

/// Read a client's answer, using the id→kind map [`build_options`] built for
/// this same ask to turn the selected option back into allow/remember.
///
/// Anything this function cannot place — a cancelled prompt, an unrecognised
/// option id, or a future `PermissionOptionKind` variant the
/// `#[non_exhaustive]` wire type gains later — is a **Deny** with no option
/// credited. A permission prompt is the one place where failing open on an
/// unparsed answer would be indefensible — the silent fallback that runs the
/// command anyway is exactly the bug class we refuse to ship.
fn map_response(
    response: &RequestPermissionResponse,
    kinds: &HashMap<String, PermissionOptionKind>,
) -> PermissionAskAnswer {
    let RequestPermissionOutcome::Selected(selected) = &response.outcome else {
        // `Cancelled`, plus any variant added later — the outcome enum is
        // `#[non_exhaustive]`.
        return deny();
    };
    let id = selected.option_id.0.to_string();
    match kinds.get(id.as_str()) {
        Some(PermissionOptionKind::AllowOnce) => PermissionAskAnswer {
            allow: true,
            selected_option_id: Some(id),
            remember: None,
        },
        Some(PermissionOptionKind::AllowAlways) => PermissionAskAnswer {
            allow: true,
            selected_option_id: Some(id),
            remember: Some("always".to_string()),
        },
        Some(PermissionOptionKind::RejectAlways) => PermissionAskAnswer {
            allow: false,
            selected_option_id: Some(id),
            remember: Some("always".to_string()),
        },
        // A recognised, explicit "no" — still worth echoing which option was
        // picked, unlike the truly-unplaceable cases below.
        Some(PermissionOptionKind::RejectOnce) => PermissionAskAnswer {
            allow: false,
            selected_option_id: Some(id),
            remember: None,
        },
        // An id we never offered, or a future non_exhaustive kind we don't
        // recognise — deny with no option to credit, since we can't say
        // what was actually picked.
        _ => deny(),
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

    // ── build_options / map_kind ────────────────────────────────────────

    #[test]
    fn empty_kernel_options_synthesize_a_plain_allow_deny_pair() {
        let (options, kinds) = build_options(&[]);
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].option_id, PermissionOptionId::new(OPT_ALLOW));
        assert_eq!(options[0].kind, PermissionOptionKind::AllowOnce);
        assert_eq!(options[1].option_id, PermissionOptionId::new(OPT_DENY));
        assert_eq!(options[1].kind, PermissionOptionKind::RejectOnce);
        assert_eq!(kinds.len(), 2);
    }

    #[test]
    fn kernel_options_round_trip_ids_and_labels() {
        let kernel = vec![PermissionOptionInfo {
            id: "opt-1".into(),
            label: "Allow forever".into(),
            kind: "allow_always".into(),
        }];
        let (options, kinds) = build_options(&kernel);
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].option_id, PermissionOptionId::new("opt-1"));
        assert_eq!(options[0].name, "Allow forever");
        assert_eq!(options[0].kind, PermissionOptionKind::AllowAlways);
        assert_eq!(kinds.get("opt-1"), Some(&PermissionOptionKind::AllowAlways));
    }

    #[test]
    fn all_four_kernel_kind_strings_map_onto_acp_kinds() {
        assert_eq!(map_kind("allow_once"), PermissionOptionKind::AllowOnce);
        assert_eq!(map_kind("allow_always"), PermissionOptionKind::AllowAlways);
        assert_eq!(map_kind("reject_once"), PermissionOptionKind::RejectOnce);
        assert_eq!(map_kind("reject_always"), PermissionOptionKind::RejectAlways);
    }

    #[test]
    fn an_unrecognised_kernel_kind_maps_to_the_safest_reading() {
        assert_eq!(map_kind("whatever a hook author typed"), PermissionOptionKind::RejectOnce);
        assert_eq!(map_kind(""), PermissionOptionKind::RejectOnce);
    }

    // ── permission_request ──────────────────────────────────────────────

    #[test]
    fn the_request_names_the_session_and_carries_every_option() {
        let (options, _) = build_options(&[]);
        let req = permission_request(SessionId::new("s"), "req-1", "rm -rf /", options);
        assert_eq!(req.session_id, SessionId::new("s"));
        assert_eq!(req.tool_call.tool_call_id.0.as_ref(), "req-1");
        assert_eq!(req.tool_call.fields.title.as_deref(), Some("rm -rf /"));
        assert_eq!(req.options.len(), 2);
    }

    // ── map_response ────────────────────────────────────────────────────

    #[test]
    fn allow_once_allows_without_remembering() {
        let (_, kinds) = build_options(&[]);
        let answer = map_response(&selected(OPT_ALLOW), &kinds);
        assert_eq!(
            answer,
            PermissionAskAnswer {
                allow: true,
                selected_option_id: Some(OPT_ALLOW.to_string()),
                remember: None,
            }
        );
    }

    #[test]
    fn allow_always_allows_and_remembers() {
        let kernel = vec![PermissionOptionInfo {
            id: "opt-1".into(),
            label: "Always allow".into(),
            kind: "allow_always".into(),
        }];
        let (_, kinds) = build_options(&kernel);
        let answer = map_response(&selected("opt-1"), &kinds);
        assert_eq!(
            answer,
            PermissionAskAnswer {
                allow: true,
                selected_option_id: Some("opt-1".to_string()),
                remember: Some("always".to_string()),
            }
        );
    }

    #[test]
    fn reject_once_denies_and_credits_the_option_without_remembering() {
        let (_, kinds) = build_options(&[]);
        let answer = map_response(&selected(OPT_DENY), &kinds);
        assert_eq!(
            answer,
            PermissionAskAnswer {
                allow: false,
                selected_option_id: Some(OPT_DENY.to_string()),
                remember: None,
            }
        );
    }

    #[test]
    fn reject_always_denies_and_remembers() {
        let kernel = vec![PermissionOptionInfo {
            id: "opt-1".into(),
            label: "Always reject".into(),
            kind: "reject_always".into(),
        }];
        let (_, kinds) = build_options(&kernel);
        let answer = map_response(&selected("opt-1"), &kinds);
        assert_eq!(
            answer,
            PermissionAskAnswer {
                allow: false,
                selected_option_id: Some("opt-1".to_string()),
                remember: Some("always".to_string()),
            }
        );
    }

    #[test]
    fn a_cancelled_prompt_denies() {
        let r = RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled);
        let (_, kinds) = build_options(&[]);
        assert_eq!(map_response(&r, &kinds), deny());
    }

    #[test]
    fn an_option_id_we_never_offered_denies_rather_than_running_it() {
        let (_, kinds) = build_options(&[]);
        assert_eq!(map_response(&selected("some-future-option"), &kinds), deny());
    }
}
