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
     description, authorized_label, rc_run_id, status, created_at, \
     expires_at, claimed_at, claimed_by, decided_at, decided_by, decided_option, \
     remember_scope, auto_reason";

/// Who is answering, and from where.
///
/// An answer that carries an identity must also name the context it came
/// from, so [`ensure_not_self_approval`] cannot be skipped by a caller that
/// simply forgets to pass one. The auto-decision path opts out by carrying no
/// `Answerer` at all — which is the same signal the ledger already uses to
/// tell a classifier's decision from a person's (`crate::ask::list_history`).
#[derive(Clone, Copy, Debug)]
pub struct Answerer<'a> {
    /// Stored as `approvals.decided_by`.
    pub principal: &'a [u8],
    /// The answering context, compared against `approvals.context_id`.
    /// `None` is refused: a caller that cannot name its context cannot show
    /// it is not the author.
    pub context: Option<&'a [u8]>,
}

/// What a decide call is presenting. `decided_by` is `None` for an
/// auto-decision (a matching [`crate::rules::RuleRow`] fired) and `Some`
/// for a human's answer — either way `auto_reason` and the stored
/// `decided_by` reflect what was given, so a read-back can always tell which
/// happened.
#[derive(Debug, Default)]
pub struct DecideInput<'a> {
    pub allow: bool,
    pub decided_by: Option<Answerer<'a>>,
    pub decided_option: Option<&'a str>,
    pub remember_scope: Option<&'a str>,
    pub auto_reason: Option<&'a str>,
}

/// Refuse an answer from the context that raised the ask — no self-approval
/// (`docs/gate-and-shell-split.md`, "No self-approval — the gate's own answer
/// path"). Peer contexts may answer each other; only the author is refused.
///
/// Call this **before claiming**. [`decide`] calls it too, so the invariant
/// holds for every caller, but a claim taken first would leave the ask
/// `claimed` by the one seat that may not answer it — locking out the seats
/// that may.
///
/// Every refusal appends an `approval_refusals` row before returning
/// [`LedgerError::SelfApproval`]: a refusal that is only returned to its
/// caller is invisible to the measurement the gate is tuned on.
pub fn ensure_not_self_approval(
    conn: &Connection,
    request_id: &str,
    answerer: Answerer<'_>,
) -> Result<()> {
    let Some(row) = crate::ask::get_approval(conn, request_id)? else {
        return Err(LedgerError::NotFound(request_id.to_string()));
    };

    let reason = match answerer.context {
        Some(ctx) if ctx != row.context_id.as_slice() => return Ok(()),
        Some(_) => "this context raised the ask",
        None => "the answer names no context",
    };

    events::append_refusal(
        conn,
        request_id,
        "self_approval",
        Some(answerer.principal),
        answerer.context,
    )?;
    Err(LedgerError::SelfApproval {
        request_id: request_id.to_string(),
        reason,
    })
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
    // The backstop, not the primary check: a caller should call this before
    // claiming (see [`ensure_not_self_approval`]). Here it guarantees the
    // invariant for every caller regardless. An auto-decision carries no
    // `Answerer` and is untouched — a classifier is a different system from
    // the one that raised the ask.
    if let Some(answerer) = input.decided_by {
        ensure_not_self_approval(conn, request_id, answerer)?;
    }

    let decided_by = input.decided_by.map(|a| a.principal);
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
                decided_by,
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
            decided_by,
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
                decided_by,
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

/// Abandon every still-`pending` ask, once at kernel cold start — the
/// sweep that makes the restart rule real: an ask must not survive a
/// kernel restart, because the machinery to safely resume an approved
/// action across one is where the exactly-once risk lives, and a
/// duplicated destructive action is worse than an honestly buried ask. A
/// human who answers a swept ask is told nothing happened and must ask
/// again — `reason` is what `kj ledger show` displays to explain why, so
/// callers should say the restart happened and that a retry is the next
/// step, not just "abandoned". Returns how many asks this call moved.
///
/// **Sweeps `claimed` as well as `pending`**, via
/// [`ask::list_unresolved`] rather than [`ask::list_pending`]. The queue
/// view hides a `claimed` row because an answerer is working it and a
/// second answerer would step on the claim — true while that answerer's
/// process is alive, and only then. At a cold start there is no claimant,
/// so a `claimed` row is an answerer that died mid-decision, and it is
/// reachable from neither the queue nor the history: leaving it is
/// leaving a row that can never be seen or answered again.
///
/// **Does not touch `allowed`/`denied`.** Those are answered but possibly
/// not yet redeemed (`redeem_ask`) — abandoning one would silently
/// destroy a human's answer that is still good to deliver. `abandoned`
/// and `expired` are already terminal and untouched for the same reason
/// [`abandon`] never overwrites a terminal row.
///
/// One [`abandon`] call per row — each already opens its own `BEGIN
/// IMMEDIATE` transaction ([`transition`]'s doc), and SQLite has no
/// nested transactions on one connection, so this cannot be one
/// transaction wrapped around the whole sweep. A row that raced away from
/// non-terminal between [`ask::list_unresolved`]'s read and this call's own
/// `abandon` ([`LedgerError::AlreadyDecided`]) is not a sweep failure —
/// something else already gave that row a terminal state, which is
/// exactly this sweep's goal for it, reached by a different path; it is
/// skipped and not counted. Any other error is a real database failure,
/// not a race, and aborts the sweep immediately rather than being
/// swallowed and continuing past it — rows already abandoned by this call
/// stay abandoned regardless (each one already committed on its own), but
/// a database that cannot complete a write is not one this function
/// should keep hammering.
pub fn abandon_unresolved_on_restart(conn: &Connection, reason: &str) -> Result<usize> {
    let unresolved = crate::ask::list_unresolved(conn)?;
    let mut swept = 0usize;
    for row in unresolved {
        match abandon(conn, &row.request_id, Some(reason)) {
            Ok(_) => swept += 1,
            Err(LedgerError::AlreadyDecided { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(swept)
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

/// Redeem a single-use, already-`allowed` ask (`docs/gate-resume.md`'s
/// "an answered ask is redeemable" half). Returns `Ok(true)` if THIS call
/// performed the redemption, `Ok(false)` if the answer was already
/// delivered to an earlier call. Errors with [`LedgerError::NotDecided`]
/// if the ask carries no answer at all — see that variant's doc for why
/// this must not collapse into `Ok(false)`.
///
/// **A denial is an answer, and is redeemed like an approval.** An allowed
/// ask authorizes one execution; a denied ask delivers one refusal. Both
/// are spent afterward. Redeeming only approvals would leave a denied
/// caller retrying, finding nothing to redeem, and creating a second
/// identical ask — forever, with the answer already given and no way for a
/// human to stop it by answering again. One question, one answer,
/// delivered once. `docs/gate-resume.md` in kaijutsu carries the reasoning.
///
/// The caller decides what an answer *means*; this function only records
/// that it was delivered. Read the status (via
/// [`crate::ask::find_redeemable`] or `get_approval`) to know which it was.
///
/// Atomicity is the `approval_redemptions.request_id` PRIMARY KEY, not a
/// SELECT-then-INSERT: the `INSERT OR IGNORE` below either changes one row
/// (this call won) or zero (someone else's redemption already exists), and
/// that row count — not a prior read — is the decision. `BEGIN IMMEDIATE`
/// (`claim.rs`'s pattern) is not needed to arbitrate this race: unlike a
/// `pending → claimed` transition, there is no window where two callers
/// could each believe they are about to win before either writes, because
/// SQLite itself, not this function, is what decides the `INSERT`'s
/// outcome. The transaction below exists only so the redemption row and
/// its `approval_events` row commit together, not to win a race.
///
/// The status check runs on `conn` directly, before the transaction opens,
/// deliberately not re-checked inside it: `allowed` and `denied` are both
/// terminal (`approvals_decided_is_immutable` in `schema.rs`), so a row
/// read as either here can never read as anything else later — there is no
/// staleness window to close.
pub fn redeem_ask(conn: &Connection, request_id: &str) -> Result<bool> {
    let status: Option<String> = conn
        .query_row("SELECT status FROM approvals WHERE request_id = ?1", params![request_id], |row| row.get(0))
        .optional()?;
    let status = status.ok_or_else(|| LedgerError::NotFound(request_id.to_string()))?;
    if status != "allowed" && status != "denied" {
        return Err(LedgerError::NotDecided { request_id: request_id.to_string(), status });
    }

    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO approval_redemptions (request_id) VALUES (?1)",
        params![request_id],
    )?;
    if inserted == 0 {
        // Lost the race (or a plain repeat call) — no event, same
        // reasoning as claim.rs's losing side: only a successful
        // redemption writes anything.
        tx.commit()?;
        return Ok(false);
    }

    events::append(&tx, request_id, EventKind::Redeemed, None, None, None, None, None)?;
    tx.commit()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use crate::ask::{create_ask, get_approval, list_events};
    use crate::error::LedgerError;
    use crate::ask::list_refusals;
    use crate::fixtures::{ASKING_CONTEXT, PEER_CONTEXT, author, minimal_ask, open_memory, open_memory_with_legacy_kind_check, peer};
    use crate::types::{ApprovalStatus, EventKind};

    use super::*;

    #[test]
    fn decide_allow_reaches_the_one_true_allowed_state() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();

        let row = decide(
            &conn,
            &request_id,
            DecideInput { allow: true, decided_by: Some(peer(b"alice")), decided_option: Some("allow_once"), ..Default::default() },
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
        let row = decide(&conn, &request_id, DecideInput { allow: false, decided_by: Some(peer(b"alice")), ..Default::default() }).unwrap();
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
            DecideInput { allow: true, decided_by: Some(peer(b"late-alice")), decided_option: Some("allow_once"), ..Default::default() },
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

    // ── `redeem_ask` (single-use consumption of an allowed ask) ──────────

    /// The regression a live kernel.db hit: a database that ran `migrate()`
    /// before `redeemed` joined `approval_events.kind` kept the narrow
    /// `CHECK`, so the redemption event was rejected — and because
    /// `redeem_ask` writes the redemption row and the event in ONE
    /// transaction, the rollback took the redemption with it. No answer,
    /// allow or deny, could be consumed on such a database.
    ///
    /// Falsified by restoring `CHECK (kind IN (...))` to `approval_events`
    /// in `schema::DDL` and reverting `drop_legacy_approval_events_kind_
    /// check` to a no-op: `redeem_ask` then failed with "CHECK constraint
    /// failed: kind IN (...)" instead of returning `true`. Reverted after.
    #[test]
    fn an_answer_is_redeemable_on_a_database_built_before_redeemed_was_a_kind() {
        let conn = open_memory_with_legacy_kind_check();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();
        decide(&conn, &request_id, DecideInput { allow: true, ..Default::default() }).unwrap();

        assert!(
            redeem_ask(&conn, &request_id).unwrap(),
            "an allowed ask must be redeemable on a pre-`redeemed` database"
        );
        let events = list_events(&conn, &request_id).unwrap();
        assert_eq!(
            events.iter().filter(|e| e.kind == EventKind::Redeemed).count(),
            1,
            "the redemption event must land, not be rejected by a stale CHECK"
        );
    }

    /// The denial half of the same regression. `decide.rs`'s own rule is
    /// that a denial is redeemed like an approval, so a stale `CHECK`
    /// stranded denied callers in a retry loop exactly as it did allowed
    /// ones — worth its own assertion because the two statuses take
    /// different branches out of `redeem_ask`'s status guard.
    #[test]
    fn a_denial_is_redeemable_on_a_database_built_before_redeemed_was_a_kind() {
        let conn = open_memory_with_legacy_kind_check();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();
        decide(&conn, &request_id, DecideInput { allow: false, ..Default::default() }).unwrap();

        assert!(
            redeem_ask(&conn, &request_id).unwrap(),
            "a denied ask must be redeemable on a pre-`redeemed` database"
        );
    }

    /// Pins the single-use guarantee: the first redemption of an allowed
    /// ask wins (`Ok(true)`) and writes exactly one `Redeemed` event; a
    /// second call against the SAME request_id must NOT win a second time.
    /// Falsified by deleting the `if inserted == 0 { return Ok(false) }`
    /// early return so every call always appended an event and returned
    /// `true` — with that change, `second` read `true` (expected `false`)
    /// and `redeemed_count` read `2` (expected `1`), so both assertions
    /// failed as expected; reverted afterward.
    #[test]
    fn redeeming_an_allowed_ask_twice_only_the_first_call_wins() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();
        decide(&conn, &request_id, DecideInput { allow: true, ..Default::default() }).unwrap();

        let first = redeem_ask(&conn, &request_id).unwrap();
        assert!(first, "the first redemption of an allowed ask must win");

        let second = redeem_ask(&conn, &request_id).unwrap();
        assert!(!second, "a second redemption of the same ask must NOT win");

        let events = list_events(&conn, &request_id).unwrap();
        let redeemed_count = events.iter().filter(|e| e.kind == EventKind::Redeemed).count();
        assert_eq!(redeemed_count, 1, "only the winning redemption may write a Redeemed event");
    }

    /// Pins the distinction `LedgerError::NotDecided` exists to protect:
    /// redeeming an ask that carries NO answer — still `pending`, or ended
    /// `expired`/`abandoned` with nobody deciding — must be a loud error
    /// naming the real status, never the `Ok(false)` an already-redeemed
    /// ask returns. A caller that only checked for `Ok(false)` could
    /// otherwise not tell "nobody answered" from "the answer was already
    /// delivered", and those call for opposite next moves.
    ///
    /// Falsified by deleting the status pre-check so every call fell
    /// through to the `INSERT OR IGNORE`: both `unwrap_err()` calls then
    /// panicked on an `Ok(true)`, i.e. an unanswered ask got silently
    /// redeemed. Reverted afterward.
    #[test]
    fn redeeming_an_unanswered_ask_errors_distinctly_from_already_redeemed() {
        let conn = open_memory();

        let pending = create_ask(&conn, &minimal_ask()).unwrap();
        let err = redeem_ask(&conn, &pending).unwrap_err();
        assert!(matches!(&err, LedgerError::NotDecided { status, .. } if status == "pending"));

        let expired = create_ask(&conn, &minimal_ask()).unwrap();
        expire(&conn, &expired).unwrap();
        let err = redeem_ask(&conn, &expired).unwrap_err();
        assert!(matches!(&err, LedgerError::NotDecided { status, .. } if status == "expired"));
    }

    /// **A denial is an answer, and is spent like an approval.** This is
    /// the test that stops the loop described in `docs/gate-resume.md`: if
    /// only approvals were redeemable, a denied caller would retry, find
    /// nothing to redeem, and mint a second identical ask forever, while a
    /// human watched duplicates pile up with no way to stop it by
    /// answering. Redeeming does not *interpret* the answer — the caller
    /// reads the status to learn which it was — it only records that the
    /// answer was delivered, once.
    ///
    /// Falsified by restoring the `status != "allowed"` check that this
    /// contract replaced: the first redemption then failed with
    /// `NotDecided { status: "denied" }` instead of winning. Reverted.
    #[test]
    fn a_denied_ask_is_redeemable_exactly_once() {
        let conn = open_memory();
        let denied = create_ask(&conn, &minimal_ask()).unwrap();
        decide(&conn, &denied, DecideInput { allow: false, ..Default::default() }).unwrap();

        assert!(redeem_ask(&conn, &denied).unwrap(), "a denial must be deliverable once");
        assert!(
            !redeem_ask(&conn, &denied).unwrap(),
            "a denial already delivered must not be delivered twice"
        );

        let events = list_events(&conn, &denied).unwrap();
        assert_eq!(
            events.iter().filter(|e| e.kind == EventKind::Redeemed).count(),
            1,
            "only the winning redemption may write a Redeemed event"
        );
    }

    #[test]
    fn redeeming_an_unknown_request_is_not_found() {
        let conn = open_memory();
        assert!(matches!(redeem_ask(&conn, "nope").unwrap_err(), LedgerError::NotFound(_)));
    }

    // ── `abandon_unresolved_on_restart` (the cold-start sweep) ──────────────

    /// The core claim: every `pending` row becomes `abandoned`, the
    /// reported count matches, and the reason is readable back.
    ///
    /// Falsified by changing the loop body's `Ok(_) => swept += 1` to `Ok(_)
    /// => {}` (never incrementing): `count` then read `0` instead of `3`,
    /// failing the assertion as expected. Reverted afterward.
    #[test]
    fn sweep_abandons_every_pending_ask_and_reports_the_count() {
        let conn = open_memory();
        let a = create_ask(&conn, &minimal_ask()).unwrap();
        let b = create_ask(&conn, &minimal_ask()).unwrap();
        let c = create_ask(&conn, &minimal_ask()).unwrap();

        let count = abandon_unresolved_on_restart(&conn, "kernel restarted; nothing ran; ask again").unwrap();
        assert_eq!(count, 3);

        for id in [&a, &b, &c] {
            let row = get_approval(&conn, id).unwrap().unwrap();
            assert_eq!(row.status, ApprovalStatus::Abandoned);
        }
    }

    /// A `claimed` ask must be swept too, and it is the one the queue
    /// cannot show you. `list_pending` hides `claimed` on purpose — an
    /// answerer is working it and a second answerer would step on the
    /// claim — but that premise dies with the process that held the claim.
    /// At a cold start a `claimed` row is reachable from neither
    /// `list_pending` nor `list_history`, so leaving it behind leaves a row
    /// that can never be seen or answered again.
    #[test]
    fn sweep_abandons_a_claimed_ask_the_queue_cannot_show() {
        let conn = open_memory();
        let id = create_ask(&conn, &minimal_ask()).unwrap();
        crate::claim::claim(&conn, &id, b"an-answerer-that-died").unwrap();
        assert_eq!(
            get_approval(&conn, &id).unwrap().unwrap().status,
            ApprovalStatus::Claimed,
        );

        // The gap this closes: invisible to both list queries.
        assert!(crate::ask::list_pending(&conn).unwrap().is_empty());
        assert!(crate::ask::list_history(&conn, 100).unwrap().is_empty());

        let count = abandon_unresolved_on_restart(&conn, "kernel restarted").unwrap();
        assert_eq!(count, 1, "the claimed ask must be swept");
        assert_eq!(
            get_approval(&conn, &id).unwrap().unwrap().status,
            ApprovalStatus::Abandoned,
        );
    }

    /// **The most important test here.** An `allowed` ask nobody has
    /// redeemed yet is still a live, deliverable answer — the sweep must
    /// leave it completely untouched, not abandon it out from under the
    /// human who already decided it.
    ///
    /// Falsified two ways. First, by widening `abandon_unresolved_on_restart`
    /// to enumerate via `list_asks_filtered` with `statuses: vec![Pending,
    /// Allowed, Denied]` instead of `list_pending`'s `pending`-only query,
    /// while still routing each row through `abandon`: this did **not**
    /// fail — `abandon`'s own `transition` only ever matches `status IN
    /// ('pending', 'claimed')`, so calling it on an `allowed`/`denied` row
    /// hits `AlreadyDecided`, which the sweep's loop already treats as
    /// "already reached a terminal state, skip" and does not count. That is
    /// a real structural fact, not a weak test: the enumeration query is
    /// redundant with `transition`'s own `WHERE` clause as the thing
    /// actually protecting a decided row, and is recorded here rather than
    /// silently dropped. Second, by replacing the whole function body with
    /// a raw bulk `UPDATE approvals SET status = 'abandoned' WHERE status
    /// IN ('pending', 'allowed', 'denied')` — bypassing `transition`
    /// entirely: this DID fail, but with a `SqliteFailure` panic from
    /// `schema.rs`'s `approvals_decided_is_immutable` trigger ("approval
    /// already reached a terminal status; re-deciding is refused") rather
    /// than a plain assertion mismatch — a third, schema-level backstop
    /// underneath `transition`'s own guard. Reverted afterward.
    #[test]
    fn sweep_does_not_touch_an_undelivered_allowed_or_denied_ask() {
        let conn = open_memory();
        let allowed = create_ask(&conn, &minimal_ask()).unwrap();
        decide(&conn, &allowed, DecideInput { allow: true, decided_by: Some(peer(b"alice")), ..Default::default() }).unwrap();
        let denied = create_ask(&conn, &minimal_ask()).unwrap();
        decide(&conn, &denied, DecideInput { allow: false, decided_by: Some(peer(b"alice")), ..Default::default() }).unwrap();

        let count = abandon_unresolved_on_restart(&conn, "kernel restarted").unwrap();
        assert_eq!(count, 0, "no pending rows exist, so nothing should be swept");

        let allowed_row = get_approval(&conn, &allowed).unwrap().unwrap();
        assert_eq!(allowed_row.status, ApprovalStatus::Allowed, "an undelivered answer must survive the sweep");
        let denied_row = get_approval(&conn, &denied).unwrap().unwrap();
        assert_eq!(denied_row.status, ApprovalStatus::Denied, "an undelivered answer must survive the sweep");

        // Still redeemable — the sweep did not spend either answer.
        assert!(redeem_ask(&conn, &allowed).unwrap(), "the allowed ask must still be deliverable after a sweep");
        assert!(redeem_ask(&conn, &denied).unwrap(), "the denied ask must still be deliverable after a sweep");
    }

    /// An already-terminal `abandoned` or `expired` row is left alone —
    /// both because [`ask::list_pending`] never returns them, and because
    /// [`abandon`] would no-op against them anyway if it somehow tried.
    ///
    /// Falsified by dropping the `Err(LedgerError::AlreadyDecided { .. }) =>
    /// {}` arm (making every non-`Ok` an aborting error): this test still
    /// passed, because `list_pending` never hands the sweep an
    /// already-terminal row to race against in the first place — recorded
    /// here rather than silently dropped, per the falsification-must-report
    /// rule. The arm still earns its place: it is what keeps a genuine
    /// pending→terminal race (a future caller invoking this mid-process,
    /// not only at cold start) from aborting the rest of the sweep.
    #[test]
    fn sweep_leaves_an_already_terminal_ask_alone() {
        let conn = open_memory();
        let expired = create_ask(&conn, &minimal_ask()).unwrap();
        expire(&conn, &expired).unwrap();
        let abandoned = create_ask(&conn, &minimal_ask()).unwrap();
        abandon(&conn, &abandoned, Some("operator gave up waiting")).unwrap();

        let count = abandon_unresolved_on_restart(&conn, "kernel restarted").unwrap();
        assert_eq!(count, 0);

        assert_eq!(get_approval(&conn, &expired).unwrap().unwrap().status, ApprovalStatus::Expired);
        let abandoned_row = get_approval(&conn, &abandoned).unwrap().unwrap();
        assert_eq!(abandoned_row.status, ApprovalStatus::Abandoned);
        // The original abandonment reason survives — the sweep did not
        // re-abandon it with its own restart reason.
        let events = list_events(&conn, &abandoned).unwrap();
        assert_eq!(events.iter().filter(|e| e.kind == EventKind::Abandoned).count(), 1);
        assert_eq!(events[0].note.as_deref(), Some("operator gave up waiting"));
    }

    /// The reason string is recorded on the row's event and readable back
    /// — what `kj ledger show` displays to a human who comes back to an
    /// ask that no longer does anything.
    ///
    /// Falsified by passing `None` instead of `Some(reason)` into the
    /// per-row `abandon` call: `events[0].note` then read `None` instead of
    /// `Some("kernel restarted; nothing ran; ask again")`, failing as
    /// expected. Reverted afterward.
    #[test]
    fn sweep_records_the_given_reason_on_each_abandoned_ask() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();

        abandon_unresolved_on_restart(&conn, "kernel restarted; nothing ran; ask again").unwrap();

        let events = list_events(&conn, &request_id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Abandoned);
        assert_eq!(events[0].note.as_deref(), Some("kernel restarted; nothing ran; ask again"));
    }

    // ── No self-approval ────────────────────────────────────────────────
    // `docs/gate-and-shell-split.md`, "No self-approval — the gate's own
    // answer path". The author of an ask may not answer it; a peer may.

    /// Falsified by dropping the `ensure_not_self_approval` call from
    /// `decide`: the decide returns Ok and the ask reads `allowed`.
    #[test]
    fn the_context_that_raised_an_ask_cannot_answer_it() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();

        let err = decide(
            &conn,
            &request_id,
            DecideInput { allow: true, decided_by: Some(author(b"the-asker")), ..Default::default() },
        )
        .unwrap_err();

        assert!(
            matches!(&err, LedgerError::SelfApproval { reason, .. } if *reason == "this context raised the ask"),
            "expected a SelfApproval refusal, got: {err}"
        );
        // Nothing decided, and nothing consumed: the ask is still answerable
        // by a seat that may answer it.
        let row = get_approval(&conn, &request_id).unwrap().unwrap();
        assert_eq!(row.status, ApprovalStatus::Pending);
        assert!(row.decided_by.is_none());
    }

    /// Peer-seat approval is permitted on purpose (Amy, 2026-08-26) — the
    /// rule bans answering *yourself*, not answering for someone else.
    #[test]
    fn a_different_context_may_answer() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();

        let row = decide(
            &conn,
            &request_id,
            DecideInput { allow: true, decided_by: Some(peer(b"a-neighbor")), ..Default::default() },
        )
        .unwrap();
        assert_eq!(row.status, ApprovalStatus::Allowed);
        assert!(list_refusals(&conn, &request_id).unwrap().is_empty());
    }

    /// Fails closed: an answerer that cannot name its context cannot show it
    /// is not the author, so it is refused exactly like the author is.
    /// Falsified by treating `context: None` as "not the author" — the
    /// decide succeeds and this trips.
    #[test]
    fn an_answer_that_names_no_context_is_refused() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();

        let err = decide(
            &conn,
            &request_id,
            DecideInput {
                allow: true,
                decided_by: Some(Answerer { principal: b"contextless", context: None }),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            matches!(&err, LedgerError::SelfApproval { reason, .. } if *reason == "the answer names no context"),
            "expected a SelfApproval refusal naming the missing context, got: {err}"
        );
        assert_eq!(get_approval(&conn, &request_id).unwrap().unwrap().status, ApprovalStatus::Pending);
    }

    /// A refusal that is only returned to its caller is invisible to the
    /// measurement the gate is tuned on, so every refusal lands a row.
    /// Falsified by dropping the `append_refusal` call: the table is empty.
    #[test]
    fn a_refused_self_approval_is_recorded_with_who_and_from_where() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();

        for _ in 0..2 {
            decide(
                &conn,
                &request_id,
                DecideInput { allow: true, decided_by: Some(author(b"the-asker")), ..Default::default() },
            )
            .unwrap_err();
        }

        let refusals = list_refusals(&conn, &request_id).unwrap();
        assert_eq!(refusals.len(), 2, "every attempt is recorded, not just the first");
        assert_eq!(refusals[0].seq, 0);
        assert_eq!(refusals[1].seq, 1);
        assert_eq!(refusals[0].reason, "self_approval");
        assert_eq!(refusals[0].actor.as_deref(), Some(&b"the-asker"[..]));
        assert_eq!(refusals[0].actor_context.as_deref(), Some(ASKING_CONTEXT));

        // A refusal is not an event: nothing was claimed, decided, or
        // late-decided, so `approval_events` stays empty.
        assert!(list_events(&conn, &request_id).unwrap().is_empty());
    }

    /// The classifier chain is a different system from the model that raised
    /// the ask, so an auto-decision is never a self-approval — even though it
    /// decides an ask its own context raised. It carries no `Answerer` at
    /// all, which is the same signal that already separates it from a
    /// person's answer.
    #[test]
    fn an_auto_decision_from_the_asking_context_is_not_a_self_approval() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();

        let row = decide(
            &conn,
            &request_id,
            DecideInput {
                allow: true,
                decided_by: None,
                auto_reason: Some("a standing rule covered every statement"),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(row.status, ApprovalStatus::Allowed);
        assert!(row.decided_by.is_none(), "no identity decided this");
        assert!(list_refusals(&conn, &request_id).unwrap().is_empty());
    }

    /// The guard is callable before `claim`, which is how the kj verb uses it
    /// — a claim taken first would leave the ask `claimed` by the one seat
    /// that may not answer it, locking out every seat that may.
    #[test]
    fn the_guard_refuses_without_claiming_and_a_peer_can_still_claim() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();

        ensure_not_self_approval(&conn, &request_id, author(b"the-asker")).unwrap_err();

        // Still `pending`, so a peer's claim wins normally.
        let claimed = crate::claim::claim(&conn, &request_id, b"a-neighbor").unwrap();
        assert_eq!(claimed.status, ApprovalStatus::Claimed);
        assert_eq!(
            ensure_not_self_approval(&conn, &request_id, peer(b"a-neighbor")).is_ok(),
            true,
            "a peer context passes the guard"
        );
        assert_eq!(PEER_CONTEXT, &[7, 7, 7, 7]);
    }

    /// An unknown request is `NotFound`, not a refusal — a caller must be able
    /// to tell "you may not answer this" from "there is nothing here".
    #[test]
    fn the_guard_reports_an_unknown_ask_as_not_found() {
        let conn = open_memory();
        let err = ensure_not_self_approval(&conn, "no-such-ask", peer(b"a-neighbor")).unwrap_err();
        assert!(matches!(err, LedgerError::NotFound(_)), "got: {err}");
    }

}
