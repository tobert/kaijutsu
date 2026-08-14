//! Creating and reading back an ask.
//!
//! `create_ask` is guarantee 1: it returns only after everything —
//! the `approvals` row, its plan tree (first time a digest is seen),
//! options, and signals — is committed to disk. There is no code path
//! that hands back a `request_id` before the row exists durably; a caller
//! that crashes between `create_ask` returning and showing a prompt loses
//! nothing but the prompt, and the row is there, `pending`, on restart.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use rusqlite::Transaction;

use crate::error::{LedgerError, Result};
use crate::types::{
    ApprovalRow, EventKind, EventRow, NewAsk, NewPlan, NewPlannedValue, Origin, OptionRow,
    PlanCommandRow, PlanRedirectRow, PlanStatementRow, PlannedValueRow, SignalRow,
    SignalSourceKind, SignalVerdict, ValueKind, VarBinding, parse_enum,
};

/// Durably record one ask and return its `request_id`. Commits before
/// returning — see the module doc for why that is the whole point.
///
/// If `req.plan` is `Some`, its tree is inserted only the first time its
/// `plan_digest` is seen (content-addressed — `schema.rs` header); a
/// repeat digest just links `approvals.plan_digest` to the existing rows.
pub fn create_ask(conn: &Connection, req: &NewAsk) -> Result<String> {
    let request_id = uuid::Uuid::now_v7().to_string();
    // DEFERRED is fine here (not the claim path's contested `BEGIN
    // IMMEDIATE`): nothing else can reference `request_id` before this
    // function hands it back, so there is no race to lose.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;

    if let Some(plan) = &req.plan {
        insert_plan_if_new(&tx, plan)?;
    }

    tx.execute(
        "INSERT INTO approvals (
            request_id, context_id, principal_id, origin, instance, tool, hook_id,
            description, plan_digest, authorized_label, rc_run_id, expires_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            request_id,
            req.context_id,
            req.principal_id,
            req.origin.as_str(),
            req.instance,
            req.tool,
            req.hook_id,
            req.description,
            req.plan.as_ref().map(|p| p.plan_digest.as_str()),
            req.authorized_label,
            req.rc_run_id,
            req.expires_at,
        ],
    )?;

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

    tx.commit()?;
    Ok(request_id)
}

/// Insert `plan`'s tree, but only if `plan.plan_digest` isn't already
/// recorded — content-addressed dedup (see `schema.rs` header). `tx`
/// derefs to `&Connection`, so this shares the outer transaction; a
/// failure here rolls back the whole `create_ask` call, never a
/// half-written plan.
fn insert_plan_if_new(tx: &Transaction, plan: &NewPlan) -> Result<()> {
    let already_known: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM approval_plans WHERE plan_digest = ?1",
            params![plan.plan_digest],
            |row| row.get(0),
        )
        .optional()?;
    if already_known.is_some() {
        return Ok(());
    }

    let has_free_vars = plan
        .statements
        .iter()
        .any(|s| s.vars.iter().any(|v| v.binding == VarBinding::Free));
    tx.execute(
        "INSERT INTO approval_plans (plan_digest, has_free_vars) VALUES (?1, ?2)",
        params![plan.plan_digest, has_free_vars as i64],
    )?;

    for (stmt_seq, stmt) in plan.statements.iter().enumerate() {
        let stmt_seq = stmt_seq as i64;
        tx.execute(
            "INSERT INTO approval_plan_statements (plan_digest, stmt_seq, rendered, statement_kind)
             VALUES (?1, ?2, ?3, ?4)",
            params![plan.plan_digest, stmt_seq, stmt.rendered, stmt.statement_kind],
        )?;

        for (cmd_seq, cmd) in stmt.commands.iter().enumerate() {
            let cmd_seq = cmd_seq as i64;
            tx.execute(
                "INSERT INTO approval_plan_commands (plan_digest, stmt_seq, cmd_seq, name, backgrounded)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![plan.plan_digest, stmt_seq, cmd_seq, cmd.name, cmd.backgrounded as i64],
            )?;

            for (arg_seq, arg) in cmd.args.iter().enumerate() {
                insert_planned_value_as_arg(tx, &plan.plan_digest, stmt_seq, cmd_seq, arg_seq as i64, arg)?;
            }
            for (redir_seq, redir) in cmd.redirects.iter().enumerate() {
                insert_planned_value_as_redirect(
                    tx,
                    &plan.plan_digest,
                    stmt_seq,
                    cmd_seq,
                    redir_seq as i64,
                    redir,
                )?;
            }
        }

        for var in &stmt.vars {
            tx.execute(
                "INSERT INTO approval_plan_vars (plan_digest, stmt_seq, name, binding)
                 VALUES (?1, ?2, ?3, ?4)",
                params![plan.plan_digest, stmt_seq, var.name, var.binding.as_str()],
            )?;
        }
    }

    Ok(())
}

fn insert_planned_value_as_arg(
    tx: &Transaction,
    plan_digest: &str,
    stmt_seq: i64,
    cmd_seq: i64,
    arg_seq: i64,
    value: &NewPlannedValue,
) -> Result<()> {
    let (value_text, redact_kind, fingerprint) = split_value(value);
    tx.execute(
        "INSERT INTO approval_plan_args (
            plan_digest, stmt_seq, cmd_seq, arg_seq, value_kind, value_text, redact_kind, fingerprint
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![plan_digest, stmt_seq, cmd_seq, arg_seq, value.kind().as_str(), value_text, redact_kind, fingerprint],
    )?;
    Ok(())
}

fn insert_planned_value_as_redirect(
    tx: &Transaction,
    plan_digest: &str,
    stmt_seq: i64,
    cmd_seq: i64,
    redir_seq: i64,
    redir: &crate::types::NewPlanRedirect,
) -> Result<()> {
    let (value_text, redact_kind, fingerprint) = split_value(&redir.target);
    tx.execute(
        "INSERT INTO approval_plan_redirects (
            plan_digest, stmt_seq, cmd_seq, redir_seq, op, value_kind, value_text, redact_kind, fingerprint
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            plan_digest,
            stmt_seq,
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
                description, plan_digest, authorized_label, rc_run_id, status, created_at,
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
    let status_raw: String = row.get(11)?;
    Ok(ApprovalRow {
        request_id: row.get(0)?,
        context_id: row.get(1)?,
        principal_id: row.get(2)?,
        origin: parse_enum::<Origin>("origin", &origin_raw).map_err(sql_err)?,
        instance: row.get(4)?,
        tool: row.get(5)?,
        hook_id: row.get(6)?,
        description: row.get(7)?,
        plan_digest: row.get(8)?,
        authorized_label: row.get(9)?,
        rc_run_id: row.get(10)?,
        status: parse_enum::<crate::types::ApprovalStatus>("status", &status_raw).map_err(sql_err)?,
        created_at: row.get(12)?,
        expires_at: row.get(13)?,
        claimed_at: row.get(14)?,
        claimed_by: row.get(15)?,
        decided_at: row.get(16)?,
        decided_by: row.get(17)?,
        decided_option: row.get(18)?,
        remember_scope: row.get(19)?,
        auto_reason: row.get(20)?,
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

/// Reconstruct a plan tree by its digest — every statement (ordered), each
/// with its commands (ordered, each with its args and redirects ordered)
/// and its free/bound variable lists. Returns an empty `Vec` for an
/// unknown digest (not an error: a `kj_verb`-origin ask may carry no plan
/// at all — see `NoPlanRecorded` in `error.rs` for the one place that
/// distinction is load-bearing).
pub fn load_plan(conn: &Connection, plan_digest: &str) -> Result<Vec<PlanStatementRow>> {
    let mut stmt_q = conn.prepare(
        "SELECT stmt_seq, rendered, statement_kind FROM approval_plan_statements
         WHERE plan_digest = ?1 ORDER BY stmt_seq",
    )?;
    let statements: Vec<(i64, String, String)> = stmt_q
        .query_map(params![plan_digest], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut out = Vec::with_capacity(statements.len());
    for (stmt_seq, rendered, statement_kind) in statements {
        let commands = load_commands(conn, plan_digest, stmt_seq)?;
        let (free_vars, bound_vars) = load_vars(conn, plan_digest, stmt_seq)?;
        out.push(PlanStatementRow {
            stmt_seq,
            rendered,
            statement_kind,
            commands,
            free_vars,
            bound_vars,
        });
    }
    Ok(out)
}

fn load_commands(conn: &Connection, plan_digest: &str, stmt_seq: i64) -> Result<Vec<PlanCommandRow>> {
    let mut cmd_q = conn.prepare(
        "SELECT cmd_seq, name, backgrounded FROM approval_plan_commands
         WHERE plan_digest = ?1 AND stmt_seq = ?2 ORDER BY cmd_seq",
    )?;
    let commands: Vec<(i64, String, bool)> = cmd_q
        .query_map(params![plan_digest, stmt_seq], |row| {
            let backgrounded: i64 = row.get(2)?;
            Ok((row.get(0)?, row.get(1)?, backgrounded != 0))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut out = Vec::with_capacity(commands.len());
    for (cmd_seq, name, backgrounded) in commands {
        let args = load_values(
            conn,
            "SELECT value_kind, value_text, redact_kind, fingerprint FROM approval_plan_args
             WHERE plan_digest = ?1 AND stmt_seq = ?2 AND cmd_seq = ?3 ORDER BY arg_seq",
            plan_digest,
            stmt_seq,
            cmd_seq,
        )?;
        let redirects = load_redirects(conn, plan_digest, stmt_seq, cmd_seq)?;
        out.push(PlanCommandRow { cmd_seq, name, args, redirects, backgrounded });
    }
    Ok(out)
}

fn load_values(
    conn: &Connection,
    sql: &str,
    plan_digest: &str,
    stmt_seq: i64,
    cmd_seq: i64,
) -> Result<Vec<PlannedValueRow>> {
    let mut q = conn.prepare(sql)?;
    let rows = q
        .query_map(params![plan_digest, stmt_seq, cmd_seq], row_to_planned_value)?
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

fn load_redirects(conn: &Connection, plan_digest: &str, stmt_seq: i64, cmd_seq: i64) -> Result<Vec<PlanRedirectRow>> {
    let mut q = conn.prepare(
        "SELECT redir_seq, op, value_kind, value_text, redact_kind, fingerprint
         FROM approval_plan_redirects
         WHERE plan_digest = ?1 AND stmt_seq = ?2 AND cmd_seq = ?3 ORDER BY redir_seq",
    )?;
    let rows = q
        .query_map(params![plan_digest, stmt_seq, cmd_seq], |row| {
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

fn load_vars(conn: &Connection, plan_digest: &str, stmt_seq: i64) -> Result<(Vec<String>, Vec<String>)> {
    let mut q = conn.prepare(
        "SELECT name, binding FROM approval_plan_vars
         WHERE plan_digest = ?1 AND stmt_seq = ?2 ORDER BY name",
    )?;
    let mut free = Vec::new();
    let mut bound = Vec::new();
    let rows = q
        .query_map(params![plan_digest, stmt_seq], |row| {
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
    use crate::fixtures::{ask_with_plan, minimal_ask, open_memory};
    use crate::types::{
        ApprovalStatus, NewPlanRedirect, NewPlannedValue, PlannedValueRow, VarBinding,
    };

    use super::*;

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
        assert!(row.plan_digest.is_none());
        assert!(row.claimed_at.is_none());
        assert!(row.decided_at.is_none());
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
    fn plan_tree_round_trips_including_free_vars_and_redaction() {
        let conn = open_memory();
        let mut ask = ask_with_plan("digest-1", VarBinding::Free, "rm target");
        // Add a redacted arg and a redirect to exercise every branch of the
        // value_kind CHECK, not just the plain path `plan_with_var` covers.
        ask.plan.as_mut().unwrap().statements[0].commands[0].args.push(
            NewPlannedValue::Redacted { redact_kind: "confirm-key".into(), fingerprint: Some("abcd".into()) },
        );
        ask.plan.as_mut().unwrap().statements[0].commands[0].redirects.push(NewPlanRedirect {
            op: ">".into(),
            target: NewPlannedValue::Plain("${LOG}".into()),
        });
        let request_id = create_ask(&conn, &ask).unwrap();
        let row = get_approval(&conn, &request_id).unwrap().unwrap();
        let digest = row.plan_digest.expect("plan_digest recorded");

        let statements = load_plan(&conn, &digest).unwrap();
        assert_eq!(statements.len(), 1);
        let stmt = &statements[0];
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
    }

    #[test]
    fn identical_plan_digest_is_stored_once_across_two_asks() {
        let conn = open_memory();
        let ask_a = ask_with_plan("shared-digest", VarBinding::Bound, "label a");
        let ask_b = ask_with_plan("shared-digest", VarBinding::Bound, "label b");
        create_ask(&conn, &ask_a).unwrap();
        create_ask(&conn, &ask_b).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM approval_plan_statements WHERE plan_digest = ?1",
                params!["shared-digest"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "the statement tree must not be duplicated for a repeat digest");

        let plans: i64 = conn
            .query_row("SELECT COUNT(*) FROM approval_plans", [], |row| row.get(0))
            .unwrap();
        assert_eq!(plans, 1);
    }

    #[test]
    fn events_list_starts_empty_for_a_fresh_ask() {
        let conn = open_memory();
        let request_id = create_ask(&conn, &minimal_ask()).unwrap();
        assert!(list_events(&conn, &request_id).unwrap().is_empty());
    }
}
