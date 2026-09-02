//! The approval ledger round trip: list pending asks, read one ask's
//! fields, and write back an allow/deny decision — the machinery every
//! client that offers asks to a player drives the same way.
//!
//! This is not a bespoke wire: there is no `PermissionEvents::onAsk`
//! (`docs/gate-and-shell-split.md`, "The shared seam: one ledger, one
//! announcement, one write path"). The ledger is the one durable record and
//! `kj ledger` is the one write path, from any surface. This module drives
//! that path through [`ActorHandle::execute_kj`]; it opens no bespoke
//! connection and holds no state beyond the `seen` set a caller passes in.
//!
//! # The kernel is the authority, and nothing expires
//!
//! There is no ask-timeout budget owned here, and there is no kernel-side
//! one either: the gate records an ask and returns, and an unanswered ask
//! stays answerable indefinitely (`docs/gate-resume.md`). Bounding an
//! outgoing round trip to a *player* (a client waiting on a human) is each
//! caller's own concern, not this module's.
//!
//! # Every call here authors blocks
//!
//! `kj ledger list` and `show` run through [`ActorHandle::execute_kj`], and a
//! `kj` run in a context leaves a tool-call/tool-result pair in that context's
//! block log. Poll on a timer and the transcript fills with the client's own
//! bookkeeping. Drive [`poll_new_asks`] from
//! `ActorHandle::subscribe_ledger_events` (one poll per generation bump) plus
//! one poll at startup, never from a clock.
//!
//! # Racing is fine and expected
//!
//! A human can answer the same ask with `kj ledger allow` from a shell while
//! another surface's prompt is still on someone's screen — the ledger's
//! `claim`+`decide` transaction makes exactly one answerer win
//! (`approval-ledger`'s guarantee 5). The loser's [`decide_ask`] comes back
//! with a nonzero exit code (`AlreadyDecided`); this is not a failure, it is
//! two players sharing one ledger, and it is the caller's call whether to
//! log it.

use std::collections::HashSet;

use kaijutsu_types::ContextId;

use crate::actor::{ActorHandle, CallError};
use crate::rpc::KjExecutionResult;

/// One pending ask's `kj ledger show` fields, decoded once so callers don't
/// hand around a raw `serde_json::Value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskInfo {
    pub context_id: ContextId,
    pub description: String,
}

/// One ask [`poll_new_asks`] has not offered to the caller before: its
/// request id plus the fields [`show_ask`] decoded for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAsk {
    pub request_id: String,
    pub info: AskInfo,
}

/// Failure decoding a `kj ledger` round trip: either the RPC call itself
/// failed, or it ran and the verb reported a nonzero exit.
#[derive(Debug, Clone, thiserror::Error)]
pub enum LedgerError {
    #[error(transparent)]
    Call(#[from] CallError),
    #[error("kj ledger {verb} exited {exit_code}: {stderr}")]
    Failed {
        verb: &'static str,
        exit_code: i32,
        stderr: String,
    },
}

/// List every pending ask's request id, via `kj ledger list` run in `ctx`
/// (the ledger is kernel-wide state, so which live context the command runs
/// in doesn't matter).
pub async fn list_pending(actor: &ActorHandle, ctx: ContextId) -> Result<Vec<String>, LedgerError> {
    let result = actor
        .execute_kj(ctx, vec!["ledger".to_string(), "list".to_string()])
        .await?;
    if result.exit_code != 0 {
        return Err(LedgerError::Failed {
            verb: "list",
            exit_code: result.exit_code,
            stderr: result.stderr,
        });
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

/// Read one ask's context id and description via `kj ledger show`. `None`
/// on any failure to run or parse — a caller should treat that as "try
/// again next poll", not as a decision (see [`poll_new_asks`]).
pub async fn show_ask(actor: &ActorHandle, ctx: ContextId, request_id: &str) -> Option<AskInfo> {
    let result = match actor
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

/// Write an allow/deny decision back through `kj ledger allow|deny`. Only
/// the RPC round trip can fail here — a nonzero exit (a race lost to
/// another answerer, or an expired ask) comes back as `Ok` with that exit
/// code in [`KjExecutionResult`], for the caller to log or ignore as it
/// sees fit (see module docs, "Racing is fine").
pub async fn decide_ask(
    actor: &ActorHandle,
    ctx: ContextId,
    request_id: &str,
    allow: bool,
) -> Result<KjExecutionResult, CallError> {
    let verb = if allow { "allow" } else { "deny" };
    actor
        .execute_kj(
            ctx,
            vec!["ledger".to_string(), verb.to_string(), request_id.to_string()],
        )
        .await
}

/// Prune `seen` down to the ids still pending, and return the pending ids
/// not yet in `seen`, in list order. Pure and unit-testable without a
/// kernel — the one decision in [`poll_new_asks`] that isn't an RPC round
/// trip.
fn diff_new(ids: &[String], seen: &mut HashSet<String>) -> Vec<String> {
    let still_pending: HashSet<&str> = ids.iter().map(String::as_str).collect();
    seen.retain(|id| still_pending.contains(id.as_str()));
    ids.iter().filter(|id| !seen.contains(id.as_str())).cloned().collect()
}

/// One poll of the ledger: list pending asks, prune `seen` to what's still
/// pending, and read the fields of every ask not yet in `seen`.
///
/// This does **not** insert into `seen` — that is the caller's job, once it
/// has decided which of the returned asks it will actually offer (e.g.
/// filtering by whether it owns the ask's context). An ask this call
/// returns but the caller declines to answer must stay out of `seen`, so it
/// is offered again if ownership changes on a later poll.
///
/// An ask whose `kj ledger show` fails is left out of both `seen` and the
/// returned list — a transient read failure retries next poll rather than
/// permanently suppressing the ask.
pub async fn poll_new_asks(
    actor: &ActorHandle,
    ctx: ContextId,
    seen: &mut HashSet<String>,
) -> Result<Vec<PendingAsk>, LedgerError> {
    let ids = list_pending(actor, ctx).await?;
    let new_ids = diff_new(&ids, seen);

    let mut new_asks = Vec::with_capacity(new_ids.len());
    for id in new_ids {
        if let Some(info) = show_ask(actor, ctx, &id).await {
            new_asks.push(PendingAsk { request_id: id, info });
        }
    }
    Ok(new_asks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_new_reports_ids_not_yet_seen() {
        let mut seen = HashSet::new();
        seen.insert("a".to_string());
        let ids = vec!["a".to_string(), "b".to_string()];
        assert_eq!(diff_new(&ids, &mut seen), vec!["b".to_string()]);
    }

    #[test]
    fn diff_new_prunes_seen_ids_that_are_no_longer_pending() {
        let mut seen = HashSet::new();
        seen.insert("answered".to_string());
        seen.insert("still-pending".to_string());
        let ids = vec!["still-pending".to_string()];

        assert_eq!(diff_new(&ids, &mut seen), Vec::<String>::new());
        assert_eq!(seen, HashSet::from(["still-pending".to_string()]));
    }

    #[test]
    fn diff_new_with_nothing_seen_returns_everything() {
        let mut seen = HashSet::new();
        let ids = vec!["x".to_string(), "y".to_string()];
        assert_eq!(diff_new(&ids, &mut seen), ids);
    }
}
