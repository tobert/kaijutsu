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
    ApprovalRow, AskEnvRow, AskStatementRow, EventKind, EventRow, NewAsk, NewPlanStatement,
    NewPlannedValue, Origin, OptionRow, PairOwner, PlanCommandRow, PlanRedirectRow,
    PlanStatementRow, PlannedValueRow, RefusalRow, SignalRow, SignalSourceKind, SignalVerdict,
    ValueKind, VarBinding, parse_enum,
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
            request_id, context_id, actor_id, reviewer_id, principal_id, origin, instance, tool, hook_id,
            description, authorized_label, rc_run_id, expires_at, cwd, exec_source
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            request_id,
            req.context_id,
            req.actor_id,
            req.reviewer_id,
            req.principal_id,
            req.origin.as_str(),
            req.instance,
            req.tool,
            req.hook_id,
            req.description,
            req.authorized_label,
            req.rc_run_id,
            req.expires_at,
            req.cwd,
            req.exec_source,
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

    for (seq, entry) in req.env.iter().enumerate() {
        tx.execute(
            "INSERT INTO approval_env (request_id, seq, name, value)
             VALUES (?1, ?2, ?3, ?4)",
            params![request_id, seq as i64, entry.name, entry.value],
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
        "SELECT request_id, context_id, actor_id, reviewer_id, principal_id, origin, instance, tool, hook_id,
                description, authorized_label, rc_run_id, status, created_at,
                expires_at, claimed_at, claimed_by, decided_at, decided_by, decided_option,
                remember_scope, auto_reason, cwd, exec_source,
                command_block_id, output_block_id, pair_owner
         FROM approvals WHERE request_id = ?1",
        params![request_id],
        row_to_approval,
    )
    .optional()
    .map_err(LedgerError::from)
}

pub(crate) fn row_to_approval(row: &rusqlite::Row) -> rusqlite::Result<ApprovalRow> {
    let origin_raw: String = row.get(5)?;
    let status_raw: String = row.get(12)?;
    let pair_owner_raw: Option<String> = row.get(26)?;
    Ok(ApprovalRow {
        request_id: row.get(0)?,
        context_id: row.get(1)?,
        actor_id: row.get(2)?,
        reviewer_id: row.get(3)?,
        principal_id: row.get(4)?,
        origin: parse_enum::<Origin>("origin", &origin_raw).map_err(sql_err)?,
        instance: row.get(6)?,
        tool: row.get(7)?,
        hook_id: row.get(8)?,
        description: row.get(9)?,
        authorized_label: row.get(10)?,
        rc_run_id: row.get(11)?,
        status: parse_enum::<crate::types::ApprovalStatus>("status", &status_raw).map_err(sql_err)?,
        created_at: row.get(13)?,
        expires_at: row.get(14)?,
        claimed_at: row.get(15)?,
        claimed_by: row.get(16)?,
        decided_at: row.get(17)?,
        decided_by: row.get(18)?,
        decided_option: row.get(19)?,
        remember_scope: row.get(20)?,
        auto_reason: row.get(21)?,
        cwd: row.get(22)?,
        exec_source: row.get(23)?,
        command_block_id: row.get(24)?,
        output_block_id: row.get(25)?,
        pair_owner: pair_owner_raw
            .map(|raw| parse_enum::<PairOwner>("pair_owner", &raw))
            .transpose()
            .map_err(sql_err)?,
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
        "SELECT request_id, context_id, actor_id, reviewer_id, principal_id, origin, instance, tool, hook_id,
                description, authorized_label, rc_run_id, status, created_at,
                expires_at, claimed_at, claimed_by, decided_at, decided_by, decided_option,
                remember_scope, auto_reason, cwd, exec_source,
                command_block_id, output_block_id, pair_owner
         FROM approvals WHERE status = 'pending' ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map([], row_to_approval)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Every ask that has not reached a terminal state — `pending` *and*
/// `claimed` — oldest first.
///
/// [`list_pending`] deliberately hides a `claimed` row because an answerer
/// is working it and a second answerer would step on the claim. That
/// reasoning holds while the process that claimed it is alive, and only
/// then. At a cold start no claimant can exist, so every `claimed` row is
/// an answerer that died mid-decision — and it is reachable from neither
/// [`list_pending`] nor [`list_history`], which makes it invisible and
/// unanswerable at once.
///
/// This is the read for a caller that owns the whole non-terminal set,
/// which today means the boot sweep
/// ([`crate::decide::abandon_unresolved_on_restart`]). Prefer
/// [`list_pending`] for anything that answers asks while the kernel runs.
pub fn list_unresolved(conn: &Connection) -> Result<Vec<ApprovalRow>> {
    let mut stmt = conn.prepare(
        "SELECT request_id, context_id, actor_id, reviewer_id, principal_id, origin, instance, tool, hook_id,
                description, authorized_label, rc_run_id, status, created_at,
                expires_at, claimed_at, claimed_by, decided_at, decided_by, decided_option,
                remember_scope, auto_reason, cwd, exec_source,
                command_block_id, output_block_id, pair_owner
         FROM approvals WHERE status IN ('pending', 'claimed') ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map([], row_to_approval)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Record the block pair an ask's call already authored, and who authored
/// it.
///
/// Written by the caller AFTER the ask escalates, because the gate does not
/// see these ids: the path that creates the pair reaches the gate through
/// the broker's hook evaluation, which knows nothing about blocks. The
/// caller holds both the returned ask id and its own block ids, so it is the
/// one place where the two are in the same scope.
///
/// Filling an existing pair is what keeps an execution on approval from
/// authoring a second pair beside the first and stranding it. `owner`
/// decides who a fill has to tell: `PairOwner::Turn` for a model turn's own
/// pair (its turn ended at the gate too), `PairOwner::Session` for a
/// connected session watching its own blocks (told nothing).
pub fn link_ask_blocks(
    conn: &Connection,
    request_id: &str,
    command_block_id: &str,
    output_block_id: &str,
    owner: PairOwner,
) -> Result<()> {
    let updated = conn.execute(
        "UPDATE approvals SET command_block_id = ?2, output_block_id = ?3, pair_owner = ?4
         WHERE request_id = ?1",
        params![request_id, command_block_id, output_block_id, owner.as_str()],
    )?;
    if updated == 0 {
        return Err(LedgerError::NotFound(request_id.to_string()));
    }
    Ok(())
}

/// Every unresolved ask raised by one context, oldest first.
///
/// [`list_unresolved`]'s predicate narrowed to a single `context_id`, for
/// the sweep that runs when a context is archived. Same statuses, because
/// the same two are the live ones.
pub fn list_unresolved_for_context(
    conn: &Connection,
    context_id: &[u8],
) -> Result<Vec<ApprovalRow>> {
    let mut stmt = conn.prepare(
        "SELECT request_id, context_id, actor_id, reviewer_id, principal_id, origin, instance, tool, hook_id,
                description, authorized_label, rc_run_id, status, created_at,
                expires_at, claimed_at, claimed_by, decided_at, decided_by, decided_option,
                remember_scope, auto_reason, cwd, exec_source,
                command_block_id, output_block_id, pair_owner
         FROM approvals
         WHERE status IN ('pending', 'claimed') AND context_id = ?1
         ORDER BY created_at ASC",
    )?;
    let rows = stmt
        .query_map(params![context_id], row_to_approval)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Every decided ask — `allowed`, `denied`, `expired`, or `abandoned`
/// (i.e. [`crate::types::ApprovalStatus::is_terminal`]) — most recently
/// created first, capped at `limit` rows. This is the audit-trail
/// read-back `list_pending` doesn't provide: `pending`/`claimed` rows
/// never appear here, and once a row lands here it never appears in
/// `list_pending` again.
///
/// **These two queries do not partition `approvals`.** A `claimed` row is
/// in neither — not terminal, so not history; not `pending`, so not the
/// queue. [`list_unresolved`] is the read that sees it.
/// Ordered by `created_at` rather than `decided_at` because `decide`'s
/// `expire`/`abandon` legs leave `decided_at` NULL (see `decide.rs`'s
/// `transition`) — `created_at` is the one timestamp every row has.
pub fn list_history(conn: &Connection, limit: i64) -> Result<Vec<ApprovalRow>> {
    let mut stmt = conn.prepare(
        "SELECT request_id, context_id, actor_id, reviewer_id, principal_id, origin, instance, tool, hook_id,
                description, authorized_label, rc_run_id, status, created_at,
                expires_at, claimed_at, claimed_by, decided_at, decided_by, decided_option,
                remember_scope, auto_reason, cwd, exec_source,
                command_block_id, output_block_id, pair_owner
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
        "SELECT request_id, context_id, actor_id, reviewer_id, principal_id, origin, instance, tool, hook_id,
                description, authorized_label, rc_run_id, status, created_at,
                expires_at, claimed_at, claimed_by, decided_at, decided_by, decided_option,
                remember_scope, auto_reason, cwd, exec_source,
                command_block_id, output_block_id, pair_owner
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

/// The free-variable value snapshot recorded on this ask, in the order it
/// was captured. Empty for an unknown `request_id`, same as every other
/// read in this module keyed off a join table (`list_options`,
/// `load_ask_statements`) — there is no row to be missing, only rows to
/// find or not find.
pub fn load_ask_env(conn: &Connection, request_id: &str) -> Result<Vec<AskEnvRow>> {
    let mut stmt = conn.prepare(
        "SELECT seq, name, value FROM approval_env
         WHERE request_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt
        .query_map(params![request_id], |row| {
            Ok(AskEnvRow {
                seq: row.get(0)?,
                name: row.get(1)?,
                value: row.get(2)?,
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

/// Every refused answer attempt on one ask, oldest first. The measurement
/// side of the no-self-approval invariant: without this, a low escalation
/// rate is indistinguishable from a gate everyone answered themselves.
pub fn list_refusals(conn: &Connection, request_id: &str) -> Result<Vec<RefusalRow>> {
    let mut stmt = conn.prepare(
        "SELECT seq, reason, actor, actor_context, created_at
         FROM approval_refusals WHERE request_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt
        .query_map(params![request_id], |row| {
            Ok(RefusalRow {
                seq: row.get(0)?,
                reason: row.get(1)?,
                actor: row.get(2)?,
                actor_context: row.get(3)?,
                created_at: row.get(4)?,
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

/// Find an ask that is redeemable RIGHT NOW for `statement_digests` +
/// `presented_label` (`docs/gate-resume.md`'s "an answered ask is
/// redeemable" half) — `status = 'allowed'`, no `approval_redemptions` row
/// yet, `authorized_label` equal to `presented_label`, `context_id`/
/// `principal_id` matching when supplied, and its own statement set (via
/// `approval_ask_statements`) EXACTLY equal to `statement_digests` — not a
/// subset and not a superset. Exactness matters: the gate never applies a
/// submission partially (`AskCoverage::verdict`'s doc — a partially covered
/// ask escalates rather than running part of itself), so an ask covering
/// three statements must never authorize a call presenting only two of
/// them, and a call presenting a fourth must never be waved through by an
/// ask that only ever saw three. Both sides are compared as sets (not
/// multisets): a statement digest repeating within one ask's own list is
/// collapsed the same way `statement_digests` would be.
///
/// **Only a human's answer is redeemable.** A row with `auto_reason` set was
/// decided by a rule or a classifier, never by a person: it is an audit
/// record of a call that already completed, not an offer waiting to be
/// taken. Including those would leave every rule-covered call minting a
/// latent authorization — and the moment the rule was forgotten, the next
/// identical request would silently redeem one instead of asking anybody.
/// Declared here as a property of the row rather than left to each caller
/// to remember after deciding.
///
/// Ties on `created_at` aside, returns the OLDEST match — mirrors
/// [`crate::rules::redeem`]'s scoping (same label/context/principal
/// parameters) so a backlog of equally-shaped allowed asks drains in the
/// order they were created, not newest-first.
pub fn find_redeemable(
    conn: &Connection,
    statement_digests: &[&str],
    presented_label: &str,
    context_id: Option<&[u8]>,
    principal_id: Option<&[u8]>,
    actor_id: &[u8],
) -> Result<Option<(String, crate::types::ApprovalStatus)>> {
    use std::collections::BTreeSet;

    let wanted: BTreeSet<&str> = statement_digests.iter().copied().collect();

    let mut candidates_q = conn.prepare(
        "SELECT request_id, status FROM approvals
         WHERE status IN ('allowed', 'denied')
           AND auto_reason IS NULL
           AND authorized_label = ?1
           AND (?2 IS NULL OR context_id = ?2)
           AND (?3 IS NULL OR principal_id = ?3)
           AND actor_id = ?4
           AND request_id NOT IN (SELECT request_id FROM approval_redemptions)
         ORDER BY created_at ASC",
    )?;
    let candidates: Vec<(String, String)> = candidates_q
        .query_map(params![presented_label, context_id, principal_id, actor_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    for (request_id, status) in candidates {
        let mut digest_q =
            conn.prepare("SELECT statement_digest FROM approval_ask_statements WHERE request_id = ?1")?;
        let has: BTreeSet<String> = digest_q
            .query_map(params![request_id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?
            .into_iter()
            .collect();
        let has_ref: BTreeSet<&str> = has.iter().map(String::as_str).collect();
        if has_ref == wanted {
            // Total because the query above admits exactly these two
            // statuses; widening that `IN` clause without widening this
            // is a compile error, not a silent misread.
            let status = match status.as_str() {
                "allowed" => crate::types::ApprovalStatus::Allowed,
                _ => crate::types::ApprovalStatus::Denied,
            };
            return Ok(Some((request_id, status)));
        }
    }
    Ok(None)
}

/// When an ask's answer was consumed, or `None` while it is still unspent.
/// Milliseconds since the unix epoch, like every other `*_at` in this schema.
///
/// The read side of `approval_redemptions`, and the reason to have one:
/// without it, `allowed` and `allowed but already spent` are
/// indistinguishable from outside the ledger, so a caller that minted a
/// second ask instead of redeeming the first reads exactly like one whose
/// answer never arrived.
pub fn redeemed_at(conn: &Connection, request_id: &str) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT redeemed_at FROM approval_redemptions WHERE request_id = ?1",
        params![request_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(LedgerError::from)
}

/// One answered ask nobody has collected yet.
pub struct UndeliveredAnswer {
    pub request_id: String,
    pub context_id: Vec<u8>,
    /// The principal that raised the ask. **Load-bearing for redemption:**
    /// [`find_redeemable`] scopes by principal, so a caller woken under a
    /// different one mints a fresh ask instead of collecting this answer.
    pub principal_id: Vec<u8>,
    pub status: crate::types::ApprovalStatus,
    pub description: String,
}

/// Every ask a human has answered whose answer has not been redeemed,
/// newest last.
///
/// The predicate is [`find_redeemable`]'s, minus the statement matching:
/// decided, `auto_reason IS NULL`, and absent from `approval_redemptions`.
/// The two must stay in step — this function decides who gets *told* an
/// answer landed, and `find_redeemable` decides whether that answer still
/// authorizes anything. A row here that `find_redeemable` would refuse
/// sends a caller back to do nothing.
///
/// Denials are included on purpose. A denied caller that is never woken
/// keeps its last word as "waiting on a human" and learns nothing; the
/// point of resuming it is that it finds out.
///
/// This reports; it does not claim. Reading it twice returns the same rows,
/// because only a redemption clears one — so a caller that wakes a context
/// and gets no retry must not expect the row to disappear.
pub fn undelivered_answers(conn: &Connection) -> Result<Vec<UndeliveredAnswer>> {
    let mut q = conn.prepare(
        "SELECT request_id, context_id, principal_id, status, description FROM approvals
         WHERE (status IN ('allowed', 'denied') OR (status = 'abandoned' AND decided_option = 'cancel'))
           AND auto_reason IS NULL
           AND request_id NOT IN (SELECT request_id FROM approval_redemptions)
         ORDER BY created_at ASC",
    )?;
    let rows = q
        .query_map([], |row| {
            let status: String = row.get(3)?;
            Ok(UndeliveredAnswer {
                request_id: row.get(0)?,
                context_id: row.get(1)?,
                principal_id: row.get(2)?,
                // Only an explicit cancellation is delivery work; ordinary
                // abandonment remains a terminal lifecycle fact.
                status: match status.as_str() {
                    "allowed" => crate::types::ApprovalStatus::Allowed,
                    "denied" => crate::types::ApprovalStatus::Denied,
                    _ => crate::types::ApprovalStatus::Abandoned,
                },
                description: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use crate::fixtures::{ask_with_statement, minimal_ask, open_memory};
    use crate::types::{
        ApprovalStatus, NewAskEnv, NewPlanRedirect, NewPlannedValue, NewSignal, PlannedValueRow,
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

    /// `stmt_seq`/`cmd_seq` are the clause position a signal judged (see
    /// `schema.rs`'s `approval_signals` doc) — round-trip both all the way
    /// through storage, not just carry them without ever reading them back.
    #[test]
    fn add_signal_round_trips_stmt_seq_and_cmd_seq() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();

        let mut sig = classifier_signal();
        sig.stmt_seq = Some(1);
        sig.cmd_seq = Some(2);
        let row = add_signal(&conn, &request_id, &sig).unwrap();
        assert_eq!(row.stmt_seq, Some(1), "the insert's own return must carry the position back");
        assert_eq!(row.cmd_seq, Some(2), "the insert's own return must carry the position back");

        let signals = list_signals(&conn, &request_id).unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].stmt_seq, Some(1), "{signals:?}");
        assert_eq!(signals[0].cmd_seq, Some(2), "{signals:?}");
    }

    /// Both fields stay OPTIONAL (task contract) — a signal that speaks to
    /// the whole ask, not one clause within it, must not be forced to
    /// invent a position.
    #[test]
    fn add_signal_stmt_seq_and_cmd_seq_stay_null_when_unset() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();

        let mut sig = classifier_signal();
        sig.stmt_seq = None;
        sig.cmd_seq = None;
        add_signal(&conn, &request_id, &sig).unwrap();

        let signals = list_signals(&conn, &request_id).unwrap();
        assert_eq!(signals[0].stmt_seq, None, "{signals:?}");
        assert_eq!(signals[0].cmd_seq, None, "{signals:?}");
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

    /// `value` round-trips as SQL `NULL` for an unset variable, never an
    /// empty string — the two mean different things (an unset variable vs.
    /// one that resolved to `""`), and only `NULL` says the first.
    ///
    /// Falsified by writing `entry.value.unwrap_or_default()` in
    /// `insert_ask` instead of `entry.value`: the raw-column assertion
    /// below would see `""`, not `NULL`.
    #[test]
    fn env_round_trips_in_seq_order_with_unset_as_null() {
        let conn = open_memory();
        let mut ask = minimal_ask();
        ask.env = vec![
            NewAskEnv { name: "FOO".into(), value: Some("bar".into()) },
            NewAskEnv { name: "BAZ".into(), value: None },
        ];
        let request_id = create_ask(&conn, &ask).unwrap();

        let env = load_ask_env(&conn, &request_id).unwrap();
        assert_eq!(env.len(), 2);
        assert_eq!(env[0].seq, 0);
        assert_eq!(env[0].name, "FOO");
        assert_eq!(env[0].value.as_deref(), Some("bar"));
        assert_eq!(env[1].seq, 1);
        assert_eq!(env[1].name, "BAZ");
        assert_eq!(env[1].value, None, "an unset variable must round-trip as NULL, not \"\"");

        let raw: rusqlite::types::Value = conn
            .query_row(
                "SELECT value FROM approval_env WHERE request_id = ?1 AND name = 'BAZ'",
                params![request_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(matches!(raw, rusqlite::types::Value::Null), "expected SQL NULL, got {raw:?}");
    }

    #[test]
    fn an_ask_with_no_free_variables_has_zero_env_rows() {
        let conn = open_memory();
        let ask = minimal_ask();
        assert!(ask.env.is_empty(), "the fixture carries no env by default");
        let request_id = create_ask(&conn, &ask).unwrap();
        assert!(load_ask_env(&conn, &request_id).unwrap().is_empty());
    }

    #[test]
    fn load_ask_env_on_an_unknown_ask_returns_empty() {
        let conn = open_memory();
        assert!(load_ask_env(&conn, "does-not-exist").unwrap().is_empty());
    }

    /// `approval_env.request_id REFERENCES approvals(request_id) ON DELETE
    /// CASCADE` — deleting the ask row removes its env rows with it, same
    /// as `approval_options` and every other ask-owned child.
    ///
    /// Falsified by a schema missing `ON DELETE CASCADE` on `approval_env`:
    /// the row deletion below would then fail an FK constraint (or, with
    /// enforcement off, leave the env row orphaned) instead of cascading.
    #[test]
    fn deleting_the_ask_cascades_its_env_rows() {
        let conn = open_memory();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        let mut ask = minimal_ask();
        ask.env = vec![NewAskEnv { name: "FOO".into(), value: Some("bar".into()) }];
        let request_id = create_ask(&conn, &ask).unwrap();
        assert_eq!(load_ask_env(&conn, &request_id).unwrap().len(), 1);

        conn.execute("DELETE FROM approvals WHERE request_id = ?1", params![request_id]).unwrap();

        assert!(
            load_ask_env(&conn, &request_id).unwrap().is_empty(),
            "ON DELETE CASCADE must remove the env rows along with their ask"
        );
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

    // ── `find_redeemable` (locating a single-use redemption candidate) ────

    /// A two-statement ask, both statements sharing `label` — the fixture
    /// [`find_redeemable_requires_an_exact_statement_set_match`] is built
    /// on: a partial digest presentation (just one of the two) must not
    /// match, only the exact pair does.
    fn two_statement_ask(label: &str) -> NewAsk {
        let mut ask = ask_with_statement("fr-digest-a", VarBinding::Bound, label);
        ask.statements.push(NewPlanStatement {
            statement_digest: "fr-digest-b".into(),
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
        ask
    }

    fn allow(conn: &Connection, request_id: &str) {
        crate::decide::decide(conn, request_id, crate::decide::DecideInput { allow: true, ..Default::default() })
            .unwrap();
    }

    /// An ask starts with no blocks named, and the caller fills them in
    /// afterwards, along with who authored them. Both halves matter: an ask
    /// raised on a path with no blocks must stay `None` rather than
    /// pointing at something, and one raised on a path that has them must
    /// carry all three.
    ///
    /// Falsified by having `link_ask_blocks` write only one column.
    #[test]
    fn an_ask_carries_the_blocks_a_caller_links_to_it() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();

        let before = get_approval(&conn, &request_id).unwrap().unwrap();
        assert_eq!(before.command_block_id, None, "nothing is named at ask time");
        assert_eq!(before.output_block_id, None);
        assert_eq!(before.pair_owner, None);

        link_ask_blocks(&conn, &request_id, "ctx_pri_1", "ctx_pri_2", PairOwner::Session).unwrap();

        let after = get_approval(&conn, &request_id).unwrap().unwrap();
        assert_eq!(after.command_block_id.as_deref(), Some("ctx_pri_1"));
        assert_eq!(after.output_block_id.as_deref(), Some("ctx_pri_2"));
        assert_eq!(after.pair_owner, Some(PairOwner::Session));
    }

    /// The other owner variant round-trips the same way — a model turn's
    /// own pair is recorded as `PairOwner::Turn`, never silently folded
    /// into `Session`.
    #[test]
    fn an_ask_carries_the_turn_owner_variant_too() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();

        link_ask_blocks(&conn, &request_id, "ctx_pri_1", "ctx_pri_2", PairOwner::Turn).unwrap();

        let after = get_approval(&conn, &request_id).unwrap().unwrap();
        assert_eq!(after.pair_owner, Some(PairOwner::Turn));
    }

    /// Linking blocks to an ask that does not exist is an error, not a
    /// silent no-op. A caller that wrote into nothing would believe an
    /// execution on approval could find its blocks, and it could not.
    ///
    /// Falsified by dropping the `updated == 0` check.
    #[test]
    fn linking_blocks_to_an_unknown_ask_is_not_found() {
        let conn = open_memory();
        assert!(matches!(
            link_ask_blocks(&conn, "no-such-ask", "a", "b", PairOwner::Session),
            Err(LedgerError::NotFound(_))
        ));
    }

    /// Pins the redemption-fast-path's whole point: an allowed, unredeemed
    /// ask whose statement set and label match is found; once
    /// `decide::redeem_ask` consumes it, the identical query must no
    /// longer find it. Falsified by deleting the `AND request_id NOT IN
    /// (SELECT request_id FROM approval_redemptions)` clause from the
    /// candidate query: the second `find_redeemable` call then still
    /// returned `Some(request_id)` instead of the expected `None`,
    /// confirming the assertion actually depends on that clause; reverted
    /// afterward.
    #[test]
    fn find_redeemable_ignores_an_already_redeemed_ask() {
        let conn = open_memory();
        let ask = ask_with_statement("fr-redeemed", VarBinding::Bound, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        allow(&conn, &request_id);

        assert_eq!(
            find_redeemable(&conn, &["fr-redeemed"], "rm target", None, None, b"coder").unwrap(),
            Some((request_id.clone(), crate::types::ApprovalStatus::Allowed)),
            "an allowed, unredeemed ask matching label+digests must be found"
        );

        assert!(crate::decide::redeem_ask(&conn, &request_id).unwrap());

        assert_eq!(
            find_redeemable(&conn, &["fr-redeemed"], "rm target", None, None, b"coder").unwrap(),
            None,
            "an already-redeemed ask must not be found again"
        );
    }

    /// The redemption state an outside reader can actually observe: an
    /// answered ask reports no redemption until one is taken, and reports
    /// the stamp the moment it is. `find_redeemable` already refuses a
    /// spent ask, but it refuses an absent one identically — this is the
    /// only way to tell those apart without reading the table directly.
    ///
    /// Falsified by returning `Ok(None)` unconditionally from
    /// `redeemed_at`: the post-redemption `expect` then panics. Reverted
    /// afterward.
    #[test]
    fn redeemed_at_is_none_until_the_answer_is_spent() {
        let conn = open_memory();
        let ask = ask_with_statement("ra-digest", VarBinding::Bound, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        allow(&conn, &request_id);

        assert_eq!(
            redeemed_at(&conn, &request_id).unwrap(),
            None,
            "an answered but unspent ask must report no redemption"
        );

        assert!(crate::decide::redeem_ask(&conn, &request_id).unwrap());
        let at = redeemed_at(&conn, &request_id)
            .unwrap()
            .expect("a spent ask must report when its answer was consumed");
        assert!(at > 0, "redeemed_at is a unix-epoch millisecond stamp, got {at}");

        assert_eq!(
            redeemed_at(&conn, "no-such-ask").unwrap(),
            None,
            "an unknown request is unspent, not an error"
        );
    }

    /// The guarantee most likely to regress into a subset match by
    /// accident: an ask covering TWO statements must not be redeemable by
    /// a call presenting only one of their digests, and must not be
    /// redeemable by a call presenting a digest the ask never covered
    /// either — only the exact pair matches. Falsified by changing the
    /// match condition from set equality (`has_ref == wanted`) to subset
    /// containment (`wanted.is_subset(&has_ref)`): the single-digest
    /// lookup then incorrectly returned `Some(request_id)` instead of the
    /// expected `None`; reverted afterward.
    #[test]
    fn find_redeemable_requires_an_exact_statement_set_match() {
        let conn = open_memory();
        let ask = two_statement_ask("two statements");
        let request_id = create_ask(&conn, &ask).unwrap();
        allow(&conn, &request_id);

        assert_eq!(
            find_redeemable(&conn, &["fr-digest-a"], "two statements", None, None, b"coder").unwrap(),
            None,
            "presenting only one of the ask's two statements must not match"
        );
        assert_eq!(
            find_redeemable(&conn, &["fr-digest-a", "fr-digest-c"], "two statements", None, None, b"coder").unwrap(),
            None,
            "presenting one real digest plus one the ask never covered must not match either"
        );
        assert_eq!(
            find_redeemable(&conn, &["fr-digest-a", "fr-digest-b"], "two statements", None, None, b"coder").unwrap(),
            Some((request_id, crate::types::ApprovalStatus::Allowed)),
            "presenting the exact pair must match"
        );
    }

    /// Guarantee 4's shape, carried into the redemption fast-path: a
    /// digest match alone is not enough — the presented label must equal
    /// what the ask actually authorized. Falsified by dropping the
    /// `authorized_label = ?1` clause from the candidate query (matching
    /// on digest set alone): the mismatched-label lookup then incorrectly
    /// returned `Some(request_id)` instead of the expected `None`;
    /// reverted afterward.
    #[test]
    fn find_redeemable_respects_authorized_label_mismatch() {
        let conn = open_memory();
        let ask = ask_with_statement("fr-label", VarBinding::Bound, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        allow(&conn, &request_id);

        assert_eq!(find_redeemable(&conn, &["fr-label"], "rm /etc", None, None, b"coder").unwrap(), None);
        assert_eq!(find_redeemable(&conn, &["fr-label"], "rm target", None, None, b"coder").unwrap(), Some((request_id, crate::types::ApprovalStatus::Allowed)));
    }

    /// When two allowed, unredeemed asks are both eligible, the backlog
    /// must drain oldest-first, not newest-first — the same ordering
    /// [`crate::claim::claim_next`] uses for its own queue. Falsified by
    /// changing the candidate query's `ORDER BY created_at ASC` to `DESC`:
    /// the lookup then returned `second` instead of the expected `first`;
    /// reverted afterward.
    #[test]
    fn find_redeemable_returns_the_oldest_match_first() {
        let conn = open_memory();
        let ask = ask_with_statement("fr-oldest", VarBinding::Bound, "rm target");
        let first = create_ask(&conn, &ask).unwrap();
        allow(&conn, &first);
        // `created_at` has millisecond resolution — see claim.rs's own
        // `claim_next_picks_the_oldest_pending_first` for why this sleep
        // is load-bearing, not cosmetic.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = create_ask(&conn, &ask).unwrap();
        allow(&conn, &second);

        assert_eq!(find_redeemable(&conn, &["fr-oldest"], "rm target", None, None, b"coder").unwrap(), Some((first, crate::types::ApprovalStatus::Allowed)));
    }

    /// A `session`-scoped redemption boundary — not required by the task's
    /// minimum list, but the same context/principal parameters
    /// [`crate::rules::redeem`] takes deserve at least one round-trip test
    /// each: a context/principal that doesn't match the ask's own must not
    /// find it, and the ask's own context/principal must.
    /// The gate hands a denial back to the caller that asked, then spends
    /// it — so `find_redeemable` has to surface a denied ask, not only an
    /// allowed one. Without this the caller would retry, find nothing, and
    /// mint a duplicate ask forever (`docs/gate-resume.md`, rule 2).
    /// Returning the status alongside the id is what lets the caller tell
    /// the two answers apart; this function never interprets them.
    ///
    /// Falsified by narrowing the query's `status IN ('allowed','denied')`
    /// back to `status = 'allowed'`: the denied ask was then not found and
    /// the first assertion tripped. Reverted.
    #[test]
    fn find_redeemable_finds_a_denied_ask_and_says_it_was_denied() {
        let conn = open_memory();
        let ask = ask_with_statement("fr-denied", VarBinding::Free, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        crate::decide::decide(
            &conn,
            &request_id,
            crate::decide::DecideInput { allow: false, ..Default::default() },
        )
        .unwrap();

        assert_eq!(
            find_redeemable(&conn, &["fr-denied"], "rm target", None, None, b"coder").unwrap(),
            Some((request_id.clone(), crate::types::ApprovalStatus::Denied)),
            "a denied ask carries an answer and must be found, marked denied"
        );

        crate::decide::redeem_ask(&conn, &request_id).unwrap();
        assert_eq!(
            find_redeemable(&conn, &["fr-denied"], "rm target", None, None, b"coder").unwrap(),
            None,
            "once delivered, a denial is spent like an approval"
        );
    }

    /// A rule-decided ask is an audit record, not an offer. Every call a
    /// rule covers mints one of these; if they were redeemable, forgetting
    /// the rule would hand the next identical request a stale
    /// authorization nobody was asked for. Found when `kj ledger forget`
    /// failed to take effect (kaijutsu `docs/gate-resume.md`).
    ///
    /// Falsified by dropping the `auto_reason IS NULL` clause: the
    /// auto-allowed ask was found and the assertion tripped. Reverted.
    #[test]
    fn find_redeemable_ignores_an_ask_a_rule_decided() {
        let conn = open_memory();
        let ask = ask_with_statement("fr-auto", VarBinding::Free, "rm target");
        let request_id = create_auto_allowed_ask(&conn, &ask, "rule:always").unwrap();

        assert_eq!(
            get_approval(&conn, &request_id).unwrap().unwrap().status,
            ApprovalStatus::Allowed,
            "fixture check: the ask really is allowed"
        );
        assert_eq!(
            find_redeemable(&conn, &["fr-auto"], "rm target", None, None, b"coder").unwrap(),
            None,
            "a rule's own decision must never be handed to a later request as an answer"
        );
    }

    // ── `undelivered_answers` ────────────────────────────────────────────
    //
    // These pin the agreement with `find_redeemable`. The two share a
    // predicate on purpose: this one decides who gets TOLD an answer
    // landed, the other decides whether that answer still authorizes
    // anything. A row here that `find_redeemable` refuses wakes a caller to
    // do nothing, so every case below asserts both.

    /// Falsified by dropping `AND auto_reason IS NULL`: the rule-decided ask
    /// appeared and the assertion tripped. Reverted.
    #[test]
    fn undelivered_answers_reports_only_what_find_redeemable_would_honor() {
        let conn = open_memory();

        // A human's allow — offered by both.
        let human = ask_with_statement("ua-human", VarBinding::Bound, "rm target");
        let human_id = create_ask(&conn, &human).unwrap();
        crate::decide::decide(
            &conn,
            &human_id,
            crate::decide::DecideInput { allow: true, ..Default::default() },
        )
        .unwrap();

        // A rule's decision — an audit record, offered by neither.
        let auto = ask_with_statement("ua-auto", VarBinding::Free, "rm other");
        let auto_id = create_auto_allowed_ask(&conn, &auto, "rule:always").unwrap();

        // Still open — nobody has answered, so there is nothing to deliver.
        let pending = ask_with_statement("ua-pending", VarBinding::Bound, "rm third");
        let pending_id = create_ask(&conn, &pending).unwrap();

        let ids: Vec<String> = undelivered_answers(&conn)
            .unwrap()
            .into_iter()
            .map(|a| a.request_id)
            .collect();
        assert_eq!(
            ids,
            vec![human_id.clone()],
            "only a human's undecided-by-rule answer is undelivered"
        );
        assert!(!ids.contains(&auto_id), "a rule's decision is not an offer");
        assert!(!ids.contains(&pending_id), "an unanswered ask delivers nothing");

        // The agreement: what this reports, find_redeemable honors.
        assert!(
            find_redeemable(&conn, &["ua-human"], "rm target", None, None, b"coder")
                .unwrap()
                .is_some(),
            "an answer reported as undelivered must still authorize"
        );
        assert!(
            find_redeemable(&conn, &["ua-auto"], "rm other", None, None, b"coder")
                .unwrap()
                .is_none(),
            "and one it refuses must not be reported"
        );
    }

    /// Falsified by dropping the `NOT IN approval_redemptions` clause: the
    /// redeemed ask kept being reported and the assertion tripped. That is
    /// the loop this guards — a caller woken forever for an answer it has
    /// already collected. Reverted.
    #[test]
    fn undelivered_answers_drops_an_answer_once_it_is_collected() {
        let conn = open_memory();
        let ask = ask_with_statement("ua-spent", VarBinding::Bound, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        crate::decide::decide(
            &conn,
            &request_id,
            crate::decide::DecideInput { allow: true, ..Default::default() },
        )
        .unwrap();
        assert_eq!(undelivered_answers(&conn).unwrap().len(), 1);

        crate::decide::redeem_ask(&conn, &request_id).unwrap();
        assert!(
            undelivered_answers(&conn).unwrap().is_empty(),
            "a collected answer is no longer waiting for anyone"
        );
    }

    /// A denial is an answer. A denied caller that is never woken keeps
    /// "waiting on a human" as its last word and never learns otherwise.
    #[test]
    fn undelivered_answers_includes_a_denial() {
        let conn = open_memory();
        let ask = ask_with_statement("ua-denied", VarBinding::Bound, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        crate::decide::decide(
            &conn,
            &request_id,
            crate::decide::DecideInput { allow: false, ..Default::default() },
        )
        .unwrap();

        let rows = undelivered_answers(&conn).unwrap();
        assert_eq!(rows.len(), 1, "a denial waits to be delivered like an allow");
        assert_eq!(rows[0].status, ApprovalStatus::Denied, "and says it was denied");
        assert_eq!(rows[0].request_id, request_id);
    }

    /// The context is what a caller needs to know WHO to wake; a row
    /// without it is unactionable. `approvals.context_id` is NOT NULL, so
    /// this pins that it survives the read rather than that it exists.
    #[test]
    fn undelivered_answers_carries_the_context_that_raised_the_ask() {
        let conn = open_memory();
        let mut ask = ask_with_statement("ua-ctx", VarBinding::Bound, "rm target");
        ask.context_id = vec![7u8; 16];
        let request_id = create_ask(&conn, &ask).unwrap();
        crate::decide::decide(
            &conn,
            &request_id,
            crate::decide::DecideInput { allow: true, ..Default::default() },
        )
        .unwrap();

        let rows = undelivered_answers(&conn).unwrap();
        assert_eq!(rows[0].context_id, vec![7u8; 16], "the raising context comes back");
    }

    #[test]
    fn find_redeemable_matches_only_the_supplied_context_and_principal() {
        let conn = open_memory();
        // `minimal_ask()` (via `ask_with_statement`) fixes context_id =
        // [1,2,3,4], principal_id = [9,9,9].
        let ask = ask_with_statement("fr-scope", VarBinding::Bound, "rm target");
        let request_id = create_ask(&conn, &ask).unwrap();
        allow(&conn, &request_id);

        let other_context = vec![9, 8, 7, 6];
        assert_eq!(
            find_redeemable(&conn, &["fr-scope"], "rm target", Some(&other_context), Some(&[9, 9, 9]), b"coder").unwrap(),
            None,
            "a different context must not match"
        );
        assert_eq!(
            find_redeemable(&conn, &["fr-scope"], "rm target", Some(&[1, 2, 3, 4]), Some(&[9, 9, 9]), b"coder").unwrap(),
            Some((request_id, crate::types::ApprovalStatus::Allowed)),
            "the ask's own context/principal must match"
        );
    }
}
