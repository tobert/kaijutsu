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
//! create an `allow` rule for a statement with any free variable
//! (`docs/gate-and-shell-split.md`, "Rulings"); this module never
//! re-implements that check, only reports what the ledger said. Once
//! a rule exists, [`crate::kj::gate::run_gate`]'s gate policy step
//! (`kj/gate_policy.rs`, user-rule layer) auto-decides the next identical
//! ask without asking anyone —
//! `kj ledger forget` is how that stops.

use approval_ledger::error::LedgerError;
use approval_ledger::types::{ApprovalStatus, NewAsk, NewSignal, Origin, RuleScope, SignalSourceKind, SignalVerdict};
use clap::{Parser, Subcommand};
use kaijutsu_types::{ContentType, ContextId, PrincipalId};
use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::effect::{Classify, Effect};
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

fn record_ask_identity(
    span: &tracing::Span,
    requester: &[u8],
    actor: Option<&[u8]>,
    reviewer: Option<&[u8]>,
    context: &[u8],
) {
    if let Some(id) = PrincipalId::try_from_slice(requester) {
        span.record("principal.id", id.to_string());
    }
    if let Some(id) = actor.and_then(PrincipalId::try_from_slice) {
        span.record("actor.id", id.to_string());
    }
    if let Some(id) = reviewer.and_then(PrincipalId::try_from_slice) {
        span.record("reviewer.id", id.to_string());
    }
    if let Some(id) = ContextId::try_from_slice(context) {
        span.record("context.id", id.to_string());
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
/// fractional numbers, no combined units — one format, no ambiguity.
/// Garbage is rejected loudly — CLAUDE.md's stance against
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
/// `--limit` cut real rows off the end. A silently truncated list is the
/// quiet fallback CLAUDE.md treats as a defect, so the line only appears
/// when something was actually cut (`None`
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
    /// Show one ask in full: the statement being authorized, what an
    /// approval runs it with — source, working directory, recorded free
    /// variable values — the context and principal that raised it, and —
    /// once it is decided — whether its answer has already been spent.
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
        // "Digest-keyed allow-always: refuse on free variables".
        /// Remember this decision as a standing rule, so future identical
        /// asks decide without asking anyone. Refused when any covered
        /// statement has a free variable; the decision on THIS ask still
        /// stands either way. Off by default. Undo with `kj ledger forget`.
        #[arg(long)]
        remember: Option<RememberScopeArg>,
        /// With --remember: remember the command family instead of the
        /// exact text — `kj handoff note`, `git push` — so any future
        /// call of that family decides the same way, whatever its
        /// arguments. Refused when a command has a redirect, a background
        /// flag, a heredoc or a non-plain argument; the decision on THIS
        /// ask still stands.
        #[arg(long, requires = "remember")]
        family: bool,
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
        /// With --remember: deny the command family instead of the exact
        /// text, whatever the arguments. Refused, like a family allow, when
        /// a command has a redirect, a background flag, a heredoc or a
        /// non-plain argument; the decision on THIS ask still stands. Undo
        /// with `kj ledger forget`.
        #[arg(long, requires = "remember")]
        family: bool,
    },
    /// Withdraw a pending ask you performed or requested. Nothing runs.
    Cancel {
        /// The ask to withdraw.
        request_id: String,
    },
    /// Assign a pending ask to another live character for review.
    Escalate {
        /// The ask to reassign.
        request_id: String,
        /// The live character who will review the ask next.
        #[arg(long)]
        to: String,
    },
    /// List the gate rules in force for this context: standing rules a
    /// human taught (exact-text and family, newest first, capped at
    /// `--limit`, default 20), then the gate.toml tiers that apply to this
    /// context's type. Each names the layer that decides it.
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
    /// decided `allowed` in the same transaction — the advisory path: the
    /// ask records what a scorer read and never blocks on a human itself. With `--request-id <id>`,
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
        /// Which statement of the ask this signal judged (0-based, matching
        /// `approval_ask_statements.stmt_seq`) — the position that makes
        /// "which clause of a multi-statement ask was this?" answerable.
        /// Omit when the signal speaks to the whole ask rather than one
        /// statement within it.
        #[arg(long = "stmt-seq")]
        stmt_seq: Option<i64>,
        /// Which command within that statement this signal judged (0-based,
        /// matching `approval_statement_commands.cmd_seq`). Omit when the
        /// signal speaks to the whole statement rather than one command
        /// within it; only meaningful alongside `--stmt-seq`.
        #[arg(long = "cmd-seq")]
        cmd_seq: Option<i64>,
        /// The signal's own recommendation: `escalate` (raise or leave a
        /// prompt), `deny`, or `allow`. Always advisory — see the module
        /// doc: nothing in `approval_ledger` ever reads a signal to
        /// auto-decide an ask, `allow` included.
        #[arg(long, value_enum)]
        verdict: SignalVerdictArg,
        /// Create a NEW ask, already decided `Allowed`, carrying this
        /// signal — the advisory classifier path. Mutually exclusive with
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
    pub(crate) async fn dispatch_ledger(&self, argv: &[String], caller: &KjCaller) -> KjResult {
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
            LedgerCommand::Allow { request_id, remember, family } => {
                self.ledger_decide(&request_id, true, caller, remember, family)
            }
            LedgerCommand::Deny { request_id, remember, family } => {
                self.ledger_decide(&request_id, false, caller, remember, family)
            }
            LedgerCommand::Cancel { request_id } => self.ledger_cancel(&request_id, caller),
            LedgerCommand::Escalate { request_id, to } => self.ledger_escalate(&request_id, &to, caller),
            LedgerCommand::Rules { limit, since } => {
                self.ledger_rules(limit, since.as_deref(), caller).await
            }
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
                    stmt_seq,
                    cmd_seq,
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
                    stmt_seq,
                    cmd_seq,
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
    /// `--history` never needs to be passed alongside either. Without
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

    /// Render a stored id blob, saying so when it does not parse. A
    /// malformed id here is a defect in whatever wrote the row, and `-`
    /// would read as "absent" — which these columns never are.
    fn ledger_id_display(parsed: Option<String>, raw: &[u8]) -> String {
        parsed.unwrap_or_else(|| format!("<malformed {}-byte id>", raw.len()))
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
        let redeemed_at = match approval_ledger::ask::redeemed_at(conn, request_id) {
            Ok(at) => at,
            Err(e) => return KjResult::Err(format!("kj ledger show: {e}")),
        };
        // The free-variable values an approval runs with, recorded on the
        // ask at raise time (`docs/gate-shape-b.md`, "The ask carries its
        // free variables") — always loaded, not gated on `--signals`, since
        // this is what execution reads, not an advisory extra.
        let env_rows = match approval_ledger::ask::load_ask_env(conn, request_id) {
            Ok(e) => e,
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
        let principal_name = |raw: &[u8]| -> Result<String, KjResult> {
            let principal = PrincipalId::try_from_slice(raw)
                .ok_or_else(|| KjResult::Err(format!("kj ledger show: malformed {}-byte principal id", raw.len())))?;
            match db.get_character(principal) {
                Ok(Some(character)) => Ok(character.name),
                Ok(None) => Ok(principal.short()),
                Err(e) => Err(KjResult::Err(format!("kj ledger show: could not resolve principal name: {e}"))),
            }
        };
        let actor_name = match row.actor_id.as_deref().map(principal_name).transpose() {
            Ok(name) => name,
            Err(result) => return result,
        };
        let reviewer_name = match row.reviewer_id.as_deref().map(principal_name).transpose() {
            Ok(name) => name,
            Err(result) => return result,
        };
        let requester_name = match principal_name(&row.principal_id) {
            Ok(name) => name,
            Err(result) => return result,
        };
        let decided_by_name = match row.decided_by.as_deref().map(principal_name).transpose() {
            Ok(name) => name,
            Err(result) => return result,
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
            // `context` and `principal` are two thirds of what
            // `find_redeemable` matches on, and the only reason to print
            // them here: an answered ask that mints a second ask instead of
            // redeeming has one of them differing, and no other verb shows
            // either.
            format!("context:    {}", Self::ledger_id_display(ContextId::try_from_slice(&row.context_id).map(|c| c.to_string()), &row.context_id)),
            format!("principal:  {} ({requester_name})", Self::ledger_id_display(PrincipalId::try_from_slice(&row.principal_id).map(|p| p.to_string()), &row.principal_id)),
            format!("description: {}", row.description),
        ];
        for s in &statements {
            lines.push(format!("statement:  {}", s.statement.rendered));
        }
        // What an allow actually runs — the source, the directory, and the
        // free variables' recorded values. Omitted line by line when the
        // ask carries nothing for it (an advisory or non-executing ask has
        // no `exec_source`/`cwd`, and a statement with no free variable has
        // no env rows at all).
        if let Some(exec_source) = &row.exec_source {
            lines.push(format!("exec_source: {exec_source}"));
        }
        if let Some(cwd) = &row.cwd {
            lines.push(format!("cwd:        {cwd}"));
        }
        for e in &env_rows {
            match &e.value {
                Some(v) => lines.push(format!("env:        {}={v:?}", e.name)),
                None => lines.push(format!("env:        {} unset", e.name)),
            }
        }
        if let Some(decided) = &row.decided_option {
            lines.push(format!("decided:    {decided}"));
        }
        // Who answered, so a card closed from another surface can say so
        // (`docs/tui.md`, "Asks"). Absent on a pending or auto-decided ask.
        if let Some(by) = &row.decided_by {
            lines.push(format!("decided_by: {} ({})", Self::ledger_id_display(PrincipalId::try_from_slice(by).map(|p| p.to_string()), by), decided_by_name.as_deref().expect("decided identity has a name")));
        }
        if let Some(reason) = &row.auto_reason {
            lines.push(format!("auto:       {reason}"));
        }
        // Only a decided ask can be spent, so `redeemed: no` on a pending
        // one would state a fact about a question nobody has answered.
        if matches!(row.status, ApprovalStatus::Allowed | ApprovalStatus::Denied) {
            match redeemed_at {
                Some(at) => lines.push(format!("redeemed:   {at}")),
                None => lines.push("redeemed:   no — this answer is still redeemable".into()),
            }
        }
        if show_signals {
            for s in &signal_rows {
                // Clause position: `stmt#cmd` when both are known, `stmt`
                // alone when only the statement is, `-` when the signal
                // speaks to the whole ask (both nullable — see `NewSignal`).
                let clause = match (s.stmt_seq, s.cmd_seq) {
                    (Some(stmt), Some(cmd)) => format!("{stmt}#{cmd}"),
                    (Some(stmt), None) => stmt.to_string(),
                    (None, _) => "-".to_string(),
                };
                lines.push(format!(
                    "signal:     {} {} clause={} label={} score={} verdict={}",
                    s.source_kind,
                    s.source_id.as_deref().unwrap_or("-"),
                    clause,
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
            "principal_id": PrincipalId::try_from_slice(&row.principal_id).map(|p| p.to_string()),
            "principal_name": requester_name,
            "actor_id": row.actor_id.as_deref().and_then(PrincipalId::try_from_slice).map(|p| p.to_string()),
            "actor_name": actor_name,
            "reviewer_id": row.reviewer_id.as_deref().and_then(PrincipalId::try_from_slice).map(|p| p.to_string()),
            "reviewer_name": reviewer_name,
            "created_at": row.created_at,
            "decided_at": row.decided_at,
            "decided_by": row.decided_by.as_deref().and_then(PrincipalId::try_from_slice).map(|p| p.to_string()),
            "decided_by_name": decided_by_name,
            "decided_option": row.decided_option,
            "remember_scope": row.remember_scope,
            "redeemed_at": redeemed_at,
            "status": row.status.to_string(),
            "origin": row.origin.to_string(),
            "instance": row.instance,
            "tool": row.tool,
            "hook_id": row.hook_id,
            "description": row.description,
            "authorized_label": row.authorized_label,
            "statements": statements.iter().map(|s| s.statement.rendered.clone()).collect::<Vec<_>>(),
            "exec_source": row.exec_source,
            "cwd": row.cwd,
            "env": env_rows.iter().map(|e| serde_json::json!({
                "name": e.name,
                "value": e.value,
            })).collect::<Vec<_>>(),
        });
        if show_signals {
            data["signals"] = serde_json::Value::Array(signal_rows.iter().map(signal_row_json).collect());
        }
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    fn ledger_cancel(&self, request_id: &str, caller: &KjCaller) -> KjResult {
        let span = tracing::info_span!(
            "approval.cancel",
            ask.id = %request_id,
            decision.actor.id = %caller.actor_id,
            principal.id = tracing::field::Empty,
            actor.id = tracing::field::Empty,
            reviewer.id = tracing::field::Empty,
            context.id = tracing::field::Empty,
        );
        let _guard = span.enter();
        let result = {
            let db = self.kernel_db.lock();
            approval_ledger::decide::cancel(db.conn_for_ledger(), request_id, caller.actor_id.as_bytes())
        };
        match result {
            Ok(row) => {
                record_ask_identity(&span, &row.principal_id, row.actor_id.as_deref(), row.reviewer_id.as_deref(), &row.context_id);
                tracing::info!("approval ask cancelled");
                crate::kj::gate::announce_ledger_change(&self.kernel_db, self.kernel.ledger_flows());
                KjResult::ok_with_data(
                    format!("cancelled ask {request_id}; nothing ran"),
                    serde_json::json!({ "request_id": row.request_id, "status": row.status.to_string() }),
                )
            }
            Err(e) => KjResult::Err(format!("kj ledger: {e}")),
        }
    }

    fn ledger_escalate(&self, request_id: &str, to: &str, caller: &KjCaller) -> KjResult {
        let span = tracing::info_span!(
            "approval.escalate",
            ask.id = %request_id,
            decision.actor.id = %caller.actor_id,
            principal.id = tracing::field::Empty,
            actor.id = tracing::field::Empty,
            reviewer.id = tracing::field::Empty,
            context.id = tracing::field::Empty,
        );
        let _guard = span.enter();
        let result = {
            let db = self.kernel_db.lock();
            let target = match db.get_character_by_name(to) {
                Ok(Some(character)) if character.retired_at.is_none() => character.principal_id,
                Ok(Some(_)) => return KjResult::Err(format!("kj ledger: character '{to}' is retired")),
                Ok(None) => return KjResult::Err(format!("kj ledger: no character named '{to}'")),
                Err(e) => return KjResult::Err(format!("kj ledger: could not resolve character '{to}': {e}")),
            };
            approval_ledger::decide::escalate(
                db.conn_for_ledger(),
                request_id,
                caller.actor_id.as_bytes(),
                target.as_bytes(),
            )
        };
        match result {
            Ok(row) => {
                record_ask_identity(&span, &row.principal_id, row.actor_id.as_deref(), row.reviewer_id.as_deref(), &row.context_id);
                tracing::info!("approval ask escalated");
                crate::kj::gate::announce_ledger_change(&self.kernel_db, self.kernel.ledger_flows());
                KjResult::ok_with_data(
                    format!("escalated ask {request_id} to {to}"),
                    serde_json::json!({
                        "request_id": row.request_id,
                        "reviewer_id": row.reviewer_id.as_deref().and_then(PrincipalId::try_from_slice).map(|p| p.to_string()),
                        "reviewer_name": to,
                    }),
                )
            }
            Err(e) => KjResult::Err(format!("kj ledger: {e}")),
        }
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
        family: bool,
    ) -> KjResult {
        let verb = if allow { "allow" } else { "deny" };
        let span = tracing::info_span!(
            "approval.decision",
            ask.id = %request_id,
            decision.actor.id = %caller.actor_id,
            decision.verb = verb,
            principal.id = tracing::field::Empty,
            actor.id = tracing::field::Empty,
            reviewer.id = tracing::field::Empty,
            context.id = tracing::field::Empty,
        );
        let _guard = span.enter();

        // The ledger work is scoped so the `KernelDb` guard is RELEASED before
        // the notification below. `announce_ledger_change` takes the same
        // mutex to read the committed generation, and `parking_lot::Mutex` is
        // not reentrant — announcing while still holding it would deadlock the
        // answerer, which is a far worse failure than the silent hang this
        // whole slice set out to fix.
        let result = {
            let db = self.kernel_db.lock();
            let conn = db.conn_for_ledger();
            let principal = caller.actor_id.as_bytes();
            let context = caller.context_id.map(|c| c.as_bytes().to_vec());
            let answerer = approval_ledger::decide::Answerer {
                principal,
                context: context.as_deref(),
            };

            // An archived context is inert: it runs nothing and answers
            // nothing. Checked here, BEFORE the claim, for the same reason
            // self-approval is — nothing has committed yet, so this returns
            // without announcing, and no claim is left stranded on an ask
            // whose answer could never be acted on.
            //
            // This is the first of two checks. The second runs at execution
            // time, because a context can be archived in the gap between an
            // answer and the run it authorizes, and a check here alone would
            // not see that. `docs/gate-shape-b.md`, "Archived contexts are
            // inert".
            match approval_ledger::ask::get_approval(conn, request_id) {
                Ok(Some(row)) => {
                    record_ask_identity(&span, &row.principal_id, row.actor_id.as_deref(), row.reviewer_id.as_deref(), &row.context_id);
                    if matches!(remember, Some(RememberScopeArg::Session)) {
                        let ask_ctx = match ContextId::try_from_slice(&row.context_id) {
                            Some(id) => id,
                            None => return KjResult::Err(format!(
                                "kj ledger: cannot --remember session for ask {request_id}: it has no valid context ID"
                            )),
                        };
                        let ask_actor = match row
                            .actor_id
                            .as_deref()
                            .and_then(PrincipalId::try_from_slice)
                        {
                            Some(id) => id,
                            None => return KjResult::Err(format!(
                                "kj ledger: cannot --remember session for ask {request_id}: it has no performer identity"
                            )),
                        };
                        let requester = match PrincipalId::try_from_slice(&row.principal_id) {
                            Some(id) => id,
                            None => return KjResult::Err(format!(
                                "kj ledger: cannot --remember session for ask {request_id}: it has no valid requester identity"
                            )),
                        };
                        let current = match db.get_context(ask_ctx) {
                            Ok(Some(context)) => context,
                            Ok(None) => return KjResult::Err(format!(
                                "kj ledger: cannot --remember session for ask {request_id}: context {} is missing",
                                ask_ctx.short()
                            )),
                            Err(e) => return KjResult::Err(format!(
                                "kj ledger: cannot --remember session for ask {request_id}: read context {}: {e}",
                                ask_ctx.short()
                            )),
                        };
                        let performer = current.played_by.unwrap_or(requester);
                        if performer != ask_actor {
                            return KjResult::Err(format!(
                                "kj ledger: cannot --remember session for ask {request_id}: the context performer changed after this ask was raised"
                            ));
                        }
                    }

                    if let Some(ask_ctx) = ContextId::try_from_slice(&row.context_id) {
                        // A MISSING context row reads as not-archived here,
                        // and that is a narrow reading of the ruling rather
                        // than an oversight: `approvals.context_id` carries
                        // no foreign key on purpose, so an ask outliving its
                        // context is an expected state, and refusing every
                        // such ask would close the audit path along with the
                        // hazard. Widening this to "no live context" is a
                        // separate decision.
                        let archived = db
                            .get_context(ask_ctx)
                            .ok()
                            .flatten()
                            .is_some_and(|c| c.is_archived());
                        if archived {
                            return KjResult::Err(format!(
                                "kj ledger: ask {request_id} belongs to context {}, which is \
                                 archived — an archived context runs nothing, so answering \
                                 this would authorize work that can never happen. Unarchive \
                                 it first if the ask still matters.",
                                ask_ctx.short()
                            ));
                        }
                    }
                }
                Ok(None) => return KjResult::Err(format!("kj ledger: no such ask {request_id}")),
                Err(e) => return KjResult::Err(format!("kj ledger: {e}")),
            }

            // No self-approval, checked BEFORE the claim. `decide` enforces it
            // too, but a claim taken first would leave the ask `claimed` by the
            // one seat that may not answer it, locking out every seat that may.
            // Nothing has committed on this path, so it returns without
            // announcing.
            if let Err(e) =
                approval_ledger::decide::ensure_not_self_approval(conn, request_id, answerer)
            {
                return match e {
                    LedgerError::NotFound(_) => {
                        KjResult::Err(format!("kj ledger: no such ask {request_id}"))
                    }
                    e => KjResult::Err(format!("kj ledger: {e}")),
                };
            }

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
                    decided_by: Some(answerer),
                    decided_option: Some(if allow { "allow_once" } else { "deny" }),
                    remember_scope: remember.map(RememberScopeArg::as_str),
                    auto_reason: None,
                },
            ) {
                Ok(row) => {
                    tracing::info!(decision.status = %row.status, "approval ask decided");
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
                    if let Some(remember) = remember
                        && family
                    {
                        match learn_family_for_ask(
                            conn,
                            request_id,
                            remember.to_rule_scope(),
                            allow,
                            principal,
                        ) {
                            Ok(keys) => {
                                message.push_str(&format!(
                                    "; remembered as a standing {} family rule for {}",
                                    remember.as_str(),
                                    keys.join(", "),
                                ));
                                data["remembered"] = serde_json::json!({
                                    "scope": remember.as_str(),
                                    "family": keys,
                                });
                            }
                            Err(e) => {
                                message.push_str(&format!(
                                    "; NOT remembered: {e} — the {verb} on THIS ask still stands"
                                ));
                                data["remembered"] = serde_json::json!(false);
                                data["remember_error"] = serde_json::json!({ "message": e });
                            }
                        }
                    } else if let Some(remember) = remember {
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
                                // this arm — surface `LedgerError::FreeVariableRule`'s
                                // fields rather than flattening to `{e}`'s prose, so
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
    /// `kj ledger rules`: every rule with a verdict for the calling
    /// context, each naming the layer that decides it — the human-taught
    /// rules (exact text and family) first, newest first and capped by
    /// `--limit`, then the `gate.toml` tiers in force for the caller's
    /// context type. `.data` is the learned rule ids, the ones `forget`
    /// takes.
    async fn ledger_rules(&self, limit: u32, since: Option<&str>, caller: &KjCaller) -> KjResult {
        let since_ms = match since {
            Some(s) => match parse_since_duration_ms(s) {
                Ok(ms) => Some(since_cutoff_ms(ms)),
                Err(e) => return KjResult::Err(e),
            },
            None => None,
        };
        let filter = approval_ledger::rules::RuleListFilter { since_ms, limit: limit as i64 };
        let gate_config = crate::kj::gate_policy::load_config(self.kernel.vfs()).await;
        let (context_type, digest_rules, family_rules, total) = {
            let db = self.kernel_db.lock();
            let context_type = crate::kj::gate_policy::context_type_of(&db, caller.context_id);
            let conn = db.conn_for_ledger();
            let (digest, t1) = match approval_ledger::rules::list_rules_filtered(conn, &filter) {
                Ok(r) => r,
                Err(e) => return KjResult::Err(format!("kj ledger rules: {e}")),
            };
            let (family, t2) = match approval_ledger::rules::list_family_rules_filtered(conn, &filter) {
                Ok(r) => r,
                Err(e) => return KjResult::Err(format!("kj ledger rules: {e}")),
            };
            (context_type, digest, family, t1 + t2)
        };

        // One list, newest first, cut to `--limit` across both kinds.
        // (key, verdict, layer, created_at, rule_id)
        let mut learned: Vec<(String, &'static str, String, i64, String)> = digest_rules
            .iter()
            .map(|r| {
                (
                    r.authorized_label.clone(),
                    if r.allow { "allow" } else { "deny" },
                    format!("user rule ({}, rule {})", r.scope.as_str(), r.rule_id),
                    r.created_at,
                    r.rule_id.clone(),
                )
            })
            .chain(family_rules.iter().map(|r| {
                (
                    r.family_key.clone(),
                    if r.allow { "allow" } else { "deny" },
                    format!("user family rule ({}, rule {})", r.scope.as_str(), r.rule_id),
                    r.created_at,
                    r.rule_id.clone(),
                )
            }))
            .collect();
        learned.sort_by(|a, b| b.3.cmp(&a.3));
        learned.truncate(limit as usize);
        let data = serde_json::Value::Array(
            learned.iter().map(|r| serde_json::json!(r.4.clone())).collect(),
        );

        let mut lines: Vec<String> = Vec::new();
        if learned.is_empty() {
            lines.push("(no active rules)".to_string());
        } else {
            lines.push(format!("  {:<40}  {:<7}  {}", "KEY", "VERDICT", "LAYER"));
            for (key, verdict, layer, _, _) in &learned {
                lines.push(format!("  {:<40}  {:<7}  {layer}", truncate_key(key), verdict));
            }
        }
        lines.push(String::new());
        if let Some(notice) = truncation_notice(learned.len(), total, "") {
            lines.push(notice);
            lines.push(String::new());
        }
        match &gate_config {
            Ok(config) => {
                let entries = config.entries_for(context_type.as_deref());
                if entries.is_empty() {
                    lines.push(format!(
                        "gate.toml: no tiers apply to this context (type {})",
                        context_type.as_deref().unwrap_or("unknown")
                    ));
                } else {
                    lines.push(format!("  {:<40}  {:<7}  {}", "GATE.TOML KEY", "VERDICT", "LAYER"));
                    for (layer, key, verdict) in entries {
                        lines.push(format!("  {:<40}  {:<7}  {layer}", truncate_key(&key), verdict));
                    }
                }
            }
            Err(e) => lines.push(format!("gate.toml: {e}")),
        }
        lines.push(String::new());
        lines.push(
            "builtin: every kj verb declaring Read, kj ledger, and kj … --help are allowed beneath these"
                .to_string(),
        );
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
        // `script_count` is NULL for a run that predates this field, or
        // that failed before its script list was ever loaded — omit the
        // line rather than print a count that was never recorded.
        if let Some(expected) = run.script_count {
            lines.push(format!("script_count: {expected}"));
        }
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
        // The point of `script_count`: fewer recorded rows than intended
        // means the run was cut short (dropped mid-lifecycle, `RunGuard`'s
        // `Drop` backstop stamped it `failed`) — not that a script itself
        // failed. Say so explicitly rather than leaving a reader to guess
        // from a gap in `SEQ`.
        if let Some(expected) = run.script_count {
            let recorded = scripts.len() as i64;
            if recorded < expected {
                lines.push(String::new());
                lines.push(format!(
                    "only {recorded} of {expected} scripts ran — the run stopped before the rest \
                     started (cancelled or aborted, not a script failure)"
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
            "script_count": run.script_count,
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
    /// error needs to name BOTH the missing choice and why (advisory vs.
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
        stmt_seq: Option<i64>,
        cmd_seq: Option<i64>,
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
                    "kj ledger signal add: give --auto-allow (create a new advisory ask) or \
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
            stmt_seq,
            cmd_seq,
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
            // "advisory", not "log-only": this ask records what a classifier
            // read, and the caller may still escalate on it. Saying log-only
            // told a human the call had been waved through, which stopped
            // being true when S50 gained its escalate mode.
            let auto_reason = format!(
                "{} (advisory)",
                source_id.as_deref().unwrap_or("classifier")
            );
            let ask = NewAsk {
                context_id: context_id.as_bytes().to_vec(),
                actor_id: caller.actor_id.as_bytes().to_vec(),
                reviewer_id: caller.reviewer_id.unwrap_or(caller.actor_id).as_bytes().to_vec(),
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
                // An auto-allowed advisory record, not a question anyone
                // answers, so there is nothing to run later and no
                // directory to run it in.
                cwd: None,
                exec_source: None,
                env: vec![],
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

/// Generalize an ask into family rules — one per command in the program it
/// carries — returning the keys learned. A family covers a key and never
/// arguments, so the program is re-planned from the ask's `exec_source`
/// and every command must be structurally plain
/// (`gate_policy::family_keys_for_program` names the condition otherwise).
/// An ask with no shell program (a `kj`-verb ask) has nothing to learn a
/// family from.
fn learn_family_for_ask(
    conn: &Connection,
    request_id: &str,
    scope: RuleScope,
    allow: bool,
    created_by: &[u8],
) -> Result<Vec<String>, String> {
    let row = approval_ledger::ask::get_approval(conn, request_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no such ask {request_id}"))?;
    let Some(source) = row.exec_source.as_deref() else {
        return Err(
            "this ask carries no shell program to learn a family from; --remember without \
             --family remembers its exact statement"
                .to_string(),
        );
    };
    let planned = kaish_kernel::plan_program(source).map_err(|errors| {
        let msg = errors.iter().map(|e| e.format(source)).collect::<Vec<_>>().join("\n");
        format!("the ask's program does not plan: {msg}")
    })?;
    let keys = crate::kj::gate_policy::family_keys_for_program(&planned)
        .map_err(|e| format!("a family rule covers a key, never arguments: {e}"))?;
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    // One transaction for the whole family set, as `learn_every_statement`
    // does: a fault mid-way leaves no half-learned family behind a message
    // that says nothing was remembered.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).map_err(|e| e.to_string())?;
    approval_ledger::rules::learn_family_from_approval(&tx, request_id, &refs, scope, allow, Some(created_by))
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(keys)
}

/// A key or label on one listing line.
fn truncate_key(s: &str) -> String {
    const LIMIT: usize = 40;
    if s.chars().count() <= LIMIT {
        return s.replace('\n', "⏎");
    }
    let head: String = s.chars().take(LIMIT - 1).collect();
    format!("{}…", head.replace('\n', "⏎"))
}

// Verb class: kj/effect.rs
//
// `kj ledger` stays out of readonly.rs's tables entirely — the whole verb is
// exempt there as the gate's own answer path (`is_gate_exempt_kj`), a
// different rule from read-only. Classified here on each leaf's own merits:
// `list`/`show`/`rules`/`runs` read the ledger, nothing else does.
impl Classify for LedgerArgs {
    fn effect(&self) -> Effect {
        self.command.effect()
    }
}

impl Classify for LedgerCommand {
    fn effect(&self) -> Effect {
        match self {
            LedgerCommand::List { .. }
            | LedgerCommand::Show { .. }
            | LedgerCommand::Rules { .. }
            | LedgerCommand::Runs { .. } => Effect::Read,
            LedgerCommand::Allow { .. }
            | LedgerCommand::Deny { .. }
            | LedgerCommand::Cancel { .. }
            | LedgerCommand::Escalate { .. }
            | LedgerCommand::Forget { .. } => {
                Effect::Write
            }
            LedgerCommand::Signal { command } => command.effect(),
        }
    }
}

impl Classify for SignalCommand {
    fn effect(&self) -> Effect {
        match self {
            SignalCommand::Add { .. } => Effect::Write,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::gate::{run_gate, GateSpec};
    use crate::kj::test_helpers::{register_context, test_caller, test_dispatcher};
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
            exec_source: None,
            planned: Vec::new(),
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
            exec_source: None,
            planned: Vec::new(),
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

    /// Run the gate once. It never blocks, so an escalation returns
    /// `Pending` immediately and the caller comes back after answering
    /// (`docs/gate-resume.md`). Most tests below call this twice: once to
    /// raise the ask, once after a human answers to redeem it.
    async fn gate_once(d: &crate::kj::KjDispatcher, c: &KjCaller, spec: GateSpec) -> crate::kj::gate::GateOutcome {
        run_gate(&d.kernel_db.clone(), c, spec, d.kernel.ledger_flows(), &crate::kj::gate_policy::no_config()).await
    }

    #[tokio::test]
    async fn ledger_list_is_empty_then_shows_a_gated_ask() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let result = d.dispatch(&[s("ledger"), s("list")], &c).await;
        assert!(result.is_ok());
        assert!(result.message().contains("no pending approvals"));

        // The gate escalates and returns immediately, leaving a pending
        // ask; it must appear in the list.
        let first = gate_once(&d, &c, spec()).await;
        assert_eq!(first.verdict, crate::kj::gate::GateVerdict::Pending);

        let listed = d.dispatch(&[s("ledger"), s("list")], &c).await;
        assert!(listed.is_ok(), "{listed:?}");
        let listed = listed.message().to_string();
        assert!(listed.contains("test ask"), "list must show the ask: {listed}");
        assert!(listed.contains("kj ledger allow"));

        // Deny it through the verb; the next attempt must observe the
        // refusal.
        let request_id = listed
            .lines()
            .find(|l| l.contains("test ask"))
            .and_then(|l| l.split_whitespace().next())
            .expect("request id is the first column")
            .to_string();
        let result = d
            .dispatch(&[s("ledger"), s("deny"), s(&request_id)], &answering_seat())
            .await;
        assert!(result.is_ok(), "deny must succeed: {result:?}");

        let second = gate_once(&d, &c, spec()).await;
        assert!(!second.allowed());
        // A human said no. That IS a verdict — distinct from the gate
        // being unable to reach one.
        assert_eq!(second.verdict, crate::kj::gate::GateVerdict::Denied);
        assert_eq!(second.ask.expect("a denied ask has a row").request_id, request_id);
    }

    /// An archived context's ask cannot be answered. Refused BEFORE the
    /// claim, so nothing is left claimed by an answer that could never be
    /// acted on.
    ///
    /// This is the first of two checks; `run_gate` refuses again at
    /// execution time, because a context can be archived in the gap between
    /// an answer and the run it authorizes. Neither covers the other.
    ///
    /// Falsified by deleting the archived check from the answer path: the
    /// allow succeeds on a context that will never run anything.
    #[tokio::test]
    async fn an_archived_context_s_ask_cannot_be_answered() {
        let d = test_dispatcher().await;
        // A REGISTERED context, because the check reads the context row —
        // `test_caller` mints an id that was never stored.
        let ctx_id = crate::kj::test_helpers::register_context(
            &d,
            Some("arch-answer"),
            None,
            kaijutsu_types::PrincipalId::new(),
        );
        let mut c = test_caller();
        c.context_id = Some(ctx_id);

        let first = gate_once(&d, &c, spec()).await;
        let request_id = first.ask.expect("an escalated ask has a row").request_id;

        {
            let db = d.kernel_db.lock();
            assert!(db.archive_context(ctx_id).expect("archive"), "was live before");
        }

        let refused = d
            .dispatch(&[s("ledger"), s("allow"), s(&request_id)], &answering_seat())
            .await;
        assert!(!refused.is_ok(), "an archived context's ask must not be answerable");
        assert!(
            refused.message().contains("archived"),
            "the refusal must say why, got: {}",
            refused.message()
        );
    }

    /// A second seat, used wherever a test answers a gate it raised itself.
    /// No self-approval: an ask may not be answered from the context that
    /// raised it (`docs/gate-and-shell-split.md`, "No self-approval — the
    /// gate's own answer path"). `test_caller` mints a fresh `ContextId`, so
    /// this is a peer, and peer-seat approval is permitted.
    fn answering_seat() -> crate::kj::KjCaller {
        let mut caller = test_caller();
        caller.actor_id = crate::kj::test_helpers::test_reviewer_principal();
        caller
    }

    /// No self-approval, at the surface a person actually types. The seat
    /// that tripped the gate is refused; the ask stays answerable by anyone
    /// else, and the attempt is on the record.
    ///
    /// Falsified by answering with `&answering_seat()` instead of `&c`: the
    /// allow succeeds and every assertion below trips.
    #[tokio::test]
    async fn ledger_allow_refuses_the_seat_that_raised_the_ask() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let first = gate_once(&d, &c, spec()).await;
        let request_id = first.ask.expect("an escalated ask has a row").request_id;

        let refused = d
            .dispatch(&[s("ledger"), s("allow"), s(&request_id)], &c)
            .await;
        assert!(!refused.is_ok(), "a seat must not answer its own ask");
        assert!(
            refused.message().contains("the answering actor performed this operation"),
            "the refusal must say why, got: {}",
            refused.message()
        );

        {
            let db = d.kernel_db.lock();
            let conn = db.conn_for_ledger();
            // Not claimed by the refused answerer: a burnt claim would lock
            // out the seats that MAY answer.
            let row = approval_ledger::ask::get_approval(conn, &request_id).unwrap().unwrap();
            assert_eq!(row.status, approval_ledger::types::ApprovalStatus::Pending);
            assert!(row.claimed_by.is_none());

            let refusals = approval_ledger::ask::list_refusals(conn, &request_id).unwrap();
            assert_eq!(refusals.len(), 1, "the refused attempt must be on the record");
            assert_eq!(refusals[0].reason, "self_approval");
        }

        // Peer-seat approval is permitted, so the ask is still answerable.
        let allowed = d
            .dispatch(&[s("ledger"), s("allow"), s(&request_id)], &answering_seat())
            .await;
        assert!(allowed.is_ok(), "a peer seat must still be able to answer: {allowed:?}");
    }

    #[tokio::test]
    async fn ledger_allow_opens_the_gate_and_a_second_answer_is_loud() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let first = gate_once(&d, &c, spec()).await;
        assert_eq!(first.verdict, crate::kj::gate::GateVerdict::Pending);
        let request_id = first.ask.expect("an escalated ask has a row").request_id;

        let result = d
            .dispatch(&[s("ledger"), s("allow"), s(&request_id)], &answering_seat())
            .await;
        assert!(result.is_ok(), "allow must succeed: {result:?}");
        assert!(result.message().starts_with("allowed ask"));

        // Answering again is a loud error naming the terminal status, never
        // a silent no-op (guarantee 6).
        let again = d
            .dispatch(&[s("ledger"), s("allow"), s(&request_id)], &answering_seat())
            .await;
        assert!(!again.is_ok());
        assert!(again.message().contains("not `pending`") || again.message().contains("already"));

        // The answer already given is redeemed by the next attempt.
        let second = gate_once(&d, &c, spec()).await;
        assert!(second.allowed());
        assert_eq!(second.ask.expect("a redeemed ask has a row").request_id, request_id);
    }

    #[tokio::test]
    async fn ledger_show_renders_the_statement_being_authorized() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let db = d.kernel_db.clone();
        let caller = c.clone();
        let flows = d.kernel.ledger_flows().clone();
        let gate = tokio::spawn(async move {
            run_gate(&db, &caller, spec(), &flows, &crate::kj::gate_policy::no_config()).await
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
        d.dispatch(&[s("ledger"), s("deny"), s(&request_id)], &answering_seat())
            .await;
        let _ = gate.await;
    }

    /// `show` reports the two identity fields redemption turns on, and
    /// whether the answer has been spent. `find_redeemable` matches on
    /// `principal_id` and `context_id`; until this landed, neither was
    /// printed anywhere, so an answered ask that minted a SECOND ask
    /// instead of redeeming looked identical to one nobody had answered.
    ///
    /// The three-phase walk is the point: pending (no redemption line at
    /// all — nothing has been answered), answered-and-unspent, and spent.
    ///
    /// Falsified by dropping the `principal:` line, and separately by
    /// making the spent phase's line unconditional — the pending assertion
    /// then trips. Reverted afterward.
    #[tokio::test]
    async fn ledger_show_reports_the_principal_and_whether_the_answer_is_spent() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let first = gate_once(&d, &c, spec()).await;
        let request_id = first.ask.expect("an escalated ask has a row").request_id;

        let show = |id: String| {
            let d = &d;
            let c = &c;
            async move { d.dispatch(&[s("ledger"), s("show"), s(&id)], c).await }
        };

        let pending = show(request_id.clone()).await;
        assert!(pending.is_ok(), "{pending:?}");
        let msg = pending.message().to_string();
        assert!(
            msg.contains(&format!("principal:  {}", c.principal_id)),
            "show must name the principal that raised the ask: {msg}"
        );
        assert!(
            msg.contains(&format!("context:    {}", c.context_id.expect("the test caller has a context"))),
            "show must name the context the ask belongs to: {msg}"
        );
        assert!(
            !msg.contains("redeemed:"),
            "a pending ask has no answer to spend, so it must claim nothing about redemption: {msg}"
        );
        let pending_data = match &pending {
            KjResult::Ok { data: Some(d), .. } => d.clone(),
            other => panic!("kj ledger show must emit structured data: {other:?}"),
        };
        assert!(pending_data["decided_by"].is_null(), "nobody has decided a pending ask: {pending_data}");
        assert!(pending_data["decided_at"].is_null(), "{pending_data}");
        assert!(
            pending_data["created_at"].as_i64().is_some_and(|at| at > 0),
            "created_at is a unix-epoch millisecond stamp: {pending_data}"
        );

        let answerer = answering_seat();
        let allowed = d
            .dispatch(&[s("ledger"), s("allow"), s(&request_id)], &answerer)
            .await;
        assert!(allowed.is_ok(), "{allowed:?}");

        let unspent = show(request_id.clone()).await;
        assert!(
            unspent.message().contains("redeemed:   no"),
            "an answered ask nobody has collected must say so: {}",
            unspent.message()
        );
        let data = match &unspent {
            KjResult::Ok { data: Some(d), .. } => d.clone(),
            other => panic!("kj ledger show must emit structured data: {other:?}"),
        };
        assert_eq!(
            data["principal_id"].as_str(),
            Some(c.principal_id.to_string().as_str()),
            "{data}"
        );
        assert!(data["redeemed_at"].is_null(), "an unspent answer has no redemption stamp: {data}");
        // The answer names its answerer — a client whose card was answered
        // from another surface reads this to say who (`docs/tui.md`, "Asks").
        assert_eq!(
            data["decided_by"].as_str(),
            Some(answerer.actor_id.to_string().as_str()),
            "{data}"
        );
        assert_eq!(data["decided_option"].as_str(), Some("allow_once"), "{data}");
        assert!(data["decided_at"].as_i64().is_some_and(|at| at > 0), "{data}");
        assert!(
            unspent.message().contains(&format!("decided_by: {}", answerer.actor_id)),
            "show must name who decided: {}",
            unspent.message()
        );

        // The retry collects the answer; that is what makes it spent.
        let second = gate_once(&d, &c, spec()).await;
        assert!(second.allowed(), "the stored answer must authorize the retry");

        let spent = show(request_id).await;
        let msg = spent.message().to_string();
        assert!(
            msg.contains("redeemed:") && !msg.contains("redeemed:   no"),
            "a collected answer must report its redemption stamp: {msg}"
        );
        let data = match &spent {
            KjResult::Ok { data: Some(d), .. } => d.clone(),
            other => panic!("kj ledger show must emit structured data: {other:?}"),
        };
        assert!(
            data["redeemed_at"].as_i64().is_some_and(|at| at > 0),
            "redeemed_at is a unix-epoch millisecond stamp: {data}"
        );
    }

    /// `show` renders what an approval NOW RUNS WITH — the source, the
    /// directory it runs in, and the free-variable values recorded at ask
    /// time — not only the statement text. Until this landed, none of the
    /// three appeared in `kj ledger show`, so a human deciding a shell ask
    /// could not see what execution would actually do.
    ///
    /// Falsified by leaving out any of the three render blocks, or by
    /// leaving `exec_source`/`cwd`/`env` out of `.data`.
    #[tokio::test]
    async fn ledger_show_reports_what_an_approval_runs_with() {
        let d = test_dispatcher().await;
        let ctx_id = crate::kj::test_helpers::register_context(
            &d,
            Some("show-exec-source"),
            None,
            PrincipalId::new(),
        );
        d.kernel_db.lock().set_context_env(ctx_id, "FOO", "asked").unwrap();
        d.kernel_db
            .lock()
            .upsert_context_shell(&crate::kernel_db::ContextShellRow {
                context_id: ctx_id,
                cwd: Some("/work/dir".into()),
                updated_at: 0,
            })
            .unwrap();
        let mut c = test_caller();
        c.context_id = Some(ctx_id);

        let spec = crate::kj::shell_gate::build_shell_gate_spec("echo ${FOO} ${BAR}").unwrap();
        let first = gate_once(&d, &c, spec).await;
        let request_id = first.ask.expect("an escalated ask has a row").request_id;

        let result = d
            .dispatch(&[s("ledger"), s("show"), s(&request_id)], &c)
            .await;
        assert!(result.is_ok(), "{result:?}");
        let msg = result.message().to_string();
        assert!(msg.contains("exec_source: echo ${FOO} ${BAR}"), "{msg}");
        assert!(msg.contains("cwd:        /work/dir"), "{msg}");
        assert!(msg.contains("env:        FOO=\"asked\""), "{msg}");
        assert!(msg.contains("env:        BAR unset"), "{msg}");

        let data = match &result {
            KjResult::Ok { data: Some(d), .. } => d.clone(),
            other => panic!("kj ledger show must emit structured data: {other:?}"),
        };
        assert_eq!(data["exec_source"].as_str(), Some("echo ${FOO} ${BAR}"), "{data}");
        assert_eq!(data["cwd"].as_str(), Some("/work/dir"), "{data}");
        // Rows come in the ask's recorded order, which follows kaish's
        // free-variable listing rather than the source; look up by name.
        let env = data["env"].as_array().expect("env is an array");
        assert_eq!(env.len(), 2, "{data}");
        let by_name = |name: &str| {
            env.iter()
                .find(|e| e["name"] == name)
                .unwrap_or_else(|| panic!("env row {name} missing: {data}"))
        };
        assert_eq!(by_name("FOO")["value"], "asked");
        assert!(by_name("BAR")["value"].is_null());

        // Clean up the pending ask so the spawned gate terminates.
        d.dispatch(&[s("ledger"), s("deny"), s(&request_id)], &answering_seat())
            .await;
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

        // A distinct label per verb, or the `deny` iteration's ask would
        // collide with the `allow` iteration's (same digest + label) and get
        // REDEEMED instead of freshly created — each iteration wants its own
        // ask to decide.
        fn spec_for(label: &str) -> GateSpec {
            GateSpec { authorized_label: label.to_string(), ..spec() }
        }

        for (verb, expected) in [("allow", "allowed ask"), ("deny", "denied ask")] {
            let first = gate_once(&d, &c, spec_for(verb)).await;
            assert_eq!(first.verdict, crate::kj::gate::GateVerdict::Pending);
            let request_id = first.ask.expect("an escalated ask has a row").request_id;

            let result = d.dispatch(&[s("ledger"), s(verb), s(&request_id)], &answering_seat()).await;
            assert!(result.is_ok(), "{verb} must succeed: {result:?}");
            assert!(
                result.message().starts_with(expected),
                "`kj ledger {verb}` must report \"{expected}\", got: {}",
                result.message()
            );
        }
    }

    #[tokio::test]
    async fn ledger_of_an_unknown_id_errors_loudly() {
        let d = test_dispatcher().await;
        let result = d
            .dispatch(&[s("ledger"), s("allow"), s("deadbeef")], &answering_seat())
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

    /// `--stmt-seq`/`--cmd-seq` are the clause position (the escalation-seat
    /// gap `docs/issues.md` names) — round-trip through storage AND show up
    /// in `kj ledger show --signals`'s text, not just its `.data`.
    #[tokio::test]
    async fn ledger_signal_add_stmt_and_cmd_seq_round_trip_through_storage_and_show() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let created = d
            .dispatch(
                &signal_add_argv(
                    "rm build artifacts now",
                    &["--auto-allow", "--stmt-seq", "0", "--cmd-seq", "2"],
                ),
                &c,
            )
            .await;
        assert!(created.is_ok(), "{created:?}");
        let request_id = match &created {
            KjResult::Ok { data: Some(v), .. } => v["request_id"].as_str().unwrap().to_string(),
            other => panic!("{other:?}"),
        };

        {
            let db = d.kernel_db.lock();
            let signals = approval_ledger::ask::list_signals(db.conn_for_ledger(), &request_id).unwrap();
            assert_eq!(signals.len(), 1);
            assert_eq!(signals[0].stmt_seq, Some(0), "{signals:?}");
            assert_eq!(signals[0].cmd_seq, Some(2), "{signals:?}");
        }

        let shown = d.dispatch(&[s("ledger"), s("show"), s(&request_id), s("--signals")], &c).await;
        assert!(shown.is_ok(), "{shown:?}");
        assert!(
            shown.message().contains("clause=0#2"),
            "kj ledger show --signals must render the clause position: {}",
            shown.message()
        );
        let data = match &shown {
            KjResult::Ok { data: Some(v), .. } => v.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(data["signals"][0]["stmt_seq"], serde_json::json!(0), "{data}");
        assert_eq!(data["signals"][0]["cmd_seq"], serde_json::json!(2), "{data}");
    }

    /// Both fields stay optional: a signal with no clause position must
    /// still render, showing `-` rather than a silently-omitted line or an
    /// invented `0`.
    #[tokio::test]
    async fn ledger_signal_add_without_stmt_seq_shows_clause_as_dash() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let created = d.dispatch(&signal_add_argv("rm build artifacts now", &["--auto-allow"]), &c).await;
        let request_id = match &created {
            KjResult::Ok { data: Some(v), .. } => v["request_id"].as_str().unwrap().to_string(),
            other => panic!("{other:?}"),
        };

        {
            let db = d.kernel_db.lock();
            let signals = approval_ledger::ask::list_signals(db.conn_for_ledger(), &request_id).unwrap();
            assert_eq!(
                signals[0].stmt_seq, None,
                "a signal with no --stmt-seq must stay NULL, not invent 0: {signals:?}"
            );
        }

        let shown = d.dispatch(&[s("ledger"), s("show"), s(&request_id), s("--signals")], &c).await;
        assert!(shown.is_ok(), "{shown:?}");
        assert!(
            shown.message().contains("clause=-"),
            "an unset clause position must render as '-': {}",
            shown.message()
        );
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
            run_gate(&db, &caller, spec(), &flows, &crate::kj::gate_policy::no_config()).await
        });
        let request_id = wait_for_pending(&d).await;

        let deny = d
            .dispatch(&[s("ledger"), s("deny"), s(&request_id)], &answering_seat())
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

        // A distinct label per decision, or the second ask would collide
        // with the first's (same digest + label) and get REDEEMED instead
        // of freshly created — this test wants two independent decided
        // asks.
        fn spec_for(label: &str) -> GateSpec {
            GateSpec { authorized_label: label.to_string(), ..spec() }
        }

        async fn decide_one(d: &crate::kj::KjDispatcher, c: &KjCaller, label: &str, allow: bool) -> String {
            let first = gate_once(d, c, spec_for(label)).await;
            assert_eq!(first.verdict, crate::kj::gate::GateVerdict::Pending);
            let request_id = first.ask.expect("an escalated ask has a row").request_id;
            let verb = if allow { "allow" } else { "deny" };
            let result = d.dispatch(&[s("ledger"), s(verb), s(&request_id)], &answering_seat()).await;
            assert!(result.is_ok(), "{verb} must succeed: {result:?}");
            request_id
        }

        let first = decide_one(&d, &c, "first-target", false).await;
        // Force a distinct `created_at` millisecond so "newest first" is
        // asserting real ordering, not a coincidence.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let second = decide_one(&d, &c, "second-target", true).await;

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

        let first = gate_once(&d, &c, shell_spec(label, rendered)).await;
        assert_eq!(first.verdict, crate::kj::gate::GateVerdict::Pending);
        let request_id = first.ask.expect("an escalated ask has a row").request_id;

        let result = d
            .dispatch(&[s("ledger"), s("allow"), s(&request_id), s("--remember"), s("always")], &answering_seat())
            .await;
        assert!(result.is_ok(), "allow --remember must succeed: {result:?}");
        assert!(
            result.message().contains("remembered"),
            "message must say what was remembered: {}",
            result.message()
        );

        // The next attempt at the same request redeems what just happened —
        // here, since `--remember` just taught a rule, it is the rule
        // matching rather than the specific ask being redeemed, but either
        // way the answered ask must come back allowed.
        let second = gate_once(&d, &c, shell_spec(label, rendered)).await;
        assert!(second.allowed(), "the answered ask itself must still be allowed");

        // A rule now exists. Fire another IDENTICAL, wholly unattended ask —
        // nobody calls `kj ledger allow` this time — and it must auto-allow.
        let third = gate_once(&d, &c, shell_spec(label, rendered)).await;

        assert!(
            third.allowed(),
            "a remembered allow rule must auto-allow the next identical ask: {third:?}"
        );
        assert_eq!(third.verdict, crate::kj::gate::GateVerdict::Allowed);
        assert!(
            third.reason.contains("gate policy: user rule allows"),
            "the reason must name the user-rule layer, not a human: {}",
            third.reason
        );
        let ask3 = third.ask.expect("an auto-decided ask still gets a durable row");
        assert_eq!(ask3.status, kaijutsu_types::AskStatus::Allowed);
    }

    fn planned_shell_spec(source: &str) -> GateSpec {
        crate::kj::shell_gate::build_shell_gate_spec(source).expect("test source plans")
    }

    /// `--family`: the next ask of the same family auto-allows whatever
    /// its arguments — the note text differs, so the label differs, and no
    /// `LabelMismatch` fires (guarantee 4 does not apply to a family). The
    /// same family with a redirect stays uncovered (the structural veto).
    ///
    /// Falsified by dropping the family layer from `gate_policy::evaluate`
    /// (the second note comes back `Pending`).
    #[tokio::test]
    async fn remembering_a_family_allows_the_next_ask_of_that_family_whatever_its_arguments() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let first = gate_once(&d, &c, planned_shell_spec("kj handoff note 'first'")).await;
        assert_eq!(first.verdict, crate::kj::gate::GateVerdict::Pending);
        let request_id = first.ask.expect("an escalated ask has a row").request_id;

        let result = d
            .dispatch(
                &[s("ledger"), s("allow"), s(&request_id), s("--remember"), s("always"), s("--family")],
                &answering_seat(),
            )
            .await;
        assert!(result.is_ok(), "allow --remember --family must succeed: {result:?}");
        assert!(
            result.message().contains("family rule for kj handoff note"),
            "{}",
            result.message()
        );

        let second = gate_once(&d, &c, planned_shell_spec("kj handoff note 'a completely different note'")).await;
        assert_eq!(second.verdict, crate::kj::gate::GateVerdict::Allowed, "{}", second.reason);
        assert!(
            second.reason.contains("user family rule allows kj handoff note"),
            "{}",
            second.reason
        );

        // Guarantee 3 does not apply either: a free variable in the value slot.
        let free = gate_once(&d, &c, planned_shell_spec("kj handoff note ${NOTE}")).await;
        assert_eq!(free.verdict, crate::kj::gate::GateVerdict::Allowed, "{}", free.reason);

        // The structural veto: a redirect is not `handoff note`.
        let redirected = gate_once(&d, &c, planned_shell_spec("kj handoff note 'x' > ~/.bashrc")).await;
        assert_eq!(redirected.verdict, crate::kj::gate::GateVerdict::Pending, "{}", redirected.reason);

        // Listed with its layer, and forgettable like any rule.
        let rules = d.dispatch(&[s("ledger"), s("rules")], &c).await;
        assert!(rules.is_ok(), "{rules:?}");
        assert!(
            rules.message().contains("user family rule") && rules.message().contains("kj handoff note"),
            "{}",
            rules.message()
        );
        let rule_id = match &rules {
            KjResult::Ok { data: Some(v), .. } => {
                let ids = v.as_array().unwrap();
                assert_eq!(ids.len(), 1, "{ids:?}");
                ids[0].as_str().unwrap().to_string()
            }
            other => panic!("rules must carry the rule ids: {other:?}"),
        };
        let forgotten = d.dispatch(&[s("ledger"), s("forget"), s(&rule_id)], &c).await;
        assert!(forgotten.is_ok(), "{forgotten:?}");
        let after = gate_once(&d, &c, planned_shell_spec("kj handoff note 'third'")).await;
        assert_eq!(after.verdict, crate::kj::gate::GateVerdict::Pending, "a forgotten family must not keep allowing");
    }

    /// A family DENY outranks a config allow on the same key and fires
    /// regardless of structure.
    #[tokio::test]
    async fn a_remembered_family_deny_refuses_the_next_ask_of_that_family() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let first = gate_once(&d, &c, planned_shell_spec("git push origin main")).await;
        let request_id = first.ask.expect("row").request_id;
        let result = d
            .dispatch(
                &[s("ledger"), s("deny"), s(&request_id), s("--remember"), s("always"), s("--family")],
                &answering_seat(),
            )
            .await;
        assert!(result.is_ok(), "{result:?}");

        let config = Ok(crate::kj::gate_policy::GateConfig::parse("[global]\nallow = [\"git push\"]\n").unwrap());
        let next = run_gate(
            &d.kernel_db.clone(),
            &c,
            planned_shell_spec("git push other branch > /tmp/log"),
            d.kernel.ledger_flows(),
            &config,
        )
        .await;
        assert_eq!(next.verdict, crate::kj::gate::GateVerdict::Denied, "{}", next.reason);
        assert!(next.reason.contains("user family rule denies git push"), "{}", next.reason);
    }

    /// A family is refused on structure, naming the condition, while the
    /// decision on the ask itself stands.
    #[tokio::test]
    async fn a_family_is_refused_when_the_ask_s_program_has_a_redirect_and_the_decision_stands() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let first = gate_once(&d, &c, planned_shell_spec("kj handoff note 'x' > /tmp/out")).await;
        let request_id = first.ask.expect("row").request_id;
        let result = d
            .dispatch(
                &[s("ledger"), s("allow"), s(&request_id), s("--remember"), s("always"), s("--family")],
                &answering_seat(),
            )
            .await;
        assert!(result.is_ok(), "the decision itself succeeds: {result:?}");
        assert!(
            result.message().contains("NOT remembered") && result.message().contains("has a redirect"),
            "{}",
            result.message()
        );
        let redeemed = gate_once(&d, &c, planned_shell_spec("kj handoff note 'x' > /tmp/out")).await;
        assert!(redeemed.allowed(), "the answered ask is still redeemed once: {}", redeemed.reason);
        let again = gate_once(&d, &c, planned_shell_spec("kj handoff note 'x' > /tmp/out")).await;
        assert_eq!(again.verdict, crate::kj::gate::GateVerdict::Pending, "no rule was learned");
    }

    #[tokio::test]
    async fn family_needs_remember() {
        let d = test_dispatcher().await;
        let result = d.dispatch(&[s("ledger"), s("allow"), s("01a0-x"), s("--family")], &test_caller()).await;
        assert!(!result.is_ok());
        assert!(result.message().contains("--remember"), "{}", result.message());
    }

    /// The composed view names the gate.toml tiers in force for the caller
    /// beneath the learned rules.
    #[tokio::test]
    async fn ledger_rules_lists_the_config_tiers_for_the_calling_context() {
        // The rc-shaped dispatcher mounts /config/kernel with the seeded
        // gate.toml; the plain one has no config tree at all.
        let d = crate::kj::test_helpers::test_dispatcher_rc().await;
        let result = d.dispatch(&[s("ledger"), s("rules")], &test_caller()).await;
        assert!(result.is_ok(), "{result:?}");
        let text = result.message();
        assert!(text.contains("no active rules"), "{text}");
        assert!(text.contains("kj handoff note") && text.contains("global config"), "{text}");
    }

    /// Learning a family spends the answer it was learned from, exactly as
    /// a digest rule does: after `forget`, the next IDENTICAL ask must
    /// escalate rather than redeem the human's original answer.
    ///
    /// Falsified by dropping the `approval_redemptions` insert from
    /// `learn_family_from_approval`: the identical ask comes back allowed.
    #[tokio::test]
    async fn forgetting_a_family_makes_the_identical_ask_escalate_again() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let source = "kj handoff note 'the same note'";
        let first = gate_once(&d, &c, planned_shell_spec(source)).await;
        let request_id = first.ask.expect("row").request_id;
        let result = d
            .dispatch(
                &[s("ledger"), s("allow"), s(&request_id), s("--remember"), s("always"), s("--family")],
                &answering_seat(),
            )
            .await;
        assert!(result.is_ok(), "{result:?}");
        let rules = d.dispatch(&[s("ledger"), s("rules")], &c).await;
        let rule_id = match &rules {
            KjResult::Ok { data: Some(v), .. } => v.as_array().unwrap()[0].as_str().unwrap().to_string(),
            other => panic!("{other:?}"),
        };
        assert!(d.dispatch(&[s("ledger"), s("forget"), s(&rule_id)], &c).await.is_ok());
        let again = gate_once(&d, &c, planned_shell_spec(source)).await;
        assert_eq!(
            again.verdict,
            crate::kj::gate::GateVerdict::Pending,
            "the original answer was spent by learning the family: {}",
            again.reason
        );
    }

    /// Ruling 1: a human's family allow outranks a config deny on its key.
    #[tokio::test]
    async fn a_remembered_family_allow_outranks_a_config_deny() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let first = gate_once(&d, &c, planned_shell_spec("rg -n todo src")).await;
        let request_id = first.ask.expect("row").request_id;
        let result = d
            .dispatch(
                &[s("ledger"), s("allow"), s(&request_id), s("--remember"), s("always"), s("--family")],
                &answering_seat(),
            )
            .await;
        assert!(result.is_ok(), "{result:?}");
        let config = Ok(crate::kj::gate_policy::GateConfig::parse("[global]\ndeny = [\"rg\"]\n").unwrap());
        let next = run_gate(
            &d.kernel_db.clone(),
            &c,
            planned_shell_spec("rg -n fixme src"),
            d.kernel.ledger_flows(),
            &config,
        )
        .await;
        assert_eq!(next.verdict, crate::kj::gate::GateVerdict::Allowed, "{}", next.reason);
        assert!(next.reason.contains("user family rule allows rg"), "{}", next.reason);
    }

    /// A session-scoped family applies to the context that raised the ask
    /// and to no other.
    #[tokio::test]
    async fn a_session_scoped_family_covers_its_own_context_only() {
        let d = test_dispatcher().await;
        let mut mine = test_caller();
        let mine_context = register_context(&d, Some("session-family-mine"), None, mine.principal_id);
        mine.context_id = Some(mine_context);
        let mut theirs = test_caller();
        let theirs_context = register_context(&d, Some("session-family-theirs"), None, theirs.principal_id);
        theirs.context_id = Some(theirs_context);
        let first = gate_once(&d, &mine, planned_shell_spec("kj handoff note 'mine'")).await;
        let request_id = first.ask.expect("row").request_id;
        let result = d
            .dispatch(
                &[s("ledger"), s("allow"), s(&request_id), s("--remember"), s("session"), s("--family")],
                &answering_seat(),
            )
            .await;
        assert!(result.is_ok(), "{result:?}");
        let same = gate_once(&d, &mine, planned_shell_spec("kj handoff note 'mine again'")).await;
        assert_eq!(same.verdict, crate::kj::gate::GateVerdict::Allowed, "{}", same.reason);
        let other = gate_once(&d, &theirs, planned_shell_spec("kj handoff note 'theirs'")).await;
        assert_eq!(other.verdict, crate::kj::gate::GateVerdict::Pending, "{}", other.reason);
    }

    /// A kj-verb ask carries no shell program, so `--family` is refused
    /// with the digest fallback named, and the decision still stands.
    #[tokio::test]
    async fn a_kj_verb_ask_cannot_teach_a_family() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let first = gate_once(&d, &c, spec()).await;
        let request_id = first.ask.expect("row").request_id;
        let result = d
            .dispatch(
                &[s("ledger"), s("allow"), s(&request_id), s("--remember"), s("always"), s("--family")],
                &answering_seat(),
            )
            .await;
        assert!(result.is_ok(), "the decision itself succeeds: {result:?}");
        assert!(
            result.message().contains("NOT remembered")
                && result.message().contains("no shell program"),
            "{}",
            result.message()
        );
    }

    /// A `kj cc send`-shaped ask has a free `MESSAGE` variable, so
    /// `--remember` must refuse to create a rule for it — while the
    /// decision on THIS ask still goes through exactly as if `--remember`
    /// had never been passed.
    #[tokio::test]
    async fn remembering_an_allow_is_refused_when_a_statement_has_a_free_variable() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let first = gate_once(&d, &c, spec()).await;
        assert_eq!(first.verdict, crate::kj::gate::GateVerdict::Pending);
        let request_id = first.ask.expect("an escalated ask has a row").request_id;

        let result = d
            .dispatch(&[s("ledger"), s("allow"), s(&request_id), s("--remember"), s("always")], &answering_seat())
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

        // No rule was learned (refused above), so this redeems the answer
        // on THIS ask, not a rule shortcut — the same request id comes back.
        let second = gate_once(&d, &c, spec()).await;
        assert!(second.allowed(), "the ask itself was allowed, independent of the refused rule");
        assert_eq!(second.ask.expect("a redeemed ask has a row").request_id, request_id);

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
        let mut c = test_caller();
        let context = register_context(&d, Some("session-rule"), None, c.principal_id);
        c.context_id = Some(context);
        let label = "kaish-source";
        let rendered = "echo session-scoped";

        let first = gate_once(&d, &c, shell_spec(label, rendered)).await;
        assert_eq!(first.verdict, crate::kj::gate::GateVerdict::Pending);
        let request_id = first.ask.expect("an escalated ask has a row").request_id;
        let result = d
            .dispatch(&[s("ledger"), s("allow"), s(&request_id), s("--remember"), s("session")], &answering_seat())
            .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(result.message().contains("session"), "{}", result.message());

        let second = gate_once(&d, &c, shell_spec(label, rendered)).await;
        assert!(second.allowed());

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

    /// Session rules attach to a performer, not merely a requester and a
    /// context. A decision about coder A must not teach a rule that coder B
    /// inherits after taking over the same context.
    #[tokio::test]
    async fn a_changed_performer_cannot_remember_an_old_ask_for_the_session() {
        let d = test_dispatcher().await;
        let mut coder_a = test_caller();
        let requester = coder_a.principal_id;
        let reviewer = crate::kj::test_helpers::test_reviewer_principal();
        let context = register_context(&d, Some("session-rule-performer"), None, requester);
        let actor_a = PrincipalId::new();
        let actor_b = PrincipalId::new();
        coder_a.actor_id = actor_a;
        coder_a.reviewer_id = Some(reviewer);
        coder_a.context_id = Some(context);
        d.kernel_db
            .lock()
            .update_context_review(context, Some(actor_a), Some(reviewer))
            .unwrap();

        let first = gate_once(&d, &coder_a, shell_spec("performer-swap", "echo reviewed")).await;
        let request_id = first.ask.expect("the first ask").request_id;
        d.kernel_db
            .lock()
            .update_context_review(context, Some(actor_b), Some(reviewer))
            .unwrap();

        let reviewer_call = answering_seat();
        let refused = d
            .dispatch(
                &[s("ledger"), s("allow"), s(&request_id), s("--remember"), s("session")],
                &reviewer_call,
            )
            .await;
        assert!(!refused.is_ok(), "a changed performer cannot inherit a session rule");
        assert!(refused.message().contains("performer changed"), "{}", refused.message());
        {
            let db = d.kernel_db.lock();
            let row = approval_ledger::ask::get_approval(db.conn_for_ledger(), &request_id)
                .unwrap()
                .expect("the old ask remains pending");
            assert_eq!(row.status, ApprovalStatus::Pending);
            assert!(row.claimed_by.is_none());
        }

        let plain = d
            .dispatch(&[s("ledger"), s("allow"), s(&request_id)], &reviewer_call)
            .await;
        assert!(plain.is_ok(), "the historical decision itself remains auditable: {plain:?}");
        let replay = gate_once(&d, &coder_a, shell_spec("performer-swap", "echo reviewed")).await;
        assert!(replay.allowed(), "the old ask itself remains redeemable");
        assert_eq!(replay.ask.expect("the old answer").request_id, request_id);

        let replacement = gate_once(&d, &coder_a, shell_spec("performer-swap", "echo reviewed")).await;
        let replacement_id = replacement.ask.expect("a replacement ask").request_id;
        let family_refused = d
            .dispatch(
                &[
                    s("ledger"), s("allow"), s(&replacement_id), s("--remember"), s("session"),
                    s("--family"),
                ],
                &reviewer_call,
            )
            .await;
        assert!(!family_refused.is_ok(), "family session learning has the same performer guard");
        assert!(family_refused.message().contains("performer changed"), "{}", family_refused.message());

        let db = d.kernel_db.lock();
        let rules: i64 = db
            .conn_for_ledger()
            .query_row("SELECT COUNT(*) FROM approval_rules", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rules, 0, "neither old ask may mint a session rule for actor B");
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

        let first = gate_once(&d, &c, shell_spec(label, rendered)).await;
        assert_eq!(first.verdict, crate::kj::gate::GateVerdict::Pending);
        let request_id = first.ask.expect("an escalated ask has a row").request_id;
        d.dispatch(&[s("ledger"), s("allow"), s(&request_id), s("--remember"), s("always")], &answering_seat())
            .await;
        let second = gate_once(&d, &c, shell_spec(label, rendered)).await;
        assert!(second.allowed());

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

        // The next identical ask must escalate again — no rule left to
        // auto-allow it, and no prior answer left to redeem, so it lands
        // back on a human exactly like a first-time ask: `Pending`, not a
        // fault and not an allow.
        let outcome = gate_once(&d, &c, shell_spec(label, rendered)).await;
        assert!(!outcome.allowed(), "a forgotten rule must not keep auto-allowing: {outcome:?}");
        assert_eq!(
            outcome.verdict,
            crate::kj::gate::GateVerdict::Pending,
            "a forgotten rule sends the next ask back to a human waiting, not a ledger fault"
        );
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
        let result = d
            .dispatch(&[s("ledger"), s("allow"), s("deadbeef"), s("--remember"), s("forever")], &answering_seat())
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
            actor_id: kaijutsu_types::PrincipalId::new(),
            reviewer_id: None,
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
        install_rc_script_file(&d, "/config/rc/ledgertest/create/S00-noop.kai", "true").await;

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
        install_rc_script_file(&d, "/config/rc/ledgertest2/create/S00-hello.kai", "echo hi").await;

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

    /// `script_count` set and every intended script actually recorded:
    /// the header carries the count, but no "cut short" line — a matching
    /// count is not itself a signal of anything wrong.
    #[tokio::test]
    async fn ledger_runs_show_with_matching_script_count_has_no_short_run_line() {
        use crate::kj::test_helpers::install_rc_script_file;

        let d = test_dispatcher().await;
        let c = unjoined_caller();
        install_rc_script_file(&d, "/config/rc/ledgertest3/create/S00-hello.kai", "echo hi").await;

        let created = d
            .dispatch(
                &[s("context"), s("create"), s("ledger-run-count-ok"), s("--type"), s("ledgertest3")],
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
        // The count comes from the lifecycle itself, not from the test.
        assert!(shown.message().contains("script_count: 1"), "{}", shown.message());
        assert!(!shown.message().contains("scripts ran"), "{}", shown.message());
        let obj = match &shown {
            KjResult::Ok { data: Some(v), .. } => v.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(obj["script_count"], serde_json::json!(1));
    }

    /// The whole point of `script_count`: fewer recorded script rows than
    /// intended must render an explicit line naming the gap, distinguishing
    /// a run cancelled part-way from a run where a script actually failed.
    #[tokio::test]
    async fn ledger_runs_show_with_short_script_count_names_the_gap() {
        use approval_ledger::{rc_runs, types::RcOutcome};

        let d = test_dispatcher().await;
        let c = unjoined_caller();

        // A run that stopped part-way cannot be produced by a lifecycle that
        // finishes, so drive the same writer calls the lifecycle makes and
        // simply stop early: three scripts intended, one recorded.
        let ctx = kaijutsu_types::ContextId::new();
        let run_id = {
            let db = d.kernel_db.lock();
            let conn = db.conn_for_ledger();
            let run_id = rc_runs::start_run(conn, ctx.as_bytes(), "ledgertest4", "create").unwrap();
            rc_runs::set_run_script_count(conn, &run_id, 3).unwrap();
            let sha = rc_runs::insert_script_body(conn, "echo hi").unwrap();
            rc_runs::record_run_script(
                conn,
                &run_id,
                "/config/rc/ledgertest4/create/S00-hello.kai",
                &sha,
                Some(0),
                1,
                Some(2),
            )
            .unwrap();
            rc_runs::finish_run(conn, &run_id, RcOutcome::Failed).unwrap();
            run_id
        };

        let shown = d.dispatch(&[s("ledger"), s("runs"), s(&run_id)], &c).await;
        assert!(shown.is_ok(), "{shown:?}");
        assert!(shown.message().contains("1 of 3 scripts ran"), "{}", shown.message());
        let obj = match &shown {
            KjResult::Ok { data: Some(v), .. } => v.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(obj["script_count"], serde_json::json!(3));
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
    // The pending queue is capped too (default 20), and truncation is
    // reported loudly when rows are cut. These tests seed rows DIRECTLY through
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
                    actor_id: principal.as_bytes().to_vec(),
                    reviewer_id: principal.as_bytes().to_vec(),
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
                    cwd: None,
                    exec_source: None,
                    env: vec![],
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
            "a truncated listing must say so loudly: {}",
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

    /// `--status allowed` alone (no `--history`) must show
    /// allowed asks — the user should never have to pass both.
    #[tokio::test]
    async fn ledger_list_status_allowed_implies_history_without_the_flag() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let db = d.kernel_db.clone();
        let caller = c.clone();
        let flows = d.kernel.ledger_flows().clone();
        let gate = tokio::spawn(async move { run_gate(&db, &caller, spec(), &flows, &crate::kj::gate_policy::no_config()).await });
        let request_id = wait_for_pending(&d).await;
        let allow = d.dispatch(&[s("ledger"), s("allow"), s(&request_id)], &answering_seat()).await;
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
        let gate = tokio::spawn(async move { run_gate(&db, &caller, spec(), &flows, &crate::kj::gate_policy::no_config()).await });
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

        let _ = d.dispatch(&[s("ledger"), s("deny"), s(&request_id)], &answering_seat()).await;
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
                    actor_id: c.actor_id.as_bytes().to_vec(),
                    reviewer_id: c.reviewer_id.unwrap_or(c.actor_id).as_bytes().to_vec(),
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
                    cwd: None,
                    exec_source: None,
                    env: vec![],
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
                actor_id: kaijutsu_types::PrincipalId::new(),
                reviewer_id: None,
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
