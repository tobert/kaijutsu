//! Reading the durable, trigger-maintained `ledger_generation` counter
//! (`schema.rs`'s "Ledger generation" section owns the table and every
//! trigger that writes it during normal operation).
//!
//! This module holds exactly two functions, and they divide the work the
//! same way `decide.rs`'s module doc divides guarantees 2 and 6: one read
//! ([`current`]), and one deliberate, narrowly-scoped write
//! ([`bump_for_restart`]) that is the sole exception to "only a trigger
//! writes `generation`". Nothing else belongs here — every ordinary bump
//! happens as a side effect of `ask::create_ask`, `claim::claim`,
//! `decide::decide`/`expire`/`abandon`, and `rules::learn_from_approval`/
//! `revoke`, via the triggers those functions' own `INSERT`/`UPDATE`
//! statements fire. None of those call sites import this module.

use rusqlite::{Connection, params};

use crate::error::Result;

/// How far [`bump_for_restart`] advances the counter, once, at boot.
/// Small on purpose (Amy: "keep it small") — the gap only needs to be
/// bigger than any plausible number of trigger-fired bumps a client could
/// have missed between its last observed generation and this restart, not
/// a value with headroom for some other purpose.
pub const RESTART_GAP: i64 = 100;

/// Read the current generation. Synchronous, like every other read in
/// this crate — there is exactly one row (`id = 1`), seeded by `migrate`,
/// so this can never legitimately return "no row" the way `get_approval`
/// returns `Ok(None)` for an unknown id; a missing row here means the
/// caller's connection was never migrated, which is a caller bug, not a
/// normal outcome, so it surfaces through the ordinary `rusqlite::Error`
/// (`QueryReturnedNoRows`) `?` already converts into
/// [`crate::error::LedgerError::Db`] rather than a bespoke `NotFound`
/// variant invented for a case that should not occur.
pub fn current(conn: &Connection) -> Result<i64> {
    let generation = conn.query_row("SELECT generation FROM ledger_generation WHERE id = 1", [], |row| row.get(0))?;
    Ok(generation)
}

/// Advance the generation by [`RESTART_GAP`] — call this exactly once, at
/// kernel boot, before anything else touches the ledger. Returns the new
/// generation.
///
/// This is deliberately NOT folded into [`crate::schema::migrate`].
/// `migrate` is documented there as idempotent and safe to call on every
/// process start, possibly more than once, possibly per-connection — if
/// the restart bump lived inside it, the jump size would depend on how
/// many times `migrate` happened to run, which turns a fixed, legible gap
/// into an accident of call-site plumbing. The kernel owns calling this
/// function once; this crate only owns making the call cheap and correct
/// when it happens.
///
/// Why a gap exists at all: durability alone (the trigger-maintained
/// counter surviving on disk) already gives monotonicity across an
/// ordinary restart — the value on disk when the process comes back up is
/// exactly the value it was when the process went down, and no trigger
/// fires while nothing is running to fire one. The gap is insurance for
/// the cases that break that: a crash mid-transaction can leave
/// SQLite's own rollback journal/WAL recovery as the only thing that ran
/// before this process resumes, and a database restored from an
/// out-of-band backup can resume from a generation a client already
/// observed with completely different facts now sitting behind it. In
/// both cases, advancing by a visible gap rather than by the usual `+ 1`
/// makes the restart legible to a client as "something discontinuous
/// happened here, re-fetch state instead of assuming you only missed one
/// change" instead of a silent resume that looks like an ordinary bump.
pub fn bump_for_restart(conn: &Connection) -> Result<i64> {
    let generation = conn.query_row(
        "UPDATE ledger_generation SET generation = generation + ?1 WHERE id = 1 RETURNING generation",
        params![RESTART_GAP],
        |row| row.get(0),
    )?;
    Ok(generation)
}

#[cfg(test)]
mod tests {
    use rusqlite::params;

    use crate::fixtures::{minimal_ask, open_memory};
    use crate::schema::migrate;

    use super::*;

    #[test]
    fn a_freshly_migrated_ledger_starts_at_generation_zero() {
        let conn = open_memory();
        assert_eq!(current(&conn).unwrap(), 0);
    }

    #[test]
    fn inserting_an_approval_bumps_the_generation() {
        let conn = open_memory();
        let before = current(&conn).unwrap();
        crate::ask::create_ask(&conn, &minimal_ask()).unwrap();
        assert_eq!(current(&conn).unwrap(), before + 1, "approvals INSERT must bump exactly once");
    }

    // `claim()`/`decide()` each cause TWO bumps in one call (an approvals
    // UPDATE plus an approval_events INSERT — both real triggers firing),
    // which would conflate the two trigger sources if used here. To pin
    // each trigger individually, these two tests reach past the public
    // API with raw SQL — the same style `schema.rs`'s own trigger tests
    // use — so only the ONE statement under test executes between `before`
    // and the assertion.

    #[test]
    fn updating_an_approval_bumps_the_generation() {
        let conn = open_memory();
        let request_id = crate::ask::create_ask(&conn, &minimal_ask()).unwrap();
        let before = current(&conn).unwrap();

        // Any column update fires the trigger (`AFTER UPDATE ON
        // approvals`, no `OF status` restriction) — `description` is
        // chosen because it carries no side effects of its own and, unlike
        // `status`, can never trip `approvals_decided_is_immutable`.
        conn.execute("UPDATE approvals SET description = 'updated' WHERE request_id = ?1", params![request_id]).unwrap();
        assert_eq!(current(&conn).unwrap(), before + 1, "a bare approvals UPDATE must bump exactly once");
    }

    #[test]
    fn appending_an_approval_event_bumps_the_generation() {
        let conn = open_memory();
        // create_ask() itself writes no approval_events row (ask.rs's
        // `events_list_starts_empty_for_a_fresh_ask`), so this ask's
        // creation cannot have already bumped the generation via this
        // trigger — only via the approvals-insert trigger, which is a
        // different source and does not confound this test.
        let request_id = crate::ask::create_ask(&conn, &minimal_ask()).unwrap();
        let before = current(&conn).unwrap();

        conn.execute(
            "INSERT INTO approval_events (request_id, seq, kind) VALUES (?1, 0, 'claimed')",
            params![request_id],
        )
        .unwrap();
        assert_eq!(current(&conn).unwrap(), before + 1, "a bare approval_events INSERT must bump exactly once");
    }

    #[test]
    fn inserting_a_rule_bumps_the_generation() {
        let conn = open_memory();
        let ask = crate::fixtures::ask_with_statement("digest-gen", crate::types::VarBinding::Bound, "rm target");
        let request_id = crate::ask::create_ask(&conn, &ask).unwrap();
        crate::decide::decide(
            &conn,
            &request_id,
            crate::decide::DecideInput { allow: true, decided_by: Some(b"alice"), ..Default::default() },
        )
        .unwrap();

        let before = current(&conn).unwrap();
        crate::rules::learn_from_approval(&conn, &request_id, 0, crate::types::RuleScope::Always, true, None).unwrap();
        assert_eq!(current(&conn).unwrap(), before + 1, "approval_rules INSERT must bump exactly once");
    }

    #[test]
    fn updating_a_rule_bumps_the_generation() {
        let conn = open_memory();
        let ask = crate::fixtures::ask_with_statement("digest-gen-upd", crate::types::VarBinding::Bound, "rm target");
        let request_id = crate::ask::create_ask(&conn, &ask).unwrap();
        crate::decide::decide(
            &conn,
            &request_id,
            crate::decide::DecideInput { allow: true, decided_by: Some(b"alice"), ..Default::default() },
        )
        .unwrap();
        let rule = crate::rules::learn_from_approval(&conn, &request_id, 0, crate::types::RuleScope::Always, true, None).unwrap();

        let before = current(&conn).unwrap();
        crate::rules::revoke(&conn, &rule.rule_id).unwrap();
        assert_eq!(current(&conn).unwrap(), before + 1, "approval_rules UPDATE (revoke) must bump exactly once");
    }

    /// The DDL creates the DELETE trigger too, even though no Rust write
    /// path in this crate deletes an `approval_rules` row today — exercise
    /// it directly against the schema, the same way `schema.rs`'s own
    /// tests probe `CHECK`/trigger behavior with raw SQL rather than only
    /// through this crate's public functions.
    #[test]
    fn deleting_a_rule_bumps_the_generation() {
        let conn = open_memory();
        let ask = crate::fixtures::ask_with_statement("digest-gen-del", crate::types::VarBinding::Bound, "rm target");
        let request_id = crate::ask::create_ask(&conn, &ask).unwrap();
        crate::decide::decide(
            &conn,
            &request_id,
            crate::decide::DecideInput { allow: true, decided_by: Some(b"alice"), ..Default::default() },
        )
        .unwrap();
        let rule = crate::rules::learn_from_approval(&conn, &request_id, 0, crate::types::RuleScope::Always, true, None).unwrap();

        let before = current(&conn).unwrap();
        conn.execute("DELETE FROM approval_rules WHERE rule_id = ?1", params![rule.rule_id]).unwrap();
        assert_eq!(current(&conn).unwrap(), before + 1, "approval_rules DELETE must bump exactly once");
    }

    #[test]
    fn generation_survives_across_separate_migrate_calls_on_the_same_connection() {
        let conn = open_memory(); // already migrated once by the fixture
        crate::ask::create_ask(&conn, &minimal_ask()).unwrap();
        let before = current(&conn).unwrap();
        assert!(before > 0, "the insert above must have bumped it already");

        // A second migrate() call, exactly what the kernel does on every
        // process start (schema.rs: "safe to call on every process
        // start") — the seed's `INSERT OR IGNORE` must not re-zero an
        // already-advancing counter.
        migrate(&conn).unwrap();
        assert_eq!(current(&conn).unwrap(), before, "idempotent migrate() must not reset the generation");
    }

    #[test]
    fn bump_for_restart_advances_by_exactly_the_restart_gap() {
        let conn = open_memory();
        let before = current(&conn).unwrap();
        let returned = bump_for_restart(&conn).unwrap();
        assert_eq!(returned, before + RESTART_GAP);
        assert_eq!(current(&conn).unwrap(), before + RESTART_GAP);
    }

    #[test]
    fn bump_for_restart_stacks_on_top_of_ordinary_trigger_bumps() {
        let conn = open_memory();
        crate::ask::create_ask(&conn, &minimal_ask()).unwrap();
        let before_restart = current(&conn).unwrap();

        bump_for_restart(&conn).unwrap();
        assert_eq!(current(&conn).unwrap(), before_restart + RESTART_GAP);

        crate::ask::create_ask(&conn, &minimal_ask()).unwrap();
        assert_eq!(current(&conn).unwrap(), before_restart + RESTART_GAP + 1, "ordinary bumps resume after a restart bump");
    }

    /// The property that makes this counter trustworthy at all (pinned
    /// per the task): a trigger fires inside the SAME transaction as its
    /// mutation, so a rolled-back transaction's bump never lands. Without
    /// this, a client could observe a generation that claims a fact which
    /// never actually committed.
    #[test]
    fn generation_does_not_advance_on_a_rolled_back_transaction() {
        let conn = open_memory();
        let before = current(&conn).unwrap();

        conn.execute_batch("BEGIN;").unwrap();
        conn.execute(
            "INSERT INTO approvals (request_id, context_id, principal_id, origin, description)
             VALUES ('rollback-me', X'01', X'02', 'shell_gate', 'x')",
            [],
        )
        .unwrap();
        // Sanity: inside the still-open transaction, the trigger DID fire —
        // this confirms the assertion after ROLLBACK is testing rollback
        // behavior, not a trigger that silently never ran.
        assert_eq!(current(&conn).unwrap(), before + 1, "the trigger must have fired before the rollback");
        conn.execute_batch("ROLLBACK;").unwrap();

        assert_eq!(current(&conn).unwrap(), before, "a rolled-back transaction's bump must not survive");
        assert!(
            crate::ask::get_approval(&conn, "rollback-me").unwrap().is_none(),
            "the approval itself must also be gone — this is what the bump must not outlive"
        );
    }
}
