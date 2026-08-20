//! Creating and reading back an ask.
//!
//! `create_ask` is guarantee 1: it returns only after everything —
//! the `approvals` row, its ordered statement list (each statement's tree
//! inserted the first time its digest is seen), options, and signals — is
//! committed to disk. There is no code path that hands back a
//! `request_id` before the row exists durably; a caller that crashes
//! between `create_ask` returning and showing a prompt loses nothing but
//! the prompt, and the row is there, `pending`, on restart.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use rusqlite::Transaction;

use crate::error::{LedgerError, Result};
use crate::events;
use crate::time::now_millis;
use crate::types::{
    ApprovalRow, AskStatementRow, EventKind, EventRow, NewAsk, NewPlanStatement, NewPlannedValue,
    Origin, OptionRow, PlanCommandRow, PlanRedirectRow, PlanStatementRow, PlannedValueRow,
    SignalRow, SignalSourceKind, SignalVerdict, ValueKind, VarBinding, parse_enum,
};

/// Durably record one ask and return its `request_id`. Commits before
/// returning — see the module doc for why that is the whole point.
///
/// Each entry in `req.statements` is inserted only the first time its
/// `statement_digest` is seen (content-addressed — `schema.rs` header); a
/// repeat digest just adds a new `approval_ask_statements` join row
/// pointing at the existing tree. Position in `req.statements` becomes
/// `stmt_seq`.
pub fn create_ask(conn: &Connection, req: &NewAsk) -> Result<String> {
    let request_id = uuid::Uuid::now_v7().to_string();
    // DEFERRED is fine here (not the claim path's contested `BEGIN
    // IMMEDIATE`): nothing else can reference `request_id` before this
    // function hands it back, so there is no race to lose.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
    insert_ask(&tx, &request_id, req)?;
    tx.commit()?;
    Ok(request_id)
}

/// Create an ask that is decided `Allowed` in the SAME transaction as its
/// creation — the log-only path for an advisory classifier (Amy's ruling:
/// *"log-only = an auto-approved ask with the classifier signal attached;
/// `approval_signals.request_id` stays NOT NULL"* — never a nullable
/// half-row). `req.signals` (typically one `NewSignal` with
/// `source_kind: Classifier`) is what attaches the classifier's read;
/// this function only decides who gets credit for the ALLOW.
///
/// `auto_reason` names the source that auto-allowed this ask (e.g.
/// `"lfm2d:kube_ordinal_v8 (log-only)"`) and lands on both the `approvals`
/// row's `auto_reason` column and the `approval_events` "decided" row's
/// `auto_reason` — an audit read can tell "a human said yes"
/// (`decided_by` set, `auto_reason` NULL) from "a classifier auto-allowed
/// this in log-only mode" (`decided_by` NULL, `auto_reason` set) without
/// ever inspecting `approval_signals`. `decided_option` is the fixed
/// literal `"auto_allow"` — there is no human-offered option list to pick
/// from (`req.options` is typically empty for this path; nothing here
/// requires it to be).
///
/// Deliberately NOT a thin wrapper calling [`create_ask`] then
/// [`crate::decide::decide`]: SQLite has no nested transactions, so the
/// "same transaction" guarantee this function exists to provide can only
/// be had by inlining the insert and the decide-update under one
/// `Transaction`, not by composing two functions that each open their own.
pub fn create_auto_allowed_ask(conn: &Connection, req: &NewAsk, auto_reason: &str) -> Result<String> {
    let request_id = uuid::Uuid::now_v7().to_string();
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
    insert_ask(&tx, &request_id, req)?;

    let now = now_millis();
    let decided_option = "auto_allow";
    let updated = tx.execute(
        "UPDATE approvals SET status = 'allowed', decided_at = ?1, decided_option = ?2, auto_reason = ?3
         WHERE request_id = ?4 AND status = 'pending'",
        params![now, decided_option, auto_reason, request_id],
    )?;
    debug_assert_eq!(updated, 1, "the row this function just inserted must still be pending");

    events::append(
        &tx,
        &request_id,
        EventKind::Decided,
        None,
        Some(decided_option),
        None,
        Some(auto_reason),
        None,
    )?;

    tx.commit()?;
    Ok(request_id)
}

/// Shared insert body for [`create_ask`] and [`create_auto_allowed_ask`]:
/// the `approvals` row plus its statements/options/signals. Leaves the row
/// `pending` — callers decide whether (and how) to transition it further,
/// inside the same transaction `tx` already holds open.
fn insert_ask(tx: &Transaction, request_id: &str, req: &NewAsk) -> Result<()> {
    tx.execute(
        "INSERT INTO approvals (
            request_id, context_id, principal_id, origin, instance, tool, hook_id,
            description, authorized_label, rc_run_id, expires_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            request_id,
            req.context_id,
            req.principal_id,
            req.origin.as_str(),
            req.instance,
            req.tool,
            req.hook_id,
            req.description,
            req.authorized_label,
            req.rc_run_id,
            req.expires_at,
        ],
    )?;

    for (stmt_seq, stmt) in req.statements.iter().enumerate() {
        insert_statement_if_new(tx, stmt)?;
        tx.execute(
            "INSERT INTO approval_ask_statements (request_id, stmt_seq, statement_digest)
             VALUES (?1, ?2, ?3)",
            params![request_id, stmt_seq as i64, stmt.statement_digest],
        )?;
    }

    for (seq, opt) in req.options.iter().enumerate() {
        tx.execute(
            "INSERT INTO approval_options (request_id, seq, option_id, label, kind)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![request_id, seq as i64, opt.option_id, opt.label, opt.kind],
        )?;
    }

    for (seq, sig) in req.signals.iter().enumerate() {
        tx.execute(
            "INSERT INTO approval_signals (
                request_id, seq, source_kind, source_id, model_id, weight_hash,
                stmt_seq, cmd_seq, label, score, verdict
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                request_id,
                seq as i64,
                sig.source_kind.as_str(),
                sig.source_id,
                sig.model_id,
                sig.weight_hash,
                sig.stmt_seq,
                sig.cmd_seq,
                sig.label,
                sig.score,
                sig.verdict.as_str(),
            ],
        )?;
    }

    Ok(())
}

/// Insert `stmt`'s tree, but only if `stmt.statement_digest` isn't already
/// recorded — content-addressed dedup (see `schema.rs` header). `tx`
/// derefs to `&Connection`, so this shares the outer transaction; a
/// failure here rolls back the whole `create_ask` call, never a
/// half-written statement.
fn insert_statement_if_new(tx: &Transaction, stmt: &NewPlanStatement) -> Result<()> {
    let already_known: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM approval_statements WHERE statement_digest = ?1",
            params![stmt.statement_digest],
            |row| row.get(0),
        )
        .optional()?;
    if already_known.is_some() {
        return Ok(());
    }

    let has_free_vars = stmt.vars.iter().any(|v| v.binding == VarBinding::Free);
    tx.execute(
        "INSERT INTO approval_statements (statement_digest, rendered, statement_kind, has_free_vars)
         VALUES (?1, ?2, ?3, ?4)",
        params![stmt.statement_digest, stmt.rendered, stmt.statement_kind, has_free_vars as i64],
    )?;

    for (cmd_seq, cmd) in stmt.commands.iter().enumerate() {
        let cmd_seq = cmd_seq as i64;
        tx.execute(
            "INSERT INTO approval_statement_commands (statement_digest, cmd_seq, name, backgrounded)
             VALUES (?1, ?2, ?3, ?4)",
            params![stmt.statement_digest, cmd_seq, cmd.name, cmd.backgrounded as i64],
        )?;

        for (arg_seq, arg) in cmd.args.iter().enumerate() {
            insert_planned_value_as_arg(tx, &stmt.statement_digest, cmd_seq, arg_seq as i64, arg)?;
        }
        for (redir_seq, redir) in cmd.redirects.iter().enumerate() {
            insert_planned_value_as_redirect(tx, &stmt.statement_digest, cmd_seq, redir_seq as i64, redir)?;
        }
    }

    for var in &stmt.vars {
        tx.execute(
            "INSERT INTO approval_statement_vars (statement_digest, name, binding)
             VALUES (?1, ?2, ?3)",
            params![stmt.statement_digest, var.name, var.binding.as_str()],
        )?;
    }

    Ok(())
}

fn insert_planned_value_as_arg(
    tx: &Transaction,
    statement_digest: &str,
    cmd_seq: i64,
    arg_seq: i64,
    value: &NewPlannedValue,
) -> Result<()> {
    let (value_text, redact_kind, fingerprint) = split_value(value);
    tx.execute(
        "INSERT INTO approval_statement_args (
            statement_digest, cmd_seq, arg_seq, value_kind, value_text, redact_kind, fingerprint
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![statement_digest, cmd_seq, arg_seq, value.kind().as_str(), value_text, redact_kind, fingerprint],
    )?;
    Ok(())
}

fn insert_planned_value_as_redirect(
    tx: &Transaction,
    statement_digest: &str,
    cmd_seq: i64,
    redir_seq: i64,
    redir: &crate::types::NewPlanRedirect,
) -> Result<()> {
    let (value_text, redact_kind, fingerprint) = split_value(&redir.target);
    tx.execute(
        "INSERT INTO approval_statement_redirects (
            statement_digest, cmd_seq, redir_seq, op, value_kind, value_text, redact_kind, fingerprint
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            statement_digest,
            cmd_seq,
            redir_seq,
            redir.op,
            redir.target.kind().as_str(),
            value_text,
            redact_kind,
            fingerprint,
        ],
    )?;
    Ok(())
}

/// Split a `NewPlannedValue` into the three nullable columns the `CHECK`
/// in `schema.rs` requires stay in lockstep with `value_kind`.
fn split_value(value: &NewPlannedValue) -> (Option<&str>, Option<&str>, Option<&str>) {
    match value {
        NewPlannedValue::Plain(text) => (Some(text.as_str()), None, None),
        NewPlannedValue::Redacted { redact_kind, fingerprint } => {
            (None, Some(redact_kind.as_str()), fingerprint.as_deref())
        }
    }
}

/// Fetch one approval by id.
pub fn get_approval(conn: &Connection, request_id: &str) -> Result<Option<ApprovalRow>> {
    conn.query_row(
        "SELECT request_id, context_id, principal_id, origin, instance, tool, hook_id,
                description, authorized_label, rc_run_id, status, created_at,
                expires_at, claimed_at, claimed_by, decided_at, decided_by, decided_option,
                remember_scope, auto_reason
         FROM approvals WHERE request_id = ?1",
        params![request_id],
        row_to_approval,
    )
    .optional()
    .map_err(LedgerError::from)
}

pub(crate) fn row_to_approval(row: &rusqlite::Row) -> rusqlite::Result<ApprovalRow> {
    let origin_raw: String = row.get(3)?;
    let status_raw: String = row.get(10)?;
    Ok(ApprovalRow {
        request_id: row.get(0)?,
        context_id: row.get(1)?,
        principal_id: row.get(2)?,
        origin: parse_enum::<Origin>("origin", &origin_raw).map_err(sql_err)?,
        instance: row.get(4)?,
        tool: row.get(5)?,
        hook_id: row.get(6)?,
        description: row.get(7)?,
        authorized_label: row.get(8)?,
        rc_run_id: row.get(9)?,
        status: parse_enum::<crate::types::ApprovalStatus>("status", &status_raw).map_err(sql_err)?,
        created_at: row.get(11)?,
        expires_at: row.get(12)?,
        claimed_at: row.get(13)?,
        claimed_by: row.get(14)?,
        decided_at: row.get(15)?,
        decided_by: row.get(16)?,
        decided_option: row.get(17)?,
        remember_scope: row.get(18)?,
        auto_reason: row.get(19)?,
    })
}

/// A `LedgerError` can't cross the `rusqlite::Result` boundary a row-mapper
/// closure must return; fold it back into a `rusqlite::Error` (still
/// distinguishable — see `parse_enum`) and let the outer call site's
/// `?`/`From` unwrap it back into a `LedgerError` on the way out.
fn sql_err(e: LedgerError) -> rusqlite::Error {
    match e {
        LedgerError::Db(inner) => inner,
        other => rusqlite::Error::InvalidColumnType(0, other.to_string(), rusqlite::types::Type::Text),
    }
}

/// Every `pending` ask, oldest first — the CLI-answerer's queue
/// (`kj ledger list`). Same ordering as [`crate::claim::claim_next`]'s
/// `ORDER BY created_at`, so "what's next" reads the same whether a caller
/// claims one at a time or lists the whole backlog at once. Deliberately
/// `pending` only, not `claimed` too: a claimed-but-undecided row already
/// has an answerer working it (guarantee 5), and surfacing it in the same
/// queue would invite a second answerer to step on the first one's claim.
pub fn list_pending(conn: &Connection) -> Result<Vec<ApprovalRow>> {
    let mut stmt = conn.prepare(
        "SELECT request_id, context_id, principal_id, origin, instance, tool, hook_id,
                description, authorized_label, rc_run_id, status, created_at,
                expires_at, claimed_at, claimed_by, decided_at, decided_by, decided_option,
                remember_scope, auto_reason
         FROM approvals WHERE status = 'pending' ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map([], row_to_approval)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Every decided ask — `allowed`, `denied`, `expired`, or `abandoned`
/// (i.e. [`crate::types::ApprovalStatus::is_terminal`]) — most recently
/// created first, capped at `limit` rows. This is the audit-trail
/// read-back `list_pending` doesn't provide: `pending`/`claimed` rows
/// never appear here, and once a row lands here it never appears in
/// `list_pending` again (the two queries partition `approvals` by status).
/// Ordered by `created_at` rather than `decided_at` because `decide`'s
/// `expire`/`abandon` legs leave `decided_at` NULL (see `decide.rs`'s
/// `transition`) — `created_at` is the one timestamp every row has.
pub fn list_history(conn: &Connection, limit: i64) -> Result<Vec<ApprovalRow>> {
    let mut stmt = conn.prepare(
        "SELECT request_id, context_id, principal_id, origin, instance, tool, hook_id,
                description, authorized_label, rc_run_id, status, created_at,
                expires_at, claimed_at, claimed_by, decided_at, decided_by, decided_option,
                remember_scope, auto_reason
         FROM approvals WHERE status IN ('allowed', 'denied', 'expired', 'abandoned')
         ORDER BY created_at DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], row_to_approval)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Filter + page for [`list_asks_filtered`] — the CLI's `kj ledger list`
/// (`--limit`/`--since`/`--origin`/`--status`). `list_pending`/
/// `list_history` above are UNCHANGED and stay unfiltered/fixed-order —
/// the gate's polling loop and MCP's ask surfacing call those directly
/// and this struct exists so the CLI's new flags never have to reach
/// through those signatures.
#[derive(Debug, Clone)]
pub struct AskListFilter {
    /// `status IN (...)`. Never empty in practice — every caller passes
    /// at least one status; an empty list here would mean "no status
    /// filter at all", which no `kj ledger list` mode wants.
    pub statuses: Vec<crate::types::ApprovalStatus>,
    pub origin: Option<crate::types::Origin>,
    /// `created_at >= since_ms`, if set. An absolute epoch-ms cutoff —
    /// this crate never reads the wall clock for a filter (the one place
    /// it reads the clock at all is `time.rs`'s `now_millis`, and that's
    /// for values THIS crate writes, not a query bound); the caller
    /// resolves `--since <duration>` against "now" before calling in.
    pub since_ms: Option<i64>,
    pub limit: i64,
    /// `ORDER BY created_at DESC` when true (history-style, newest
    /// first), `ASC` when false (queue-style, oldest first — the order
    /// you drain a work queue in).
    pub newest_first: bool,
}

/// Run `filter` against `approvals` and return the page of rows plus the
/// COUNT of every row that matched before `LIMIT` was applied. The count
/// is what `kj ledger list` needs to say "showing N of TOTAL" whenever a
/// listing was cut — Amy's ruling: a silently truncated list is exactly
/// the quiet fallback CLAUDE.md treats as a defect, so the caller must be
/// able to tell "there were more rows" from "that's everything" without a
/// second query.
pub fn list_asks_filtered(conn: &Connection, filter: &AskListFilter) -> Result<(Vec<ApprovalRow>, i64)> {
    use rusqlite::types::Value;

    let mut where_sql = Vec::new();
    let mut params: Vec<Value> = Vec::new();

    if !filter.statuses.is_empty() {
        let placeholders = filter.statuses.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        where_sql.push(format!("status IN ({placeholders})"));
        params.extend(filter.statuses.iter().map(|s| Value::Text(s.as_str().to_string())));
    }
    if let Some(origin) = filter.origin {
        where_sql.push("origin = ?".to_string());
        params.push(Value::Text(origin.as_str().to_string()));
    }
    if let Some(since_ms) = filter.since_ms {
        where_sql.push("created_at >= ?".to_string());
        params.push(Value::Integer(since_ms));
    }
    let where_clause =
        if where_sql.is_empty() { String::new() } else { format!("WHERE {}", where_sql.join(" AND ")) };

    let count_sql = format!("SELECT COUNT(*) FROM approvals {where_clause}");
    let total: i64 =
        conn.query_row(&count_sql, rusqlite::params_from_iter(params.iter().cloned()), |row| row.get(0))?;

    let order = if filter.newest_first { "DESC" } else { "ASC" };
    let select_sql = format!(
        "SELECT request_id, context_id, principal_id, origin, instance, tool, hook_id,
                description, authorized_label, rc_run_id, status, created_at,
                expires_at, claimed_at, claimed_by, decided_at, decided_by, decided_option,
                remember_scope, auto_reason
         FROM approvals {where_clause} ORDER BY created_at {order} LIMIT ?"
    );
    let mut select_params = params;
    select_params.push(Value::Integer(filter.limit));
    let mut stmt = conn.prepare(&select_sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(select_params.into_iter()), row_to_approval)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((rows, total))
}

/// The choices offered on this ask, in presentation order.
pub fn list_options(conn: &Connection, request_id: &str) -> Result<Vec<OptionRow>> {
    let mut stmt = conn.prepare(
        "SELECT seq, option_id, label, kind FROM approval_options
         WHERE request_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt
        .query_map(params![request_id], |row| {
            Ok(OptionRow {
                seq: row.get(0)?,
                option_id: row.get(1)?,
                label: row.get(2)?,
                kind: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Attach one advisory signal to an ask that already exists — the
/// non-auto-allow half of `kj ledger signal add`: a hook body that already
/// created the ask via [`create_auto_allowed_ask`]'s winner call attaches
/// every OTHER scored clause this way, via `--request-id`. Fails with
/// [`LedgerError::NotFound`] rather than silently inserting an orphan row —
/// `approval_signals.request_id` is a real `REFERENCES approvals` foreign
/// key (`schema.rs`), and this is the same check made loud at the Rust
/// layer instead of only at the SQLite one. Not gated on the ask's own
/// status: a signal is an annotation, attachable to a still-pending, a
/// claimed, or an already-decided ask alike — it never changes what the
/// ask decided (see the crate root docs: a classifier's `verdict` is
/// advisory forever, never read by [`crate::rules::redeem`]).
pub fn add_signal(conn: &Connection, request_id: &str, sig: &crate::types::NewSignal) -> Result<SignalRow> {
    if get_approval(conn, request_id)?.is_none() {
        return Err(LedgerError::NotFound(request_id.to_string()));
    }
    let seq: i64 = conn.query_row(
        "INSERT INTO approval_signals (
            request_id, seq, source_kind, source_id, model_id, weight_hash,
            stmt_seq, cmd_seq, label, score, verdict
         ) VALUES (
            ?1, (SELECT COALESCE(MAX(seq), -1) + 1 FROM approval_signals WHERE request_id = ?1),
            ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10
         ) RETURNING seq",
        params![
            request_id,
            sig.source_kind.as_str(),
            sig.source_id,
            sig.model_id,
            sig.weight_hash,
            sig.stmt_seq,
            sig.cmd_seq,
            sig.label,
            sig.score,
            sig.verdict.as_str(),
        ],
        |row| row.get(0),
    )?;
    Ok(SignalRow {
        seq,
        source_kind: sig.source_kind,
        source_id: sig.source_id.clone(),
        model_id: sig.model_id.clone(),
        weight_hash: sig.weight_hash.clone(),
        stmt_seq: sig.stmt_seq,
        cmd_seq: sig.cmd_seq,
        label: sig.label.clone(),
        score: sig.score,
        verdict: sig.verdict,
    })
}

/// The advisory signals attached to this ask, in insertion order.
pub fn list_signals(conn: &Connection, request_id: &str) -> Result<Vec<SignalRow>> {
    let mut stmt = conn.prepare(
        "SELECT seq, source_kind, source_id, model_id, weight_hash, stmt_seq, cmd_seq, label, score, verdict
         FROM approval_signals WHERE request_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt
        .query_map(params![request_id], |row| {
            let source_kind_raw: String = row.get(1)?;
            let verdict_raw: String = row.get(9)?;
            Ok(SignalRow {
                seq: row.get(0)?,
                source_kind: parse_enum::<SignalSourceKind>("source_kind", &source_kind_raw).map_err(sql_err)?,
                source_id: row.get(2)?,
                model_id: row.get(3)?,
                weight_hash: row.get(4)?,
                stmt_seq: row.get(5)?,
                cmd_seq: row.get(6)?,
                label: row.get(7)?,
                score: row.get(8)?,
                verdict: parse_enum::<SignalVerdict>("verdict", &verdict_raw).map_err(sql_err)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The full append-only event history for this ask (guarantee 6) —
/// claims, decisions, and every rejected late attempt, oldest first.
pub fn list_events(conn: &Connection, request_id: &str) -> Result<Vec<EventRow>> {
    let mut stmt = conn.prepare(
        "SELECT seq, kind, actor, decided_option, remember_scope, auto_reason, note, created_at
         FROM approval_events WHERE request_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt
        .query_map(params![request_id], |row| {
            let kind_raw: String = row.get(1)?;
            Ok(EventRow {
                seq: row.get(0)?,
                kind: parse_enum::<EventKind>("kind", &kind_raw).map_err(sql_err)?,
                actor: row.get(2)?,
                decided_option: row.get(3)?,
                remember_scope: row.get(4)?,
                auto_reason: row.get(5)?,
                note: row.get(6)?,
                created_at: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Reconstruct one statement by its digest: its commands (ordered, each
/// with its args and redirects ordered) and its free/bound variable
/// lists. `Ok(None)` for an unknown digest — not an error, since a
/// caller may probe speculatively (e.g. before deciding whether to call
/// `create_ask` at all).
pub fn load_statement(conn: &Connection, statement_digest: &str) -> Result<Option<PlanStatementRow>> {
    let header: Option<(String, String)> = conn
        .query_row(
            "SELECT rendered, statement_kind FROM approval_statements WHERE statement_digest = ?1",
            params![statement_digest],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((rendered, statement_kind)) = header else {
        return Ok(None);
    };

    let commands = load_commands(conn, statement_digest)?;
    let (free_vars, bound_vars) = load_vars(conn, statement_digest)?;
    Ok(Some(PlanStatementRow {
        statement_digest: statement_digest.to_string(),
        rendered,
        statement_kind,
        commands,
        free_vars,
        bound_vars,
    }))
}

/// An ask's ordered statement list, each joined against its
/// content-addressed body — replaces reading a `plan_digest` off
/// `approvals` (which no longer exists; see `schema.rs` header).
pub fn load_ask_statements(conn: &Connection, request_id: &str) -> Result<Vec<AskStatementRow>> {
    let mut stmt_q = conn.prepare(
        "SELECT stmt_seq, statement_digest FROM approval_ask_statements
         WHERE request_id = ?1 ORDER BY stmt_seq",
    )?;
    let links: Vec<(i64, String)> = stmt_q
        .query_map(params![request_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut out = Vec::with_capacity(links.len());
    for (stmt_seq, statement_digest) in links {
        let statement = load_statement(conn, &statement_digest)?.ok_or_else(|| {
            // The join row's FK target vanished — only possible if a
            // caller wrote around this crate's API (or ran with FK
            // enforcement off and deleted a shared statement out from
            // under a still-live ask). Surface as a DB error rather than
            // silently dropping the statement from the ask's list.
            LedgerError::Db(rusqlite::Error::InvalidColumnType(
                0,
                format!("approval_ask_statements references unknown statement_digest {statement_digest:?}"),
                rusqlite::types::Type::Text,
            ))
        })?;
        out.push(AskStatementRow { stmt_seq, statement });
    }
    Ok(out)
}

fn load_commands(conn: &Connection, statement_digest: &str) -> Result<Vec<PlanCommandRow>> {
    let mut cmd_q = conn.prepare(
        "SELECT cmd_seq, name, backgrounded FROM approval_statement_commands
         WHERE statement_digest = ?1 ORDER BY cmd_seq",
    )?;
    let commands: Vec<(i64, String, bool)> = cmd_q
        .query_map(params![statement_digest], |row| {
            let backgrounded: i64 = row.get(2)?;
            Ok((row.get(0)?, row.get(1)?, backgrounded != 0))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut out = Vec::with_capacity(commands.len());
    for (cmd_seq, name, backgrounded) in commands {
        let args = load_values(
            conn,
            "SELECT value_kind, value_text, redact_kind, fingerprint FROM approval_statement_args
             WHERE statement_digest = ?1 AND cmd_seq = ?2 ORDER BY arg_seq",
            statement_digest,
            cmd_seq,
        )?;
        let redirects = load_redirects(conn, statement_digest, cmd_seq)?;
        out.push(PlanCommandRow { cmd_seq, name, args, redirects, backgrounded });
    }
    Ok(out)
}

fn load_values(conn: &Connection, sql: &str, statement_digest: &str, cmd_seq: i64) -> Result<Vec<PlannedValueRow>> {
    let mut q = conn.prepare(sql)?;
    let rows = q
        .query_map(params![statement_digest, cmd_seq], row_to_planned_value)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn row_to_planned_value(row: &rusqlite::Row) -> rusqlite::Result<PlannedValueRow> {
    let kind_raw: String = row.get(0)?;
    let value_text: Option<String> = row.get(1)?;
    let redact_kind: Option<String> = row.get(2)?;
    let fingerprint: Option<String> = row.get(3)?;
    match parse_enum::<ValueKind>("value_kind", &kind_raw).map_err(sql_err)? {
        ValueKind::Plain => Ok(PlannedValueRow::Plain(value_text.unwrap_or_default())),
        ValueKind::Redacted => Ok(PlannedValueRow::Redacted {
            redact_kind: redact_kind.unwrap_or_default(),
            fingerprint,
        }),
    }
}

fn load_redirects(conn: &Connection, statement_digest: &str, cmd_seq: i64) -> Result<Vec<PlanRedirectRow>> {
    let mut q = conn.prepare(
        "SELECT redir_seq, op, value_kind, value_text, redact_kind, fingerprint
         FROM approval_statement_redirects
         WHERE statement_digest = ?1 AND cmd_seq = ?2 ORDER BY redir_seq",
    )?;
    let rows = q
        .query_map(params![statement_digest, cmd_seq], |row| {
            let redir_seq: i64 = row.get(0)?;
            let op: String = row.get(1)?;
            let target = row_to_planned_value_offset(row, 2)?;
            Ok(PlanRedirectRow { redir_seq, op, target })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Same shape as `row_to_planned_value` but the `value_kind`/… columns
/// don't start at index 0 in the redirects query — `base` is where
/// `value_kind` lives.
fn row_to_planned_value_offset(row: &rusqlite::Row, base: usize) -> rusqlite::Result<PlannedValueRow> {
    let kind_raw: String = row.get(base)?;
    let value_text: Option<String> = row.get(base + 1)?;
    let redact_kind: Option<String> = row.get(base + 2)?;
    let fingerprint: Option<String> = row.get(base + 3)?;
    match parse_enum::<ValueKind>("value_kind", &kind_raw).map_err(sql_err)? {
        ValueKind::Plain => Ok(PlannedValueRow::Plain(value_text.unwrap_or_default())),
        ValueKind::Redacted => Ok(PlannedValueRow::Redacted {
            redact_kind: redact_kind.unwrap_or_default(),
            fingerprint,
        }),
    }
}

fn load_vars(conn: &Connection, statement_digest: &str) -> Result<(Vec<String>, Vec<String>)> {
    let mut q = conn.prepare(
        "SELECT name, binding FROM approval_statement_vars
         WHERE statement_digest = ?1 ORDER BY name",
    )?;
    let mut free = Vec::new();
    let mut bound = Vec::new();
    let rows = q
        .query_map(params![statement_digest], |row| {
            let name: String = row.get(0)?;
            let binding_raw: String = row.get(1)?;
            Ok((name, binding_raw))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (name, binding_raw) in rows {
        match parse_enum::<VarBinding>("binding", &binding_raw)? {
            VarBinding::Free => free.push(name),
            VarBinding::Bound => bound.push(name),
        }
    }
    Ok((free, bound))
}

#[cfg(test)]
mod tests {
    use crate::fixtures::{ask_with_statement, minimal_ask, open_memory};
    use crate::types::{
        ApprovalStatus, NewPlanRedirect, NewPlannedValue, NewSignal, PlannedValueRow,
        SignalSourceKind, SignalVerdict, VarBinding,
    };

    use super::*;

    // ── `create_auto_allowed_ask` (log-only classifier path) ─────────────

    fn classifier_signal() -> NewSignal {
        NewSignal {
            source_kind: SignalSourceKind::Classifier,
            source_id: Some("lfm2d".into()),
            model_id: Some("kube_ordinal_v8".into()),
            weight_hash: Some("abc123".into()),
            stmt_seq: Some(0),
            cmd_seq: None,
            label: Some("situation-normal".into()),
            score: Some(0.6),
            verdict: SignalVerdict::Escalate,
        }
    }

    #[test]
    fn create_auto_allowed_ask_row_exists_and_is_allowed() {
        let conn = open_memory();
        let mut ask = minimal_ask();
        ask.signals = vec![classifier_signal()];
        let request_id = create_auto_allowed_ask(&conn, &ask, "lfm2d:kube_ordinal_v8 (log-only)").unwrap();

        let row = get_approval(&conn, &request_id).unwrap().expect("row must exist");
        assert_eq!(row.status, ApprovalStatus::Allowed);
        assert!(row.status.is_allowed());
        assert!(row.decided_by.is_none(), "no human decided this — decided_by must stay NULL");
        assert_eq!(row.decided_option.as_deref(), Some("auto_allow"));
        assert_eq!(row.auto_reason.as_deref(), Some("lfm2d:kube_ordinal_v8 (log-only)"));
        assert!(row.decided_at.is_some());
    }

    #[test]
    fn create_auto_allowed_ask_list_signals_returns_the_signal() {
        let conn = open_memory();
        let mut ask = minimal_ask();
        ask.signals = vec![classifier_signal()];
        let request_id = create_auto_allowed_ask(&conn, &ask, "lfm2d:kube_ordinal_v8 (log-only)").unwrap();

        let signals = list_signals(&conn, &request_id).unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].source_kind, SignalSourceKind::Classifier);
        assert_eq!(signals[0].source_id.as_deref(), Some("lfm2d"));
        assert_eq!(signals[0].model_id.as_deref(), Some("kube_ordinal_v8"));
        assert_eq!(signals[0].weight_hash.as_deref(), Some("abc123"));
        assert_eq!(signals[0].label.as_deref(), Some("situation-normal"));
        assert_eq!(signals[0].verdict, SignalVerdict::Escalate);
    }

    /// The event row is what an audit read uses to tell "a human said yes"
    /// from "a classifier auto-allowed this in log-only mode" — `actor`
    /// stays NULL (nobody's `PrincipalId` decided this) and `auto_reason`
    /// names the classifier source, on the SAME `Decided` event a human
    /// answer would also produce (never a distinct event kind — this is a
    /// decision, made differently, not a different KIND of thing).
    #[test]
    fn create_auto_allowed_ask_event_names_the_classifier() {
        let conn = open_memory();
        let mut ask = minimal_ask();
        ask.signals = vec![classifier_signal()];
        let request_id = create_auto_allowed_ask(&conn, &ask, "lfm2d:kube_ordinal_v8 (log-only)").unwrap();

        let events = list_events(&conn, &request_id).unwrap();
        assert_eq!(events.len(), 1, "creation and decision are ONE event, not two: {events:?}");
        assert_eq!(events[0].kind, EventKind::Decided);
        assert!(events[0].actor.is_none(), "no human actor decided this");
        assert_eq!(events[0].auto_reason.as_deref(), Some("lfm2d:kube_ordinal_v8 (log-only)"));
        assert_eq!(events[0].decided_option.as_deref(), Some("auto_allow"));
    }

    /// `approval_signals.request_id` stays `NOT NULL` (Amy's ruling) — this
    /// is provable at the schema level, not just by this crate's own API
    /// never emitting a NULL: a raw INSERT bypassing this crate entirely
    /// must also fail. This pins the schema decision this whole function
    /// exists to serve, not just this function's own behavior.
    #[test]
    fn approval_signals_request_id_column_is_not_null() {
        let conn = open_memory();
        let err = conn
            .execute(
                "INSERT INTO approval_signals (request_id, seq, source_kind, verdict)
                 VALUES (NULL, 0, 'classifier', 'escalate')",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().contains("NOT NULL"), "expected a NOT NULL violation, got: {err}");
    }

    // ── `add_signal` (attach to an existing ask) ──────────────────────────

    #[test]
    fn add_signal_attaches_to_an_existing_ask() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();

        let row = add_signal(&conn, &request_id, &classifier_signal()).unwrap();
        assert_eq!(row.seq, 0);
        assert_eq!(row.label.as_deref(), Some("situation-normal"));

        let signals = list_signals(&conn, &request_id).unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].seq, 0);
    }

    /// Two attaches to the SAME ask get distinct, increasing `seq` — this
    /// is how the hook body logs every scored clause as its own signal on
    /// one ask, not just the winner's.
    #[test]
    fn add_signal_twice_gets_distinct_increasing_seq() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();

        let first = add_signal(&conn, &request_id, &classifier_signal()).unwrap();
        let mut second_sig = classifier_signal();
        second_sig.label = Some("informative".into());
        let second = add_signal(&conn, &request_id, &second_sig).unwrap();

        assert_eq!(first.seq, 0);
        assert_eq!(second.seq, 1);
        let signals = list_signals(&conn, &request_id).unwrap();
        assert_eq!(signals.len(), 2);
        assert_eq!(signals[0].label.as_deref(), Some("situation-normal"));
        assert_eq!(signals[1].label.as_deref(), Some("informative"));
    }

    #[test]
    fn add_signal_to_an_unknown_ask_is_not_found() {
        let conn = open_memory();
        let err = add_signal(&conn, "does-not-exist", &classifier_signal()).unwrap_err();
        assert!(matches!(err, LedgerError::NotFound(id) if id == "does-not-exist"));
    }

    /// A signal is an annotation, not a gate participant — attachable even
    /// after the ask is already decided, and attaching one must never
    /// change the decided status.
    #[test]
    fn add_signal_to_an_already_decided_ask_does_not_change_its_status() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();
        crate::decide::decide(&conn, &request_id, crate::decide::DecideInput { allow: true, ..Default::default() })
            .unwrap();

        add_signal(&conn, &request_id, &classifier_signal()).unwrap();

        let row = get_approval(&conn, &request_id).unwrap().unwrap();
        assert_eq!(row.status, ApprovalStatus::Allowed);
    }

    #[test]
    fn create_ask_round_trips_through_get_approval() {
        let conn = open_memory();
        let ask = minimal_ask();
        let request_id = create_ask(&conn, &ask).unwrap();

        let row = get_approval(&conn, &request_id).unwrap().expect("row must exist");
        assert_eq!(row.status, ApprovalStatus::Pending);
        assert_eq!(row.context_id, ask.context_id);
        assert_eq!(row.principal_id, ask.principal_id);
        assert_eq!(row.origin, ask.origin);
        assert_eq!(row.description, ask.description);
        assert_eq!(row.authorized_label, ask.authorized_label);
        assert!(row.claimed_at.is_none());
        assert!(row.decided_at.is_none());
        assert!(load_ask_statements(&conn, &request_id).unwrap().is_empty());
    }

    #[test]
    fn get_approval_on_unknown_id_is_none_not_an_error() {
        let conn = open_memory();
        assert!(get_approval(&conn, "does-not-exist").unwrap().is_none());
    }

    #[test]
    fn options_round_trip_in_order() {
        let conn = open_memory();
        let ask = minimal_ask();
        let request_id = create_ask(&conn, &ask).unwrap();
        let opts = list_options(&conn, &request_id).unwrap();
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].option_id, "allow_once");
        assert_eq!(opts[1].option_id, "deny");
        assert_eq!(opts[0].seq, 0);
        assert_eq!(opts[1].seq, 1);
    }

    #[test]
    fn statement_tree_round_trips_including_free_vars_and_redaction() {
        let conn = open_memory();
        let mut ask = ask_with_statement("digest-1", VarBinding::Free, "rm target");
        // Add a redacted arg and a redirect to exercise every branch of the
        // value_kind CHECK, not just the plain path `statement_with_var` covers.
        ask.statements[0].commands[0].args.push(
            NewPlannedValue::Redacted { redact_kind: "confirm-key".into(), fingerprint: Some("abcd".into()) },
        );
        ask.statements[0].commands[0].redirects.push(NewPlanRedirect {
            op: ">".into(),
            target: NewPlannedValue::Plain("${LOG}".into()),
        });
        let request_id = create_ask(&conn, &ask).unwrap();

        let linked = load_ask_statements(&conn, &request_id).unwrap();
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].stmt_seq, 0);
        let stmt = &linked[0].statement;
        assert_eq!(stmt.statement_digest, "digest-1");
        assert_eq!(stmt.rendered, "rm ${TARGET}");
        assert_eq!(stmt.statement_kind, "command");
        assert_eq!(stmt.free_vars, vec!["TARGET".to_string()]);
        assert!(stmt.bound_vars.is_empty());

        assert_eq!(stmt.commands.len(), 1);
        let cmd = &stmt.commands[0];
        assert_eq!(cmd.name, "rm");
        assert_eq!(cmd.args.len(), 2);
        assert_eq!(cmd.args[0], PlannedValueRow::Plain("${TARGET}".to_string()));
        assert_eq!(
            cmd.args[1],
            PlannedValueRow::Redacted { redact_kind: "confirm-key".to_string(), fingerprint: Some("abcd".to_string()) }
        );
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, ">");
        assert_eq!(cmd.redirects[0].target, PlannedValueRow::Plain("${LOG}".to_string()));

        // load_statement gives the identical content directly by digest.
        let direct = load_statement(&conn, "digest-1").unwrap().unwrap();
        assert_eq!(direct, *stmt);
    }

    #[test]
    fn load_statement_on_unknown_digest_is_none_not_an_error() {
        let conn = open_memory();
        assert!(load_statement(&conn, "no-such-digest").unwrap().is_none());
    }

    #[test]
    fn identical_statement_digest_is_stored_once_across_two_asks() {
        let conn = open_memory();
        let ask_a = ask_with_statement("shared-digest", VarBinding::Bound, "label a");
        let ask_b = ask_with_statement("shared-digest", VarBinding::Bound, "label b");
        create_ask(&conn, &ask_a).unwrap();
        create_ask(&conn, &ask_b).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM approval_statements WHERE statement_digest = ?1",
                params!["shared-digest"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "the statement body must not be duplicated for a repeat digest");

        let links: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM approval_ask_statements WHERE statement_digest = 'shared-digest'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(links, 2, "each ask still gets its own join row");
    }

    /// A multi-statement ask (a small kaish script) links every statement
    /// in order — this is the shape `rules::redeem`'s coverage tests build
    /// on.
    #[test]
    fn a_multi_statement_ask_preserves_order() {
        let conn = open_memory();
        let mut ask = ask_with_statement("digest-a", VarBinding::Bound, "two-statement ask");
        ask.statements.push(crate::types::NewPlanStatement {
            statement_digest: "digest-b".into(),
            rendered: "pwd".into(),
            statement_kind: "command".into(),
            commands: vec![crate::types::NewPlanCommand {
                name: "pwd".into(),
                args: vec![],
                redirects: vec![],
                backgrounded: false,
            }],
            vars: vec![],
        });
        let request_id = create_ask(&conn, &ask).unwrap();

        let linked = load_ask_statements(&conn, &request_id).unwrap();
        assert_eq!(linked.len(), 2);
        assert_eq!(linked[0].stmt_seq, 0);
        assert_eq!(linked[0].statement.statement_digest, "digest-a");
        assert_eq!(linked[1].stmt_seq, 1);
        assert_eq!(linked[1].statement.statement_digest, "digest-b");
    }

    #[test]
    fn events_list_starts_empty_for_a_fresh_ask() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();
        assert!(list_events(&conn, &request_id).unwrap().is_empty());
    }

    #[test]
    fn list_history_only_returns_terminal_asks_and_excludes_them_from_list_pending() {
        let conn = open_memory();
        let id_a = create_ask(&conn, &minimal_ask()).unwrap();
        let id_b = create_ask(&conn, &minimal_ask()).unwrap();

        // Both still pending: history is empty, list_pending has both.
        assert!(list_history(&conn, 10).unwrap().is_empty());
        assert_eq!(list_pending(&conn).unwrap().len(), 2);

        let decided = crate::decide::decide(
            &conn,
            &id_a,
            crate::decide::DecideInput { allow: false, ..Default::default() },
        )
        .unwrap();
        assert_eq!(decided.status, ApprovalStatus::Denied);

        let history = list_history(&conn, 10).unwrap();
        assert_eq!(history.len(), 1, "only the decided ask shows up in history: {history:?}");
        assert_eq!(history[0].request_id, id_a);
        assert_eq!(history[0].status, ApprovalStatus::Denied);

        let pending = list_pending(&conn).unwrap();
        assert_eq!(pending.len(), 1, "the decided ask must drop out of the pending queue");
        assert_eq!(pending[0].request_id, id_b);
    }

    #[test]
    fn list_history_caps_at_limit_newest_decided_first() {
        let conn = open_memory();
        let id_a = create_ask(&conn, &minimal_ask()).unwrap();
        crate::decide::decide(
            &conn,
            &id_a,
            crate::decide::DecideInput { allow: false, ..Default::default() },
        )
        .unwrap();

        // Force a distinct `created_at` millisecond so ordering isn't
        // coincidental.
        std::thread::sleep(std::time::Duration::from_millis(5));

        let id_b = create_ask(&conn, &minimal_ask()).unwrap();
        crate::decide::decide(
            &conn,
            &id_b,
            crate::decide::DecideInput { allow: true, ..Default::default() },
        )
        .unwrap();

        let capped = list_history(&conn, 1).unwrap();
        assert_eq!(capped.len(), 1, "limit=1 must return exactly one row");
        assert_eq!(capped[0].request_id, id_b, "newest-created decided ask must sort first");

        let all = list_history(&conn, 10).unwrap();
        assert_eq!(all.len(), 2);
    }

    // ── `list_asks_filtered` (kj ledger list --limit/--since/--origin/--status) ──

    fn set_created_at(conn: &Connection, request_id: &str, ms: i64) {
        conn.execute(
            "UPDATE approvals SET created_at = ?1 WHERE request_id = ?2",
            params![ms, request_id],
        )
        .unwrap();
    }

    #[test]
    fn list_asks_filtered_limit_caps_rows_and_reports_the_true_total() {
        let conn = open_memory();
        for _ in 0..5 {
            create_ask(&conn, &minimal_ask()).unwrap();
        }
        let filter = AskListFilter {
            statuses: vec![ApprovalStatus::Pending],
            origin: None,
            since_ms: None,
            limit: 2,
            newest_first: false,
        };
        let (rows, total) = list_asks_filtered(&conn, &filter).unwrap();
        assert_eq!(rows.len(), 2, "limit must cap the returned page");
        assert_eq!(total, 5, "the count must reflect every matching row, not just the page");
    }

    #[test]
    fn list_asks_filtered_since_ms_excludes_older_rows() {
        let conn = open_memory();
        let old_id = create_ask(&conn, &minimal_ask()).unwrap();
        set_created_at(&conn, &old_id, 1_000);
        let new_id = create_ask(&conn, &minimal_ask()).unwrap();
        set_created_at(&conn, &new_id, 10_000);

        let filter = AskListFilter {
            statuses: vec![ApprovalStatus::Pending],
            origin: None,
            since_ms: Some(5_000),
            limit: 20,
            newest_first: false,
        };
        let (rows, total) = list_asks_filtered(&conn, &filter).unwrap();
        assert_eq!(total, 1, "the older row must not count toward the total either");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, new_id, "only the row at/after the cutoff must appear");
    }

    #[test]
    fn list_asks_filtered_origin_narrows_to_the_requested_origin() {
        let conn = open_memory();
        let mut hook_ask = minimal_ask();
        hook_ask.origin = Origin::Hook;
        let hook_id = create_ask(&conn, &hook_ask).unwrap();
        let mut verb_ask = minimal_ask();
        verb_ask.origin = Origin::KjVerb;
        create_ask(&conn, &verb_ask).unwrap();

        let filter = AskListFilter {
            statuses: vec![ApprovalStatus::Pending],
            origin: Some(Origin::Hook),
            since_ms: None,
            limit: 20,
            newest_first: false,
        };
        let (rows, total) = list_asks_filtered(&conn, &filter).unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, hook_id);
    }

    /// `kj ledger list`'s default query deliberately excludes `claimed` —
    /// but a caller that explicitly asks for `claimed` (e.g. `--status
    /// claimed`) must still be able to see it. This is the guarantee
    /// `list_pending`'s doc comment carves out an exception for.
    #[test]
    fn list_asks_filtered_status_claimed_is_visible_when_explicitly_requested() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();
        crate::claim::claim(&conn, &request_id, b"claimant").unwrap();

        let filter = AskListFilter {
            statuses: vec![ApprovalStatus::Claimed],
            origin: None,
            since_ms: None,
            limit: 20,
            newest_first: false,
        };
        let (rows, total) = list_asks_filtered(&conn, &filter).unwrap();
        assert_eq!(total, 1, "an explicit --status claimed must find the claimed row");
        assert_eq!(rows[0].request_id, request_id);

        // And the default pending-only filter must NOT show it.
        let pending_only = AskListFilter {
            statuses: vec![ApprovalStatus::Pending],
            origin: None,
            since_ms: None,
            limit: 20,
            newest_first: false,
        };
        let (pending_rows, pending_total) = list_asks_filtered(&conn, &pending_only).unwrap();
        assert_eq!(pending_total, 0, "a claimed row must not leak into the pending-only filter");
        assert!(pending_rows.is_empty());
    }

    #[test]
    fn list_asks_filtered_newest_first_toggles_order() {
        let conn = open_memory();
        let first = create_ask(&conn, &minimal_ask()).unwrap();
        set_created_at(&conn, &first, 1_000);
        let second = create_ask(&conn, &minimal_ask()).unwrap();
        set_created_at(&conn, &second, 2_000);

        let desc_filter = AskListFilter {
            statuses: vec![ApprovalStatus::Pending],
            origin: None,
            since_ms: None,
            limit: 20,
            newest_first: true,
        };
        let (rows, _) = list_asks_filtered(&conn, &desc_filter).unwrap();
        assert_eq!(rows.iter().map(|r| r.request_id.clone()).collect::<Vec<_>>(), vec![second.clone(), first.clone()]);

        let asc_filter = AskListFilter { newest_first: false, ..desc_filter };
        let (rows, _) = list_asks_filtered(&conn, &asc_filter).unwrap();
        assert_eq!(rows.iter().map(|r| r.request_id.clone()).collect::<Vec<_>>(), vec![first, second]);
    }
}
