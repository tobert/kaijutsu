//! One clause per scored unit, rendered from a kaish plan.
//!
//! A classifier scores *clauses*, not statements: a severe token quoted
//! inside a benign carrier scores below the carrier when the whole statement
//! is judged as one string. This module is the single place that decides
//! where a statement is cut, so every consumer sends the same text.
//!
//! Two consumers share it:
//!
//! - `mcp/broker.rs` mirrors each command's clause onto the `KJ_TOOL_PLAN`
//!   JSON twin as `commands[].clause`, so the rc hook body
//!   (`assets/defaults/rc/lib/hooks/lfm2d.kai`) reads a field instead of
//!   re-deriving the cut in `jq`.
//! - `kaijutsu-mcp`'s advisory scorer renders the same clauses for a Claude
//!   Code `Bash` call.
//!
//! ## The cut
//!
//! `kj block list && kj fork` → two clauses, `"kj block list"` and
//! `"kj fork"`; `echo "${HOME}"` → one clause, the statement's rendered text.
//!
//! A statement is cut per command only when it has commands **and** every
//! argument of every command is [`PlannedValue::Plain`]. A non-plain
//! argument means the reassembled argv would not be the text that was asked
//! for, so the whole statement falls back to
//! [`Plan::rendered`](kaish_types::plan::Plan::rendered) as one clause —
//! kaish's own unexpanded rendering, which is what a classifier should judge.
//! The linked kaish produces only `Plain`, so today a bare assignment (a
//! statement with no commands) is the only text that reaches the fallback;
//! the guard is there because [`PlannedValue`] is `#[non_exhaustive]` and a
//! redaction pass is exactly what its seam is for.
//!
//! On the fallback path `kj_readonly` and `has_redirect` are both `false`.
//! They describe one command, and the fallback clause is not one command;
//! reporting a per-command property for a whole statement would be a claim
//! the text cannot support, and both fields gate exemptions that must fail
//! closed.

use kaish_kernel::PlannedStatement;
use kaish_types::plan::PlannedValue;

/// One clause as it will be sent to a classifier, with the placement and
/// exemption facts a caller needs to interpret the score.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanClause {
    /// The statement this clause came from, carried verbatim from
    /// [`PlannedStatement::index`].
    pub stmt_seq: usize,
    /// The command's position within its statement, or `None` when the
    /// statement was rendered whole.
    pub cmd_seq: Option<usize>,
    /// The text to score.
    pub clause: String,
    /// The whole statement's unexpanded rendering, for a reader that needs
    /// the carrier a clause was cut out of.
    pub stmt_rendered: String,
    /// `true` only when this clause is one command whose own declared
    /// effect (`kj/effect.rs`, `docs/kj-verb-class.md`) is `Read`, per
    /// [`crate::kj::readonly::is_read_only_kj`]. Always `false` on the
    /// whole-statement path.
    pub kj_readonly: bool,
    /// Whether the command declares a redirect. Always `false` on the
    /// whole-statement path.
    pub has_redirect: bool,
}

/// Render every clause for a planned program, in statement then command
/// order.
pub fn render_clauses(statements: &[PlannedStatement]) -> Vec<PlanClause> {
    let mut out = Vec::new();
    for stmt in statements {
        out.extend(render_statement(stmt));
    }
    out
}

/// The clause text each command of `stmt` is scored under, one entry per
/// command in `stmt.plan.commands`.
///
/// On the per-command path each command gets its own clause. On the
/// whole-statement path every command gets the statement's rendered text,
/// because that is the string the classifier will actually see for it —
/// a reader of one command's `clause` must never be handed text that was
/// not sent.
pub fn command_clause_texts(stmt: &PlannedStatement) -> Vec<String> {
    let rendered = render_statement(stmt);
    if rendered.first().is_some_and(|c| c.cmd_seq.is_some()) {
        return rendered.into_iter().map(|c| c.clause).collect();
    }
    let whole = rendered
        .first()
        .map(|c| c.clause.clone())
        .unwrap_or_default();
    vec![whole; stmt.plan.commands.len()]
}

/// Render one statement: either one clause per command, or one clause for
/// the whole statement. The single place the cut is decided.
fn render_statement(stmt: &PlannedStatement) -> Vec<PlanClause> {
    let commands = &stmt.plan.commands;
    let every_arg_plain = commands
        .iter()
        .all(|cmd| cmd.args.iter().all(|a| matches!(a, PlannedValue::Plain(_))));

    if commands.is_empty() || !every_arg_plain {
        return vec![PlanClause {
            stmt_seq: stmt.index,
            cmd_seq: None,
            clause: stmt.plan.rendered.clone(),
            stmt_rendered: stmt.plan.rendered.clone(),
            kj_readonly: false,
            has_redirect: false,
        }];
    }

    commands
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let mut clause = String::from(cmd.name.as_str());
            for arg in &cmd.args {
                if let PlannedValue::Plain(s) = arg {
                    clause.push(' ');
                    clause.push_str(s);
                }
            }
            PlanClause {
                stmt_seq: stmt.index,
                cmd_seq: Some(i),
                clause,
                stmt_rendered: stmt.plan.rendered.clone(),
                kj_readonly: crate::kj::readonly::is_read_only_kj(cmd),
                has_redirect: !cmd.redirects.is_empty(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planned(source: &str) -> Vec<PlannedStatement> {
        kaish_kernel::plan_program(source)
            .unwrap_or_else(|e| panic!("plan_program({source:?}) failed to parse: {e:?}"))
    }

    fn texts(source: &str) -> Vec<String> {
        render_clauses(&planned(source))
            .into_iter()
            .map(|c| c.clause)
            .collect()
    }

    /// A plain command is one clause, argv joined by single spaces — the
    /// quoting the author typed is not reconstructed, because the classifier
    /// is judging the words, not the shell syntax.
    #[test]
    fn a_plain_command_renders_its_argv() {
        assert_eq!(texts("ls -la /tmp"), vec!["ls -la /tmp".to_string()]);
    }

    /// A pipeline is cut per command: two clauses, not one carrier string.
    #[test]
    fn a_pipeline_is_cut_per_command() {
        assert_eq!(
            texts("cat notes.txt | grep -i todo"),
            vec!["cat notes.txt".to_string(), "grep -i todo".to_string()]
        );
    }

    /// Command position and statement position both travel with the clause.
    #[test]
    fn clause_positions_are_recorded() {
        let clauses = render_clauses(&planned("echo one; echo two | wc -l"));
        let seats: Vec<(usize, Option<usize>)> =
            clauses.iter().map(|c| (c.stmt_seq, c.cmd_seq)).collect();
        assert_eq!(seats, vec![(0, Some(0)), (1, Some(0)), (1, Some(1))]);
    }

    /// The non-plain guard has no source text that reaches it with the
    /// linked kaish: [`PlannedValue`] carries exactly one variant, `Plain`.
    /// This test pins that fact, so the day kaish (or an embedder-side
    /// redaction pass) adds a variant, this goes red and the fallback
    /// branch above needs a real test rather than staying quietly dead.
    #[test]
    fn every_planned_value_the_linked_kaish_produces_is_plain() {
        for stmt in planned("rm --confirm=abc123 x | wc -l") {
            for cmd in &stmt.plan.commands {
                for arg in &cmd.args {
                    assert!(
                        matches!(arg, PlannedValue::Plain(_)),
                        "a non-plain value is now reachable from source text — \
                         give the fallback branch a real test"
                    );
                }
            }
        }
    }

    /// A statement with no commands at all — a bare assignment — is one
    /// clause of its rendered text, not zero clauses. Dropping it would
    /// silently remove the statement from everything downstream.
    #[test]
    fn a_statement_with_no_commands_still_renders_one_clause() {
        let clauses = render_clauses(&planned("FOO=bar"));
        assert_eq!(clauses.len(), 1);
        assert_eq!(clauses[0].cmd_seq, None);
        assert_eq!(clauses[0].clause, "FOO=bar");
    }

    /// A redirect is reported off kaish's structured field, never a scan of
    /// the clause text for `>`.
    #[test]
    fn a_redirect_is_flagged_on_its_command() {
        let clauses = render_clauses(&planned("kj ledger list > /tmp/out"));
        assert_eq!(clauses.len(), 1);
        assert!(clauses[0].has_redirect, "the redirect must be reported");
        assert!(
            !clauses[0].kj_readonly,
            "a redirect turns any command into a write"
        );
        assert_eq!(clauses[0].clause, "kj ledger list");
    }

    /// A read-only `kj` verb carries the exemption its own declared effect
    /// grants it, so a caller can skip scoring without re-deriving verb
    /// structure.
    #[test]
    fn a_read_only_kj_command_is_marked() {
        let clauses = render_clauses(&planned("kj block list"));
        assert_eq!(clauses.len(), 1);
        assert_eq!(clauses[0].clause, "kj block list");
        assert!(clauses[0].kj_readonly);
    }

    /// `command_clause_texts` answers per command on both paths: one clause
    /// each when the statement was cut, the whole rendering repeated when it
    /// was not.
    #[test]
    fn command_clause_texts_covers_every_command_on_both_paths() {
        let cut = planned("a | b");
        assert_eq!(
            command_clause_texts(&cut[0]),
            vec!["a".to_string(), "b".to_string()]
        );

        let whole = planned("FOO=bar");
        assert!(
            command_clause_texts(&whole[0]).is_empty(),
            "a statement with no commands has no per-command clause to report"
        );
    }
}
