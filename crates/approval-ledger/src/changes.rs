//! Which asks changed after a generation.
//!
//! The triggers that bump `ledger_generation` for an ask (`schema.rs`'s
//! "Ledger generation" section) also stamp the ask in `approval_changes`
//! with the generation they produced, in the same trigger body. A reader
//! that has seen everything through generation `g` asks for the rows
//! stamped after `g` and misses nothing.

use rusqlite::{Connection, params};

use crate::ask::row_to_approval;
use crate::error::Result;
use crate::types::ApprovalRow;

const APPROVAL_COLUMNS: &str = "request_id, context_id, actor_id, reviewer_id, principal_id, origin, instance, tool, hook_id, \
     description, authorized_label, rc_run_id, status, created_at, \
     expires_at, claimed_at, claimed_by, decided_at, decided_by, decided_option, \
     remember_scope, auto_reason, cwd, exec_source, exec_stdin, command_block_id, output_block_id, pair_owner, continuation_epoch";
/// The stamp's index: one past the last approval column.
const STAMP: usize = 29;

/// Every ask whose latest change is newer than `generation`, oldest change
/// first, each with the generation that stamped it.
pub fn changed_since(conn: &Connection, generation: i64) -> Result<Vec<(i64, ApprovalRow)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {APPROVAL_COLUMNS}, c.generation FROM approvals
         JOIN approval_changes c USING (request_id)
         WHERE c.generation > ?1 ORDER BY c.generation"
    ))?;
    let rows = stmt
        .query_map(params![generation], |row| {
            Ok((row.get(STAMP)?, row_to_approval(row)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use rusqlite::params;

    use crate::fixtures::{minimal_ask, open_memory};
    use crate::generation::current;

    use super::*;

    #[test]
    fn a_new_ask_is_stamped_with_the_generation_that_announced_it() {
        let conn = open_memory();
        let before = current(&conn).unwrap();
        let request_id = crate::ask::create_ask(&conn, &minimal_ask()).unwrap();
        let changed = changed_since(&conn, before).unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].1.request_id, request_id);
        assert_eq!(changed[0].0, current(&conn).unwrap(), "the stamp is the announced generation");
    }

    #[test]
    fn a_later_change_restamps_only_the_ask_it_touched() {
        let conn = open_memory();
        let first = crate::ask::create_ask(&conn, &minimal_ask()).unwrap();
        let _second = crate::ask::create_ask(&conn, &minimal_ask()).unwrap();
        let seen = current(&conn).unwrap();
        assert!(changed_since(&conn, seen).unwrap().is_empty());

        crate::claim::claim(&conn, &first, b"amy").unwrap();
        let changed = changed_since(&conn, seen).unwrap();
        assert_eq!(changed.iter().map(|(_, row)| row.request_id.as_str()).collect::<Vec<_>>(), vec![first.as_str()]);
        assert_eq!(changed[0].0, current(&conn).unwrap());
    }

    #[test]
    fn an_event_or_a_signal_alone_restamps_its_ask() {
        let conn = open_memory();
        let request_id = crate::ask::create_ask(&conn, &minimal_ask()).unwrap();
        let seen = current(&conn).unwrap();
        conn.execute("INSERT INTO approval_events (request_id, seq, kind) VALUES (?1, 0, 'claimed')", params![request_id]).unwrap();
        assert_eq!(changed_since(&conn, seen).unwrap()[0].0, current(&conn).unwrap());

        let seen = current(&conn).unwrap();
        conn.execute(
            "INSERT INTO approval_signals (request_id, seq, source_kind, verdict) VALUES (?1, 99, 'rule', 'escalate')",
            params![request_id],
        ).unwrap();
        assert_eq!(changed_since(&conn, seen).unwrap()[0].0, current(&conn).unwrap());
    }

    /// A database whose triggers predate the stamp gets stamping triggers
    /// from `migrate`, which drops a trigger whose body does not stamp.
    #[test]
    fn migrate_replaces_a_trigger_that_does_not_stamp() {
        let conn = open_memory();
        conn.execute_batch(
            "DROP TRIGGER ledger_generation_bump_on_approval_insert;
             CREATE TRIGGER ledger_generation_bump_on_approval_insert
             AFTER INSERT ON approvals
             BEGIN
                 UPDATE ledger_generation SET generation = generation + 1 WHERE id = 1;
             END;",
        ).unwrap();
        crate::schema::migrate(&conn).unwrap();
        let before = current(&conn).unwrap();
        crate::ask::create_ask(&conn, &minimal_ask()).unwrap();
        assert_eq!(changed_since(&conn, before).unwrap().len(), 1);
        assert_eq!(current(&conn).unwrap(), before + 1, "still one bump per insert");
    }
}
