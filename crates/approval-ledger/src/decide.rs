//! Deciding, expiring, and abandoning an ask — guarantees 2 and 6.
//!
//! All three transitions share one shape: an atomic `UPDATE ... WHERE
//! status IN ('pending', 'claimed') ... RETURNING`, inside a single `BEGIN
//! IMMEDIATE` transaction that also appends the matching
//! `approval_events` row. A row that has already left `pending`/`claimed`
//! matches zero rows in the `UPDATE` — it is *never mutated*, satisfying
//! guarantee 6 by construction rather than by a separate check-then-write
//! race. `schema.rs`'s `approvals_decided_is_immutable` trigger is the
//! backstop for any write path that skips this module's `WHERE` clause.
//!
//! There is exactly one predicate a caller should use to ask "is this
//! actually allowed": [`crate::types::ApprovalStatus::is_allowed`]. It
//! returns `true` for precisely one variant. Every terminal path this
//! module produces — `denied`, `expired`, `abandoned` — and every
//! not-yet-decided state (`pending`, `claimed`) reads `false` through that
//! one predicate. There is no second place in this crate that decides
//! "was this allowed" differently (guarantee 2).

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::ask::row_to_approval;
use crate::error::{LedgerError, Result};
use crate::events;
use crate::time::now_millis;
use crate::types::{ApprovalRow, EventKind};

const APPROVAL_COLUMNS: &str = "request_id, context_id, principal_id, origin, instance, tool, hook_id, \
     description, plan_digest, authorized_label, rc_run_id, status, created_at, \
     expires_at, claimed_at, claimed_by, decided_at, decided_by, decided_option, \
     remember_scope, auto_reason";

/// What a decide call is presenting. `decided_by` is `None` for an
/// auto-decision (a matching [`crate::rules::RuleRow`] fired) and `Some`
/// for a human's answer — either way `auto_reason` and `decided_by` are
/// stored as given, so a read-back can always tell which happened.
#[derive(Debug, Default)]
pub struct DecideInput<'a> {
    pub allow: bool,
    pub decided_by: Option<&'a [u8]>,
    pub decided_option: Option<&'a str>,
    pub remember_scope: Option<&'a str>,
    pub auto_reason: Option<&'a str>,
}

/// Decide a `pending` or `claimed` ask. On success, `status` becomes
/// `allowed` or `denied` per `input.allow`.
///
/// If the ask already reached a terminal status, this is a **late
/// answer**: it does not overwrite anything (the `UPDATE` matches zero
/// rows), but what was presented is still appended to `approval_events`
/// as a `late_decision` row before returning
/// [`LedgerError::AlreadyDecided`] — the history is recorded, never
/// silently dropped and never silently applied (guarantee 6).
pub fn decide(conn: &Connection, request_id: &str, input: DecideInput) -> Result<ApprovalRow> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let now = now_millis();
    let new_status = if input.allow { "allowed" } else { "denied" };

    let decided = tx
        .query_row(
            &format!(
                "UPDATE approvals SET status = ?1, decided_at = ?2, decided_by = ?3,
                     decided_option = ?4, remember_scope = ?5, auto_reason = ?6
                 WHERE request_id = ?7 AND status IN ('pending', 'claimed')
                 RETURNING {APPROVAL_COLUMNS}"
            ),
            params![
                new_status,
                now,
                input.decided_by,
                input.decided_option,
                input.remember_scope,
                input.auto_reason,
                request_id
            ],
            row_to_approval,
        )
        .optional()?;

    if let Some(row) = decided {
        events::append(
            &tx,
            request_id,
            EventKind::Decided,
            input.decided_by,
            input.decided_option,
            input.remember_scope,
            input.auto_reason,
            None,
        )?;
        tx.commit()?;
        return Ok(row);
    }

    // Zero rows matched: either the request doesn't exist, or it already
    // left pending/claimed. Read the current row inside the SAME
    // transaction (still holding the IMMEDIATE write lock) so this can't
    // race a concurrent decide of its own.
    let existing = tx
        .query_row(
            &format!("SELECT {APPROVAL_COLUMNS} FROM approvals WHERE request_id = ?1"),
            params![request_id],
            row_to_approval,
        )
        .optional()?;

    match existing {
        None => Err(LedgerError::NotFound(request_id.to_string())),
        Some(row) => {
            events::append(
                &tx,
                request_id,
                EventKind::LateDecision,
                input.decided_by,
                input.decided_option,
                input.remember_scope,
                input.auto_reason,
                Some(&format!("rejected: already {}", row.status)),
            )?;
            tx.commit()?;
            Err(LedgerError::AlreadyDecided {
                request_id: request_id.to_string(),
                status: row.status.to_string(),
            })
        }
    }
}

/// Fail an ask closed on a timeout: `pending`/`claimed` → `expired`.
/// Guarantee 2's timeout leg. A caller that never set `expires_at` on the
/// ask owns deciding when (or whether) to call this — this crate does not
/// run a clock of its own.
pub fn expire(conn: &Connection, request_id: &str) -> Result<ApprovalRow> {
    transition(conn, request_id, "expired", EventKind::Expired, None)
}

/// Fail an ask closed because whatever was waiting on it gave up:
/// `pending`/`claimed` → `abandoned`. Guarantee 2's abandonment leg.
pub fn abandon(conn: &Connection, request_id: &str, reason: Option<&str>) -> Result<ApprovalRow> {
    transition(conn, request_id, "abandoned", EventKind::Abandoned, reason)
}

fn transition(
    conn: &Connection,
    request_id: &str,
    new_status: &str,
    event_kind: EventKind,
    note: Option<&str>,
) -> Result<ApprovalRow> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;

    let updated = tx
        .query_row(
            &format!(
                "UPDATE approvals SET status = ?1
                 WHERE request_id = ?2 AND status IN ('pending', 'claimed')
                 RETURNING {APPROVAL_COLUMNS}"
            ),
            params![new_status, request_id],
            row_to_approval,
        )
        .optional()?;

    if let Some(row) = updated {
        events::append(&tx, request_id, event_kind, None, None, None, None, note)?;
        tx.commit()?;
        return Ok(row);
    }

    let existing = tx
        .query_row(
            &format!("SELECT {APPROVAL_COLUMNS} FROM approvals WHERE request_id = ?1"),
            params![request_id],
            row_to_approval,
        )
        .optional()?;

    match existing {
        None => Err(LedgerError::NotFound(request_id.to_string())),
        Some(row) => Err(LedgerError::AlreadyDecided {
            request_id: request_id.to_string(),
            status: row.status.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use crate::ask::{create_ask, get_approval, list_events};
    use crate::error::LedgerError;
    use crate::fixtures::{minimal_ask, open_memory};
    use crate::types::{ApprovalStatus, EventKind};

    use super::*;

    #[test]
    fn decide_allow_reaches_the_one_true_allowed_state() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();

        let row = decide(
            &conn,
            &request_id,
            DecideInput { allow: true, decided_by: Some(b"alice"), decided_option: Some("allow_once"), ..Default::default() },
        )
        .unwrap();
        assert_eq!(row.status, ApprovalStatus::Allowed);
        assert!(row.status.is_allowed());
        assert_eq!(row.decided_option.as_deref(), Some("allow_once"));

        let events = list_events(&conn, &request_id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Decided);
    }

    #[test]
    fn decide_deny_is_not_allowed() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();
        let row = decide(&conn, &request_id, DecideInput { allow: false, decided_by: Some(b"alice"), ..Default::default() }).unwrap();
        assert_eq!(row.status, ApprovalStatus::Denied);
        assert!(!row.status.is_allowed());
    }

    /// Guarantee 2, exhaustively: every terminal path this module can
    /// produce other than an explicit allow must read `is_allowed() ==
    /// false`. If a future variant is added to `ApprovalStatus` without
    /// updating `is_allowed`, this is the test that would need a matching
    /// new arm here too — not a reason `is_allowed` should grow a catch-all.
    #[test]
    fn every_non_allow_terminal_path_is_not_allowed() {
        let conn = open_memory();

        let denied = create_ask(&conn, &minimal_ask()).unwrap();
        let denied = decide(&conn, &denied, DecideInput { allow: false, ..Default::default() }).unwrap();
        assert!(!denied.status.is_allowed());

        let expired = create_ask(&conn, &minimal_ask()).unwrap();
        let expired = expire(&conn, &expired).unwrap();
        assert!(!expired.status.is_allowed());

        let abandoned = create_ask(&conn, &minimal_ask()).unwrap();
        let abandoned = abandon(&conn, &abandoned, Some("operator gave up waiting")).unwrap();
        assert!(!abandoned.status.is_allowed());

        // Pending/claimed are not-yet-decided, not "allowed" either.
        let pending = create_ask(&conn, &minimal_ask()).unwrap();
        let pending = get_approval(&conn, &pending).unwrap().unwrap();
        assert!(!pending.status.is_allowed());
    }

    /// Guarantee 6's core claim: a late answer arriving after the ask
    /// already reached a terminal state must NOT change that state, but
    /// must still be recorded.
    #[test]
    fn a_late_decision_after_expiry_does_not_overwrite_and_is_still_recorded() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();
        expire(&conn, &request_id).unwrap();

        let err = decide(
            &conn,
            &request_id,
            DecideInput { allow: true, decided_by: Some(b"late-alice"), decided_option: Some("allow_once"), ..Default::default() },
        )
        .unwrap_err();
        assert!(matches!(&err, LedgerError::AlreadyDecided { status, .. } if status == "expired"));

        // The row itself is untouched — still `expired`, never flipped to
        // `allowed` by the late attempt.
        let row = get_approval(&conn, &request_id).unwrap().unwrap();
        assert_eq!(row.status, ApprovalStatus::Expired);
        assert!(!row.status.is_allowed());

        // But the attempt is not lost: a `late_decision` event names who
        // tried and what they presented.
        let events = list_events(&conn, &request_id).unwrap();
        assert_eq!(events.len(), 2, "expired event, then the rejected late attempt");
        assert_eq!(events[0].kind, EventKind::Expired);
        assert_eq!(events[1].kind, EventKind::LateDecision);
        assert_eq!(events[1].actor.as_deref(), Some(b"late-alice".as_slice()));
        assert_eq!(events[1].decided_option.as_deref(), Some("allow_once"));
    }

    #[test]
    fn re_deciding_an_already_allowed_request_is_refused() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();
        decide(&conn, &request_id, DecideInput { allow: true, ..Default::default() }).unwrap();

        let err = decide(&conn, &request_id, DecideInput { allow: false, ..Default::default() }).unwrap_err();
        assert!(matches!(&err, LedgerError::AlreadyDecided { status, .. } if status == "allowed"));

        // Still allowed — a denial attempt cannot flip it either.
        let row = get_approval(&conn, &request_id).unwrap().unwrap();
        assert_eq!(row.status, ApprovalStatus::Allowed);
    }

    #[test]
    fn decide_on_unknown_request_is_not_found() {
        let conn = open_memory();
        let err = decide(&conn, "does-not-exist", DecideInput::default()).unwrap_err();
        assert!(matches!(err, LedgerError::NotFound(id) if id == "does-not-exist"));
    }

    #[test]
    fn expire_and_abandon_on_unknown_request_are_not_found() {
        let conn = open_memory();
        assert!(matches!(expire(&conn, "nope").unwrap_err(), LedgerError::NotFound(_)));
        assert!(matches!(abandon(&conn, "nope", None).unwrap_err(), LedgerError::NotFound(_)));
    }

    #[test]
    fn expiring_an_already_claimed_request_still_expires_it() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();
        crate::claim::claim(&conn, &request_id, b"alice").unwrap();
        let row = expire(&conn, &request_id).unwrap();
        assert_eq!(row.status, ApprovalStatus::Expired);
    }
}
