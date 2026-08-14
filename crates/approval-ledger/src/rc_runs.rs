//! The rc lifecycle run log — "a checklist table of the rc scripts that
//! ran, snapshotted" (Amy, filing this crate: the concrete incident this
//! answers is a morning where an assistant seat's startup rc scripts
//! silently never ran, and nothing recorded that — a durable run log would
//! have made it visible on run one instead of by review).
//!
//! Loosely coupled to the approval side of this crate: an `approvals` row
//! may optionally point at the `rc_run_id` it happened during
//! (`approvals.rc_run_id`), but `rc_runs` has no opinion about approvals.

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::error::{LedgerError, Result};
use crate::types::{RcOutcome, RcRunRow, RcRunScriptRow, parse_enum};

/// Start a run and return its `run_id` (UUIDv7).
pub fn start_run(conn: &Connection, context_id: &[u8], context_type: &str, verb: &str) -> Result<String> {
    let run_id = uuid::Uuid::now_v7().to_string();
    conn.execute(
        "INSERT INTO rc_runs (run_id, context_id, context_type, verb) VALUES (?1, ?2, ?3, ?4)",
        params![run_id, context_id, context_type, verb],
    )?;
    Ok(run_id)
}

/// Mark a run finished. Fails loudly on a run that's already finished
/// (same reasoning as a decided approval: outcome doesn't silently
/// change) rather than overwriting `finished_at`/`outcome`.
pub fn finish_run(conn: &Connection, run_id: &str, outcome: RcOutcome) -> Result<()> {
    let now = crate::time::now_millis();
    let rows = conn.execute(
        "UPDATE rc_runs SET finished_at = ?1, outcome = ?2 WHERE run_id = ?3 AND finished_at IS NULL",
        params![now, outcome.as_str(), run_id],
    )?;
    if rows > 0 {
        return Ok(());
    }
    match get_run(conn, run_id)? {
        None => Err(LedgerError::RunNotFound(run_id.to_string())),
        Some(_) => Err(LedgerError::RunAlreadyFinished(run_id.to_string())),
    }
}

pub fn get_run(conn: &Connection, run_id: &str) -> Result<Option<RcRunRow>> {
    conn.query_row(
        "SELECT run_id, context_id, context_type, verb, started_at, finished_at, outcome
         FROM rc_runs WHERE run_id = ?1",
        params![run_id],
        row_to_run,
    )
    .optional()
    .map_err(LedgerError::from)
}

fn row_to_run(row: &rusqlite::Row) -> rusqlite::Result<RcRunRow> {
    let outcome_raw: Option<String> = row.get(6)?;
    let outcome = outcome_raw
        .map(|raw| parse_enum::<RcOutcome>("outcome", &raw))
        .transpose()
        .map_err(|e| match e {
            LedgerError::Db(inner) => inner,
            other => rusqlite::Error::InvalidColumnType(0, other.to_string(), rusqlite::types::Type::Text),
        })?;
    Ok(RcRunRow {
        run_id: row.get(0)?,
        context_id: row.get(1)?,
        context_type: row.get(2)?,
        verb: row.get(3)?,
        started_at: row.get(4)?,
        finished_at: row.get(5)?,
        outcome,
    })
}

/// Store a script body content-addressed (same pattern as `hook_scripts`
/// in `kernel_db.rs` and `approval_statements` in this crate): identical text
/// across many runs stores once. Returns the sha256 hex digest, computed
/// here — callers never supply their own hash, so it can't disagree with
/// what's actually stored.
pub fn insert_script_body(conn: &Connection, body: &str) -> Result<String> {
    let sha256 = hex::encode_sha256(body.as_bytes());
    conn.execute(
        "INSERT INTO script_bodies (sha256, body) VALUES (?1, ?2)
         ON CONFLICT(sha256) DO NOTHING",
        params![sha256, body],
    )?;
    Ok(sha256)
}

/// Record one script's execution within a run, in order.
pub fn record_run_script(
    conn: &Connection,
    run_id: &str,
    path: &str,
    body_sha256: &str,
    exit_code: Option<i64>,
    finished_at: Option<i64>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO rc_run_scripts (run_id, seq, path, body_sha256, exit_code, finished_at)
         VALUES (?1, (SELECT COALESCE(MAX(seq), -1) + 1 FROM rc_run_scripts WHERE run_id = ?1),
                 ?2, ?3, ?4, ?5)",
        params![run_id, path, body_sha256, exit_code, finished_at],
    )?;
    Ok(())
}

pub fn list_run_scripts(conn: &Connection, run_id: &str) -> Result<Vec<RcRunScriptRow>> {
    let mut stmt = conn.prepare(
        "SELECT seq, path, body_sha256, exit_code, started_at, finished_at
         FROM rc_run_scripts WHERE run_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt
        .query_map(params![run_id], |row| {
            Ok(RcRunScriptRow {
                seq: row.get(0)?,
                path: row.get(1)?,
                body_sha256: row.get(2)?,
                exit_code: row.get(3)?,
                started_at: row.get(4)?,
                finished_at: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// A tiny local shim so `insert_script_body` reads as "hex-encode a
/// sha256" at the call site without pulling in the `hex` crate for one
/// line — `sha2::Sha256` + `format!("{:x}", ...)` already gives lowercase
/// hex.
mod hex {
    use super::{Digest, Sha256};

    pub(super) fn encode_sha256(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        // Manual byte->hex rather than leaning on `GenericArray`'s `LowerHex`
        // (not guaranteed across sha2/generic-array versions) — this is
        // unambiguous and needs no extra dependency.
        hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::fixtures::open_memory;

    use super::*;

    #[test]
    fn start_and_finish_a_run() {
        let conn = open_memory();
        let run_id = start_run(&conn, b"ctx", "coder", "create").unwrap();
        let row = get_run(&conn, &run_id).unwrap().unwrap();
        assert_eq!(row.verb, "create");
        assert!(row.finished_at.is_none());
        assert!(row.outcome.is_none());

        finish_run(&conn, &run_id, RcOutcome::Ok).unwrap();
        let row = get_run(&conn, &run_id).unwrap().unwrap();
        assert!(row.finished_at.is_some());
        assert_eq!(row.outcome, Some(RcOutcome::Ok));
    }

    #[test]
    fn finishing_an_unknown_run_is_not_found() {
        let conn = open_memory();
        assert!(matches!(finish_run(&conn, "nope", RcOutcome::Ok).unwrap_err(), LedgerError::RunNotFound(_)));
    }

    #[test]
    fn finishing_twice_is_refused_not_overwritten() {
        let conn = open_memory();
        let run_id = start_run(&conn, b"ctx", "coder", "create").unwrap();
        finish_run(&conn, &run_id, RcOutcome::Ok).unwrap();
        let err = finish_run(&conn, &run_id, RcOutcome::Failed).unwrap_err();
        assert!(matches!(err, LedgerError::RunAlreadyFinished(_)));
        // Still `ok` — the second call's `Failed` must not have landed.
        assert_eq!(get_run(&conn, &run_id).unwrap().unwrap().outcome, Some(RcOutcome::Ok));
    }

    #[test]
    fn identical_script_bodies_dedupe_to_one_row() {
        let conn = open_memory();
        let a = insert_script_body(&conn, "echo hi").unwrap();
        let b = insert_script_body(&conn, "echo hi").unwrap();
        assert_eq!(a, b);
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM script_bodies", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn run_scripts_round_trip_in_order() {
        let conn = open_memory();
        let run_id = start_run(&conn, b"ctx", "coder", "create").unwrap();
        let sha_a = insert_script_body(&conn, "S00-stance.kai body").unwrap();
        let sha_b = insert_script_body(&conn, "S10-tools.kai body").unwrap();
        record_run_script(&conn, &run_id, "S00-stance.kai", &sha_a, Some(0), Some(1)).unwrap();
        record_run_script(&conn, &run_id, "S10-tools.kai", &sha_b, Some(0), Some(2)).unwrap();

        let scripts = list_run_scripts(&conn, &run_id).unwrap();
        assert_eq!(scripts.len(), 2);
        assert_eq!(scripts[0].path, "S00-stance.kai");
        assert_eq!(scripts[1].path, "S10-tools.kai");
        assert_eq!(scripts[0].seq, 0);
        assert_eq!(scripts[1].seq, 1);
    }
}
