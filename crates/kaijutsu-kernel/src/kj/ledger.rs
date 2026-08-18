//! `kj ledger` — answer the approval ledger's pending asks.
//!
//! The answering half of the gate in [`crate::kj::gate`]: a gated verb
//! (`kj cc send` today) leaves a durable ask row and waits; a human — in
//! any shell, any client, minutes later if need be — answers it here.
//!
//! ```sh
//! kj ledger list                            # what is waiting for a decision
//! kj ledger show <request-id>               # one ask, with its statement
//! kj ledger allow <request-id>               # claim + allow (exactly one answerer wins)
//! kj ledger deny <request-id>                # claim + deny
//! kj ledger allow <request-id> --remember always  # + generalize into a standing rule
//! kj ledger rules                            # list standing rules
//! kj ledger forget <rule-id>                 # revoke one
//! ```
//!
//! Answering is a two-step ledger transaction by design: [`claim`] moves
//! the row `pending → claimed` under `BEGIN IMMEDIATE`, so concurrent
//! answerers race safely (guarantee 5 — exactly one wins; losers read a
//! loud `NotClaimable`, never a silent no-op), and [`decide`] makes the
//! terminal write. A row that went terminal elsewhere first answers back
//! `AlreadyDecided` — the late answer is still recorded in the event log,
//! but it does not overwrite anything (guarantee 6).
//!
//! `--remember <session|always>` is a THIRD step, gated on the second: once
//! `decide` commits, [`learn_every_statement`] tries to generalize every
//! statement of the ask into a standing [`approval_ledger::rules`] row, all
//! in one transaction — either every statement's rule is written, or none
//! are (see that function's doc for why a partial rule set is worse than no
//! rule at all). `approval_ledger::rules::learn_from_approval` refuses to
//! create an `allow` rule for a statement with any free variable (Amy's
//! 2026-08-17 ruling, `docs/gate-and-shell-split.md` ruling 3); this module
//! never re-implements that check, only reports what the ledger said. Once
//! a rule exists, [`crate::kj::gate::run_gate`]'s `rules::redeem` step
//! auto-decides the next identical ask without asking anyone —
//! `kj ledger forget` is how that stops.

use approval_ledger::error::LedgerError;
use approval_ledger::types::RuleScope;
use clap::{Parser, Subcommand};
use kaijutsu_types::{ContentType, ContextId};
use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "ledger",
    about = "Answer pending approval-ledger asks left by gated kj verbs",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct LedgerArgs {
    #[command(subcommand)]
    command: LedgerCommand,
}

/// `--remember <scope>` on `allow`/`deny` — a clap `ValueEnum` so a typo
/// (`--remember forever`) fails at parse time with clap's own "possible
/// values are..." message, rather than surfacing as a ledger error deep
/// inside `ledger_decide`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
enum RememberScopeArg {
    /// Only within the context/principal that asked.
    Session,
    /// Any context/principal presenting the same statement + label.
    Always,
}

impl RememberScopeArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Always => "always",
        }
    }

    fn to_rule_scope(self) -> RuleScope {
        match self {
            Self::Session => RuleScope::Session,
            Self::Always => RuleScope::Always,
        }
    }
}

#[derive(Subcommand, Debug)]
enum LedgerCommand {
    /// List asks. By default shows the live queue — asks still `pending`
    /// or `claimed`. With `--history`, shows decided asks instead —
    /// `allowed`, `denied`, `expired`, or `abandoned` — most recently
    /// created first, capped at `--limit` (default 20).
    List {
        /// Show decided asks instead of the pending/claimed queue.
        #[arg(long)]
        history: bool,
        /// Maximum decided asks to return with --history. Ignored
        /// without --history. Default 20.
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Show one ask in full, including the statement being authorized.
    /// Works for a decided ask as well as a pending one.
    Show {
        /// The ask to show. Request ids come from `kj ledger list`.
        request_id: String,
    },
    /// Allow one ask (claims it first; exactly one answerer wins).
    Allow {
        /// The ask to allow. Request ids come from `kj ledger list`.
        request_id: String,
        // Why an `allow` rule is refused over a free variable, and why the
        // ask's own decision survives that refusal: `docs/gate-and-shell-split.md`,
        // "Rulings". Published help states the behavior, not the ruling.
        /// Remember this decision as a standing rule, so future identical
        /// asks decide without asking anyone. Refused when any covered
        /// statement has a free variable; the decision on THIS ask still
        /// stands either way. Off by default. Undo with `kj ledger forget`.
        #[arg(long)]
        remember: Option<RememberScopeArg>,
    },
    /// Deny one ask (claims it first; exactly one answerer wins).
    Deny {
        /// The ask to deny. Request ids come from `kj ledger list`.
        request_id: String,
        /// Remember this decision as a standing rule, so future identical
        /// asks are denied without asking anyone. Always permitted, even
        /// when a covered statement has a free variable. Off by default.
        /// Undo with `kj ledger forget`.
        #[arg(long)]
        remember: Option<RememberScopeArg>,
    },
    /// List active (not-yet-forgotten) standing rules.
    Rules,
    /// Forget a standing rule so its statement escalates to a human again.
    Forget {
        /// The rule to forget. Rule ids come from `kj ledger rules`.
        rule_id: String,
    },
    // A bare positional rather than a `--run` flag, to match `Show`'s shape:
    // the same list-vs-show split as the ask verbs, applied to the run log.
    /// List rc lifecycle runs, most recent first — the durable record of
    /// whether a context's rc scripts actually ran.
    Runs {
        /// Show this run's per-script detail instead of the run listing.
        /// Run ids come from `kj ledger runs`. Lists all runs when omitted.
        run_id: Option<String>,
    },
}

impl KjDispatcher {
    pub(crate) fn dispatch_ledger(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<LedgerArgs>();
        }
        let parsed = match LedgerArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj ledger: {e}"));
            }
        };
        match parsed.command {
            LedgerCommand::List { history, limit } => self.ledger_list(history, limit),
            LedgerCommand::Show { request_id } => self.ledger_show(&request_id),
            LedgerCommand::Allow { request_id, remember } => {
                self.ledger_decide(&request_id, true, caller, remember)
            }
            LedgerCommand::Deny { request_id, remember } => {
                self.ledger_decide(&request_id, false, caller, remember)
            }
            LedgerCommand::Rules => self.ledger_rules(),
            LedgerCommand::Forget { rule_id } => self.ledger_forget(&rule_id),
            LedgerCommand::Runs { run_id } => match run_id {
                Some(id) => self.ledger_run_show(&id),
                None => self.ledger_runs_list(),
            },
        }
    }

    /// `kj ledger list` / `kj ledger list --history`. `history` selects
    /// which of `approval_ledger::ask`'s two partitioning queries runs —
    /// `list_pending` (the live queue) or `list_history` (decided asks,
    /// bounded by `limit`) — the rendering below is shared because both
    /// return the same `ApprovalRow` shape.
    fn ledger_list(&self, history: bool, limit: u32) -> KjResult {
        let rows = {
            let db = self.kernel_db.lock();
            let conn = db.conn_for_ledger();
            let result = if history {
                approval_ledger::ask::list_history(conn, limit as i64)
            } else {
                approval_ledger::ask::list_pending(conn)
            };
            match result {
                Ok(rows) => rows,
                Err(e) => return KjResult::Err(format!("kj ledger list: {e}")),
            }
        };
        let data = serde_json::Value::Array(
            rows.iter()
                .map(|r| serde_json::json!(r.request_id.clone()))
                .collect(),
        );
        if rows.is_empty() {
            let msg = if history { "(no decided asks)" } else { "(no pending approvals)" };
            return KjResult::ok_with_data(msg.to_string(), data);
        }
        let mut lines = vec![format!(
            "  {:<38}  {:<8}  {:<9}  {}",
            "REQUEST", "ORIGIN", "STATUS", "DESCRIPTION"
        )];
        for r in &rows {
            lines.push(format!(
                "  {:<38}  {:<8}  {:<9}  {}",
                r.request_id,
                r.origin,
                r.status,
                r.description,
            ));
        }
        lines.push(String::new());
        if history {
            lines.push("see full detail with: kj ledger show <request-id>".into());
        } else {
            lines.push("answer with: kj ledger allow <request-id>  |  kj ledger deny <request-id>".into());
        }
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    fn ledger_show(&self, request_id: &str) -> KjResult {
        let db = self.kernel_db.lock();
        let conn = db.conn_for_ledger();
        let row = match approval_ledger::ask::get_approval(conn, request_id) {
            Ok(Some(r)) => r,
            Ok(None) => {
                return KjResult::Err(format!("kj ledger: no such ask {request_id}"));
            }
            Err(e) => return KjResult::Err(format!("kj ledger show: {e}")),
        };
        let statements = match approval_ledger::ask::load_ask_statements(conn, request_id) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj ledger show: {e}")),
        };

        let mut lines = vec![
            format!("request:    {}", row.request_id),
            format!("status:     {}", row.status),
            format!("origin:     {}", row.origin),
            format!(
                "tool:       {}.{}",
                row.instance.as_deref().unwrap_or("-"),
                row.tool.as_deref().unwrap_or("-")
            ),
            format!("label:      {}", row.authorized_label.as_deref().unwrap_or("-")),
            format!("description: {}", row.description),
        ];
        for s in &statements {
            lines.push(format!("statement:  {}", s.statement.rendered));
        }
        if let Some(decided) = &row.decided_option {
            lines.push(format!("decided:    {decided}"));
        }
        // `context_id`, `instance`, `tool` and `hook_id` are here for a
        // client that has to ROUTE this ask, not just render it: an ACP
        // session, or the app, needs to know which context an ask belongs
        // to before it can decide whose screen to put it on. They were
        // missing while the only reader was a human at a shell, who
        // already knew.
        let data = serde_json::json!({
            "request_id": row.request_id,
            "context_id": ContextId::try_from_slice(&row.context_id).map(|c| c.to_string()),
            "status": row.status.to_string(),
            "origin": row.origin.to_string(),
            "instance": row.instance,
            "tool": row.tool,
            "hook_id": row.hook_id,
            "description": row.description,
            "authorized_label": row.authorized_label,
            "statements": statements.iter().map(|s| s.statement.rendered.clone()).collect::<Vec<_>>(),
        });
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    /// Claim + decide one ask as the calling principal, then — only if
    /// `--remember` was given and the decide succeeded — try to generalize
    /// EVERY statement of the ask into a standing rule (step 2 of the
    /// module's design; see `learn_every_statement`'s doc for why this is
    /// all-or-nothing).
    ///
    /// `remember_scope` on the `decide` call is the audit record of what the
    /// human ASKED for, independent of whether a rule ended up existing —
    /// those are different facts and this keeps them separately true even
    /// when the rule write is refused below.
    fn ledger_decide(
        &self,
        request_id: &str,
        allow: bool,
        caller: &KjCaller,
        remember: Option<RememberScopeArg>,
    ) -> KjResult {
        let verb = if allow { "allow" } else { "deny" };

        // The ledger work is scoped so the `KernelDb` guard is RELEASED before
        // the notification below. `announce_ledger_change` takes the same
        // mutex to read the committed generation, and `parking_lot::Mutex` is
        // not reentrant — announcing while still holding it would deadlock the
        // answerer, which is a far worse failure than the silent hang this
        // whole slice set out to fix.
        let result = {
            let db = self.kernel_db.lock();
            let conn = db.conn_for_ledger();
            let principal = caller.principal_id.as_bytes();

            match approval_ledger::claim::claim(conn, request_id, principal) {
                Ok(_) => {}
                // Nothing committed on any of these arms, so there is nothing
                // to announce — return straight out rather than falling
                // through to the notification below.
                Err(LedgerError::NotFound(_)) => {
                    return KjResult::Err(format!("kj ledger: no such ask {request_id}"));
                }
                Err(e @ LedgerError::NotClaimable { .. }) => {
                    // Someone else is answering, or the gate already expired it —
                    // say which, loudly; a silent no-op here reads as "done".
                    return KjResult::Err(format!("kj ledger: {e}"));
                }
                Err(e) => return KjResult::Err(format!("kj ledger: {e}")),
            }

            // Past the claim, a mutation HAS committed (pending → claimed), so
            // every path from here announces — including the error arms. A
            // failed decide after a successful claim still moved the ledger,
            // and a client rendering this ask as pending needs to know.
            match approval_ledger::decide::decide(
                conn,
                request_id,
                approval_ledger::decide::DecideInput {
                    allow,
                    decided_by: Some(principal),
                    decided_option: Some(if allow { "allow_once" } else { "deny" }),
                    remember_scope: remember.map(RememberScopeArg::as_str),
                    auto_reason: None,
                },
            ) {
                Ok(row) => {
                    // Past tense is spelled out, not built by appending "ed"
                    // to `verb` — that produced "denyed" in shipped output.
                    // Past tense is spelled out rather than built by appending
                    // "ed" to `verb` — that construction shipped "denyed".
                    let decided = if allow { "allowed" } else { "denied" };
                    let mut message = format!("{decided} ask {request_id} ({})", row.description);
                    let mut data = serde_json::json!({
                        "request_id": row.request_id,
                        "status": row.status.to_string(),
                        "verb": verb,
                    });

                    // Step 2, gated on `--remember`: this ask's decision has
                    // already committed above, so nothing below can undo it —
                    // a refusal here only changes whether a RULE exists, never
                    // whether this ask was allowed or denied.
                    if let Some(remember) = remember {
                        match learn_every_statement(
                            conn,
                            request_id,
                            remember.to_rule_scope(),
                            allow,
                            principal,
                        ) {
                            Ok(n) => {
                                message.push_str(&format!(
                                    "; remembered as a standing {} rule ({n} statement{})",
                                    remember.as_str(),
                                    if n == 1 { "" } else { "s" },
                                ));
                                data["remembered"] = serde_json::json!({
                                    "scope": remember.as_str(),
                                    "statements": n,
                                });
                            }
                            Err(e) => {
                                // The offending variable is the whole point of
                                // this arm (Amy's 2026-08-17 ruling) — surface
                                // `LedgerError::FreeVariableRule`'s fields
                                // rather than flattening to `{e}`'s prose, so
                                // a caller parsing `.data` can act on it too.
                                message.push_str(&format!(
                                    "; NOT remembered: {e} — the {verb} on THIS ask still stands"
                                ));
                                data["remembered"] = serde_json::json!(false);
                                data["remember_error"] = serde_json::json!({
                                    "message": e.to_string(),
                                    "free_variable": match &e {
                                        LedgerError::FreeVariableRule { var_name, .. } => {
                                            serde_json::json!(var_name)
                                        }
                                        _ => serde_json::Value::Null,
                                    },
                                });
                            }
                        }
                    }

                    KjResult::ok_with_data(message, data)
                }
                Err(LedgerError::AlreadyDecided { status, .. }) => KjResult::Err(format!(
                    "kj ledger: ask {request_id} was already decided ({status}) — \
                     your late answer was recorded in the event log but changed nothing"
                )),
                Err(e) => KjResult::Err(format!("kj ledger: {e}")),
            }
        };

        crate::kj::gate::announce_ledger_change(&self.kernel_db, self.kernel.ledger_flows());
        result
    }

    fn ledger_rules(&self) -> KjResult {
        let rows = {
            let db = self.kernel_db.lock();
            match approval_ledger::rules::list_rules(db.conn_for_ledger()) {
                Ok(rows) => rows,
                Err(e) => return KjResult::Err(format!("kj ledger rules: {e}")),
            }
        };
        let data = serde_json::Value::Array(
            rows.iter().map(|r| serde_json::json!(r.rule_id.clone())).collect(),
        );
        if rows.is_empty() {
            return KjResult::ok_with_data("(no active rules)".to_string(), data);
        }
        let mut lines = vec![format!(
            "  {:<36}  {:<7}  {:<5}  {}",
            "RULE", "SCOPE", "ALLOW", "LABEL"
        )];
        for r in &rows {
            lines.push(format!(
                "  {:<36}  {:<7}  {:<5}  {}",
                r.rule_id,
                r.scope.as_str(),
                if r.allow { "yes" } else { "no" },
                r.authorized_label,
            ));
        }
        lines.push(String::new());
        lines.push("forget with: kj ledger forget <rule-id>".into());
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    fn ledger_forget(&self, rule_id: &str) -> KjResult {
        let result = {
            let db = self.kernel_db.lock();
            match approval_ledger::rules::revoke(db.conn_for_ledger(), rule_id) {
                Ok(()) => KjResult::ok_with_data(
                    format!("forgot rule {rule_id} — its statement escalates to a human again"),
                    serde_json::json!({ "rule_id": rule_id }),
                ),
                Err(LedgerError::RuleNotFound(_)) => {
                    return KjResult::Err(format!("kj ledger: no such rule {rule_id}"));
                }
                Err(e) => return KjResult::Err(format!("kj ledger forget: {e}")),
            }
        };
        // A revoke is a ledger mutation like any other (its trigger bumps the
        // same `ledger_generation` counter `announce_ledger_change` reads) —
        // announce it for the same reason `ledger_decide` does.
        crate::kj::gate::announce_ledger_change(&self.kernel_db, self.kernel.ledger_flows());
        result
    }

    /// `kj ledger runs` — the rc lifecycle run log, newest first. `.data`
    /// stays a flat array of run-id strings per the kj list-command
    /// convention (`ledger_list`/`ledger_rules` do the same).
    fn ledger_runs_list(&self) -> KjResult {
        let rows = {
            let db = self.kernel_db.lock();
            match approval_ledger::rc_runs::list_runs(db.conn_for_ledger()) {
                Ok(rows) => rows,
                Err(e) => return KjResult::Err(format!("kj ledger runs: {e}")),
            }
        };
        let data = serde_json::Value::Array(
            rows.iter().map(|r| serde_json::json!(r.run_id.clone())).collect(),
        );
        if rows.is_empty() {
            return KjResult::ok_with_data("(no rc runs recorded)".to_string(), data);
        }
        let mut lines = vec![format!(
            "  {:<38}  {:<12}  {:<20}  {:<8}  {:<13}  {}",
            "RUN", "CONTEXT", "TYPE", "VERB", "STARTED_MS", "OUTCOME"
        )];
        for r in &rows {
            let context = ContextId::try_from_slice(&r.context_id)
                .map(|c| c.to_string())
                .unwrap_or_else(|| "(unparseable)".to_string());
            let outcome = match r.outcome {
                Some(o) => o.to_string(),
                None => "(running)".to_string(),
            };
            lines.push(format!(
                "  {:<38}  {:<12}  {:<20}  {:<8}  {:<13}  {}",
                r.run_id, context, r.context_type, r.verb, r.started_at, outcome,
            ));
        }
        lines.push(String::new());
        lines.push("see one run's scripts with: kj ledger runs <run-id>".into());
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    /// `kj ledger runs <run-id>` — one run's metadata plus the per-script
    /// rows the lifecycle recorded for it (if the run predates this
    /// wiring, or every script-record write degraded per its own
    /// `tracing::warn!`, the script list is legitimately empty — reported
    /// as such, not confused with "no such run").
    fn ledger_run_show(&self, run_id: &str) -> KjResult {
        let db = self.kernel_db.lock();
        let conn = db.conn_for_ledger();
        let run = match approval_ledger::rc_runs::get_run(conn, run_id) {
            Ok(Some(r)) => r,
            Ok(None) => return KjResult::Err(format!("kj ledger runs: no such run {run_id}")),
            Err(e) => return KjResult::Err(format!("kj ledger runs: {e}")),
        };
        let scripts = match approval_ledger::rc_runs::list_run_scripts(conn, run_id) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj ledger runs: {e}")),
        };

        let context = ContextId::try_from_slice(&run.context_id).map(|c| c.to_string());
        let outcome = run.outcome.map(|o| o.to_string());

        let mut lines = vec![
            format!("run:          {}", run.run_id),
            format!("context:      {}", context.as_deref().unwrap_or("(unparseable)")),
            format!("context_type: {}", run.context_type),
            format!("verb:         {}", run.verb),
            format!("started_at:   {}", run.started_at),
            format!(
                "finished_at:  {}",
                run.finished_at
                    .map(|f| f.to_string())
                    .unwrap_or_else(|| "(still running)".to_string())
            ),
            format!("outcome:      {}", outcome.as_deref().unwrap_or("(none yet)")),
        ];
        if scripts.is_empty() {
            lines.push(String::new());
            lines.push("(no scripts recorded for this run)".to_string());
        } else {
            lines.push(String::new());
            lines.push(format!("  {:<4}  {:<50}  {:<6}  {}", "SEQ", "PATH", "EXIT", "SHA256"));
            for s in &scripts {
                lines.push(format!(
                    "  {:<4}  {:<50}  {:<6}  {}",
                    s.seq,
                    s.path,
                    s.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "-".to_string()),
                    s.body_sha256,
                ));
            }
        }

        let data = serde_json::json!({
            "run_id": run.run_id,
            "context_id": context,
            "context_type": run.context_type,
            "verb": run.verb,
            "started_at": run.started_at,
            "finished_at": run.finished_at,
            "outcome": outcome,
            "scripts": scripts.iter().map(|s| serde_json::json!({
                "seq": s.seq,
                "path": s.path,
                "body_sha256": s.body_sha256,
                "exit_code": s.exit_code,
                "started_at": s.started_at,
                "finished_at": s.finished_at,
            })).collect::<Vec<_>>(),
        });
        KjResult::ok_with_data(lines.join("\n"), data)
    }
}

/// Generalize EVERY statement of `request_id` into a standing rule, all in
/// one transaction: if any statement's rule is refused (guarantee 3, a free
/// variable on an `allow`), NONE of the rules for this ask are written.
///
/// This is not a nice-to-have. `run_gate`'s `AskCoverage::verdict` only
/// auto-allows when EVERY statement of a future identical ask is covered
/// (`approval-ledger/src/types.rs`'s `AskCoverage::verdict`) — a partial rule
/// set from a multi-statement `shell_write` submission would sit in the
/// table doing nothing (every future redemption of that ask still escalates
/// on the uncovered statement) while `kj ledger allow --remember` told the
/// human "remembered". That is exactly the silent fallback CLAUDE.md warns
/// against: writing rows that appear to do something but don't.
///
/// Returns the number of statements learned (== the ask's statement count)
/// on success.
fn learn_every_statement(
    conn: &Connection,
    request_id: &str,
    scope: RuleScope,
    allow: bool,
    created_by: &[u8],
) -> approval_ledger::error::Result<usize> {
    let statements = approval_ledger::ask::load_ask_statements(conn, request_id)?;
    // `Transaction::new_unchecked` (not `Connection::transaction`, which
    // needs `&mut Connection` — this crate is only ever handed a shared
    // `&Connection` via `KernelDb::conn_for_ledger`) opens a real `BEGIN
    // IMMEDIATE` transaction over the same connection `decide` already
    // committed on above; dropping it without `commit()` rolls back
    // (rusqlite's default `DropBehavior`), which is what every early
    // `?`-return below relies on.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    for stmt in &statements {
        approval_ledger::rules::learn_from_approval(
            &tx,
            request_id,
            stmt.stmt_seq,
            scope,
            allow,
            Some(created_by),
        )?;
    }
    let learned = statements.len();
    tx.commit()?;
    Ok(learned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::gate::{run_gate, GateSpec};
    use crate::kj::test_helpers::{test_caller, test_dispatcher};
    use approval_ledger::types::VarBinding;
    use std::time::Duration;

    fn s(v: &str) -> String {
        v.to_string()
    }

    fn spec() -> GateSpec {
        GateSpec {
            origin: approval_ledger::types::Origin::KjVerb,
            instance: "builtin.kj".into(),
            tool: "cc.send".into(),
            hook_id: None,
            description: "test ask".into(),
            authorized_label: "some-target".into(),
            statements: vec![crate::kj::gate::GatedStatement {
                rendered: "kj cc send ${TARGET} ${MESSAGE}".into(),
                statement_kind: "kj_verb".into(),
                vars: vec![
                    ("TARGET".into(), VarBinding::Bound),
                    ("MESSAGE".into(), VarBinding::Free),
                ],
                source_index: None,
            }],
        }
    }

    /// A `shell_write`-shaped spec with NO free variables — unlike `spec()`
    /// above (whose `MESSAGE` var is deliberately free so it can never be
    /// remembered, per `the_ask_message_body_is_free_so_allow_rules_cannot_learn_it`
    /// in `kj/gate.rs`), this one can actually be learned as an allow rule.
    /// Two calls with the same `(label, rendered)` build digest-identical
    /// asks, which is what makes "remember, then ask again unattended" a
    /// meaningful test.
    fn shell_spec(label: &str, rendered: &str) -> GateSpec {
        GateSpec {
            origin: approval_ledger::types::Origin::ShellGate,
            instance: "builtin.shell_write".into(),
            tool: "shell_write".into(),
            hook_id: None,
            description: format!("shell_write: {rendered:?}"),
            authorized_label: label.to_string(),
            statements: vec![crate::kj::gate::GatedStatement {
                rendered: rendered.to_string(),
                statement_kind: "command".into(),
                vars: vec![],
                source_index: Some(0),
            }],
        }
    }

    /// Poll `list_pending` until one ask shows up, returning its id. Shared
    /// by every test below that fires `run_gate` in the background and needs
    /// the request id to answer it.
    async fn wait_for_pending(d: &crate::kj::KjDispatcher) -> String {
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let db = d.kernel_db.lock();
            if let Some(row) = approval_ledger::ask::list_pending(db.conn_for_ledger()).unwrap().first() {
                return row.request_id.clone();
            }
        }
        panic!("no ask ever went pending");
    }

    #[tokio::test]
    async fn ledger_list_is_empty_then_shows_a_gated_ask() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let result = d.dispatch(&[s("ledger"), s("list")], &c).await;
        assert!(result.is_ok());
        assert!(result.message().contains("no pending approvals"));

        // Fire a gate with a long budget in the background, leaving a
        // pending ask; it must appear in the list.
        let db = d.kernel_db.clone();
        let caller = c.clone();
        let flows = d.kernel.ledger_flows().clone();
        let gate = tokio::spawn(async move {
            run_gate(&db, &caller, spec(), Duration::from_secs(30), &flows).await
        });
        let mut listed = String::new();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let result = d.dispatch(&[s("ledger"), s("list")], &c).await;
            if result.message().contains("test ask") {
                listed = result.message().to_string();
                break;
            }
        }
        assert!(listed.contains("test ask"), "list must show the ask: {listed}");
        assert!(listed.contains("kj ledger allow"));

        // Deny it through the verb; the gate must observe the refusal.
        let request_id = listed
            .lines()
            .find(|l| l.contains("test ask"))
            .and_then(|l| l.split_whitespace().next())
            .expect("request id is the first column")
            .to_string();
        let result = d
            .dispatch(&[s("ledger"), s("deny"), s(&request_id)], &c)
            .await;
        assert!(result.is_ok(), "deny must succeed: {result:?}");
        let outcome = gate.await.unwrap();
        assert!(!outcome.allowed());
        // A human said no. That IS a verdict — distinct from the gate
        // being unable to reach one.
        assert_eq!(outcome.verdict, crate::kj::gate::GateVerdict::Denied);
        assert_eq!(outcome.ask.expect("a denied ask has a row").request_id, request_id);
    }

    #[tokio::test]
    async fn ledger_allow_opens_the_gate_and_a_second_answer_is_loud() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let db = d.kernel_db.clone();
        let caller = c.clone();
        let flows = d.kernel.ledger_flows().clone();
        let gate = tokio::spawn(async move {
            run_gate(&db, &caller, spec(), Duration::from_secs(30), &flows).await
        });

        let mut request_id = String::new();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let db = d.kernel_db.lock();
            if let Some(row) = approval_ledger::ask::list_pending(db.conn_for_ledger())
                .unwrap()
                .first()
            {
                request_id = row.request_id.clone();
                break;
            }
        }
        assert!(!request_id.is_empty());

        let result = d
            .dispatch(&[s("ledger"), s("allow"), s(&request_id)], &c)
            .await;
        assert!(result.is_ok(), "allow must succeed: {result:?}");
        assert!(result.message().starts_with("allowed ask"));

        // Answering again is a loud error naming the terminal status, never
        // a silent no-op (guarantee 6).
        let again = d
            .dispatch(&[s("ledger"), s("allow"), s(&request_id)], &c)
            .await;
        assert!(!again.is_ok());
        assert!(again.message().contains("not `pending`") || again.message().contains("already"));

        let outcome = gate.await.unwrap();
        assert!(outcome.allowed());
    }

    #[tokio::test]
    async fn ledger_show_renders_the_statement_being_authorized() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let db = d.kernel_db.clone();
        let caller = c.clone();
        let flows = d.kernel.ledger_flows().clone();
        let gate = tokio::spawn(async move {
            run_gate(&db, &caller, spec(), Duration::from_secs(30), &flows).await
        });

        let mut request_id = String::new();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let db = d.kernel_db.lock();
            if let Some(row) = approval_ledger::ask::list_pending(db.conn_for_ledger())
                .unwrap()
                .first()
            {
                request_id = row.request_id.clone();
                break;
            }
        }

        let result = d
            .dispatch(&[s("ledger"), s("show"), s(&request_id)], &c)
            .await;
        assert!(result.is_ok(), "{result:?}");
        let msg = result.message();
        assert!(msg.contains("kj cc send ${TARGET} ${MESSAGE}"), "{msg}");
        assert!(msg.contains("some-target"), "raw typed label, finding #3: {msg}");

        // The structured payload has to carry enough to ROUTE the ask, not
        // just print it: a client with several live sessions decides whose
        // screen this belongs on by its context, and cannot do that from
        // prose. Asserted here because the human-readable rendering above
        // would keep passing while a client silently lost the ability.
        let data = match &result {
            KjResult::Ok { data: Some(d), .. } => d.clone(),
            other => panic!("kj ledger show must emit structured data: {other:?}"),
        };
        assert_eq!(
            data["context_id"].as_str(),
            Some(c.context_id.expect("the test caller has a context").to_string().as_str()),
            "show must name the context the ask belongs to: {data}"
        );
        assert_eq!(data["instance"].as_str(), Some("builtin.kj"), "{data}");
        assert_eq!(data["tool"].as_str(), Some("cc.send"), "{data}");
        assert_eq!(data["request_id"].as_str(), Some(request_id.as_str()), "{data}");

        // Clean up the pending ask so the spawned gate terminates.
        d.dispatch(&[s("ledger"), s("deny"), s(&request_id)], &c)
            .await;
        let _ = gate.await;
    }

    /// Both decision verbs report themselves in correct English. The `deny`
    /// half is the point: the message is built from the verb name, and
    /// appending "ed" to "deny" shipped "denyed" to every human who denied
    /// an ask. The pre-existing assertion covered only the `allow` branch,
    /// where that construction happens to be correct.
    #[tokio::test]
    async fn a_decision_reports_itself_in_correct_english() {
        let d = test_dispatcher().await;
        let c = test_caller();

        for (verb, expected) in [("allow", "allowed ask"), ("deny", "denied ask")] {
            let db = d.kernel_db.clone();
            let flows = d.kernel.ledger_flows().clone();
            let caller = c.clone();
            let gate = tokio::spawn(async move {
                run_gate(&db, &caller, spec(), Duration::from_secs(30), &flows).await
            });
            let request_id = wait_for_pending(&d).await;

            let result = d.dispatch(&[s("ledger"), s(verb), s(&request_id)], &c).await;
            assert!(result.is_ok(), "{verb} must succeed: {result:?}");
            assert!(
                result.message().starts_with(expected),
                "`kj ledger {verb}` must report \"{expected}\", got: {}",
                result.message()
            );
            let _ = gate.await;
        }
    }

    #[tokio::test]
    async fn ledger_of_an_unknown_id_errors_loudly() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("ledger"), s("allow"), s("deadbeef")], &c)
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("no such ask"));
    }

    /// A decided ask must drop off `kj ledger list`'s live queue and show
    /// up under `kj ledger list --history` instead — the audit-trail
    /// read-back this slice adds. Asserted on the typed `.data` payload
    /// (an array of request-id strings, per the kj list-command
    /// convention), never on a substring of the human-readable table.
    #[tokio::test]
    async fn ledger_list_history_shows_a_decided_ask_and_the_plain_list_drops_it() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let db = d.kernel_db.clone();
        let caller = c.clone();
        let flows = d.kernel.ledger_flows().clone();
        let gate = tokio::spawn(async move {
            run_gate(&db, &caller, spec(), Duration::from_secs(30), &flows).await
        });
        let request_id = wait_for_pending(&d).await;

        let deny = d
            .dispatch(&[s("ledger"), s("deny"), s(&request_id)], &c)
            .await;
        assert!(deny.is_ok(), "deny must succeed: {deny:?}");
        let _ = gate.await;

        let plain = d.dispatch(&[s("ledger"), s("list")], &c).await;
        assert!(plain.is_ok(), "{plain:?}");
        let plain_data = match &plain {
            KjResult::Ok { data: Some(v), .. } => v.clone(),
            other => panic!("kj ledger list must emit structured data: {other:?}"),
        };
        assert!(
            !plain_data
                .as_array()
                .expect(".data must be an array")
                .iter()
                .any(|v| v.as_str() == Some(request_id.as_str())),
            "a decided ask must not appear in the live queue: {plain_data}"
        );

        let history = d
            .dispatch(&[s("ledger"), s("list"), s("--history")], &c)
            .await;
        assert!(history.is_ok(), "{history:?}");
        let history_data = match &history {
            KjResult::Ok { data: Some(v), .. } => v.clone(),
            other => panic!("kj ledger list --history must emit structured data: {other:?}"),
        };
        assert!(
            history_data
                .as_array()
                .expect(".data must be an array")
                .iter()
                .any(|v| v.as_str() == Some(request_id.as_str())),
            "the decided ask must appear in --history: {history_data}"
        );
    }

    /// `--limit` bounds the `.data` array, and the default keeps the most
    /// recently created decided ask first.
    #[tokio::test]
    async fn ledger_list_history_limit_caps_the_data_array_newest_first() {
        let d = test_dispatcher().await;
        let c = test_caller();

        async fn decide_one(d: &crate::kj::KjDispatcher, c: &KjCaller, allow: bool) -> String {
            let db = d.kernel_db.clone();
            let caller = c.clone();
            let flows = d.kernel.ledger_flows().clone();
            let gate =
                tokio::spawn(async move { run_gate(&db, &caller, spec(), Duration::from_secs(30), &flows).await });
            let request_id = wait_for_pending(d).await;
            let verb = if allow { "allow" } else { "deny" };
            let result = d.dispatch(&[s("ledger"), s(verb), s(&request_id)], c).await;
            assert!(result.is_ok(), "{verb} must succeed: {result:?}");
            let _ = gate.await;
            request_id
        }

        let first = decide_one(&d, &c, false).await;
        // Force a distinct `created_at` millisecond so "newest first" is
        // asserting real ordering, not a coincidence.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let second = decide_one(&d, &c, true).await;

        let capped = d
            .dispatch(&[s("ledger"), s("list"), s("--history"), s("--limit"), s("1")], &c)
            .await;
        assert!(capped.is_ok(), "{capped:?}");
        let capped_ids: Vec<String> = match &capped {
            KjResult::Ok { data: Some(v), .. } => {
                v.as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect()
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(capped_ids, vec![second.clone()], "--limit 1 keeps only the newest decided ask");

        let all = d.dispatch(&[s("ledger"), s("list"), s("--history")], &c).await;
        let all_ids: Vec<String> = match &all {
            KjResult::Ok { data: Some(v), .. } => {
                v.as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect()
            }
            other => panic!("{other:?}"),
        };
        assert!(all_ids.contains(&first), "{all_ids:?}");
        assert!(all_ids.contains(&second), "{all_ids:?}");
    }

    /// The whole point of this slice: a remembered allow rule must make the
    /// NEXT identical ask auto-allow without anyone answering it — no second
    /// `kj ledger allow`. This is the test that proves the auto-allow branch
    /// in `approval_ledger::rules::redeem` is reachable in production, not
    /// just exercised from inside the ledger crate's own test suite.
    ///
    /// Uses `shell_spec` (no free variables), NOT `spec()` — `spec()`'s
    /// `MESSAGE` var is deliberately free and can never be remembered (see
    /// `the_ask_message_body_is_free_so_allow_rules_cannot_learn_it` in
    /// `kj/gate.rs`).
    #[tokio::test]
    async fn remembering_an_allow_makes_the_next_identical_ask_auto_allow() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let label = "kaish-source";
        let rendered = "ls -la /srv/builds";

        let db = d.kernel_db.clone();
        let caller = c.clone();
        let flows = d.kernel.ledger_flows().clone();
        let gate = tokio::spawn(async move {
            run_gate(&db, &caller, shell_spec(label, rendered), Duration::from_secs(30), &flows).await
        });

        let request_id = wait_for_pending(&d).await;
        let result = d
            .dispatch(&[s("ledger"), s("allow"), s(&request_id), s("--remember"), s("always")], &c)
            .await;
        assert!(result.is_ok(), "allow --remember must succeed: {result:?}");
        assert!(
            result.message().contains("remembered"),
            "message must say what was remembered: {}",
            result.message()
        );
        let outcome = gate.await.unwrap();
        assert!(outcome.allowed(), "the answered ask itself must still be allowed");

        // A rule now exists. Fire an IDENTICAL ask (same origin, label,
        // rendered text) and let it run unattended, on a short budget —
        // nobody calls `kj ledger allow` this time.
        let db2 = d.kernel_db.clone();
        let flows2 = d.kernel.ledger_flows().clone();
        let outcome2 = run_gate(
            &db2,
            &c,
            shell_spec(label, rendered),
            Duration::from_millis(400),
            &flows2,
        )
        .await;

        assert!(
            outcome2.allowed(),
            "a remembered allow rule must auto-allow the next identical ask: {outcome2:?}"
        );
        assert_eq!(outcome2.verdict, crate::kj::gate::GateVerdict::Allowed);
        assert!(
            outcome2.reason.contains("rule"),
            "the reason must show this was a RULE decision, not a human one: {}",
            outcome2.reason
        );
        let ask2 = outcome2.ask.expect("an auto-decided ask still gets a durable row");
        assert_eq!(ask2.status, approval_ledger::types::ApprovalStatus::Allowed);
    }

    /// Amy's 2026-08-17 ruling pinned at the CLI seam: a `kj cc send`-shaped
    /// ask has a free `MESSAGE` variable, so `--remember` must refuse to
    /// create a rule for it — while the decision on THIS ask still goes
    /// through exactly as if `--remember` had never been passed.
    #[tokio::test]
    async fn remembering_an_allow_is_refused_when_a_statement_has_a_free_variable() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let db = d.kernel_db.clone();
        let caller = c.clone();
        let flows = d.kernel.ledger_flows().clone();
        let gate = tokio::spawn(async move {
            run_gate(&db, &caller, spec(), Duration::from_secs(30), &flows).await
        });

        let request_id = wait_for_pending(&d).await;
        let result = d
            .dispatch(&[s("ledger"), s("allow"), s(&request_id), s("--remember"), s("always")], &c)
            .await;
        assert!(
            result.is_ok(),
            "the ask's own allow/deny must still succeed even when remembering is refused: {result:?}"
        );
        assert!(
            result.message().contains("NOT remembered"),
            "must say plainly that nothing was remembered: {}",
            result.message()
        );
        assert!(
            result.message().contains("MESSAGE"),
            "must name the offending free variable: {}",
            result.message()
        );
        assert!(
            result.message().contains("still stands"),
            "must make clear the decision on THIS ask was not undone: {}",
            result.message()
        );

        let outcome = gate.await.unwrap();
        assert!(outcome.allowed(), "the ask itself was allowed, independent of the refused rule");

        let db = d.kernel_db.lock();
        let count: i64 = db
            .conn_for_ledger()
            .query_row("SELECT COUNT(*) FROM approval_rules", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "no rule may exist after a refused remember");
    }

    /// `--remember session` round-trips through `kj ledger rules` — the
    /// scope typed on the CLI is exactly the scope the rule reports, and the
    /// structured `.data` is the array-of-ids `KjResult::ok_with_data`'s
    /// doc comment promises for list commands.
    #[tokio::test]
    async fn remember_session_scope_round_trips_through_kj_ledger_rules() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let label = "kaish-source";
        let rendered = "echo session-scoped";

        let db = d.kernel_db.clone();
        let caller = c.clone();
        let flows = d.kernel.ledger_flows().clone();
        let gate = tokio::spawn(async move {
            run_gate(&db, &caller, shell_spec(label, rendered), Duration::from_secs(30), &flows).await
        });
        let request_id = wait_for_pending(&d).await;
        let result = d
            .dispatch(&[s("ledger"), s("allow"), s(&request_id), s("--remember"), s("session")], &c)
            .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(result.message().contains("session"), "{}", result.message());
        assert!(gate.await.unwrap().allowed());

        let rules = d.dispatch(&[s("ledger"), s("rules")], &c).await;
        assert!(rules.is_ok(), "{rules:?}");
        assert!(rules.message().contains("session"), "{}", rules.message());
        let data = match &rules {
            KjResult::Ok { data: Some(d), .. } => d.clone(),
            other => panic!("kj ledger rules must emit structured data: {other:?}"),
        };
        let ids = data.as_array().expect(".data must be a JSON array of rule ids");
        assert_eq!(ids.len(), 1, "{data}");
        assert!(ids[0].is_string(), "{data}");
    }

    /// A remember with no way to un-remember is a trap: `kj ledger forget`
    /// must make the NEXT identical ask escalate again, exactly as if the
    /// rule had never been learned.
    #[tokio::test]
    async fn forgetting_a_rule_makes_the_next_identical_ask_escalate_again() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let label = "kaish-source";
        let rendered = "echo forget-me";

        let db = d.kernel_db.clone();
        let caller = c.clone();
        let flows = d.kernel.ledger_flows().clone();
        let gate = tokio::spawn(async move {
            run_gate(&db, &caller, shell_spec(label, rendered), Duration::from_secs(30), &flows).await
        });
        let request_id = wait_for_pending(&d).await;
        d.dispatch(&[s("ledger"), s("allow"), s(&request_id), s("--remember"), s("always")], &c)
            .await;
        assert!(gate.await.unwrap().allowed());

        let rules = d.dispatch(&[s("ledger"), s("rules")], &c).await;
        let data = match &rules {
            KjResult::Ok { data: Some(d), .. } => d.clone(),
            other => panic!("{other:?}"),
        };
        let rule_id = data[0].as_str().expect("a rule id string").to_string();

        let forgotten = d.dispatch(&[s("ledger"), s("forget"), s(&rule_id)], &c).await;
        assert!(forgotten.is_ok(), "{forgotten:?}");

        let rules_after = d.dispatch(&[s("ledger"), s("rules")], &c).await;
        assert!(
            rules_after.message().contains("no active rules"),
            "{}",
            rules_after.message()
        );

        // The next identical ask must escalate — no rule left to auto-allow
        // it, so an unattended short budget must expire, not allow.
        let outcome = run_gate(
            &d.kernel_db.clone(),
            &c,
            shell_spec(label, rendered),
            Duration::from_millis(400),
            d.kernel.ledger_flows(),
        )
        .await;
        assert!(!outcome.allowed(), "a forgotten rule must not keep auto-allowing: {outcome:?}");
        assert_eq!(outcome.verdict, crate::kj::gate::GateVerdict::Unavailable);
    }

    #[tokio::test]
    async fn forgetting_an_unknown_rule_id_errors_loudly() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("ledger"), s("forget"), s("no-such-rule")], &c).await;
        assert!(!result.is_ok());
        assert!(result.message().contains("no such rule"));
    }

    #[tokio::test]
    async fn ledger_rules_on_a_fresh_ledger_is_empty() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("ledger"), s("rules")], &c).await;
        assert!(result.is_ok());
        assert!(result.message().contains("no active rules"));
        let data = match &result {
            KjResult::Ok { data: Some(d), .. } => d.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(data.as_array().unwrap().len(), 0);
    }

    /// `--remember <bad-value>` must fail at clap parse time with a message
    /// naming the valid choices, not deep inside `ledger_decide`.
    #[tokio::test]
    async fn an_invalid_remember_scope_fails_at_parse_time() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("ledger"), s("allow"), s("deadbeef"), s("--remember"), s("forever")], &c)
            .await;
        assert!(!result.is_ok());
        assert!(
            result.message().contains("session") && result.message().contains("always"),
            "clap's error should name the valid choices: {}",
            result.message()
        );
    }

    // ── `kj ledger runs` ────────────────────────────────────────────────

    /// A privileged caller with no joined context — `kj context create`
    /// without `--parent` resolves `context_id: None` to no FK lookup,
    /// unlike `test_caller()`'s fake unregistered context id (which trips
    /// the `forked_from` FK). Mirrors `lifecycle::tests::unjoined_caller`.
    fn unjoined_caller() -> KjCaller {
        KjCaller {
            principal_id: kaijutsu_types::PrincipalId::new(),
            context_id: None,
            session_id: kaijutsu_types::SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: true,
        }
    }

    #[tokio::test]
    async fn ledger_runs_on_a_fresh_ledger_is_empty() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("ledger"), s("runs")], &c).await;
        assert!(result.is_ok(), "{result:?}");
        assert!(result.message().contains("no rc runs recorded"), "{}", result.message());
        let data = match &result {
            KjResult::Ok { data: Some(d), .. } => d.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(data.as_array().unwrap().len(), 0);
    }

    /// `kj ledger runs` lists the rc lifecycle run log after a real
    /// lifecycle fires, and `.data` stays the flat array-of-ids the kj
    /// list-command convention promises (same shape `ledger_list`/
    /// `ledger_rules` use).
    #[tokio::test]
    async fn ledger_runs_lists_a_run_after_a_lifecycle_fires() {
        use crate::kj::test_helpers::install_rc_script_file;

        let d = test_dispatcher().await;
        let c = unjoined_caller();
        install_rc_script_file(&d, "/etc/rc/ledgertest/create/S00-noop.kai", "true").await;

        let created = d
            .dispatch(
                &[s("context"), s("create"), s("ledger-runs-ctx"), s("--type"), s("ledgertest")],
                &c,
            )
            .await;
        assert!(created.is_ok(), "context create failed: {}", created.message());

        let result = d.dispatch(&[s("ledger"), s("runs")], &c).await;
        assert!(result.is_ok(), "{result:?}");
        assert!(result.message().contains("ledgertest"), "{}", result.message());
        assert!(result.message().contains("create"), "{}", result.message());

        let data = match &result {
            KjResult::Ok { data: Some(d), .. } => d.clone(),
            other => panic!("kj ledger runs must emit structured data: {other:?}"),
        };
        let ids = data.as_array().expect(".data must be a JSON array of run ids");
        assert_eq!(ids.len(), 1, "{data}");
        assert!(ids[0].is_string(), "{data}");
    }

    /// `kj ledger runs <run-id>` shows one run's metadata plus the
    /// per-script rows the lifecycle recorded for it.
    #[tokio::test]
    async fn ledger_runs_show_lists_one_runs_scripts() {
        use crate::kj::test_helpers::install_rc_script_file;

        let d = test_dispatcher().await;
        let c = unjoined_caller();
        install_rc_script_file(&d, "/etc/rc/ledgertest2/create/S00-hello.kai", "echo hi").await;

        let created = d
            .dispatch(
                &[
                    s("context"),
                    s("create"),
                    s("ledger-run-show-ctx"),
                    s("--type"),
                    s("ledgertest2"),
                ],
                &c,
            )
            .await;
        assert!(created.is_ok(), "context create failed: {}", created.message());

        let list = d.dispatch(&[s("ledger"), s("runs")], &c).await;
        let data = match &list {
            KjResult::Ok { data: Some(d), .. } => d.clone(),
            other => panic!("{other:?}"),
        };
        let run_id = data[0].as_str().expect("a run id string").to_string();

        let shown = d.dispatch(&[s("ledger"), s("runs"), s(&run_id)], &c).await;
        assert!(shown.is_ok(), "{shown:?}");
        assert!(shown.message().contains("S00-hello.kai"), "{}", shown.message());
        let obj = match &shown {
            KjResult::Ok { data: Some(v), .. } => v.clone(),
            other => panic!("kj ledger runs <id> must emit structured data: {other:?}"),
        };
        assert_eq!(obj["run_id"].as_str(), Some(run_id.as_str()));
        let scripts = obj["scripts"].as_array().expect("scripts array");
        assert_eq!(scripts.len(), 1, "{obj}");
        assert!(
            scripts[0]["path"].as_str().unwrap().ends_with("S00-hello.kai"),
            "{obj}"
        );
    }

    /// An unknown run id errors loudly — never a blank "show" that could
    /// read as an empty-but-real run.
    #[tokio::test]
    async fn ledger_runs_show_of_an_unknown_id_errors_loudly() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("ledger"), s("runs"), s("no-such-run")], &c).await;
        assert!(!result.is_ok());
        assert!(result.message().contains("no such run"), "{}", result.message());
    }

    mod dispatch_wiring {
        use super::*;

        #[tokio::test]
        async fn ledger_bare_renders_help() {
            let d = test_dispatcher().await;
            let c = test_caller();
            let result = d.dispatch(&[s("ledger")], &c).await;
            assert!(
                matches!(&result, KjResult::Ok { ephemeral: true, .. }),
                "kj ledger (no subcommand) should render help, got {result:?}"
            );
        }

        /// `kj ledger` reads the kernel DB, not a context — it must work
        /// from a shell with no context joined (the place a human goes to
        /// answer a gate).
        #[tokio::test]
        async fn ledger_works_without_a_joined_context() {
            let d = test_dispatcher().await;
            let c = KjCaller {
                principal_id: kaijutsu_types::PrincipalId::new(),
                context_id: None,
                session_id: kaijutsu_types::SessionId::new(),
                confirmed: false,
                rc_depth: 0,
                privileged: false,
            };
            let result = d.dispatch(&[s("ledger"), s("list")], &c).await;
            assert!(result.is_ok(), "{result:?}");
        }
    }
}
