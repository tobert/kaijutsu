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

/// List every run, most recently started first — the "checklist of what
/// ran" a human or `kj ledger runs` reads to answer "did the rc lifecycle
/// actually fire" without already knowing a `run_id` to look up.
pub fn list_runs(conn: &Connection) -> Result<Vec<RcRunRow>> {
    let mut stmt = conn.prepare(
        "SELECT run_id, context_id, context_type, verb, started_at, finished_at, outcome
         FROM rc_runs ORDER BY started_at DESC",
    )?;
    let rows = stmt.query_map([], row_to_run)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Filter + page for [`list_runs_filtered`] — `kj ledger runs`'s
/// `--limit`/`--since`/`--context`/`--verb`. [`list_runs`] above stays
/// unfiltered/unbounded for its existing caller (`kj rc` lifecycle
/// bookkeeping in `kaijutsu-kernel`); this struct is the CLI's own query,
/// kept separate so that caller's signature never has to change.
#[derive(Debug, Clone, Default)]
pub struct RunListFilter {
    pub context_id: Option<Vec<u8>>,
    pub verb: Option<String>,
    /// `started_at >= since_ms`, if set — an absolute epoch-ms cutoff the
    /// caller resolves against "now" before calling in (same convention
    /// as [`crate::ask::AskListFilter::since_ms`]).
    pub since_ms: Option<i64>,
    pub limit: i64,
}

/// Run `filter` against `rc_runs`, newest-started first, and return the
/// page plus the COUNT of every matching row before `LIMIT` — the number
/// `kj ledger runs` needs to say "showing N of TOTAL" when a listing was
/// cut (same reasoning as [`crate::ask::list_asks_filtered`]'s doc).
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
        "SELECT run_id, context_id, context_type, verb, started_at, finished_at, outcome
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
///
/// `started_at` must be captured by the caller *before* the script runs —
/// not left to the `rc_run_scripts.started_at` SQL `DEFAULT`, which fires
/// at INSERT time and is therefore always at or after `finished_at` (the
/// script has already finished by the time anything calls this function).
/// The schema `DEFAULT` stays in place as a backstop for any other writer,
/// but this is the one caller and it always supplies an explicit value.
pub fn record_run_script(
    conn: &Connection,
    run_id: &str,
    path: &str,
    body_sha256: &str,
    exit_code: Option<i64>,
    started_at: i64,
    finished_at: Option<i64>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO rc_run_scripts (run_id, seq, path, body_sha256, exit_code, started_at, finished_at)
         VALUES (?1, (SELECT COALESCE(MAX(seq), -1) + 1 FROM rc_run_scripts WHERE run_id = ?1),
                 ?2, ?3, ?4, ?5, ?6)",
        params![run_id, path, body_sha256, exit_code, started_at, finished_at],
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
    fn list_runs_returns_every_run_newest_first() {
        let conn = open_memory();
        let first = start_run(&conn, b"ctx-a", "coder", "create").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2)); // distinct started_at
        let second = start_run(&conn, b"ctx-b", "musician", "fork").unwrap();
        finish_run(&conn, &first, RcOutcome::Ok).unwrap();

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
        record_run_script(&conn, &run_id, "S00-stance.kai", &sha_a, Some(0), 1, Some(2)).unwrap();
        record_run_script(&conn, &run_id, "S10-tools.kai", &sha_b, Some(0), 3, Some(4)).unwrap();

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
    /// Before the fix, `record_run_script` didn't accept a `started_at` at
    /// all — the SQL `DEFAULT` on `rc_run_scripts.started_at` fired at
    /// *insert* time, which is necessarily after the caller already
    /// captured `finished_at` (the script already ran by the time anyone
    /// calls this function). This test pins the real ordering: capture
    /// `finished_at`, let measurable time pass, THEN call the recording
    /// function — reproducing exactly the shape of the bug (insert
    /// happens strictly after the timestamp the caller is trying to
    /// record against) instead of relying on it showing up at ms
    /// resolution by chance, which is why it was hidden in production.
    #[test]
    fn started_at_is_measurably_before_finished_at() {
        let conn = open_memory();
        let run_id = start_run(&conn, b"ctx", "coder", "create").unwrap();
        let sha = insert_script_body(&conn, "S00-stance.kai body").unwrap();

        let started_at = crate::time::now_millis();
        std::thread::sleep(std::time::Duration::from_millis(50)); // measurable script runtime
        let finished_at = crate::time::now_millis();

        record_run_script(&conn, &run_id, "S00-stance.kai", &sha, Some(0), started_at, Some(finished_at)).unwrap();

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
