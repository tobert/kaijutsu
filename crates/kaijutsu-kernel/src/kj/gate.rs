//! The approval-ledger gate for `kj` verbs with external effects.
//!
//! First consumer: `kj cc send` — injecting a turn into a Claude Code
//! session is exactly the "agent action a human should authorize" case the
//! ledger exists for (Amy, 2026-08-16: *"yeah kj cc send should go through
//! the ledger"*). The shape here is the template gate slice 1a
//! (`docs/issues.md`) will extend to the destructive verbs.
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
//!    via `kj approve`) or `gate_wait_timeout` elapses. Elapse calls
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
//! From any shell: `kj approve list`, `kj approve allow <id>` /
//! `kj approve deny <id>` (see [`crate::kj::approve`]). The ledger's claim
//! race (guarantee 5) makes concurrent answers safe: exactly one answerer
//! wins, losers read a loud `AlreadyDecided`/claim failure, never a silent
//! no-op.

use std::sync::Arc;
use std::time::{Duration, Instant};

use approval_ledger::types::{
    ApprovalStatus, AskVerdict, NewAsk, NewOption, NewPlanCommand, NewPlanStatement, NewPlanVar,
    NewPlannedValue, Origin, VarBinding,
};

use crate::kernel_db::KernelDb;
use crate::kj::KjCaller;

/// How a gate wait ended.
#[derive(Debug, Clone)]
pub(crate) struct GateOutcome {
    pub allowed: bool,
    pub request_id: String,
    pub status: ApprovalStatus,
    /// Human-readable reason, always populated — on refusal it says exactly
    /// why (denied by whom, expired after how long, which rule fired).
    pub reason: String,
}

/// One gated action, described as a ledger statement.
///
/// `rendered` is the statement text with `${VAR}` placeholders; `vars`
/// names each placeholder and whether it is free or bound. The ledger
/// refuses to learn ALLOW rules for statements with any free variable
/// (guarantee 3) — which is precisely why [`crate::kj::cc`] marks the
/// message body free: every send stays human-approved until the policy
/// changes deliberately.
pub(crate) struct GateSpec {
    /// Ledger `tool` column, e.g. `"cc.send"`.
    pub tool: &'static str,
    /// Human-readable summary shown to whoever answers.
    pub description: String,
    pub rendered: String,
    /// The RAW typed reference (research-pass finding #3) — never a resolved
    /// label or id.
    pub authorized_label: String,
    pub vars: Vec<(String, VarBinding)>,
}

/// Poll interval for the wait loop. Sub-second keeps `kj approve` answers
/// feeling instant without hammering the (sub-ms, mutex-guarded) DB read.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Statement digest: the ledger keys statements by an opaque
/// content-address string (`approval_statements.statement_digest`), so the
/// requirement is uniqueness per distinct statement, not a specific hash.
/// A labeled digest of the rendered text gives exactly that, stays stable
/// across kernels, and reads for free in a `sqlite3` session — no new
/// crypto dependency for one gate.
fn statement_digest(rendered: &str) -> String {
    format!("kj-verb:v1:{}", rendered)
}

fn caller_context_bytes(caller: &KjCaller) -> Vec<u8> {
    caller
        .context_id
        .map(|c| c.as_bytes().to_vec())
        .unwrap_or_default()
}

fn build_ask(caller: &KjCaller, spec: &GateSpec) -> NewAsk {
    let digest = statement_digest(&spec.rendered);
    NewAsk {
        context_id: caller_context_bytes(caller),
        principal_id: caller.principal_id.as_bytes().to_vec(),
        origin: Origin::KjVerb,
        instance: Some("builtin.kj".into()),
        tool: Some(spec.tool.into()),
        hook_id: None,
        description: spec.description.clone(),
        statements: vec![NewPlanStatement {
            statement_digest: digest,
            rendered: spec.rendered.clone(),
            statement_kind: "kj_verb".into(),
            commands: vec![NewPlanCommand {
                name: spec.tool.into(),
                args: vec![NewPlannedValue::Plain(spec.rendered.clone())],
                redirects: vec![],
                backgrounded: false,
            }],
            vars: spec
                .vars
                .iter()
                .map(|(name, binding)| NewPlanVar {
                    name: name.clone(),
                    binding: *binding,
                })
                .collect(),
        }],
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

/// Run the gate: rules → durable ask → wait for a decision or expire.
///
/// `wait` must be the SAME `gate_wait_timeout` the caller's patient hold is
/// using (module docs) — the dispatcher passes
/// `kernel.timeouts().gate_wait_timeout` for both.
pub(crate) async fn run_gate(
    db: &Arc<parking_lot::Mutex<KernelDb>>,
    caller: &KjCaller,
    spec: GateSpec,
    wait: Duration,
) -> GateOutcome {
    let digest = statement_digest(&spec.rendered);
    let context = caller_context_bytes(caller);
    let principal = caller.principal_id.as_bytes().to_vec();

    // 1. Rules first — an explicit DENY rule short-circuits without asking
    //    anyone; a full ALLOW coverage auto-allows. Both leave a durable row.
    let verdict = {
        let db = db.lock();
        match approval_ledger::rules::redeem(
            db.conn_for_ledger(),
            &[digest.as_str()],
            &spec.authorized_label,
            Some(context.as_slice()),
            Some(principal.as_slice()),
        ) {
            Ok(coverage) => coverage.verdict(),
            Err(e) => {
                return GateOutcome {
                    allowed: false,
                    request_id: String::new(),
                    status: ApprovalStatus::Denied,
                    reason: format!("approval gate failed to check rules: {e} (fail-closed)"),
                };
            }
        }
    };

    let ask = build_ask(caller, &spec);

    // 2. Durable before asked — the row exists before any waiting starts.
    let request_id = {
        let db = db.lock();
        match approval_ledger::ask::create_ask(db.conn_for_ledger(), &ask) {
            Ok(id) => id,
            Err(e) => {
                return GateOutcome {
                    allowed: false,
                    request_id: String::new(),
                    status: ApprovalStatus::Denied,
                    reason: format!("approval gate failed to record the ask: {e} (fail-closed)"),
                };
            }
        }
    };

    // An auto decision still gets its durable row, decided immediately with
    // no human in the loop (`decided_by` None + `auto_reason` mark it).
    if matches!(verdict, AskVerdict::Allow | AskVerdict::Deny) {
        let allow = matches!(verdict, AskVerdict::Allow);
        let auto_reason = if allow {
            "rule coverage: every statement matched an active allow rule"
        } else {
            "rule coverage: a statement matched an active deny rule"
        };
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
        return match row {
            Ok(row) => GateOutcome {
                allowed: row.status.is_allowed(),
                request_id,
                status: row.status,
                reason: auto_reason.to_string(),
            },
            Err(e) => GateOutcome {
                allowed: false,
                request_id,
                status: ApprovalStatus::Denied,
                reason: format!("approval gate failed to record the rule decision: {e}"),
            },
        };
    }

    // 3. Escalate: wait for a human answer (`kj approve`) until terminal or
    //    the shared budget elapses.
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
                            "refused: ask {request_id} ended `{}` — answer it with `kj approve \
                             allow {request_id}` before the gate expires next time",
                            row.status
                        )
                    };
                    return GateOutcome {
                        allowed,
                        request_id,
                        status: row.status,
                        reason,
                    };
                }
                Ok(_) => {} // still pending/claimed — keep waiting
                Err(e) => {
                    return GateOutcome {
                        allowed: false,
                        request_id,
                        status: ApprovalStatus::Denied,
                        reason: format!("approval gate lost its ask row: {e} (fail-closed)"),
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
            let reason = format!(
                "no human answered ask {request_id} within {}s — expired, fail-closed \
                 (answer faster next time, or raise gate_wait_timeout)",
                wait.as_secs()
            );
            return GateOutcome {
                allowed: false,
                request_id,
                status,
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
            tool: "cc.send",
            description: format!("send a cross-session message to CC session {target:?}"),
            rendered: "kj cc send ${TARGET} ${MESSAGE}".into(),
            authorized_label: target.to_string(),
            vars: vec![
                ("TARGET".into(), VarBinding::Bound),
                ("MESSAGE".into(), VarBinding::Free),
            ],
        }
    }

    async fn short_gate_dispatcher() -> crate::kj::KjDispatcher {
        let mut policy = TimeoutPolicy::default();
        policy.gate_wait_timeout = Duration::from_millis(400);
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
        )
        .await;

        assert!(!outcome.allowed, "an unanswered gate must refuse");
        assert_eq!(outcome.status, ApprovalStatus::Expired);
        assert!(outcome.reason.contains("no human answered"));
        assert!(start.elapsed() >= Duration::from_millis(400));
        assert!(!outcome.request_id.is_empty());

        // Guarantee 1, verified from the outside: the row is on disk,
        // terminal, and carries the RAW typed label (finding #3), not a
        // resolved one.
        let db = d.kernel_db.clone();
        let db = db.lock();
        let row = approval_ledger::ask::get_approval(db.conn_for_ledger(), &outcome.request_id)
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
        let gate = tokio::spawn(async move {
            run_gate(
                &gate_db,
                &caller,
                cc_spec("kaijutsu-chan"),
                Duration::from_secs(10),
            )
            .await
        });

        // ...then answer it from a DIFFERENT principal, the way a human in
        // another shell would (`kj approve allow`).
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
        assert!(outcome.allowed, "an allow answer must open the gate");
        assert_eq!(outcome.status, ApprovalStatus::Allowed);
        assert_eq!(outcome.request_id, request_id);
    }

    #[tokio::test]
    async fn a_deny_answer_refuses_loudly() {
        let d = short_gate_dispatcher().await;
        let db = d.kernel_db.clone();
        let caller = test_caller();

        let gate_db = db.clone();
        let gate = tokio::spawn(async move {
            run_gate(
                &gate_db,
                &caller,
                cc_spec("fleet-lead"),
                Duration::from_secs(10),
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
        assert!(!outcome.allowed);
        assert_eq!(outcome.status, ApprovalStatus::Denied);
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
        let gate = tokio::spawn(async move {
            run_gate(
                &gate_db,
                &caller,
                cc_spec("kaijutsu-chan"),
                Duration::from_secs(10),
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
        assert!(gate.await.unwrap().allowed);
    }
}
