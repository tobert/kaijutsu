//! Typed reads of the approval ledger: the asks a client shows, one ask's
//! full record, and the asks that changed after a generation. The RPC
//! methods (`listAsks`, `getAsk`, the `LedgerEvents` push) and `kj ledger
//! list|show` all read through here, so the shell and the wire agree.
//!
//! Each read takes the `KernelDb` guard once, so a listing and the
//! generation it reports come from one consistent view.

use approval_ledger::types::{ApprovalRow, ApprovalStatus, Origin};
use kaijutsu_types::{
    AskDecision, AskDetail, AskEnv, AskFilter, AskOrigin, AskReassignment, AskStatus, AskSummary, AskView, BlockId, ContextId,
    PrincipalId, PrincipalRef,
};

use crate::kernel_db::KernelDb;

/// A ledger listing: the asks, the generation they were read at, and how
/// many matched before the limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskListing {
    pub generation: i64,
    pub asks: Vec<AskSummary>,
    pub total: u64,
}

/// Asks matching `filter`, oldest first in the queue and newest first in
/// history. A `status` decides the view by its own terminality.
pub fn list_asks(db: &KernelDb, filter: &AskFilter) -> Result<AskListing, String> {
    let (statuses, newest_first) = match filter.status {
        Some(status) => {
            let status = ledger_status(status);
            (vec![status], status.is_terminal())
        }
        None => match filter.view {
            AskView::Queue => (vec![ApprovalStatus::Pending], false),
            AskView::History => (
                vec![ApprovalStatus::Allowed, ApprovalStatus::Denied, ApprovalStatus::Expired, ApprovalStatus::Abandoned],
                true,
            ),
        },
    };
    let ledger_filter = approval_ledger::ask::AskListFilter {
        statuses,
        origin: filter.origin.map(ledger_origin),
        since_ms: filter.since_ms,
        limit: filter.limit.map_or(i64::MAX, i64::from),
        newest_first,
    };
    let conn = db.conn_for_ledger();
    let generation = approval_ledger::generation::current(conn).map_err(|e| e.to_string())?;
    let (rows, total) = approval_ledger::ask::list_asks_filtered(conn, &ledger_filter).map_err(|e| e.to_string())?;
    let asks = rows.iter().map(|row| summary(db, row)).collect::<Result<Vec<_>, _>>()?;
    Ok(AskListing { generation, asks, total: u64::try_from(total).map_err(|_| format!("negative ask count {total}"))? })
}

/// One ask's full record, or `None` when no such ask exists.
pub fn get_ask(db: &KernelDb, request_id: &str) -> Result<Option<AskDetail>, String> {
    let conn = db.conn_for_ledger();
    let Some(row) = approval_ledger::ask::get_approval(conn, request_id).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let env = approval_ledger::ask::load_ask_env(conn, request_id).map_err(|e| e.to_string())?
        .into_iter().map(|e| AskEnv { name: e.name, value: e.value }).collect();
    let redeemed_at_ms = approval_ledger::ask::redeemed_at(conn, request_id).map_err(|e| e.to_string())?;
    let publication_abandoned = db.approval_pair_abandoned_reason(request_id).map_err(|e| e.to_string())?;
    let tool_call_block_id: Option<BlockId> = db.approval_tool_call(request_id).map_err(|e| e.to_string())?;
    // A cancellation records who cancelled and `cancel`; an expiry or a
    // restart sweep records neither.
    let decision = if row.decided_by.is_some() || row.decided_option.is_some() || row.auto_reason.is_some() {
        Some(AskDecision {
            decided_by: row.decided_by.as_deref().map(|raw| principal_ref(db, raw)).transpose()?,
            option: row.decided_option.clone(),
            remember_scope: row.remember_scope.clone(),
            auto_reason: row.auto_reason.clone(),
        })
    } else {
        None
    };
    let mut reassignments = Vec::new();
    for event in approval_ledger::ask::list_events(conn, request_id).map_err(|e| e.to_string())? {
        if event.kind != approval_ledger::types::EventKind::Escalated {
            continue;
        }
        let (Some(to), Some(by)) = (event.to_reviewer.as_deref(), event.actor.as_deref()) else {
            // An escalation recorded before the reviewers were columns.
            continue;
        };
        reassignments.push(AskReassignment {
            from: event.from_reviewer.as_deref().map(|raw| principal_ref(db, raw)).transpose()?,
            to: principal_ref(db, to)?,
            by: principal_ref(db, by)?,
            at_ms: event.created_at,
        });
    }
    Ok(Some(AskDetail {
        summary: summary(db, &row)?,
        instance: row.instance,
        tool: row.tool,
        hook_id: row.hook_id,
        label: row.authorized_label,
        tool_call_block_id,
        exec_source: row.exec_source,
        cwd: row.cwd,
        env,
        decision,
        redeemed_at_ms,
        publication_abandoned,
        reassignments,
    }))
}

/// Every ask whose latest change is newer than `generation`, each in its
/// current state, and the generation they bring a reader to. The returned
/// generation is at least `generation`, so a reader can always store it.
pub fn asks_changed_since(db: &KernelDb, generation: i64) -> Result<(i64, Vec<AskSummary>), String> {
    let conn = db.conn_for_ledger();
    let current = approval_ledger::generation::current(conn).map_err(|e| e.to_string())?;
    let changed = approval_ledger::changes::changed_since(conn, generation).map_err(|e| e.to_string())?;
    let asks = changed.iter().map(|(_, row)| summary(db, row)).collect::<Result<Vec<_>, _>>()?;
    Ok((current.max(generation), asks))
}

/// The card for one ledger row.
pub(crate) fn summary(db: &KernelDb, row: &ApprovalRow) -> Result<AskSummary, String> {
    let statements = approval_ledger::ask::load_ask_statements(db.conn_for_ledger(), &row.request_id)
        .map_err(|e| e.to_string())?
        .into_iter().map(|s| s.statement.rendered).collect();
    Ok(AskSummary {
        request_id: row.request_id.clone(),
        status: crate::kj::gate::ask_status(row.status),
        origin: ask_origin(row.origin),
        context_id: ContextId::try_from_slice(&row.context_id),
        description: row.description.clone(),
        statements,
        requester: Some(principal_ref(db, &row.principal_id)?),
        performer: row.actor_id.as_deref().map(|raw| principal_ref(db, raw)).transpose()?,
        reviewer: row.reviewer_id.as_deref().map(|raw| principal_ref(db, raw)).transpose()?,
        created_at_ms: row.created_at,
        decided_at_ms: row.decided_at,
    })
}

/// A stored principal and its current name. A malformed id is a defect in
/// whatever wrote the row, so it fails the read.
fn principal_ref(db: &KernelDb, raw: &[u8]) -> Result<PrincipalRef, String> {
    let id = PrincipalId::try_from_slice(raw)
        .ok_or_else(|| format!("malformed {}-byte principal id in the ledger", raw.len()))?;
    let name = db.get_character(id)
        .map_err(|e| format!("could not resolve principal name: {e}"))?
        .map_or_else(|| id.short(), |character| character.name);
    Ok(PrincipalRef { id, name })
}

pub(crate) fn ask_origin(origin: Origin) -> AskOrigin {
    match origin {
        Origin::Hook => AskOrigin::Hook,
        Origin::HookResult => AskOrigin::HookResult,
        Origin::ShellGate => AskOrigin::ShellGate,
        Origin::KjVerb => AskOrigin::KjVerb,
    }
}

pub(crate) fn ledger_origin(origin: AskOrigin) -> Origin {
    match origin {
        AskOrigin::Hook => Origin::Hook,
        AskOrigin::HookResult => Origin::HookResult,
        AskOrigin::ShellGate => Origin::ShellGate,
        AskOrigin::KjVerb => Origin::KjVerb,
    }
}

pub(crate) fn ledger_status(status: AskStatus) -> ApprovalStatus {
    match status {
        AskStatus::Pending => ApprovalStatus::Pending,
        AskStatus::Claimed => ApprovalStatus::Claimed,
        AskStatus::Allowed => ApprovalStatus::Allowed,
        AskStatus::Denied => ApprovalStatus::Denied,
        AskStatus::Expired => ApprovalStatus::Expired,
        AskStatus::Abandoned => ApprovalStatus::Abandoned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The origin and status maps are hand-written across a crate boundary,
    /// so a swapped pair would typecheck. Comparing rendered names catches it.
    #[test]
    fn origin_and_status_maps_agree_with_the_ledger_names() {
        for origin in [Origin::Hook, Origin::HookResult, Origin::ShellGate, Origin::KjVerb] {
            assert_eq!(ask_origin(origin).as_str(), origin.as_str());
            assert_eq!(ledger_origin(ask_origin(origin)), origin);
        }
        for status in [
            AskStatus::Pending, AskStatus::Claimed, AskStatus::Allowed,
            AskStatus::Denied, AskStatus::Expired, AskStatus::Abandoned,
        ] {
            assert_eq!(ledger_status(status).to_string(), status.as_str());
        }
    }
}
