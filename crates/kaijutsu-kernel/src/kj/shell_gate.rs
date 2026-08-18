//! Builds a [`crate::kj::gate::GateSpec`] for the `shell_write` gate
//! (`docs/gate-and-shell-split.md`, "Slice 4") from a submission's kaish
//! source text.
//!
//! ## What this covers, and what it structurally cannot
//!
//! There is no execute-a-`Plan` API and no per-command interception hook in
//! kaish — a caller submits *source text* and it runs top to bottom. So this
//! gate is **all-or-nothing per submission**: `plan_program(source)` gives
//! one [`kaish_kernel::PlannedStatement`] per top-level statement, this
//! module turns each into a [`crate::kj::gate::GatedStatement`], and
//! `run_gate`'s [`approval_ledger::rules::AskCoverage::verdict`] composes
//! them — a single denied statement refuses the WHOLE submission, every
//! statement must be covered to auto-allow, anything else escalates. There
//! is no way to pause between statement 2 and 3 of one call to ask a
//! question mid-run, and this module does not pretend otherwise.
//!
//! **This gate only ever sees what `plan_program` can see: the kaish source
//! text of the one statement being planned.** It does NOT cover, and a
//! reader must not assume it covers:
//! - `python3 -c '<payload>'` or any other interpreter invocation whose
//!   actual behavior is inside a string argument this gate cannot parse as
//!   kaish and does not attempt to.
//! - `echo <payload> | python3` or any pipeline that hands a program to an
//!   interpreter over stdin — the interpreter's actions are invisible to a
//!   kaish-level plan.
//! - The `stdin` parameter `ShellParams` accepts separately from `command`
//!   — content piped in that way never appears in the source this module
//!   plans at all.
//! - Write-then-run-later: a command that writes a script now and a LATER,
//!   separately gated call executes it — each call is gated on its own
//!   text, never on what an earlier call wrote to disk.
//! - `background: true` (`ShellParams::background`) — a backgrounded
//!   command runs as a direct host subprocess (`/bin/sh -c <command>`,
//!   `mcp/servers/shell.rs`'s `start_background`), NOT through kaish, so
//!   `plan_program` cannot describe it (it isn't kaish source in the first
//!   place). Gating that path is separate, unbuilt work — background
//!   execution is ungated today, and this comment is the record of that gap
//!   rather than a silent one.
//!
//! The airtight configuration, as `docs/gate-and-shell-split.md` says
//! plainly, is `subprocess` off. This gate improves the common case: a
//! model that types a destructive kaish command directly gets stopped and a
//! human sees exactly what would run before it does.
//!
//! ## Heredocs: honest, once, against the real surface
//!
//! `kaish_types::plan::PlannedHeredoc::literal` says whether a heredoc body
//! is what the command actually reads (`<<'EOF'`, quoted delimiter — no
//! expansion runs) or gets shell-expanded first (`<<EOF`, unquoted — a
//! substitution can land inside a string literal in the language the body
//! is written in). kaish's own `Plan::rendered` already carries every
//! heredoc body **verbatim and unexpanded**
//! (`kaish-kernel/src/ast/plan.rs::render_redirect`), so the raw rendered
//! text is never itself a lie — it always shows exactly what was submitted.
//!
//! What raw text alone does NOT communicate is that an unquoted delimiter
//! means the bytes shown are not the bytes the command will read.
//! [`honest_render`] closes that gap by appending an explicit note, naming
//! every free variable ([`PlannedHeredoc::free_variables`]) a non-literal
//! heredoc will substitute, for every heredoc where `literal` is false.
//!
//! **Deliberately not used here: `kaish_kernel::expand_fragment`.** It can
//! compute the ACTUAL substituted body given a scope, but doing that at
//! ask-time would show a human a preview computed against session state
//! that can change before they answer and the statement actually executes
//! (the gate cannot pause mid-run — see above). A preview that goes stale
//! between "what was shown" and "what ran" is exactly the kind of quiet
//! mismatch a gate exists to prevent, not a feature worth the risk. Showing
//! the free-variable names instead of a guessed value is the honest
//! version of "say what will be substituted" that
//! `docs/gate-and-shell-split.md` asks for.

use approval_ledger::types::{Origin, VarBinding};

use super::gate::{GateSpec, GatedStatement};

/// The `shell_write` tool's ledger identity.
const INSTANCE: &str = "builtin.shell_write";
const TOOL: &str = "shell_write";

/// A submission this gate refuses to build an ask for at all — nothing
/// durable is recorded, because there is nothing honest to show a human.
#[derive(Debug)]
pub(crate) enum ShellGateBuildError {
    /// The source did not parse. kaish's own executor hits the same parse
    /// error on the same text, so refusing here costs nothing: the
    /// statement could not have run anyway, and a human is never asked to
    /// approve something that cannot be rendered.
    Parse(String),
}

impl std::fmt::Display for ShellGateBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(msg) => write!(f, "{msg}"),
        }
    }
}

/// Build the gate ask for one `shell_write` submission.
///
/// `authorized_label` is the submitted source text itself, trimmed — for
/// `shell_write` there is no separate "target" the way `kj cc send` has a
/// session name to resolve; what the caller typed IS the whole statement,
/// so the label that scopes confirmation to what was typed (the property
/// kept from the deleted latch, `docs/gate-and-shell-split.md`) is the
/// source text.
pub(crate) fn build_shell_gate_spec(source: &str) -> Result<GateSpec, ShellGateBuildError> {
    let planned = kaish_kernel::plan_program(source).map_err(|errors| {
        let msg = errors
            .iter()
            .map(|e| e.format(source))
            .collect::<Vec<_>>()
            .join("\n");
        ShellGateBuildError::Parse(format!(
            "shell_write: source does not parse, so it cannot be gated (and could not run \
             either):\n{msg}"
        ))
    })?;

    let statements: Vec<GatedStatement> = planned
        .iter()
        .map(|ps| GatedStatement {
            rendered: honest_render(ps),
            statement_kind: ps.plan.statement_kind.clone(),
            vars: ps
                .plan
                .free_variables
                .iter()
                .cloned()
                .map(|v| (v, VarBinding::Free))
                .chain(
                    ps.plan
                        .bound_variables
                        .iter()
                        .cloned()
                        .map(|v| (v, VarBinding::Bound)),
                )
                .collect(),
            source_index: Some(ps.index),
        })
        .collect();

    let label = source.trim().to_string();
    let preview: String = source.chars().take(200).collect();
    let truncated = source.chars().count() > 200;
    let description = format!(
        "shell_write: {} statement(s) — {preview}{}",
        statements.len(),
        if truncated { "…" } else { "" }
    );

    Ok(GateSpec {
        origin: Origin::ShellGate,
        instance: INSTANCE.into(),
        tool: TOOL.into(),
        hook_id: None,
        description,
        authorized_label: label,
        statements,
    })
}

/// Render one planned statement's text for a human deciding a gate ask,
/// honest about `literal` (module docs). kaish's `Plan::rendered` already
/// carries every heredoc body verbatim; this only APPENDS a caveat for a
/// non-literal heredoc, it never rewrites or hides the raw text.
fn honest_render(ps: &kaish_kernel::PlannedStatement) -> String {
    let mut out = ps.plan.rendered.clone();
    let mut notes = Vec::new();
    for cmd in &ps.plan.commands {
        for hd in &cmd.heredocs {
            if hd.literal {
                continue;
            }
            let vars = if hd.free_variables.is_empty() {
                "(none by name, but the delimiter is still unquoted, so kaish still runs \
                 substitution before the command reads it)"
                    .to_string()
            } else {
                hd.free_variables.join(", ")
            };
            notes.push(format!(
                "NOTE: the heredoc <<{} on `{}` has an UNQUOTED delimiter. The body shown above \
                 is exactly what was submitted, NOT what the command will read — kaish \
                 substitutes these variables first: {vars}.",
                hd.delimiter, cmd.name
            ));
        }
    }
    if !notes.is_empty() {
        out.push_str("\n\n");
        out.push_str(&notes.join("\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_statement_submission_builds_one_gated_statement() {
        let spec = build_shell_gate_spec("rm -rf build/").unwrap();
        assert_eq!(spec.statements.len(), 1);
        assert_eq!(spec.statements[0].rendered, "rm -rf build/");
        assert_eq!(spec.statements[0].source_index, Some(0));
        assert_eq!(spec.origin, Origin::ShellGate);
        assert_eq!(spec.tool, TOOL);
        assert_eq!(spec.authorized_label, "rm -rf build/");
    }

    #[test]
    fn unparseable_source_refuses_before_anything_is_built() {
        let err = build_shell_gate_spec("echo 'unclosed").unwrap_err();
        assert!(matches!(err, ShellGateBuildError::Parse(_)));
    }

    /// The landmine, as a regression test: `plan_program` numbers
    /// statements BEFORE dropping the empty ones (a leading comment or
    /// blank line), so a consumer that re-derives a position by counting
    /// the FILTERED list reports the wrong statement. This asserts the
    /// PUBLISHED index survives into `GatedStatement::source_index`
    /// unshifted.
    #[test]
    fn a_leading_comment_does_not_shift_which_statement_is_reported() {
        let source = "# a comment\nls\nrm -rf foo\n";
        let spec = build_shell_gate_spec(source).unwrap();
        assert_eq!(spec.statements.len(), 2, "the comment itself plans nothing");

        // If this module had (incorrectly) enumerated the filtered Vec
        // instead of carrying `PlannedStatement::index`, these would read
        // 0 and 1 instead of 1 and 2 — silently blaming the wrong
        // statement in every downstream reason string.
        assert_eq!(spec.statements[0].rendered, "ls");
        assert_eq!(spec.statements[0].source_index, Some(1));
        assert_eq!(spec.statements[1].rendered, "rm -rf foo");
        assert_eq!(spec.statements[1].source_index, Some(2));
    }

    /// Same landmine, two comments/blanks deep, to rule out an off-by-one
    /// that only a single gap happens to hide. Doesn't assert kaish's exact
    /// statement-numbering rule for consecutive blank/comment lines (that's
    /// kaish's own contract, not this module's) — only that a leading gap
    /// is never silently collapsed to the naive "re-enumerate the filtered
    /// Vec" answer of 0, which is exactly the bug this regression test
    /// exists to catch.
    #[test]
    fn multiple_leading_gaps_still_report_a_nonzero_source_index() {
        let source = "\n# one\n# two\nrm -rf foo\n";
        let spec = build_shell_gate_spec(source).unwrap();
        assert_eq!(spec.statements.len(), 1);
        assert!(
            spec.statements[0].source_index.unwrap() > 0,
            "a leading gap must shift the published index away from a naive 0: {:?}",
            spec.statements[0].source_index
        );
    }

    #[test]
    fn a_literal_heredoc_renders_with_no_caveat() {
        let source = "cat <<'EOF'\nhello ${NOT_EXPANDED}\nEOF\n";
        let spec = build_shell_gate_spec(source).unwrap();
        assert_eq!(spec.statements.len(), 1);
        let rendered = &spec.statements[0].rendered;
        assert!(rendered.contains("hello ${NOT_EXPANDED}"), "{rendered}");
        assert!(!rendered.contains("NOTE:"), "a literal heredoc needs no caveat: {rendered}");
    }

    /// The replacement for the spec's old heredoc-REFUSAL test: a
    /// `literal: false` heredoc is rendered and gated normally, but the
    /// prompt must not present the unexpanded text as final — it must name
    /// the variables that will be substituted.
    #[test]
    fn a_non_literal_heredoc_is_rendered_honestly_not_as_final_text() {
        let source = "cat <<EOF\nhello ${NAME}, your key is ${SECRET}\nEOF\n";
        let spec = build_shell_gate_spec(source).unwrap();
        assert_eq!(spec.statements.len(), 1);
        let rendered = &spec.statements[0].rendered;

        // The raw unexpanded body is still shown (kaish's own `rendered`
        // already carries it) — this gate never hides source text.
        assert!(rendered.contains("hello ${NAME}, your key is ${SECRET}"), "{rendered}");

        // But it must be unmistakably labeled as NOT final, naming exactly
        // what will be substituted.
        assert!(rendered.contains("NOTE:"), "{rendered}");
        assert!(rendered.contains("UNQUOTED"), "{rendered}");
        assert!(rendered.contains("NAME"), "{rendered}");
        assert!(rendered.contains("SECRET"), "{rendered}");
        assert!(
            !rendered.contains("your key is ${SECRET}\n\nyour key is"),
            "must never show a SUBSTITUTED preview alongside the raw text: {rendered}"
        );
    }

    #[test]
    fn free_and_bound_variables_carry_through_as_ledger_vars() {
        let spec = build_shell_gate_spec("X=1\necho $Y").unwrap();
        assert_eq!(spec.statements.len(), 2);
        assert!(spec.statements[0]
            .vars
            .iter()
            .any(|(n, b)| n == "X" && *b == VarBinding::Bound));
        assert!(spec.statements[1]
            .vars
            .iter()
            .any(|(n, b)| n == "Y" && *b == VarBinding::Free));
    }
}
