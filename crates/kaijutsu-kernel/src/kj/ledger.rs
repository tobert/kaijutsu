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
use approval_ledger::types::{ApprovalStatus, NewAsk, NewSignal, Origin, RuleScope, SignalSourceKind, SignalVerdict};
use clap::{Parser, Subcommand};
use kaijutsu_types::{ContentType, ContextId};
use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::{clap_help_for, refs, KjCaller, KjDispatcher, KjResult};

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

/// `--origin <origin>` on `list` — a `ValueEnum` so a typo fails at parse
/// time with clap's own "possible values are..." message rather than
/// coming back as a silently empty result. Values match `approvals.origin`'s
/// own `CHECK`-constrained strings (`schema.rs` in `approval-ledger`)
/// exactly — `shell_gate`/`kj_verb`, not clap's default kebab-case.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "snake_case")]
enum OriginArg {
    /// A hook's `ask` action fired.
    Hook,
    /// The shell tool gated a command before executing it.
    ShellGate,
    /// A privileged `kj` verb gated itself.
    KjVerb,
}

impl OriginArg {
    fn to_ledger(self) -> Origin {
        match self {
            Self::Hook => Origin::Hook,
            Self::ShellGate => Origin::ShellGate,
            Self::KjVerb => Origin::KjVerb,
        }
    }
}

/// `--status <status>` on `list` — same ValueEnum-for-typos reasoning as
/// `OriginArg`. A terminal value (`allowed`/`denied`/`expired`/
/// `abandoned`) switches the listing to decided asks by itself; `pending`/
/// `claimed` stay in the live queue. See `ledger_list`'s doc for exactly
/// how this composes with `--history`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "snake_case")]
enum StatusArg {
    Pending,
    Claimed,
    Allowed,
    Denied,
    Expired,
    Abandoned,
}

impl StatusArg {
    fn to_ledger(self) -> ApprovalStatus {
        match self {
            Self::Pending => ApprovalStatus::Pending,
            Self::Claimed => ApprovalStatus::Claimed,
            Self::Allowed => ApprovalStatus::Allowed,
            Self::Denied => ApprovalStatus::Denied,
            Self::Expired => ApprovalStatus::Expired,
            Self::Abandoned => ApprovalStatus::Abandoned,
        }
    }
}

/// `--verdict <verdict>` on `signal add` — a `ValueEnum` for the same
/// typo-fails-loud reason as `OriginArg`/`StatusArg`. Values match
/// `approval_signals.verdict`'s own `CHECK`-constrained strings exactly.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "snake_case")]
enum SignalVerdictArg {
    /// The signal recommends raising a prompt that would not otherwise
    /// fire, or leaving an already-firing one alone. Never auto-decides.
    Escalate,
    /// The signal recommends denial. Advisory only — see the module doc:
    /// nothing in `approval_ledger` reads a signal to auto-decide an ask.
    Deny,
    /// The signal recommends allowing. Advisory only, same as `deny` —
    /// this can never become an auto-allow by itself (`rules::redeem`
    /// composes coverage from `approval_rules` alone, never from
    /// `approval_signals`).
    Allow,
}

impl SignalVerdictArg {
    fn to_ledger(self) -> SignalVerdict {
        match self {
            Self::Escalate => SignalVerdict::Escalate,
            Self::Deny => SignalVerdict::Deny,
            Self::Allow => SignalVerdict::Allow,
        }
    }
}

/// `--verb <verb>` on `runs` — validated against
/// [`super::lifecycle::RC_VERBS`] instead of its own duplicate `ValueEnum`.
/// That list is already documented as "the single source of truth" for
/// which verbs the scheduler fires; a second, hand-maintained enum here
/// would be exactly the drift hazard its own doc comment warns about. Still
/// fails at clap parse time with a "possible values" style message — just
/// sourced from the one place that's allowed to define the set.
fn parse_verb_arg(s: &str) -> Result<String, String> {
    if super::lifecycle::RC_VERBS.contains(&s) {
        Ok(s.to_string())
    } else {
        Err(format!(
            "invalid value '{s}' for '--verb': possible values are: {}",
            super::lifecycle::RC_VERBS.join(", ")
        ))
    }
}

/// Parse `--since <duration>` — `30m`, `2h`, `7d`. One integer immediately
/// followed by exactly one unit letter; no absolute timestamps, no
/// fractional numbers, no combined units (Amy's ruling: one format, no
/// ambiguity). Garbage is rejected loudly — CLAUDE.md's stance against
/// silent fallbacks applies just as much to a mistyped flag as to
/// anything else; `--since 5x` must be an error, never "no filter".
fn parse_since_duration_ms(s: &str) -> Result<i64, String> {
    let bad = || {
        format!(
            "kj ledger: invalid --since {s:?} — use an integer followed by m/h/d, e.g. 30m, 2h, 7d"
        )
    };
    if s.is_empty() {
        return Err(bad());
    }
    let unit = s.chars().next_back().expect("checked non-empty above");
    let digits = &s[..s.len() - unit.len_utf8()];
    let n: i64 = digits.parse().map_err(|_| bad())?;
    if n <= 0 {
        return Err(bad());
    }
    let per_unit_ms: i64 = match unit {
        'm' => 60_000,
        'h' => 3_600_000,
        'd' => 86_400_000,
        _ => return Err(bad()),
    };
    n.checked_mul(per_unit_ms).ok_or_else(bad)
}

/// Resolve a `--since` duration (in ms) against the current wall clock
/// into an absolute epoch-ms cutoff — `created_at`/`started_at` at or
/// after this value is "within the window".
fn since_cutoff_ms(duration_ms: i64) -> i64 {
    kaijutsu_types::now_millis() as i64 - duration_ms
}

/// The "showing N of TOTAL" line every `kj ledger` listing prints when
/// `--limit` cut real rows off the end — Amy's ruling: a silently
/// truncated list is the quiet fallback CLAUDE.md treats as a defect, so
/// the line only appears when something was actually cut (`None`
/// otherwise; an untruncated listing stays uncluttered). `extra_flags` is
/// the command-specific narrowing flags beyond `--since`/`--limit` to
/// mention (asks have `--origin`/`--status`; runs have `--context`/
/// `--verb`; rules have neither).
fn truncation_notice(shown: usize, total: i64, extra_flags: &str) -> Option<String> {
    if (shown as i64) < total {
        Some(format!(
            "showing {shown} of {total} — raise with --limit, or narrow with --since{extra_flags}"
        ))
    } else {
        None
    }
}

/// One `SignalRow` as `.data` JSON — shared by `--signals` on `list` and
/// `show`, so the two commands never drift on what a signal looks like on
/// the wire.
fn signal_row_json(s: &approval_ledger::types::SignalRow) -> serde_json::Value {
    serde_json::json!({
        "seq": s.seq,
        "source_kind": s.source_kind.to_string(),
        "source_id": s.source_id,
        "model_id": s.model_id,
        "weight_hash": s.weight_hash,
        "stmt_seq": s.stmt_seq,
        "cmd_seq": s.cmd_seq,
        "label": s.label,
        "score": s.score,
        "verdict": s.verdict.to_string(),
    })
}

#[derive(Subcommand, Debug)]
enum LedgerCommand {
    /// List asks. By default, shows the live pending queue, oldest first
    /// (the order you drain a work queue in), capped at `--limit` (default
    /// 20) — `claimed` asks are deliberately excluded: an answerer is
    /// already working them, and listing them invites a second answerer to
    /// step on that claim (see `--status claimed` to see them anyway).
    /// `--history` shows decided asks instead — `allowed`, `denied`,
    /// `expired`, or `abandoned` — most recently created first.
    /// `--since`/`--origin`/`--status` narrow either listing; a decided
    /// `--status` value (e.g. `allowed`) switches to the history view by
    /// itself, so `--history` never needs to be passed alongside it.
    List {
        /// Show decided asks instead of the pending/claimed queue.
        #[arg(long)]
        history: bool,
        /// Newest N rows (oldest N for the pending queue). Default 20.
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Only asks created within this long of now: an integer plus
        /// m/h/d, e.g. 30m, 2h, 7d. No absolute timestamps.
        #[arg(long)]
        since: Option<String>,
        /// Only asks from this origin.
        #[arg(long)]
        origin: Option<OriginArg>,
        /// Only asks in this status. A decided value (allowed/denied/
        /// expired/abandoned) shows history without needing --history too;
        /// `claimed` shows the in-flight queue the default hides.
        #[arg(long)]
        status: Option<StatusArg>,
        /// Add a SIGNALS column (the advisory-signal count) to the table,
        /// and switch `.data` from a flat array of request-id strings to
        /// an array of `{request_id, signals}` objects. Off by default —
        /// most listings don't want per-row detail.
        #[arg(long)]
        signals: bool,
    },
    /// Show one ask in full, including the statement being authorized.
    /// Works for a decided ask as well as a pending one.
    Show {
        /// The ask to show. Request ids come from `kj ledger list`.
        request_id: String,
        /// Also show every advisory signal attached to this ask (a rule
        /// that almost matched, a classifier's read) — informational only,
        /// never itself a gate. Off by default.
        #[arg(long)]
        signals: bool,
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
    /// List active (not-yet-forgotten) standing rules, most recently
    /// created first, capped at `--limit` (default 20).
    Rules {
        /// Newest N rules. Default 20.
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Only rules created within this long of now: an integer plus
        /// m/h/d, e.g. 30m, 2h, 7d. No absolute timestamps.
        #[arg(long)]
        since: Option<String>,
    },
    /// Forget a standing rule so its statement escalates to a human again.
    Forget {
        /// The rule to forget. Rule ids come from `kj ledger rules`.
        rule_id: String,
    },
    // A bare positional rather than a `--run` flag, to match `Show`'s shape:
    // the same list-vs-show split as the ask verbs, applied to the run log.
    /// List rc lifecycle runs, most recent first, capped at `--limit`
    /// (default 20) — the durable record of whether a context's rc
    /// scripts actually ran.
    Runs {
        /// Show this run's per-script detail instead of the run listing.
        /// Run ids come from `kj ledger runs`. Lists all runs when omitted.
        run_id: Option<String>,
        /// Newest N runs. Default 20.
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Only runs started within this long of now: an integer plus
        /// m/h/d, e.g. 30m, 2h, 7d. No absolute timestamps.
        #[arg(long)]
        since: Option<String>,
        /// Only runs for this context. Accepts `.` (current), a label, or
        /// a hex id prefix — same resolution `kj context` commands use.
        #[arg(long)]
        context: Option<String>,
        /// Only runs for this rc lifecycle verb (create, fork, ...).
        #[arg(long, value_parser = parse_verb_arg)]
        verb: Option<String>,
    },
    /// Advisory signals — a rule that almost matched, a classifier's risk
    /// read — attached to an ask but never themselves a gate.
    Signal {
        #[command(subcommand)]
        command: SignalCommand,
    },
}

#[derive(Subcommand, Debug)]
enum SignalCommand {
    /// Add one advisory signal from a scorer (a hook body calls this after
    /// scoring a command). With `--auto-allow`, creates a new ask that is
    /// decided `allowed` in the same transaction — the log-only path: the
    /// ask is recorded and never blocks on a human. With `--request-id <id>`,
    /// attaches the signal to an existing ask (a second scored command from
    /// the same statement lands on the ask the first one created). Exactly
    /// one of the two is required. A signal is advisory: nothing reads it to
    /// decide an ask.
    Add {
        /// The statement text this signal is about. Becomes the new ask's
        /// description under `--auto-allow`; under `--request-id` it is
        /// informational only (the row it attaches to already has its own
        /// description). Unquoted kaish variable expansion carries embedded
        /// spaces as one argument (kaish does no word splitting), so a
        /// caller with arbitrarily long or `"`-laden text needs nothing
        /// beyond passing `$var` here — there is no `--stdin` on this verb.
        statement: String,
        /// Who produced this signal, e.g. `lfm2d`.
        #[arg(long = "source-id")]
        source_id: Option<String>,
        /// Which model scored it, e.g. `kube_ordinal_v8` — read from the
        /// scorer's own response at call time, never hard-coded by a
        /// caller (label/model vocabularies change between checkpoints).
        #[arg(long = "model-id")]
        model_id: Option<String>,
        /// The scoring checkpoint's weight hash — the audit pairing a
        /// verdict to the exact weights that produced it.
        #[arg(long = "weight-hash")]
        weight_hash: Option<String>,
        /// The winning label, read from the scorer's response (e.g.
        /// `situation-normal`) — never a value this verb invents.
        #[arg(long)]
        label: Option<String>,
        /// The winning label's score, as the scorer reported it. Not
        /// thresholded here or anywhere in this crate — see `--verdict`.
        #[arg(long)]
        score: Option<f64>,
        /// The signal's own recommendation: `escalate` (raise or leave a
        /// prompt), `deny`, or `allow`. Always advisory — see the module
        /// doc: nothing in `approval_ledger` ever reads a signal to
        /// auto-decide an ask, `allow` included.
        #[arg(long, value_enum)]
        verdict: SignalVerdictArg,
        /// Create a NEW ask, already decided `Allowed`, carrying this
        /// signal — the log-only classifier path. Mutually exclusive with
        /// `--request-id`.
        #[arg(long = "auto-allow")]
        auto_allow: bool,
        /// Attach this signal to an ask that already exists, instead of
        /// creating one. Required unless `--auto-allow` is given.
        #[arg(long = "request-id")]
        request_id: Option<String>,
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
            LedgerCommand::List { history, limit, since, origin, status, signals } => {
                self.ledger_list(history, limit, since.as_deref(), origin, status, signals)
            }
            LedgerCommand::Show { request_id, signals } => self.ledger_show(&request_id, signals),
            LedgerCommand::Allow { request_id, remember } => {
                self.ledger_decide(&request_id, true, caller, remember)
            }
            LedgerCommand::Deny { request_id, remember } => {
                self.ledger_decide(&request_id, false, caller, remember)
            }
            LedgerCommand::Rules { limit, since } => self.ledger_rules(limit, since.as_deref()),
            LedgerCommand::Forget { rule_id } => self.ledger_forget(&rule_id),
            LedgerCommand::Runs { run_id, limit, since, context, verb } => match run_id {
                Some(id) => self.ledger_run_show(&id),
                None => self.ledger_runs_list(caller, limit, since.as_deref(), context.as_deref(), verb.as_deref()),
            },
            LedgerCommand::Signal { command } => match command {
                SignalCommand::Add {
                    statement,
                    source_id,
                    model_id,
                    weight_hash,
                    label,
                    score,
                    verdict,
                    auto_allow,
                    request_id,
                } => self.ledger_signal_add(
                    caller,
                    &statement,
                    source_id,
                    model_id,
                    weight_hash,
                    label,
                    score,
                    verdict,
                    auto_allow,
                    request_id.as_deref(),
                ),
            },
        }
    }

    /// `kj ledger list`. `--status` fully determines both which single
    /// status is queried AND the listing's mode: a status is queue-style
    /// (oldest first, the order you drain a work queue in) unless it's
    /// terminal, in which case it's history-style (newest first) — its own
    /// terminality decides that, so `--status allowed` alone switches to
    /// history and `--status claimed` alone stays queue-style, and
    /// `--history` never needs to be passed alongside either (Amy's
    /// ruling: "`--status allowed` clearly implies history; make that
    /// work sensibly rather than making the user pass both"). Without
    /// `--status`, the bare `--history` flag picks between the two modes
    /// this command has always had: the default pending-only queue, or
    /// the four-terminal-status history view.
    fn ledger_list(
        &self,
        history: bool,
        limit: u32,
        since: Option<&str>,
        origin: Option<OriginArg>,
        status: Option<StatusArg>,
        show_signals: bool,
    ) -> KjResult {
        let since_ms = match since {
            Some(s) => match parse_since_duration_ms(s) {
                Ok(ms) => Some(since_cutoff_ms(ms)),
                Err(e) => return KjResult::Err(e),
            },
            None => None,
        };

        let (statuses, newest_first, is_default_pending) = match status {
            Some(s) => {
                let ledger_status = s.to_ledger();
                (vec![ledger_status], ledger_status.is_terminal(), false)
            }
            None if history => (
                vec![
                    ApprovalStatus::Allowed,
                    ApprovalStatus::Denied,
                    ApprovalStatus::Expired,
                    ApprovalStatus::Abandoned,
                ],
                true,
                false,
            ),
            None => (vec![ApprovalStatus::Pending], false, true),
        };

        let filter = approval_ledger::ask::AskListFilter {
            statuses,
            origin: origin.map(OriginArg::to_ledger),
            since_ms,
            limit: limit as i64,
            newest_first,
        };
        let (rows, total) = {
            let db = self.kernel_db.lock();
            match approval_ledger::ask::list_asks_filtered(db.conn_for_ledger(), &filter) {
                Ok(r) => r,
                Err(e) => return KjResult::Err(format!("kj ledger list: {e}")),
            }
        };

        // `--signals` fetches every listed row's advisory signals — one
        // query per row, acceptable at `--limit`'s default (20) and never
        // unbounded (the same cap that bounds the listing itself). Off by
        // default: most listings don't want per-row detail, and `.data`'s
        // shape stays the plain array-of-ids convention every other kj
        // list command uses unless a caller opts into more.
        let signals_by_row: Vec<Vec<approval_ledger::types::SignalRow>> = if show_signals {
            let db = self.kernel_db.lock();
            let conn = db.conn_for_ledger();
            let mut out = Vec::with_capacity(rows.len());
            for r in &rows {
                match approval_ledger::ask::list_signals(conn, &r.request_id) {
                    Ok(s) => out.push(s),
                    Err(e) => return KjResult::Err(format!("kj ledger list: {e}")),
                }
            }
            out
        } else {
            Vec::new()
        };

        let data = if show_signals {
            serde_json::Value::Array(
                rows.iter()
                    .zip(signals_by_row.iter())
                    .map(|(r, sigs)| {
                        serde_json::json!({
                            "request_id": r.request_id.clone(),
                            "signals": sigs.iter().map(signal_row_json).collect::<Vec<_>>(),
                        })
                    })
                    .collect(),
            )
        } else {
            serde_json::Value::Array(
                rows.iter()
                    .map(|r| serde_json::json!(r.request_id.clone()))
                    .collect(),
            )
        };
        if rows.is_empty() {
            let msg = if is_default_pending {
                "(no pending approvals)"
            } else if newest_first {
                "(no decided asks)"
            } else {
                "(no matching asks)"
            };
            return KjResult::ok_with_data(msg.to_string(), data);
        }
        let mut lines = vec![if show_signals {
            format!("  {:<38}  {:<8}  {:<9}  {:<7}  {}", "REQUEST", "ORIGIN", "STATUS", "SIGNALS", "DESCRIPTION")
        } else {
            format!("  {:<38}  {:<8}  {:<9}  {}", "REQUEST", "ORIGIN", "STATUS", "DESCRIPTION")
        }];
        for (i, r) in rows.iter().enumerate() {
            if show_signals {
                lines.push(format!(
                    "  {:<38}  {:<8}  {:<9}  {:<7}  {}",
                    r.request_id,
                    r.origin,
                    r.status,
                    signals_by_row[i].len(),
                    r.description,
                ));
            } else {
                lines.push(format!(
                    "  {:<38}  {:<8}  {:<9}  {}",
                    r.request_id,
                    r.origin,
                    r.status,
                    r.description,
                ));
            }
        }
        lines.push(String::new());
        if let Some(notice) = truncation_notice(rows.len(), total, "/--origin/--status") {
            lines.push(notice);
            lines.push(String::new());
        }
        if newest_first {
            lines.push("see full detail with: kj ledger show <request-id>".into());
        } else {
            lines.push("answer with: kj ledger allow <request-id>  |  kj ledger deny <request-id>".into());
        }
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    fn ledger_show(&self, request_id: &str, show_signals: bool) -> KjResult {
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
        let signal_rows = if show_signals {
            match approval_ledger::ask::list_signals(conn, request_id) {
                Ok(s) => s,
                Err(e) => return KjResult::Err(format!("kj ledger show: {e}")),
            }
        } else {
            Vec::new()
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
        if let Some(reason) = &row.auto_reason {
            lines.push(format!("auto:       {reason}"));
        }
        if show_signals {
            for s in &signal_rows {
                lines.push(format!(
                    "signal:     {} {} label={} score={} verdict={}",
                    s.source_kind,
                    s.source_id.as_deref().unwrap_or("-"),
                    s.label.as_deref().unwrap_or("-"),
                    s.score.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string()),
                    s.verdict,
                ));
            }
        }
        // `context_id`, `instance`, `tool` and `hook_id` are here for a
        // client that has to ROUTE this ask, not just render it: an ACP
        // session, or the app, needs to know which context an ask belongs
        // to before it can decide whose screen to put it on. They were
        // missing while the only reader was a human at a shell, who
        // already knew.
        let mut data = serde_json::json!({
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
        if show_signals {
            data["signals"] = serde_json::Value::Array(signal_rows.iter().map(signal_row_json).collect());
        }
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

    /// `kj ledger rules --limit/--since`, most recently created first.
    fn ledger_rules(&self, limit: u32, since: Option<&str>) -> KjResult {
        let since_ms = match since {
            Some(s) => match parse_since_duration_ms(s) {
                Ok(ms) => Some(since_cutoff_ms(ms)),
                Err(e) => return KjResult::Err(e),
            },
            None => None,
        };
        let filter = approval_ledger::rules::RuleListFilter { since_ms, limit: limit as i64 };
        let (rows, total) = {
            let db = self.kernel_db.lock();
            match approval_ledger::rules::list_rules_filtered(db.conn_for_ledger(), &filter) {
                Ok(r) => r,
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
        if let Some(notice) = truncation_notice(rows.len(), total, "") {
            lines.push(notice);
            lines.push(String::new());
        }
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

    /// `kj ledger runs --limit/--since/--context/--verb` — the rc
    /// lifecycle run log, newest first. `.data` stays a flat array of
    /// run-id strings per the kj list-command convention (`ledger_list`/
    /// `ledger_rules` do the same). `--context` accepts `.`/label/hex
    /// prefix through the same resolver `kj context` commands use
    /// ([`refs::resolve_context_ref`]) rather than requiring a caller to
    /// type a full id.
    fn ledger_runs_list(
        &self,
        caller: &KjCaller,
        limit: u32,
        since: Option<&str>,
        context: Option<&str>,
        verb: Option<&str>,
    ) -> KjResult {
        let since_ms = match since {
            Some(s) => match parse_since_duration_ms(s) {
                Ok(ms) => Some(since_cutoff_ms(ms)),
                Err(e) => return KjResult::Err(e),
            },
            None => None,
        };

        let (rows, total) = {
            let db = self.kernel_db.lock();
            let context_id = match context {
                Some(c) => match refs::resolve_context_ref(&refs::parse_context_ref(c), caller, &db) {
                    Ok(id) => Some(id),
                    Err(e) => return KjResult::Err(format!("kj ledger runs: {e}")),
                },
                None => None,
            };
            let filter = approval_ledger::rc_runs::RunListFilter {
                context_id: context_id.map(|c| c.as_bytes().to_vec()),
                verb: verb.map(|v| v.to_string()),
                since_ms,
                limit: limit as i64,
            };
            match approval_ledger::rc_runs::list_runs_filtered(db.conn_for_ledger(), &filter) {
                Ok(r) => r,
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
        if let Some(notice) = truncation_notice(rows.len(), total, "/--context/--verb") {
            lines.push(notice);
            lines.push(String::new());
        }
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

    /// `kj ledger signal add` — see [`SignalCommand::Add`] for the full
    /// contract. `--auto-allow` and `--request-id` are validated mutually
    /// exclusive-and-required here rather than via clap's `ArgGroup`: the
    /// error needs to name BOTH the missing choice and why (log-only vs.
    /// attach), which reads better as prose than as clap's generic
    /// "one of these is required" message.
    #[allow(clippy::too_many_arguments)]
    fn ledger_signal_add(
        &self,
        caller: &KjCaller,
        statement: &str,
        source_id: Option<String>,
        model_id: Option<String>,
        weight_hash: Option<String>,
        label: Option<String>,
        score: Option<f64>,
        verdict: SignalVerdictArg,
        auto_allow: bool,
        request_id: Option<&str>,
    ) -> KjResult {
        match (auto_allow, request_id) {
            (true, Some(_)) => {
                return KjResult::Err(
                    "kj ledger signal add: --auto-allow and --request-id are mutually exclusive \
                     — --auto-allow creates a new ask, --request-id attaches to an existing one"
                        .to_string(),
                );
            }
            (false, None) => {
                return KjResult::Err(
                    "kj ledger signal add: give --auto-allow (create a new log-only ask) or \
                     --request-id <id> (attach to an existing one)"
                        .to_string(),
                );
            }
            _ => {}
        }

        let sig = NewSignal {
            source_kind: SignalSourceKind::Classifier,
            source_id: source_id.clone(),
            model_id: model_id.clone(),
            weight_hash,
            stmt_seq: None,
            cmd_seq: None,
            label,
            score,
            verdict: verdict.to_ledger(),
        };

        if auto_allow {
            let Some(context_id) = caller.context_id else {
                return KjResult::Err(
                    "kj ledger signal add --auto-allow: no active context to attribute this ask to"
                        .to_string(),
                );
            };
            let auto_reason = format!(
                "{} (log-only)",
                source_id.as_deref().unwrap_or("classifier")
            );
            let ask = NewAsk {
                context_id: context_id.as_bytes().to_vec(),
                principal_id: caller.principal_id.as_bytes().to_vec(),
                origin: Origin::Hook,
                instance: None,
                tool: None,
                hook_id: None,
                description: statement.to_string(),
                statements: vec![],
                authorized_label: None,
                rc_run_id: None,
                expires_at: None,
                options: vec![],
                signals: vec![sig],
            };
            let result = {
                let db = self.kernel_db.lock();
                approval_ledger::ask::create_auto_allowed_ask(db.conn_for_ledger(), &ask, &auto_reason)
            };
            let new_request_id = match result {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj ledger signal add: {e}")),
            };
            crate::kj::gate::announce_ledger_change(&self.kernel_db, self.kernel.ledger_flows());
            KjResult::ok_with_data(
                format!("auto-allowed ask {new_request_id} ({statement})"),
                serde_json::json!({ "request_id": new_request_id, "auto_allow": true }),
            )
        } else {
            // `request_id` is `Some` here — checked above.
            let request_id = request_id.expect("checked above: request_id is Some without --auto-allow");
            let result = {
                let db = self.kernel_db.lock();
                approval_ledger::ask::add_signal(db.conn_for_ledger(), request_id, &sig)
            };
            match result {
                Ok(row) => {
                    crate::kj::gate::announce_ledger_change(&self.kernel_db, self.kernel.ledger_flows());
                    KjResult::ok_with_data(
                        format!("attached signal (seq {}) to ask {request_id}", row.seq),
                        serde_json::json!({ "request_id": request_id, "seq": row.seq }),
                    )
                }
                Err(LedgerError::NotFound(_)) => {
                    KjResult::Err(format!("kj ledger signal add: no such ask {request_id}"))
                }
                Err(e) => KjResult::Err(format!("kj ledger signal add: {e}")),
            }
        }
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

    // ── `kj ledger signal add` ─────────────────────────────────────────

    fn signal_add_argv(statement: &str, extra: &[&str]) -> Vec<String> {
        let mut argv = vec![
            s("ledger"),
            s("signal"),
            s("add"),
            s(statement),
            s("--source-id"),
            s("lfm2d"),
            s("--model-id"),
            s("kube_ordinal_v8"),
            s("--weight-hash"),
            s("abc123"),
            s("--label"),
            s("situation-normal"),
            s("--score"),
            s("0.6"),
            s("--verdict"),
            s("escalate"),
        ];
        argv.extend(extra.iter().map(|s| (*s).to_string()));
        argv
    }

    #[tokio::test]
    async fn ledger_signal_add_auto_allow_creates_an_allowed_ask() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let argv = signal_add_argv("rm build artifacts now", &["--auto-allow"]);
        let result = d.dispatch(&argv, &c).await;
        assert!(result.is_ok(), "{result:?}");

        let data = match &result {
            KjResult::Ok { data: Some(v), .. } => v.clone(),
            other => panic!("kj ledger signal add --auto-allow must emit structured data: {other:?}"),
        };
        let request_id = data["request_id"].as_str().expect("request_id string").to_string();
        assert_eq!(data["auto_allow"], serde_json::json!(true));

        let db = d.kernel_db.lock();
        let row = approval_ledger::ask::get_approval(db.conn_for_ledger(), &request_id).unwrap().unwrap();
        assert_eq!(row.status, approval_ledger::types::ApprovalStatus::Allowed);
        assert!(row.decided_by.is_none(), "log-only: no human decided this");
        assert_eq!(row.decided_option.as_deref(), Some("auto_allow"));
        assert!(
            row.auto_reason.as_deref().unwrap_or_default().contains("lfm2d"),
            "auto_reason must name the classifier source: {:?}",
            row.auto_reason
        );

        let signals = approval_ledger::ask::list_signals(db.conn_for_ledger(), &request_id).unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].label.as_deref(), Some("situation-normal"));
    }

    /// The second half of the design: a hook body's later scored clauses
    /// attach to the SAME ask the first `--auto-allow` call created,
    /// rather than minting a new ask per clause.
    #[tokio::test]
    async fn ledger_signal_add_attaches_to_an_existing_ask_via_request_id() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let first = d.dispatch(&signal_add_argv("rm build artifacts now", &["--auto-allow"]), &c).await;
        assert!(first.is_ok(), "{first:?}");
        let request_id = match &first {
            KjResult::Ok { data: Some(v), .. } => v["request_id"].as_str().unwrap().to_string(),
            other => panic!("{other:?}"),
        };

        let second = d
            .dispatch(&signal_add_argv("rm build artifacts now", &["--request-id", &request_id]), &c)
            .await;
        assert!(second.is_ok(), "{second:?}");
        let data = match &second {
            KjResult::Ok { data: Some(v), .. } => v.clone(),
            other => panic!("kj ledger signal add --request-id must emit structured data: {other:?}"),
        };
        assert_eq!(data["request_id"], serde_json::json!(request_id));
        assert_eq!(data["seq"], serde_json::json!(1), "the second attach must get seq 1, after the auto-allow call's seq 0");

        let db = d.kernel_db.lock();
        let signals = approval_ledger::ask::list_signals(db.conn_for_ledger(), &request_id).unwrap();
        assert_eq!(signals.len(), 2, "both signals must land on the ONE ask: {signals:?}");
    }

    #[tokio::test]
    async fn ledger_signal_add_requires_auto_allow_or_request_id() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&signal_add_argv("echo hi", &[]), &c).await;
        assert!(!result.is_ok());
        assert!(result.message().contains("--auto-allow"), "{}", result.message());
        assert!(result.message().contains("--request-id"), "{}", result.message());
    }

    #[tokio::test]
    async fn ledger_signal_add_auto_allow_and_request_id_are_mutually_exclusive() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d
            .dispatch(&signal_add_argv("echo hi", &["--auto-allow", "--request-id", "deadbeef"]), &c)
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("mutually exclusive"), "{}", result.message());
    }

    #[tokio::test]
    async fn ledger_signal_add_attach_to_an_unknown_ask_errors_loudly() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d
            .dispatch(&signal_add_argv("echo hi", &["--request-id", "does-not-exist"]), &c)
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("no such ask"), "{}", result.message());
    }

    #[tokio::test]
    async fn ledger_signal_add_invalid_verdict_fails_at_parse_time() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d
            .dispatch(
                &[
                    s("ledger"), s("signal"), s("add"), s("echo hi"),
                    s("--verdict"), s("maybe"), s("--auto-allow"),
                ],
                &c,
            )
            .await;
        assert!(!result.is_ok());
        assert!(
            result.message().contains("escalate") && result.message().contains("allow"),
            "clap's error should name the valid choices: {}",
            result.message()
        );
    }

    // ── `--signals` on `kj ledger list`/`show` ─────────────────────────

    #[tokio::test]
    async fn ledger_show_signals_flag_includes_signal_detail() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let created = d.dispatch(&signal_add_argv("rm build artifacts now", &["--auto-allow"]), &c).await;
        let request_id = match &created {
            KjResult::Ok { data: Some(v), .. } => v["request_id"].as_str().unwrap().to_string(),
            other => panic!("{other:?}"),
        };

        // Without --signals: no signals key at all, and no "signal:" line.
        let plain = d.dispatch(&[s("ledger"), s("show"), s(&request_id)], &c).await;
        assert!(plain.is_ok(), "{plain:?}");
        assert!(!plain.message().contains("signal:"), "{}", plain.message());
        let plain_data = match &plain {
            KjResult::Ok { data: Some(v), .. } => v.clone(),
            other => panic!("{other:?}"),
        };
        assert!(plain_data.get("signals").is_none(), "{plain_data}");

        // With --signals: the signal shows up in both the text and .data.
        let shown = d.dispatch(&[s("ledger"), s("show"), s(&request_id), s("--signals")], &c).await;
        assert!(shown.is_ok(), "{shown:?}");
        assert!(shown.message().contains("signal:"), "{}", shown.message());
        assert!(shown.message().contains("situation-normal"), "{}", shown.message());
        let data = match &shown {
            KjResult::Ok { data: Some(v), .. } => v.clone(),
            other => panic!("{other:?}"),
        };
        let signals = data["signals"].as_array().expect("signals array");
        assert_eq!(signals.len(), 1, "{data}");
        assert_eq!(signals[0]["label"], serde_json::json!("situation-normal"));
        assert_eq!(signals[0]["source_kind"], serde_json::json!("classifier"));
    }

    #[tokio::test]
    async fn ledger_list_signals_flag_adds_a_signals_column_and_reshapes_data() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let created = d.dispatch(&signal_add_argv("rm build artifacts now", &["--auto-allow"]), &c).await;
        let request_id = match &created {
            KjResult::Ok { data: Some(v), .. } => v["request_id"].as_str().unwrap().to_string(),
            other => panic!("{other:?}"),
        };

        // Default .data stays the flat array-of-ids convention.
        let plain = d.dispatch(&[s("ledger"), s("list"), s("--status"), s("allowed")], &c).await;
        assert!(plain.is_ok(), "{plain:?}");
        let plain_data = match &plain {
            KjResult::Ok { data: Some(v), .. } => v.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(plain_data[0], serde_json::json!(request_id), "{plain_data}");

        // --signals reshapes .data to {request_id, signals} objects and
        // adds a SIGNALS column to the table.
        let with_signals = d
            .dispatch(&[s("ledger"), s("list"), s("--status"), s("allowed"), s("--signals")], &c)
            .await;
        assert!(with_signals.is_ok(), "{with_signals:?}");
        assert!(with_signals.message().contains("SIGNALS"), "{}", with_signals.message());
        let data = match &with_signals {
            KjResult::Ok { data: Some(v), .. } => v.clone(),
            other => panic!("{other:?}"),
        };
        let rows = data.as_array().expect("array of row objects");
        assert_eq!(rows.len(), 1, "{data}");
        assert_eq!(rows[0]["request_id"], serde_json::json!(request_id));
        let sigs = rows[0]["signals"].as_array().expect("signals array");
        assert_eq!(sigs.len(), 1, "{data}");
        assert_eq!(sigs[0]["label"], serde_json::json!("situation-normal"));
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

    // ── `--limit`/`--since`/`--origin`/`--status`/`--context`/`--verb` ──
    //
    // Amy's ruling: cap the pending queue too (default 20), and say so
    // loudly when rows are cut. These tests seed rows DIRECTLY through
    // `approval_ledger`'s own write API (bypassing `run_gate`/kaish
    // entirely) so a 25-row queue doesn't mean 25 real gate round trips —
    // this is a CLI listing test, not another exercise of the gate.

    /// Insert `n` pending asks with caller-controlled, strictly increasing
    /// `created_at` timestamps (`start_ms`, `start_ms + step_ms`, ...) —
    /// not the DB's own-clock default, which can't be trusted to give
    /// distinct milliseconds across a tight loop. Deterministic ordering
    /// is the whole point: `--since`/`--limit`/newest-vs-oldest-first
    /// assertions need to know exactly which id is "older".
    fn seed_pending_asks(
        conn: &rusqlite::Connection,
        ctx: ContextId,
        principal: kaijutsu_types::PrincipalId,
        origin: approval_ledger::types::Origin,
        label: &str,
        n: usize,
        start_ms: i64,
        step_ms: i64,
    ) -> Vec<String> {
        (0..n)
            .map(|i| {
                let ask = approval_ledger::types::NewAsk {
                    context_id: ctx.as_bytes().to_vec(),
                    principal_id: principal.as_bytes().to_vec(),
                    origin,
                    instance: None,
                    tool: None,
                    hook_id: None,
                    description: format!("{label} {i}"),
                    statements: vec![],
                    authorized_label: None,
                    rc_run_id: None,
                    expires_at: None,
                    options: vec![],
                    signals: vec![],
                };
                let request_id = approval_ledger::ask::create_ask(conn, &ask).unwrap();
                conn.execute(
                    "UPDATE approvals SET created_at = ?1 WHERE request_id = ?2",
                    rusqlite::params![start_ms + (i as i64) * step_ms, request_id],
                )
                .unwrap();
                request_id
            })
            .collect()
    }

    fn data_ids(result: &KjResult) -> Vec<String> {
        match result {
            KjResult::Ok { data: Some(v), .. } => v
                .as_array()
                .expect(".data must be an array")
                .iter()
                .map(|x| x.as_str().expect("array of id strings").to_string())
                .collect(),
            other => panic!("expected structured .data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ledger_list_default_limit_caps_the_pending_queue_and_says_so() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let ctx = c.context_id.expect("test caller has a context");
        {
            let db = d.kernel_db.lock();
            seed_pending_asks(
                db.conn_for_ledger(),
                ctx,
                c.principal_id,
                approval_ledger::types::Origin::ShellGate,
                "ask",
                25,
                1_000,
                10,
            );
        }

        let result = d.dispatch(&[s("ledger"), s("list")], &c).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(data_ids(&result).len(), 20, "default limit must cap the pending queue at 20");
        assert!(
            result.message().contains("showing 20 of 25"),
            "a truncated listing must say so loudly, per Amy's ruling: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn ledger_list_no_truncation_notice_when_nothing_is_cut() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let ctx = c.context_id.expect("test caller has a context");
        {
            let db = d.kernel_db.lock();
            seed_pending_asks(
                db.conn_for_ledger(),
                ctx,
                c.principal_id,
                approval_ledger::types::Origin::ShellGate,
                "ask",
                3,
                1_000,
                10,
            );
        }

        let result = d.dispatch(&[s("ledger"), s("list")], &c).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(data_ids(&result).len(), 3);
        assert!(
            !result.message().to_lowercase().contains("showing"),
            "no truncation notice belongs on an untruncated listing: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn ledger_list_limit_flag_narrows_the_pending_queue() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let ctx = c.context_id.expect("test caller has a context");
        {
            let db = d.kernel_db.lock();
            seed_pending_asks(
                db.conn_for_ledger(),
                ctx,
                c.principal_id,
                approval_ledger::types::Origin::ShellGate,
                "ask",
                5,
                1_000,
                10,
            );
        }

        let result = d.dispatch(&[s("ledger"), s("list"), s("--limit"), s("2")], &c).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(data_ids(&result).len(), 2);
        assert!(result.message().contains("showing 2 of 5"), "{}", result.message());
    }

    #[tokio::test]
    async fn ledger_list_since_excludes_older_asks() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let ctx = c.context_id.expect("test caller has a context");
        let now = kaijutsu_types::now_millis() as i64;
        let (old_ids, new_ids) = {
            let db = d.kernel_db.lock();
            let conn = db.conn_for_ledger();
            let old = seed_pending_asks(
                conn,
                ctx,
                c.principal_id,
                approval_ledger::types::Origin::ShellGate,
                "old",
                2,
                now - 5 * 3_600_000, // 5 hours ago
                1,
            );
            let new = seed_pending_asks(
                conn,
                ctx,
                c.principal_id,
                approval_ledger::types::Origin::ShellGate,
                "new",
                1,
                now - 60_000, // 1 minute ago
                1,
            );
            (old, new)
        };

        let result = d.dispatch(&[s("ledger"), s("list"), s("--since"), s("30m")], &c).await;
        assert!(result.is_ok(), "{result:?}");
        let ids = data_ids(&result);
        assert_eq!(ids, new_ids, "--since 30m must keep only the row inside the window");
        for old in &old_ids {
            assert!(!ids.contains(old), "an older row leaked through --since: {ids:?}");
        }
    }

    #[tokio::test]
    async fn ledger_list_bad_since_errors_loudly_instead_of_silently_matching_everything() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("ledger"), s("list"), s("--since"), s("5x")], &c).await;
        assert!(!result.is_ok(), "garbage --since must be refused, not treated as \"no filter\"");
        assert!(result.message().contains("--since"), "{}", result.message());
        // Must be refused as a bad DURATION, not as an unrecognized flag —
        // otherwise this assertion would pass today for the wrong reason
        // (clap's own "unexpected argument '--since'" also contains the
        // substring "--since").
        assert!(
            !result.message().contains("unexpected argument"),
            "must be a duration-format error, not clap not recognizing --since at all: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn ledger_list_origin_filter_narrows_to_the_requested_origin() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let ctx = c.context_id.expect("test caller has a context");
        let (hook_ids, shell_ids) = {
            let db = d.kernel_db.lock();
            let conn = db.conn_for_ledger();
            let hook = seed_pending_asks(conn, ctx, c.principal_id, approval_ledger::types::Origin::Hook, "hook", 2, 1_000, 10);
            let shell =
                seed_pending_asks(conn, ctx, c.principal_id, approval_ledger::types::Origin::ShellGate, "shell", 1, 2_000, 10);
            (hook, shell)
        };

        let result = d.dispatch(&[s("ledger"), s("list"), s("--origin"), s("hook")], &c).await;
        assert!(result.is_ok(), "{result:?}");
        let ids = data_ids(&result);
        for h in &hook_ids {
            assert!(ids.contains(h), "{ids:?}");
        }
        for sg in &shell_ids {
            assert!(!ids.contains(sg), "--origin hook must exclude shell_gate rows: {ids:?}");
        }
    }

    /// `--status <bad-value>` must fail at clap parse time naming the valid
    /// choices, same guarantee `--remember` already gets.
    #[tokio::test]
    async fn ledger_list_invalid_status_fails_at_parse_time() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("ledger"), s("list"), s("--status"), s("bogus")], &c).await;
        assert!(!result.is_ok());
        assert!(result.message().contains("pending") && result.message().contains("allowed"), "{}", result.message());
    }

    /// Amy's ruling: `--status allowed` alone (no `--history`) must show
    /// allowed asks — the user should never have to pass both.
    #[tokio::test]
    async fn ledger_list_status_allowed_implies_history_without_the_flag() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let db = d.kernel_db.clone();
        let caller = c.clone();
        let flows = d.kernel.ledger_flows().clone();
        let gate = tokio::spawn(async move { run_gate(&db, &caller, spec(), Duration::from_secs(30), &flows).await });
        let request_id = wait_for_pending(&d).await;
        let allow = d.dispatch(&[s("ledger"), s("allow"), s(&request_id)], &c).await;
        assert!(allow.is_ok(), "{allow:?}");
        let _ = gate.await;

        let result = d.dispatch(&[s("ledger"), s("list"), s("--status"), s("allowed")], &c).await;
        assert!(result.is_ok(), "{result:?}");
        assert!(
            data_ids(&result).contains(&request_id),
            "--status allowed alone must show the allowed ask without --history"
        );

        let pending = d.dispatch(&[s("ledger"), s("list")], &c).await;
        assert!(
            !data_ids(&pending).contains(&request_id),
            "a decided ask must not also show in the default pending queue"
        );
    }

    /// `kj ledger list`'s default excludes `claimed` on purpose (an
    /// answerer is already working it) — but `--status claimed` must still
    /// let someone see it deliberately.
    #[tokio::test]
    async fn ledger_list_status_claimed_shows_rows_hidden_by_default() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let db = d.kernel_db.clone();
        let caller = c.clone();
        let flows = d.kernel.ledger_flows().clone();
        let gate = tokio::spawn(async move { run_gate(&db, &caller, spec(), Duration::from_secs(30), &flows).await });
        let request_id = wait_for_pending(&d).await;
        {
            let db = d.kernel_db.lock();
            approval_ledger::claim::claim(db.conn_for_ledger(), &request_id, c.principal_id.as_bytes()).unwrap();
        }

        let default_list = d.dispatch(&[s("ledger"), s("list")], &c).await;
        assert!(
            !data_ids(&default_list).contains(&request_id),
            "claimed rows stay out of the default queue — an answerer already owns it"
        );

        let claimed_list = d.dispatch(&[s("ledger"), s("list"), s("--status"), s("claimed")], &c).await;
        assert!(claimed_list.is_ok(), "{claimed_list:?}");
        assert!(
            data_ids(&claimed_list).contains(&request_id),
            "--status claimed must surface it deliberately"
        );

        let _ = d.dispatch(&[s("ledger"), s("deny"), s(&request_id)], &c).await;
        let _ = gate.await;
    }

    #[tokio::test]
    async fn ledger_rules_limit_and_since_cap_and_report_truncation() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let ctx = c.context_id.expect("test caller has a context");
        let mut rule_ids = Vec::new();
        {
            let db = d.kernel_db.lock();
            let conn = db.conn_for_ledger();
            for i in 0..3 {
                let ask = approval_ledger::types::NewAsk {
                    context_id: ctx.as_bytes().to_vec(),
                    principal_id: c.principal_id.as_bytes().to_vec(),
                    origin: approval_ledger::types::Origin::ShellGate,
                    instance: None,
                    tool: None,
                    hook_id: None,
                    description: format!("rule-ask-{i}"),
                    statements: vec![approval_ledger::types::NewPlanStatement {
                        statement_digest: format!("digest-{i}"),
                        rendered: format!("echo {i}"),
                        statement_kind: "command".into(),
                        commands: vec![],
                        vars: vec![],
                    }],
                    authorized_label: Some(format!("label-{i}")),
                    rc_run_id: None,
                    expires_at: None,
                    options: vec![],
                    signals: vec![],
                };
                let request_id = approval_ledger::ask::create_ask(conn, &ask).unwrap();
                approval_ledger::decide::decide(
                    conn,
                    &request_id,
                    approval_ledger::decide::DecideInput { allow: true, ..Default::default() },
                )
                .unwrap();
                let rule = approval_ledger::rules::learn_from_approval(
                    conn,
                    &request_id,
                    0,
                    approval_ledger::types::RuleScope::Always,
                    true,
                    None,
                )
                .unwrap();
                conn.execute(
                    "UPDATE approval_rules SET created_at = ?1 WHERE rule_id = ?2",
                    rusqlite::params![1_000 + i as i64 * 10, rule.rule_id],
                )
                .unwrap();
                rule_ids.push(rule.rule_id);
            }
        }

        let result = d.dispatch(&[s("ledger"), s("rules"), s("--limit"), s("2")], &c).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(data_ids(&result).len(), 2);
        assert!(result.message().contains("showing 2 of 3"), "{}", result.message());
    }

    #[tokio::test]
    async fn ledger_runs_context_and_verb_filters_narrow_and_limit_truncates() {
        let d = test_dispatcher().await;
        let ctx_a = crate::kj::test_helpers::register_context(&d, Some("runs-ctx-a"), None, kaijutsu_types::PrincipalId::new());
        let ctx_b = crate::kj::test_helpers::register_context(&d, Some("runs-ctx-b"), None, kaijutsu_types::PrincipalId::new());
        let c = crate::kj::test_helpers::caller_with_context(ctx_a);

        let (run_a1, run_a2, run_b1) = {
            let db = d.kernel_db.lock();
            let conn = db.conn_for_ledger();
            let run_a1 = approval_ledger::rc_runs::start_run(conn, ctx_a.as_bytes(), "coder", "create").unwrap();
            conn.execute("UPDATE rc_runs SET started_at = ?1 WHERE run_id = ?2", rusqlite::params![1_000, run_a1])
                .unwrap();
            let run_a2 = approval_ledger::rc_runs::start_run(conn, ctx_a.as_bytes(), "coder", "fork").unwrap();
            conn.execute("UPDATE rc_runs SET started_at = ?1 WHERE run_id = ?2", rusqlite::params![2_000, run_a2])
                .unwrap();
            let run_b1 = approval_ledger::rc_runs::start_run(conn, ctx_b.as_bytes(), "coder", "create").unwrap();
            conn.execute("UPDATE rc_runs SET started_at = ?1 WHERE run_id = ?2", rusqlite::params![3_000, run_b1])
                .unwrap();
            (run_a1, run_a2, run_b1)
        };

        let by_context = d.dispatch(&[s("ledger"), s("runs"), s("--context"), s(&ctx_a.to_string())], &c).await;
        assert!(by_context.is_ok(), "{by_context:?}");
        let ids = data_ids(&by_context);
        assert!(ids.contains(&run_a1) && ids.contains(&run_a2), "{ids:?}");
        assert!(!ids.contains(&run_b1), "--context must exclude the other context's run: {ids:?}");

        let by_verb = d.dispatch(&[s("ledger"), s("runs"), s("--verb"), s("fork")], &c).await;
        assert!(by_verb.is_ok(), "{by_verb:?}");
        assert_eq!(data_ids(&by_verb), vec![run_a2.clone()], "--verb fork must narrow to just the fork run");

        let limited = d.dispatch(&[s("ledger"), s("runs"), s("--limit"), s("1")], &c).await;
        assert!(limited.is_ok(), "{limited:?}");
        assert!(limited.message().contains("showing 1 of 3"), "{}", limited.message());

        let bad_verb = d.dispatch(&[s("ledger"), s("runs"), s("--verb"), s("nonsense")], &c).await;
        assert!(!bad_verb.is_ok(), "an unwired verb must be refused at parse time, not silently accepted");
    }

    /// AGENTS.md "Writing style" → "Published text": help is a product
    /// surface. The old wording claimed the default queue shows asks
    /// still "`pending` or `claimed`" — the query is `status = 'pending'`
    /// only (`ask.rs`'s `list_pending` doc explains why: a claimed ask
    /// already has an answerer working it). This pins the fix.
    #[test]
    fn published_list_help_does_not_claim_claimed_asks_show_by_default() {
        use clap::CommandFactory;
        let cmd = LedgerArgs::command();
        let list_sub = cmd.find_subcommand("list").expect("list subcommand exists");
        let about = list_sub.get_about().map(|a| a.to_string()).unwrap_or_default();
        assert!(
            !about.to_lowercase().contains("or `claimed`") && !about.to_lowercase().contains("or claimed"),
            "the default queue does not include claimed asks; help must not claim it does: {about}"
        );
        assert!(about.contains("pending"), "{about}");
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


