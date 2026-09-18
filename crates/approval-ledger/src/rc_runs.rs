//! Durable records for rc lifecycle runs and their scripts.
//!
//! A script is begun before execution, then retains its result and records
//! its context-log projection. A run finishes only after its begun scripts
//! are settled.
//!
//! An approval may refer to an rc run, but this module does not require an
//! approval to exist.

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
fn finish_run(conn: &Connection, run_id: &str, outcome: RcOutcome) -> Result<()> {
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

/// Record the discovered script count once, before any source executes.
/// Fewer begun script rows mean the run stopped before attempting the full set.
pub fn set_run_script_count(conn: &Connection, run_id: &str, count: usize) -> Result<()> {
    let count = i64::try_from(count).expect("a run's script_count fits in i64");
    let rows = conn.execute(
        "UPDATE rc_runs SET script_count = ?1 WHERE run_id = ?2 AND script_count IS NULL",
        params![count, run_id],
    )?;
    if rows > 0 {
        return Ok(());
    }
    match get_run(conn, run_id)? {
        None => Err(LedgerError::RunNotFound(run_id.to_string())),
        Some(_) => Err(LedgerError::RunScriptCountAlreadySet(run_id.to_string())),
    }
}

/// List every run, most recently started first — the "checklist of what
/// ran" a human or `kj ledger runs` reads to answer "did the rc lifecycle
/// actually fire" without already knowing a `run_id` to look up.
pub fn list_runs(conn: &Connection) -> Result<Vec<RcRunRow>> {
    let mut stmt = conn.prepare(
        "SELECT run_id, context_id, context_type, verb, started_at, finished_at, outcome, script_count, intended_outcome
         FROM rc_runs ORDER BY started_at DESC",
    )?;
    let rows = stmt.query_map([], row_to_run)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Filters and pagination for [`list_runs_filtered`].
#[derive(Debug, Clone, Default)]
pub struct RunListFilter {
    pub context_id: Option<Vec<u8>>,
    pub verb: Option<String>,
    /// Include runs started at or after this epoch-millisecond cutoff.
    pub since_ms: Option<i64>,
    pub limit: i64,
}

/// Return the newest matching page and the count before the limit.
pub fn list_runs_filtered(conn: &Connection, filter: &RunListFilter) -> Result<(Vec<RcRunRow>, i64)> {
    use rusqlite::types::Value;

    let mut where_sql = Vec::new();
    let mut params: Vec<Value> = Vec::new();

    if let Some(context_id) = &filter.context_id {
        where_sql.push("context_id = ?".to_string());
        params.push(Value::Blob(context_id.clone()));
    }
    if let Some(verb) = &filter.verb {
        where_sql.push("verb = ?".to_string());
        params.push(Value::Text(verb.clone()));
    }
    if let Some(since_ms) = filter.since_ms {
        where_sql.push("started_at >= ?".to_string());
        params.push(Value::Integer(since_ms));
    }
    let where_clause =
        if where_sql.is_empty() { String::new() } else { format!("WHERE {}", where_sql.join(" AND ")) };

    let count_sql = format!("SELECT COUNT(*) FROM rc_runs {where_clause}");
    let total: i64 =
        conn.query_row(&count_sql, rusqlite::params_from_iter(params.iter().cloned()), |row| row.get(0))?;

    let select_sql = format!(
        "SELECT run_id, context_id, context_type, verb, started_at, finished_at, outcome, script_count, intended_outcome
         FROM rc_runs {where_clause} ORDER BY started_at DESC LIMIT ?"
    );
    let mut select_params = params;
    select_params.push(Value::Integer(filter.limit));
    let mut stmt = conn.prepare(&select_sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(select_params.into_iter()), row_to_run)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((rows, total))
}

pub fn get_run(conn: &Connection, run_id: &str) -> Result<Option<RcRunRow>> {
    conn.query_row(
        "SELECT run_id, context_id, context_type, verb, started_at, finished_at, outcome, script_count, intended_outcome
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
        script_count: row.get(7)?,
        intended_outcome: row
            .get::<_, Option<String>>(8)?
            .map(|raw| parse_enum::<RcOutcome>("intended_outcome", &raw))
            .transpose()
            .map_err(|e| match e {
                LedgerError::Db(inner) => inner,
                other => rusqlite::Error::InvalidColumnType(8, other.to_string(), rusqlite::types::Type::Text),
            })?,
    })
}

/// Persist a script body and its ordered start record before executing it.
pub fn begin_run_script(
    conn: &Connection,
    run_id: &str,
    path: &str,
    body: &str,
    started_at: i64,
) -> Result<i64> {
    let tx = conn.unchecked_transaction()?;
    let run = tx
        .query_row(
            "SELECT finished_at, script_count, intended_outcome FROM rc_runs WHERE run_id = ?1",
            params![run_id],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((finished_at, script_count, intended_outcome)) = run else {
        return Err(LedgerError::RunNotFound(run_id.to_string()));
    };
    if finished_at.is_some() {
        return Err(LedgerError::RunAlreadyFinished(run_id.to_string()));
    }
    if intended_outcome.is_some() {
        return Err(LedgerError::RunSettlementStarted(run_id.to_string()));
    }
    let Some(script_count) = script_count else {
        return Err(LedgerError::RunScriptCountUnset(run_id.to_string()));
    };
    let seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), -1) + 1 FROM rc_run_scripts WHERE run_id = ?1",
        params![run_id],
        |row| row.get(0),
    )?;
    if seq >= script_count {
        return Err(LedgerError::RunScriptCountReached { run_id: run_id.to_string(), script_count });
    }
    let body_sha256 = hex::encode_sha256(body.as_bytes());
    tx.execute(
        "INSERT INTO script_bodies (sha256, body) VALUES (?1, ?2)
         ON CONFLICT(sha256) DO NOTHING",
        params![body_sha256, body],
    )?;
    tx.execute(
        "INSERT INTO rc_run_scripts (run_id, seq, path, body_sha256, started_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![run_id, seq, path, body_sha256, started_at],
    )?;
    tx.commit()?;
    Ok(seq)
}

/// Retain one script's opaque execution result before projecting output.
/// A retry with the same result and exit code does not change the row.
pub fn retain_run_script_result(
    conn: &Connection,
    run_id: &str,
    seq: i64,
    result_json: &str,
    exit_code: Option<i64>,
    finished_at: i64,
) -> Result<bool> {
    let rows = conn.execute(
        "UPDATE rc_run_scripts SET result_json = ?1, exit_code = ?2, finished_at = ?3
         WHERE run_id = ?4 AND seq = ?5 AND result_json IS NULL
           AND EXISTS (SELECT 1 FROM rc_runs WHERE run_id = ?4 AND finished_at IS NULL)",
        params![result_json, exit_code, finished_at, run_id, seq],
    )?;
    if rows > 0 {
        return Ok(true);
    }
    let run = get_run(conn, run_id)?.ok_or_else(|| LedgerError::RunNotFound(run_id.to_string()))?;
    let existing = conn
        .query_row(
            "SELECT result_json, exit_code FROM rc_run_scripts WHERE run_id = ?1 AND seq = ?2",
            params![run_id, seq],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<i64>>(1)?)),
        )
        .optional()?
        .ok_or_else(|| LedgerError::RunScriptNotFound { run_id: run_id.to_string(), seq })?;
    if let Some(existing_json) = existing.0 {
        if existing_json == result_json && existing.1 == exit_code {
            return Ok(false);
        }
        return Err(LedgerError::RunScriptResultConflict { run_id: run_id.to_string(), seq });
    }
    if run.finished_at.is_some() {
        return Err(LedgerError::RunAlreadyFinished(run_id.to_string()));
    }
    Err(LedgerError::Db(rusqlite::Error::QueryReturnedNoRows))
}

/// Record the context-log projection of a retained script result.
pub fn mark_run_script_projected(
    conn: &Connection,
    run_id: &str,
    seq: i64,
    output_block_id: Option<&str>,
    projected_at: i64,
) -> Result<bool> {
    let rows = conn.execute(
        "UPDATE rc_run_scripts SET output_block_id = ?1, projected_at = ?2
         WHERE run_id = ?3 AND seq = ?4 AND result_json IS NOT NULL AND projected_at IS NULL
           AND EXISTS (SELECT 1 FROM rc_runs WHERE run_id = ?3 AND finished_at IS NULL)",
        params![output_block_id, projected_at, run_id, seq],
    )?;
    if rows > 0 {
        return Ok(true);
    }
    let run = get_run(conn, run_id)?.ok_or_else(|| LedgerError::RunNotFound(run_id.to_string()))?;
    let script = conn
        .query_row(
            "SELECT result_json, projected_at, output_block_id FROM rc_run_scripts WHERE run_id = ?1 AND seq = ?2",
            params![run_id, seq],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| LedgerError::RunScriptNotFound { run_id: run_id.to_string(), seq })?;
    if script.0.is_none() {
        return Err(LedgerError::RunScriptResultNotRetained { run_id: run_id.to_string(), seq });
    }
    if script.1.is_some() {
        if script.2.as_deref() == output_block_id {
            return Ok(false);
        }
        return Err(LedgerError::RunScriptProjectionConflict { run_id: run_id.to_string(), seq });
    }
    if run.finished_at.is_some() {
        return Err(LedgerError::RunAlreadyFinished(run_id.to_string()));
    }
    Err(LedgerError::Db(rusqlite::Error::QueryReturnedNoRows))
}

/// List runs that have not reached a terminal outcome.
pub fn list_unfinished_runs(conn: &Connection) -> Result<Vec<RcRunRow>> {
    let mut stmt = conn.prepare(
        "SELECT run_id, context_id, context_type, verb, started_at, finished_at, outcome, script_count, intended_outcome
         FROM rc_runs WHERE finished_at IS NULL ORDER BY started_at DESC",
    )?;
    Ok(stmt.query_map([], row_to_run)?.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// List retained results that still need a context-log projection.
pub fn list_pending_script_projections(conn: &Connection) -> Result<Vec<(RcRunRow, RcRunScriptRow)>> {
    let mut stmt = conn.prepare(
        "SELECT r.run_id, r.context_id, r.context_type, r.verb, r.started_at, r.finished_at, r.outcome, r.script_count, r.intended_outcome,
                s.seq, s.path, s.body_sha256, s.exit_code, s.started_at, s.finished_at, s.result_json, s.projected_at, s.output_block_id
         FROM rc_runs r JOIN rc_run_scripts s ON s.run_id = r.run_id
         WHERE s.result_json IS NOT NULL AND s.projected_at IS NULL
         ORDER BY r.started_at DESC, s.seq",
    )?;
    let rows = stmt
        .query_map([], |row| {
            let run = row_to_run(row)?;
            Ok((
                run,
                RcRunScriptRow {
                    seq: row.get(9)?,
                    path: row.get(10)?,
                    body_sha256: row.get(11)?,
                    exit_code: row.get(12)?,
                    started_at: row.get(13)?,
                    finished_at: row.get(14)?,
                    result_json: row.get(15)?,
                    projected_at: row.get(16)?,
                    output_block_id: row.get(17)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Finish a run only after every begun script has both durable settlement
/// records. Failed and abandoned runs may have begun fewer scripts than the
/// recorded script count; an ok run may not.
pub fn finish_settled_run(conn: &Connection, run_id: &str, outcome: RcOutcome) -> Result<()> {
    conn.execute(
        "UPDATE rc_runs SET intended_outcome = ?1
         WHERE run_id = ?2 AND finished_at IS NULL AND intended_outcome IS NULL",
        params![outcome.as_str(), run_id],
    )?;
    let run = get_run(conn, run_id)?.ok_or_else(|| LedgerError::RunNotFound(run_id.to_string()))?;
    if run.finished_at.is_some() {
        return Err(LedgerError::RunAlreadyFinished(run_id.to_string()));
    }
    if let Some(recorded) = run.intended_outcome {
        if recorded != outcome {
            return Err(LedgerError::RunIntendedOutcomeConflict {
                run_id: run_id.to_string(),
                recorded: recorded.as_str().to_string(),
                requested: outcome.as_str().to_string(),
            });
        }
    } else {
        return Err(LedgerError::Db(rusqlite::Error::QueryReturnedNoRows));
    }
    let unsettled: i64 = conn.query_row(
        "SELECT COUNT(*) FROM rc_run_scripts
         WHERE run_id = ?1 AND (result_json IS NULL OR projected_at IS NULL)",
        params![run_id],
        |row| row.get(0),
    )?;
    if unsettled > 0 {
        return Err(LedgerError::RunNotSettled(run_id.to_string()));
    }
    if outcome == RcOutcome::Ok {
        let Some(script_count) = run.script_count else {
            return Err(LedgerError::RunNotSettled(run_id.to_string()));
        };
        let begun: i64 = conn.query_row(
            "SELECT COUNT(*) FROM rc_run_scripts WHERE run_id = ?1",
            params![run_id],
            |row| row.get(0),
        )?;
        if begun != script_count {
            return Err(LedgerError::RunNotSettled(run_id.to_string()));
        }
    }
    finish_run(conn, run_id, outcome)
}

pub fn list_run_scripts(conn: &Connection, run_id: &str) -> Result<Vec<RcRunScriptRow>> {
    let mut stmt = conn.prepare(
        "SELECT seq, path, body_sha256, exit_code, started_at, finished_at, result_json, projected_at, output_block_id
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
                result_json: row.get(6)?,
                projected_at: row.get(7)?,
                output_block_id: row.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// A tiny local shim for the script-body digest without pulling in the
/// `hex` crate for one line.
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
    use std::sync::{Arc, Barrier};

    use rusqlite::hooks::{AuthAction, Authorization};

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

        set_run_script_count(&conn, &run_id, 0).unwrap();
        finish_settled_run(&conn, &run_id, RcOutcome::Ok).unwrap();
        let row = get_run(&conn, &run_id).unwrap().unwrap();
        assert!(row.finished_at.is_some());
        assert_eq!(row.outcome, Some(RcOutcome::Ok));
    }

    #[test]
    fn finishing_an_unknown_run_is_not_found() {
        let conn = open_memory();
        assert!(matches!(finish_settled_run(&conn, "nope", RcOutcome::Ok).unwrap_err(), LedgerError::RunNotFound(_)));
    }

    #[test]
    fn finishing_twice_is_refused_not_overwritten() {
        let conn = open_memory();
        let run_id = start_run(&conn, b"ctx", "coder", "create").unwrap();
        set_run_script_count(&conn, &run_id, 0).unwrap();
        finish_settled_run(&conn, &run_id, RcOutcome::Ok).unwrap();
        let err = finish_settled_run(&conn, &run_id, RcOutcome::Failed).unwrap_err();
        assert!(matches!(err, LedgerError::RunAlreadyFinished(_)));
        // Still `ok` — the second call's `Failed` must not have landed.
        assert_eq!(get_run(&conn, &run_id).unwrap().unwrap().outcome, Some(RcOutcome::Ok));
    }

    #[test]
    fn set_run_script_count_round_trips_through_get_run() {
        let conn = open_memory();
        let run_id = start_run(&conn, b"ctx", "coder", "create").unwrap();
        assert!(get_run(&conn, &run_id).unwrap().unwrap().script_count.is_none());

        set_run_script_count(&conn, &run_id, 3).unwrap();
        assert_eq!(get_run(&conn, &run_id).unwrap().unwrap().script_count, Some(3));
    }

    #[test]
    fn set_run_script_count_on_an_unknown_run_is_not_found() {
        let conn = open_memory();
        assert!(matches!(
            set_run_script_count(&conn, "nope", 1).unwrap_err(),
            LedgerError::RunNotFound(_)
        ));
    }

    #[test]
    fn setting_script_count_twice_is_refused_not_overwritten() {
        let conn = open_memory();
        let run_id = start_run(&conn, b"ctx", "coder", "create").unwrap();
        set_run_script_count(&conn, &run_id, 3).unwrap();
        let err = set_run_script_count(&conn, &run_id, 5).unwrap_err();
        assert!(matches!(err, LedgerError::RunScriptCountAlreadySet(_)));
        // Still 3 — the second call's 5 must not have landed.
        assert_eq!(get_run(&conn, &run_id).unwrap().unwrap().script_count, Some(3));
    }

    #[test]
    fn list_runs_returns_every_run_newest_first() {
        let conn = open_memory();
        let first = start_run(&conn, b"ctx-a", "coder", "create").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2)); // distinct started_at
        let second = start_run(&conn, b"ctx-b", "musician", "fork").unwrap();
        set_run_script_count(&conn, &first, 0).unwrap();
        finish_settled_run(&conn, &first, RcOutcome::Ok).unwrap();

        let runs = list_runs(&conn).unwrap();
        let ids: Vec<&str> = runs.iter().map(|r| r.run_id.as_str()).collect();
        assert_eq!(ids, vec![second.as_str(), first.as_str()], "newest first");
        assert_eq!(runs[1].outcome, Some(RcOutcome::Ok));
        assert!(runs[0].outcome.is_none(), "the unfinished run has no outcome yet");
    }

    #[test]
    fn list_runs_on_an_empty_table_is_empty() {
        let conn = open_memory();
        assert!(list_runs(&conn).unwrap().is_empty());
    }

    #[test]
    fn identical_script_bodies_dedupe_to_one_row() {
        let conn = open_memory();
        let first = start_run(&conn, b"ctx", "coder", "create").unwrap();
        let second = start_run(&conn, b"ctx", "coder", "create").unwrap();
        set_run_script_count(&conn, &first, 1).unwrap();
        set_run_script_count(&conn, &second, 1).unwrap();
        begin_run_script(&conn, &first, "S00.kai", "echo hi", 1).unwrap();
        begin_run_script(&conn, &second, "S00.kai", "echo hi", 1).unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM script_bodies", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn run_scripts_round_trip_in_order() {
        let conn = open_memory();
        let run_id = start_run(&conn, b"ctx", "coder", "create").unwrap();
        set_run_script_count(&conn, &run_id, 2).unwrap();
        begin_run_script(&conn, &run_id, "S00-stance.kai", "S00-stance.kai body", 1).unwrap();
        begin_run_script(&conn, &run_id, "S10-tools.kai", "S10-tools.kai body", 3).unwrap();

        let scripts = list_run_scripts(&conn, &run_id).unwrap();
        assert_eq!(scripts.len(), 2);
        assert_eq!(scripts[0].path, "S00-stance.kai");
        assert_eq!(scripts[1].path, "S10-tools.kai");
        assert_eq!(scripts[0].seq, 0);
        assert_eq!(scripts[1].seq, 1);
    }

    /// The real invariant: a script's `started_at` must never be after its
    /// `finished_at`, and a script that takes measurable time must record a
    /// `started_at` measurably before its `finished_at`.
    ///
    #[test]
    fn started_at_is_measurably_before_finished_at() {
        let conn = open_memory();
        let run_id = start_run(&conn, b"ctx", "coder", "create").unwrap();
        set_run_script_count(&conn, &run_id, 1).unwrap();

        let started_at = crate::time::now_millis();
        begin_run_script(&conn, &run_id, "S00-stance.kai", "S00-stance.kai body", started_at).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50)); // measurable script runtime
        let finished_at = crate::time::now_millis();
        retain_run_script_result(&conn, &run_id, 0, "ok", Some(0), finished_at).unwrap();

        let scripts = list_run_scripts(&conn, &run_id).unwrap();
        assert_eq!(scripts.len(), 1);
        let row = &scripts[0];
        let recorded_finished_at = row.finished_at.expect("finished_at was supplied");
        assert!(
            row.started_at <= recorded_finished_at,
            "started_at ({}) must not be after finished_at ({})",
            row.started_at,
            recorded_finished_at
        );
        assert!(
            recorded_finished_at - row.started_at >= 10,
            "a script that measurably slept must record a measurable duration; got started_at={} finished_at={}",
            row.started_at,
            recorded_finished_at
        );
    }

    #[test]
    fn a_script_is_begun_before_execution_and_settled_in_two_durable_steps() {
        let conn = open_memory();
        let run_id = start_run(&conn, b"ctx", "coder", "create").unwrap();
        set_run_script_count(&conn, &run_id, 1).unwrap();

        assert_eq!(begin_run_script(&conn, &run_id, "S00.kai", "echo ready", 10).unwrap(), 0);
        let script = &list_run_scripts(&conn, &run_id).unwrap()[0];
        let body_sha256: String = conn
            .query_row("SELECT body_sha256 FROM rc_run_scripts WHERE run_id = ?1 AND seq = 0", params![run_id], |row| row.get(0))
            .unwrap();
        assert_eq!(script.body_sha256, body_sha256);
        assert_eq!(script.result_json, None);

        assert!(retain_run_script_result(&conn, &run_id, 0, r#"{\"completed\":true}"#, Some(0), 20).unwrap());
        assert!(mark_run_script_projected(&conn, &run_id, 0, Some("block-1"), 30).unwrap());
        let script = &list_run_scripts(&conn, &run_id).unwrap()[0];
        assert_eq!(script.result_json.as_deref(), Some(r#"{\"completed\":true}"#));
        assert_eq!(script.exit_code, Some(0));
        assert_eq!(script.finished_at, Some(20));
        assert_eq!(script.projected_at, Some(30));
        assert_eq!(script.output_block_id.as_deref(), Some("block-1"));
    }

    #[test]
    fn retained_results_and_projections_are_idempotent_but_conflicts_fail() {
        let conn = open_memory();
        let run_id = start_run(&conn, b"ctx", "coder", "create").unwrap();
        set_run_script_count(&conn, &run_id, 1).unwrap();
        begin_run_script(&conn, &run_id, "S00.kai", "echo ready", 10).unwrap();

        assert!(retain_run_script_result(&conn, &run_id, 0, "first", Some(0), 20).unwrap());
        assert!(!retain_run_script_result(&conn, &run_id, 0, "first", Some(0), 99).unwrap());
        assert!(matches!(
            retain_run_script_result(&conn, &run_id, 0, "second", Some(0), 20).unwrap_err(),
            LedgerError::RunScriptResultConflict { .. }
        ));
        assert!(mark_run_script_projected(&conn, &run_id, 0, None, 30).unwrap());
        assert!(!mark_run_script_projected(&conn, &run_id, 0, None, 99).unwrap());
        assert!(matches!(
            mark_run_script_projected(&conn, &run_id, 0, Some("other"), 30).unwrap_err(),
            LedgerError::RunScriptProjectionConflict { .. }
        ));
    }

    #[test]
    fn result_retention_does_not_overwrite_a_writer_that_commits_after_its_read() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_owned();
        let setup = Connection::open(&path).unwrap();
        crate::migrate(&setup).unwrap();
        let run_id = start_run(&setup, b"ctx", "coder", "create").unwrap();
        set_run_script_count(&setup, &run_id, 1).unwrap();
        begin_run_script(&setup, &run_id, "S00.kai", "echo ready", 10).unwrap();
        drop(setup);

        let entered_update = Arc::new(Barrier::new(2));
        let release_update = Arc::new(Barrier::new(2));
        let thread_run = run_id.clone();
        let thread_path = path.clone();
        let entered = Arc::clone(&entered_update);
        let release = Arc::clone(&release_update);
        let writer = std::thread::spawn(move || {
            let conn = Connection::open(thread_path).unwrap();
            conn.authorizer(Some(move |context: rusqlite::hooks::AuthContext<'_>| {
                if matches!(context.action, AuthAction::Update { table_name, column_name }
                    if table_name == "rc_run_scripts" && column_name == "result_json")
                {
                    entered.wait();
                    release.wait();
                }
                Authorization::Allow
            }))
            .unwrap();
            retain_run_script_result(&conn, &thread_run, 0, "late", Some(0), 20)
        });

        entered_update.wait();
        let winner = Connection::open(&path).unwrap();
        winner.execute(
            "UPDATE rc_run_scripts SET result_json = 'first', exit_code = 0, finished_at = 15
             WHERE run_id = ?1 AND seq = 0",
            params![run_id],
        ).unwrap();
        release_update.wait();

        assert!(matches!(writer.join().unwrap().unwrap_err(), LedgerError::RunScriptResultConflict { .. }));
        let script = list_run_scripts(&winner, &run_id).unwrap().pop().unwrap();
        assert_eq!(script.result_json.as_deref(), Some("first"));
    }

    #[test]
    fn settled_finish_requires_every_ok_script_and_no_unsettled_begin() {
        let conn = open_memory();
        let run_id = start_run(&conn, b"ctx", "coder", "create").unwrap();
        set_run_script_count(&conn, &run_id, 2).unwrap();
        begin_run_script(&conn, &run_id, "S00.kai", "one", 10).unwrap();
        assert!(matches!(finish_settled_run(&conn, &run_id, RcOutcome::Failed).unwrap_err(), LedgerError::RunNotSettled(_)));
        assert_eq!(get_run(&conn, &run_id).unwrap().unwrap().intended_outcome, Some(RcOutcome::Failed));
        assert!(matches!(
            begin_run_script(&conn, &run_id, "S10.kai", "two", 11).unwrap_err(),
            LedgerError::RunSettlementStarted(_)
        ));
        assert!(matches!(
            finish_settled_run(&conn, &run_id, RcOutcome::Abandoned).unwrap_err(),
            LedgerError::RunIntendedOutcomeConflict { .. }
        ));
        retain_run_script_result(&conn, &run_id, 0, "failed", Some(1), 20).unwrap();
        mark_run_script_projected(&conn, &run_id, 0, None, 30).unwrap();
        finish_settled_run(&conn, &run_id, RcOutcome::Failed).unwrap();

        let ok_run = start_run(&conn, b"ctx", "coder", "create").unwrap();
        set_run_script_count(&conn, &ok_run, 2).unwrap();
        begin_run_script(&conn, &ok_run, "S00.kai", "one", 10).unwrap();
        retain_run_script_result(&conn, &ok_run, 0, "ok", Some(0), 20).unwrap();
        mark_run_script_projected(&conn, &ok_run, 0, None, 30).unwrap();
        assert!(matches!(finish_settled_run(&conn, &ok_run, RcOutcome::Ok).unwrap_err(), LedgerError::RunNotSettled(_)));
    }

    #[test]
    fn unfinished_and_pending_projection_lists_expose_recovery_work() {
        let conn = open_memory();
        let run_id = start_run(&conn, b"ctx", "coder", "create").unwrap();
        set_run_script_count(&conn, &run_id, 1).unwrap();
        begin_run_script(&conn, &run_id, "S00.kai", "one", 10).unwrap();
        retain_run_script_result(&conn, &run_id, 0, "ok", Some(0), 20).unwrap();

        assert_eq!(list_unfinished_runs(&conn).unwrap().iter().map(|run| &run.run_id).collect::<Vec<_>>(), vec![&run_id]);
        let pending = list_pending_script_projections(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0.run_id, run_id);
        assert_eq!(pending[0].1.seq, 0);

        mark_run_script_projected(&conn, &run_id, 0, None, 30).unwrap();
        finish_settled_run(&conn, &run_id, RcOutcome::Ok).unwrap();
        assert!(list_unfinished_runs(&conn).unwrap().is_empty());
        assert!(list_pending_script_projections(&conn).unwrap().is_empty());
    }

    // ── `list_runs_filtered` (kj ledger runs --limit/--since/--context/--verb) ──

    fn set_started_at(conn: &Connection, run_id: &str, ms: i64) {
        conn.execute("UPDATE rc_runs SET started_at = ?1 WHERE run_id = ?2", params![ms, run_id]).unwrap();
    }

    #[test]
    fn list_runs_filtered_limit_caps_rows_and_reports_the_true_total() {
        let conn = open_memory();
        for _ in 0..5 {
            start_run(&conn, b"ctx", "coder", "create").unwrap();
        }
        let filter = RunListFilter { limit: 2, ..Default::default() };
        let (rows, total) = list_runs_filtered(&conn, &filter).unwrap();
        assert_eq!(rows.len(), 2, "limit must cap the returned page");
        assert_eq!(total, 5, "the count must reflect every matching row, not just the page");
    }

    #[test]
    fn list_runs_filtered_since_ms_excludes_older_runs() {
        let conn = open_memory();
        let old_run = start_run(&conn, b"ctx", "coder", "create").unwrap();
        set_started_at(&conn, &old_run, 1_000);
        let new_run = start_run(&conn, b"ctx", "coder", "create").unwrap();
        set_started_at(&conn, &new_run, 10_000);

        let filter = RunListFilter { since_ms: Some(5_000), limit: 20, ..Default::default() };
        let (rows, total) = list_runs_filtered(&conn, &filter).unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].run_id, new_run);
    }

    #[test]
    fn list_runs_filtered_context_narrows_to_the_requested_context() {
        let conn = open_memory();
        let ctx_a_run = start_run(&conn, b"ctx-a", "coder", "create").unwrap();
        start_run(&conn, b"ctx-b", "coder", "create").unwrap();

        let filter =
            RunListFilter { context_id: Some(b"ctx-a".to_vec()), limit: 20, ..Default::default() };
        let (rows, total) = list_runs_filtered(&conn, &filter).unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].run_id, ctx_a_run);
    }

    #[test]
    fn list_runs_filtered_verb_narrows_to_the_requested_verb() {
        let conn = open_memory();
        let fork_run = start_run(&conn, b"ctx", "coder", "fork").unwrap();
        start_run(&conn, b"ctx", "coder", "create").unwrap();

        let filter = RunListFilter { verb: Some("fork".to_string()), limit: 20, ..Default::default() };
        let (rows, total) = list_runs_filtered(&conn, &filter).unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].run_id, fork_run);
    }
}
