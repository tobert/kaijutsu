//! The gate policy evaluator: one function composes every layer of gate
//! rules into a per-statement verdict, and both gate pinch points consult
//! it — broker PreCall ([`evaluate_planned`]) and `run_gate` ([`evaluate`]).
//! Nothing else grows a checker. `docs/gate-policy-tuning.md` is canonical.
//!
//! Layers, top wins:
//!
//! 1. **User rules** — `approval_rules`, digest-keyed, learned from a human
//!    answer. A human decision outranks everything shipped or configured.
//! 2. **Builtin** — the verb's declared effect: a `kj` call that classifies
//!    as [`Effect::Read`](super::effect::Effect::Read), or any `kj ledger`
//!    call, under the six structural conditions `kj/readonly.rs` states (a
//!    redirect, a background flag, a heredoc or a substituted argument
//!    refuses).
//!
//! Composition per program is the ledger's own [`AskCoverage`] rule: a Deny
//! anywhere denies the whole submission, every statement must be Allow to
//! auto-allow, anything else escalates. A submission is never partially
//! applied.
//!
//! **Origin boundary.** The builtin layer classifies a *planned command
//! tree*, which a [`GateSpec`] carries for `Origin::ShellGate` and for a
//! shell-shaped `Origin::Hook` ask, but not for `Origin::KjVerb`. A `KjVerb`
//! ask meets the user-rule layer only and is otherwise `Uncovered`.

use approval_ledger::types::{AskVerdict, Origin, RuleRow, StatementVerdict};
use kaish_kernel::PlannedStatement;
use kaish_types::plan::PlannedCommand;
use rusqlite::Connection;

use super::gate::{statement_digest, GateSpec, GatedStatement};
use super::readonly;

/// Which layer decided a statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Layer {
    /// The verb's declared effect, or the `kj ledger` structural exemption.
    Builtin,
    /// A digest-keyed rule a human taught the ledger.
    UserRule,
}

impl Layer {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::UserRule => "user rule",
        }
    }
}

/// One statement's verdict, naming the layer and key that decided it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PolicyVerdict {
    Allow { layer: Layer, key: String },
    Deny { layer: Layer, key: String },
    /// No layer has a verdict; the statement meets the gate as usual.
    Uncovered,
}

/// The composed verdicts for one submission, one per gated statement in the
/// order the caller gave them.
#[derive(Clone, Debug, Default)]
pub(crate) struct PolicyEvaluation {
    pub per_statement: Vec<PolicyVerdict>,
}

impl PolicyEvaluation {
    /// The whole-submission verdict, composed the way [`AskCoverage`] does:
    /// deny wins, allow needs every statement, an empty set escalates.
    ///
    /// [`AskCoverage`]: approval_ledger::types::AskCoverage
    pub(crate) fn verdict(&self) -> AskVerdict {
        if self.per_statement.iter().any(|v| matches!(v, PolicyVerdict::Deny { .. })) {
            return AskVerdict::Deny;
        }
        if !self.per_statement.is_empty()
            && self.per_statement.iter().all(|v| matches!(v, PolicyVerdict::Allow { .. }))
        {
            return AskVerdict::Allow;
        }
        AskVerdict::Escalate
    }

    /// The `auto_reason` text for an auto-decision, naming the winning
    /// layer and key per statement. On a deny only the denied statements
    /// are named, by the SOURCE index the caller published
    /// (`GatedStatement::source_index`), never a position re-derived from
    /// `statements` — that Vec is post-filter, so a recount blames the
    /// wrong line.
    pub(crate) fn describe(&self, statements: &[GatedStatement], allow: bool) -> String {
        let parts: Vec<String> = self
            .per_statement
            .iter()
            .zip(statements.iter())
            .filter_map(|(v, s)| match v {
                PolicyVerdict::Allow { layer, key } if allow => {
                    Some(format!("{} allows {key}", layer.as_str()))
                }
                PolicyVerdict::Deny { layer, key } if !allow => Some(format!(
                    "{} denies {key} — {}",
                    layer.as_str(),
                    name_statement(s)
                )),
                _ => None,
            })
            .collect();
        format!("gate policy: {}", parts.join("; "))
    }
}

fn name_statement(s: &GatedStatement) -> String {
    match s.source_index {
        Some(idx) => format!("statement #{idx} (`{}`)", truncate_for_reason(&s.rendered)),
        None => format!("`{}`", truncate_for_reason(&s.rendered)),
    }
}

/// One line of a statement, short enough for a ledger row and a refusal.
fn truncate_for_reason(s: &str) -> String {
    const LIMIT: usize = 80;
    if s.chars().count() <= LIMIT {
        return s.replace('\n', "⏎");
    }
    let head: String = s.chars().take(LIMIT).collect();
    format!("{}…", head.replace('\n', "⏎"))
}

/// The builtin layer alone, over a planned program — what broker PreCall
/// consults before any hook runs. No store is read.
pub(crate) fn evaluate_planned(statements: &[PlannedStatement]) -> PolicyEvaluation {
    PolicyEvaluation {
        per_statement: statements.iter().map(builtin_verdict).collect(),
    }
}

/// Every layer, for one gate ask: user rules over the ask's statement
/// digests, then the builtin layer over the planned program the spec
/// carries. Fails only when the ledger cannot be read.
pub(crate) fn evaluate(
    conn: &Connection,
    spec: &GateSpec,
    context_id: Option<&[u8]>,
    principal_id: Option<&[u8]>,
) -> approval_ledger::Result<PolicyEvaluation> {
    let digests: Vec<String> = spec
        .statements
        .iter()
        .map(|s| statement_digest(spec.origin, &s.rendered))
        .collect();
    let digest_refs: Vec<&str> = digests.iter().map(String::as_str).collect();
    let coverage = approval_ledger::rules::redeem(
        conn,
        &digest_refs,
        &spec.authorized_label,
        context_id,
        principal_id,
    )?;

    let builtin = builtin_per_gated_statement(spec);
    let per_statement = coverage
        .per_statement
        .iter()
        .zip(builtin)
        .map(|(rule, builtin)| match rule {
            StatementVerdict::Allow(row) => PolicyVerdict::Allow {
                layer: Layer::UserRule,
                key: rule_key(row),
            },
            StatementVerdict::Deny(row) => PolicyVerdict::Deny {
                layer: Layer::UserRule,
                key: rule_key(row),
            },
            StatementVerdict::Uncovered => builtin,
        })
        .collect();
    Ok(PolicyEvaluation { per_statement })
}

fn rule_key(row: &RuleRow) -> String {
    format!("the exact statement (rule {})", row.rule_id)
}

/// The builtin verdict for each of `spec.statements`, aligned by index.
///
/// `Origin::ShellGate` builds one gated statement per planned statement,
/// so the two align by position; a spec whose `planned` is empty or of
/// another length has no plan to classify and is `Uncovered` throughout.
/// `Origin::Hook` gates one statement standing for the whole call, allowed
/// only when every planned statement is. `Origin::KjVerb` carries no plan.
fn builtin_per_gated_statement(spec: &GateSpec) -> Vec<PolicyVerdict> {
    let n = spec.statements.len();
    match spec.origin {
        Origin::ShellGate if spec.planned.len() == n && n > 0 => {
            spec.planned.iter().map(builtin_verdict).collect()
        }
        Origin::Hook if n == 1 && !spec.planned.is_empty() => {
            let program = evaluate_planned(&spec.planned);
            let verdict = if program.verdict() == AskVerdict::Allow {
                PolicyVerdict::Allow {
                    layer: Layer::Builtin,
                    key: program
                        .per_statement
                        .iter()
                        .filter_map(|v| match v {
                            PolicyVerdict::Allow { key, .. } => Some(key.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                }
            } else {
                PolicyVerdict::Uncovered
            };
            vec![verdict]
        }
        Origin::ShellGate | Origin::Hook | Origin::KjVerb => {
            vec![PolicyVerdict::Uncovered; n]
        }
    }
}

/// The builtin verdict for one planned statement: Allow when every command
/// in it has a builtin key, else Uncovered. A statement with no commands
/// is not an allow.
fn builtin_verdict(statement: &PlannedStatement) -> PolicyVerdict {
    let mut keys: Vec<String> = Vec::new();
    for cmd in &statement.plan.commands {
        match builtin_key(cmd) {
            Some(key) => {
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
            None => return PolicyVerdict::Uncovered,
        }
    }
    if keys.is_empty() {
        return PolicyVerdict::Uncovered;
    }
    PolicyVerdict::Allow {
        layer: Layer::Builtin,
        key: keys.join(", "),
    }
}

/// The builtin key for one command — `kj <verb> [<subcommand>]` in canonical
/// names — when the command is builtin-allowed, else `None`.
///
/// The exemption itself is `readonly::is_gate_exempt_kj`; this only names
/// what it allowed. `kj ledger` keys as the whole verb, the structural
/// exemption's own shape.
fn builtin_key(cmd: &PlannedCommand) -> Option<String> {
    if !readonly::is_gate_exempt_kj(cmd) {
        return None;
    }
    let mut args = readonly::resolved_kj_args(cmd)?;
    super::parse::strip_flag(&mut args, &["--confirm", "--json"]);
    let root = super::kj_command();
    let verb = root.find_subcommand(args.first()?)?;
    let mut key = format!("kj {}", verb.get_name());
    if verb.get_name() == "ledger" {
        return Some(key);
    }
    if let Some(sub) = args.get(1).and_then(|s| verb.find_subcommand(s)) {
        key.push(' ');
        key.push_str(sub.get_name());
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(source: &str) -> Vec<PlannedStatement> {
        kaish_kernel::ast::plan::plan_program(source)
            .unwrap_or_else(|e| panic!("plan_program({source:?}) failed to parse: {e:?}"))
    }

    fn allow_key(v: &PolicyVerdict) -> &str {
        match v {
            PolicyVerdict::Allow { layer: Layer::Builtin, key } => key,
            other => panic!("expected a builtin allow, got {other:?}"),
        }
    }

    #[test]
    fn a_read_verb_keys_as_verb_and_subcommand() {
        let e = evaluate_planned(&plan("kj block list"));
        assert_eq!(allow_key(&e.per_statement[0]), "kj block list");
        assert_eq!(e.verdict(), AskVerdict::Allow);
    }

    #[test]
    fn a_ledger_call_keys_as_the_whole_verb_behind_root_flags() {
        let e = evaluate_planned(&plan("kj --json ledger allow 01a0-abc"));
        assert_eq!(allow_key(&e.per_statement[0]), "kj ledger");
    }

    /// A substitution is planned as its own command beside the kj call, so
    /// the kj half is allowed in isolation and the statement rule is what
    /// refuses: the substituted command has no builtin key.
    #[test]
    fn a_substituted_argument_drops_the_statement_to_uncovered() {
        let e = evaluate_planned(&plan("kj ledger allow $(cat /tmp/id)"));
        assert_eq!(e.per_statement[0], PolicyVerdict::Uncovered);
    }

    #[test]
    fn a_pipeline_of_reads_keys_every_command_once() {
        let e = evaluate_planned(&plan("kj block list | kj block list"));
        assert_eq!(allow_key(&e.per_statement[0]), "kj block list");
    }

    #[test]
    fn a_write_verb_is_uncovered() {
        let e = evaluate_planned(&plan("kj context remove 01a0-abc"));
        assert_eq!(e.per_statement[0], PolicyVerdict::Uncovered);
        assert_eq!(e.verdict(), AskVerdict::Escalate);
    }

    #[test]
    fn a_redirect_drops_a_read_to_uncovered() {
        let e = evaluate_planned(&plan("kj block list > /tmp/x"));
        assert_eq!(e.per_statement[0], PolicyVerdict::Uncovered);
    }

    /// The whole-program composition: one non-allowed statement escalates
    /// the submission, and an empty program is never vacuously allowed.
    #[test]
    fn a_mixed_program_escalates_and_an_empty_one_is_not_an_allow() {
        let mixed = evaluate_planned(&plan("kj block list; kj block create --role user --kind text"));
        assert_eq!(mixed.per_statement.len(), 2);
        assert!(matches!(mixed.per_statement[0], PolicyVerdict::Allow { .. }));
        assert_eq!(mixed.per_statement[1], PolicyVerdict::Uncovered);
        assert_eq!(mixed.verdict(), AskVerdict::Escalate);

        let only_answers = evaluate_planned(&plan("kj ledger allow 01a0-abc; kj block list"));
        assert_eq!(only_answers.verdict(), AskVerdict::Allow);

        assert_eq!(evaluate_planned(&[]).verdict(), AskVerdict::Escalate);
    }

    #[test]
    fn a_program_that_does_not_parse_is_not_evaluated_here() {
        // `plan_program` refuses; the caller sees no statements and the
        // hooks decide. Pinned so the evaluator never grows a lenient path.
        assert!(kaish_kernel::ast::plan::plan_program("kj block list ((").is_err());
    }

    #[test]
    fn describe_names_layer_and_key_on_allow_and_the_denied_statement_on_deny() {
        let statements = vec![
            GatedStatement {
                rendered: "kj block list".into(),
                statement_kind: "command".into(),
                vars: vec![],
                source_index: Some(1),
            },
            GatedStatement {
                rendered: "rm -rf foo".into(),
                statement_kind: "command".into(),
                vars: vec![],
                source_index: Some(3),
            },
        ];
        let allow = PolicyEvaluation {
            per_statement: vec![
                PolicyVerdict::Allow { layer: Layer::Builtin, key: "kj block list".into() },
                PolicyVerdict::Allow { layer: Layer::UserRule, key: "the exact statement (rule r1)".into() },
            ],
        };
        assert_eq!(
            allow.describe(&statements, true),
            "gate policy: builtin allows kj block list; user rule allows the exact statement (rule r1)"
        );
        let deny = PolicyEvaluation {
            per_statement: vec![
                PolicyVerdict::Uncovered,
                PolicyVerdict::Deny { layer: Layer::UserRule, key: "the exact statement (rule r2)".into() },
            ],
        };
        let text = deny.describe(&statements, false);
        assert!(text.contains("#3") && text.contains("rm -rf foo"), "{text}");
        assert!(!text.contains("#1"), "must not blame the uncovered statement: {text}");
    }
}
