//! Exactly-one-answerer claiming (guarantee 5).
//!
//! SQLite has no `SKIP LOCKED`. The pattern is `BEGIN IMMEDIATE` (takes the
//! write lock up front, so a second concurrent claim on *any* row blocks
//! at `BEGIN` rather than racing inside it) plus one atomic `UPDATE ...
//! WHERE status = 'pending' ... RETURNING`, so the row-count the driver
//! sees (0 or 1) *is* the decision — there is no separate read-then-write
//! window for a second claimant to land in between. Verified with real
//! concurrent connections against an on-disk temp file in
//! `tests/claim_race.rs` (an in-memory `Connection::open_in_memory()`
//! database would not exercise this at all — two in-memory connections
//! don't share a database).

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::ask::{get_approval, row_to_approval};
use crate::error::{LedgerError, Result};
use crate::events;
use crate::time::now_millis;
use crate::types::{ApprovalRow, EventKind};

const APPROVAL_COLUMNS: &str = "request_id, context_id, principal_id, origin, instance, tool, hook_id, \
     description, authorized_label, rc_run_id, status, created_at, \
     expires_at, claimed_at, claimed_by, decided_at, decided_by, decided_option, \
     remember_scope, auto_reason";

/// Claim one specific ask for `claimant`. On success, the returned row's
/// `status` is `claimed`. On a losing race, `status` was not `pending`
/// when this transaction's `UPDATE` ran — [`LedgerError::NotClaimable`]
/// names what it actually was (either someone else's claim landed first,
/// or the ask is already terminal). [`LedgerError::NotFound`] if
/// `request_id` doesn't exist at all.
pub fn claim(conn: &Connection, request_id: &str, claimant: &[u8]) -> Result<ApprovalRow> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let now = now_millis();

    let claimed = tx
        .query_row(
            &format!(
                "UPDATE approvals SET status = 'claimed', claimed_by = ?1, claimed_at = ?2
                 WHERE request_id = ?3 AND status = 'pending'
                 RETURNING {APPROVAL_COLUMNS}"
            ),
            params![claimant, now, request_id],
            row_to_approval,
        )
        .optional()?;

    let Some(row) = claimed else {
        // Nothing changed under this transaction — no event to append, and
        // nothing to commit. Dropping `tx` here rolls back (the default
        // `DropBehavior`), which is a no-op against an UPDATE that matched
        // zero rows, but keeps the "only a successful claim writes"
        // invariant obviously true by construction rather than by the
        // UPDATE's own no-op-ness.
        drop(tx);
        return Err(match get_approval(conn, request_id)? {
            None => LedgerError::NotFound(request_id.to_string()),
            Some(row) => LedgerError::NotClaimable {
                request_id: request_id.to_string(),
                status: row.status.to_string(),
            },
        });
    };

    events::append(&tx, request_id, EventKind::Claimed, Some(claimant), None, None, None, None)?;
    tx.commit()?;
    Ok(row)
}

/// Claim the single oldest still-`pending` ask, if any. Same atomicity as
/// [`claim`] — under `BEGIN IMMEDIATE`, the subquery picking the oldest
/// pending row and the `UPDATE` that claims it run within one exclusive
/// writer lock, so two callers racing `claim_next` never claim the same
/// row (the second one's `BEGIN IMMEDIATE` simply blocks until the first
/// commits, by which point that row is no longer `pending`).
pub fn claim_next(conn: &Connection, claimant: &[u8]) -> Result<Option<ApprovalRow>> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let now = now_millis();

    let claimed = tx
        .query_row(
            &format!(
                "UPDATE approvals SET status = 'claimed', claimed_by = ?1, claimed_at = ?2
                 WHERE request_id = (
                     SELECT request_id FROM approvals WHERE status = 'pending'
                     ORDER BY created_at LIMIT 1
                 ) AND status = 'pending'
                 RETURNING {APPROVAL_COLUMNS}"
            ),
            params![claimant, now],
            row_to_approval,
        )
        .optional()?;

    match claimed {
        None => {
            // Nothing pending — a clean, ordinary outcome, not an error.
            tx.commit()?;
            Ok(None)
        }
        Some(row) => {
            events::append(&tx, &row.request_id, EventKind::Claimed, Some(claimant), None, None, None, None)?;
            tx.commit()?;
            Ok(Some(row))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::ask::create_ask;
    use crate::error::LedgerError;
    use crate::fixtures::{minimal_ask, open_memory};
    use crate::types::{ApprovalStatus, EventKind};

    use super::*;

    #[test]
    fn claim_succeeds_from_pending_and_writes_an_event() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();

        let row = claim(&conn, &request_id, b"alice").unwrap();
        assert_eq!(row.status, ApprovalStatus::Claimed);
        assert_eq!(row.claimed_by.as_deref(), Some(b"alice".as_slice()));
        assert!(row.claimed_at.is_some());

        let events = crate::ask::list_events(&conn, &request_id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Claimed);
        assert_eq!(events[0].actor.as_deref(), Some(b"alice".as_slice()));
    }

    #[test]
    fn a_second_claim_on_the_same_pending_request_loses() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();

        claim(&conn, &request_id, b"alice").unwrap();
        let err = claim(&conn, &request_id, b"bob").unwrap_err();
        assert!(
            matches!(&err, LedgerError::NotClaimable { status, .. } if status == "claimed"),
            "expected NotClaimable(status=claimed), got {err:?}"
        );

        // The loser's attempt must not have appended a second claim event —
        // only the winner's.
        let events = crate::ask::list_events(&conn, &request_id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].actor.as_deref(), Some(b"alice".as_slice()));
    }

    #[test]
    fn claim_on_unknown_request_is_not_found() {
        let conn = open_memory();
        let err = claim(&conn, "does-not-exist", b"alice").unwrap_err();
        assert!(matches!(err, LedgerError::NotFound(id) if id == "does-not-exist"));
    }

    #[test]
    fn claim_next_returns_none_when_nothing_pending() {
        let conn = open_memory();
        assert!(claim_next(&conn, b"alice").unwrap().is_none());
    }

    #[test]
    fn claim_next_picks_the_oldest_pending_first() {
        let conn = open_memory();
        let first = create_ask(&conn, &minimal_ask()).unwrap();
        // `created_at` has millisecond resolution; without this, two
        // inserts in the same millisecond would tie and the ORDER BY
        // wouldn't deterministically prefer `first`.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = create_ask(&conn, &minimal_ask()).unwrap();

        let claimed_first = claim_next(&conn, b"alice").unwrap().expect("one pending row");
        assert_eq!(claimed_first.request_id, first);
        let claimed_second = claim_next(&conn, b"alice").unwrap().expect("the other pending row");
        assert_eq!(claimed_second.request_id, second);
        assert!(claim_next(&conn, b"alice").unwrap().is_none());
    }
}
