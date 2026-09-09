//! Read-only classification for a `kj` invocation reaching the shell.
//!
//! `lfm2d-advisory` (`assets/defaults/rc/lib/hooks/lfm2d.kai`) scores every
//! `shell_write` clause through a classifier that over-escalates on ordinary
//! reads. Measured against the live v10 scorer, 2026-09-01: `kj block read
//! <id>` lands `situation-normal` 0.791 and `kj rc show <path>` 0.675, and
//! anything but `informative` escalates — so a verb named `read` asked a
//! human for permission to read. The escalation is not uniform, which is why
//! a declared class beats tuning: `kj block list` (`informative` 0.897) and
//! `kj context list` (0.960) sail through, so neighbouring reads on the same
//! noun disagree. This module is Amy's fix: every `kj` verb declares its own
//! effect (`kj/effect.rs`, `docs/kj-verb-class.md`), and a call whose effect
//! is [`Effect::Read`] skips the classifier entirely (`KJ_TOOL_PLAN`'s
//! `kj_readonly` field, wired in `mcp/broker.rs`).
//!
//! [`is_read_only_kj`] takes one [`PlannedCommand`] — kaish's own plan
//! projection, never the raw argv text — and returns `true` only when every
//! one of six conditions holds. Each is independently necessary; getting any
//! one wrong turns this into a bypass, so read every doc comment on
//! [`is_read_only_kj`] before touching it.
//!
//! Conditions 1–5 are structural: the command is exactly `kj`, carries no
//! redirect, background flag, or heredoc, and every argument is plain text.
//! Condition 6 asks the verb itself: the plain arguments, with the leading
//! `kj` dropped, are handed to [`classify`], and the answer is `true` only
//! when it returns [`Effect::Read`]. There is no table here any more — the
//! verb's own [`Classify`] impl is the only place the question is answered.
//!
//! ## `${VAR}` is safe in a value position, and only there
//!
//! kaish renders an unexpanded `${VAR}` reference as the literal text
//! `${VAR}` inside a [`PlannedValue::Plain`] — the plan is parse
//! information, built before any substitution runs (`kaish_types::plan`'s
//! module doc). kaish also does no word splitting: an expansion can never
//! inject additional argv entries, so `${VAR}` can only ever stand for
//! *one* argument, never a flag plus a value plus a trailing clause.
//!
//! That makes `${VAR}` harmless in a value position on an already-resolved
//! read-only verb — `kj block read ${ID}` still runs `block read`, whatever
//! `${ID}` turns out to be, and a `String` positional accepts it as-is. It
//! is NOT safe in the verb or subcommand position: clap has no subcommand
//! named `${VERB}` to resolve, so `kj ${VERB} list` fails to parse and
//! condition 6 refuses before any question of expansion-time danger even
//! arises. The same failure mode catches a `${VAR}` sitting in a typed
//! flag — `kj wait ${CTX} --timeout ${T}` fails to parse `${T}` as the
//! `u64` `--timeout` takes, so it refuses too, even though `${CTX}`'s own
//! `String` slot would have been fine alone.
//!
//! ## The redirect hole this closes
//!
//! A redirect turns any command into a filesystem write — `kj block list >
//! ~/.bashrc` must never classify as read-only, no matter how inert the
//! command itself is. This module refuses one in condition 2, for every
//! command it accepts.
//!
//! The hook's other two exemptions (`--help`, `kj ledger`) once had the same
//! hole and no longer do: they gate on a `has_redirect` field the hook reads
//! from `KJ_TOOL_PLAN`'s `commands[].redirects`, which is kaish's structured
//! field rather than a scan of clause text. One narrower case stays open by
//! choice — the hook's fallback item, used only when `KJ_TOOL_PLAN` is
//! unusable, cannot see redirects at all, and failing it closed would stop
//! exempting the gate's own answer path. See that filter's comment in
//! `assets/defaults/rc/lib/hooks/lfm2d.kai` for the trade.
//!
//! ## `kj ledger` is exempt as a verb, not as a set of reads
//!
//! `kj ledger list`/`show`/`rules`/`runs` classify as [`Effect::Read`] by
//! their own declaration, but the whole verb is exempt whether or not a
//! given call reads: [`is_gate_exempt_kj`] accepts any `kj ledger` call that
//! meets conditions 1–5, regardless of what [`classify`] says about it. A
//! hook that could ask about `kj ledger allow <id>` makes answering an ask
//! require answering another ask, so the exemption is structural rather than
//! a hook's policy: the evaluator skips PreCall for a program made only of
//! exempt commands ([`program_is_gate_exempt`]). It is safe only because a
//! seat cannot answer its own ask (a context check in the ledger,
//! `docs/gate-and-shell-split.md`, "No self-approval"); remove that
//! invariant and this exemption goes with it.

use kaish_types::plan::{PlannedCommand, PlannedValue};

use super::effect::{classify, Effect};

/// Whether `cmd` is a `kj` invocation this hook may skip scoring for.
///
/// `true` only when every one of these holds:
///
/// 1. `cmd.name == "kj"` exactly — not a path (`/usr/local/bin/kj`), not a
///    suffix (`kaijutsu-kj`).
/// 2. `cmd.redirects` is empty. **The load-bearing condition** — a redirect
///    turns any command into a filesystem write (module doc, "The redirect
///    hole this closes").
/// 3. `cmd.background` is false. A backgrounded call has left the
///    classifier's control by the time anyone could act on its answer.
/// 4. `cmd.heredocs` is empty. A heredoc body is data the command consumes,
///    outside the argv this function inspects at all.
/// 5. Every argument is [`PlannedValue::Plain`]. kaish carries no redaction
///    of its own, so today this holds for everything it plans — the check
///    is a fail-closed guard on `PlannedValue`'s `#[non_exhaustive]` seam,
///    which kaish names as where an embedder-side redaction pass would add
///    its variant. A value this module cannot read as plain text is never
///    something to wave through unscored.
/// 6. The plain arguments, as an argv with the leading `kj` dropped, parse
///    through [`classify`] to [`Effect::Read`]. A parse failure — an
///    unknown verb, an unresolvable subcommand, a flag sitting where the
///    subcommand belongs, a `${VAR}` in a typed slot — refuses closed: this
///    function returns `false`, not an error, on anything [`classify`]
///    cannot place.
pub(crate) fn is_read_only_kj(cmd: &PlannedCommand) -> bool {
    let Some(args) = resolved_kj_args(cmd) else {
        return false;
    };
    matches!(classify(&args), Ok(Effect::Read))
}

/// Whether one command may reach the shell without any PreCall hook
/// seeing it: a read-only `kj` call, or any `kj ledger` call.
///
/// The ledger half resolves the first plain argument through
/// [`super::kj_command`]'s own subcommand table (aliases included, via
/// clap's [`clap::Command::find_subcommand`]) and checks whether it names
/// `ledger` — no hand-written alias list to keep in step.
///
/// The same five structural conditions as [`is_read_only_kj`] apply, so a
/// redirect, a background call, a heredoc or a substituted argument refuse.
pub(crate) fn is_gate_exempt_kj(cmd: &PlannedCommand) -> bool {
    if is_read_only_kj(cmd) {
        return true;
    }
    let Some(args) = resolved_kj_args(cmd) else {
        return false;
    };
    let Some(verb) = args.first() else {
        return false;
    };
    super::kj_command()
        .find_subcommand(verb)
        .is_some_and(|sub| sub.get_name() == "ledger")
}

/// Whether a whole planned program is exempt from PreCall hooks: every
/// command of every statement passes [`is_gate_exempt_kj`]. An empty
/// program is not exempt — there is nothing to exempt, and the hooks decide
/// what an empty submission means.
pub(crate) fn program_is_gate_exempt(statements: &[kaish_kernel::PlannedStatement]) -> bool {
    let mut commands = statements.iter().flat_map(|s| s.plan.commands.iter()).peekable();
    if commands.peek().is_none() {
        return false;
    }
    commands.all(is_gate_exempt_kj)
}

/// Conditions 1–5 of [`is_read_only_kj`], shared with [`is_gate_exempt_kj`]:
/// the command is exactly `kj`, carries no redirect, background flag or
/// heredoc, and every argument is plain text. Returns the plain arguments
/// with the leading `kj` dropped — the argv [`classify`] parses; `None`
/// refuses.
fn resolved_kj_args(cmd: &PlannedCommand) -> Option<Vec<String>> {
    if cmd.name != "kj" {
        return None;
    }
    if !cmd.redirects.is_empty() {
        return None;
    }
    if cmd.background {
        return None;
    }
    if !cmd.heredocs.is_empty() {
        return None;
    }

    let mut args = Vec::with_capacity(cmd.args.len());
    for arg in &cmd.args {
        match arg {
            PlannedValue::Plain(s) => args.push(s.clone()),
            // Any variant kaish adds to its redaction seam: never exempt.
            _ => return None,
        }
    }
    Some(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plan `source` and return the single statement's commands. Panics if
    /// `source` doesn't parse or doesn't produce exactly one statement —
    /// every test here is one statement, so a surprise there is a test bug,
    /// not a case to handle.
    fn plan_commands(source: &str) -> Vec<PlannedCommand> {
        let statements = kaish_kernel::ast::plan::plan_program(source)
            .unwrap_or_else(|e| panic!("plan_program({source:?}) failed to parse: {e:?}"));
        assert_eq!(
            statements.len(),
            1,
            "expected exactly one statement in {source:?}, got {}",
            statements.len()
        );
        statements.into_iter().next().unwrap().plan.commands
    }

    fn plan_one(source: &str) -> PlannedCommand {
        let mut commands = plan_commands(source);
        assert_eq!(
            commands.len(),
            1,
            "expected exactly one command in {source:?}, got {}",
            commands.len()
        );
        commands.remove(0)
    }

    // -- condition 1: name must be exactly "kj" --------------------------

    #[test]
    fn a_non_kj_command_name_is_never_read_only() {
        // Args alone resolve to a genuine read-only pair — only the name
        // makes this refuse, isolating condition 1 from condition 6.
        let cmd = plan_one("notkj block list");
        assert_eq!(cmd.name, "notkj");
        assert!(!is_read_only_kj(&cmd));
    }

    #[test]
    fn a_kj_path_is_not_the_bare_name_kj() {
        // Not a path a real shell would resolve, but plan_program doesn't
        // care — it plans argv0 verbatim, which is exactly what this
        // condition must reject.
        let cmd = plan_one("/usr/local/bin/kj block list");
        assert_ne!(cmd.name, "kj");
        assert!(!is_read_only_kj(&cmd));
    }

    // -- condition 2: a redirect refuses even a read verb ----------------

    #[test]
    fn a_redirect_refuses_even_a_read_only_verb() {
        let cmd = plan_one("kj block list > /tmp/x");
        assert!(!cmd.redirects.is_empty(), "test setup: expected a redirect");
        assert!(!is_read_only_kj(&cmd));
    }

    #[test]
    fn without_the_redirect_the_same_call_is_read_only() {
        // Companion to the above: proves the redirect is what flips the
        // verdict, not something else about the command text.
        let cmd = plan_one("kj block list");
        assert!(cmd.redirects.is_empty());
        assert!(is_read_only_kj(&cmd));
    }

    // -- condition 3: background ------------------------------------------

    #[test]
    fn a_backgrounded_call_is_never_read_only() {
        let cmd = plan_one("kj block list &");
        assert!(cmd.background, "test setup: expected a backgrounded command");
        assert!(!is_read_only_kj(&cmd));
    }

    // -- condition 4: heredocs ---------------------------------------------

    #[test]
    fn a_heredoc_is_never_read_only() {
        // Built directly rather than through `plan_one`: kaish's planner
        // always attaches a `<<` [`PlannedRedirect`] alongside a real
        // heredoc's [`PlannedHeredoc`] (verified against `plan_program`
        // separately), so condition 2 already refuses a plan-derived
        // heredoc call before condition 4 gets a turn. Constructing the
        // command by hand — heredocs non-empty, redirects genuinely empty —
        // isolates condition 4 as its own guard, independent of that
        // co-representation.
        let cmd = PlannedCommand::new(
            "kj",
            vec![
                PlannedValue::Plain("block".to_string()),
                PlannedValue::Plain("list".to_string()),
            ],
            vec![],
            false,
        )
        .with_heredocs(vec![kaish_types::plan::PlannedHeredoc::new(
            0,
            "EOF",
            true,
            false,
            PlannedValue::Plain("body\n".to_string()),
            0,
        )]);
        assert!(cmd.redirects.is_empty(), "test setup: no redirect on this command");
        assert!(!cmd.heredocs.is_empty(), "test setup: expected a heredoc");
        assert!(!is_read_only_kj(&cmd));
    }

    #[test]
    fn a_real_heredoc_also_carries_a_redirect_and_is_refused_by_condition_2() {
        // Documents the co-representation the previous test works around:
        // a real `<<` heredoc plans as BOTH a `PlannedHeredoc` and a `<<`
        // `PlannedRedirect` on the same command, so condition 2 alone
        // already refuses it in practice.
        let cmd = plan_one("kj block list <<'EOF'\nbody\nEOF\n");
        assert!(!cmd.heredocs.is_empty());
        assert!(
            !cmd.redirects.is_empty(),
            "kaish's planner is expected to also emit a `<<` redirect for a heredoc"
        );
        assert!(!is_read_only_kj(&cmd));
    }

    // -- condition 5: the plain-value seam ---------------------------------

    /// `--role=user` is an ordinary flag value with an `=` in it, sitting
    /// after the leaf has already resolved. Nothing about condition 5's
    /// plain-value check should treat that specially — it stays `Plain`,
    /// and [`classify`] parses it as `--role`'s value like any other.
    #[test]
    fn an_equals_flag_value_is_an_ordinary_argument_and_does_not_refuse_a_read() {
        let cmd = plan_one("kj block list --kind=text");
        assert!(
            cmd.args.iter().all(|a| matches!(a, PlannedValue::Plain(_))),
            "kaish plans every value as Plain; a new variant means condition 5 \
             has real work to do again and this test should be revisited"
        );
        assert!(is_read_only_kj(&cmd));
    }

    // Condition 5's catch-all arm has no test on purpose: `PlannedValue` is
    // `#[non_exhaustive]` with `Plain` as its only variant, so no caller
    // outside kaish can build a value that reaches it. The arm is a
    // fail-closed guard for a variant kaish has not added yet, and the
    // assertion in the test above fires when it does.

    // -- condition 6: classify resolution ----------------------------------

    #[test]
    fn an_unknown_verb_refuses() {
        let cmd = plan_one("kj bogus list");
        assert!(!is_read_only_kj(&cmd));
    }

    #[test]
    fn an_unknown_subcommand_refuses() {
        let cmd = plan_one("kj block bogus");
        assert!(!is_read_only_kj(&cmd));
    }

    #[test]
    fn a_flag_where_the_subcommand_belongs_refuses() {
        // `--role` is `block create`'s flag, not `block`'s own — with no
        // subcommand resolved at all, clap has nothing to classify.
        let cmd = plan_one("kj block --role user");
        assert!(!is_read_only_kj(&cmd));
    }

    #[test]
    fn a_known_read_only_pair_passes() {
        let cmd = plan_one("kj block list");
        assert!(is_read_only_kj(&cmd));
    }

    #[test]
    fn a_known_mutating_pair_refuses() {
        let cmd = plan_one("kj block create --role user --kind text");
        assert!(!is_read_only_kj(&cmd));
    }

    #[test]
    fn a_no_subcommand_verb_passes_regardless_of_its_own_flags() {
        let cmd = plan_one("kj search pattern --json");
        assert!(is_read_only_kj(&cmd));
    }

    #[test]
    fn a_backend_default_show_is_read_only() {
        // The nested-subcommand case a two-level table couldn't tell apart
        // from `backend default set` — the class doesn't have that problem.
        let cmd = plan_one("kj backend default show");
        assert!(is_read_only_kj(&cmd));
    }

    #[test]
    fn a_block_cat_is_read_only_only_without_out() {
        let read = plan_one("kj block cat 019a2f3c");
        assert!(is_read_only_kj(&read));
        let write = plan_one("kj block cat 019a2f3c --out /tmp/x");
        assert!(!is_read_only_kj(&write));
    }

    #[test]
    fn a_cas_get_is_read_only_only_without_out() {
        let read = plan_one("kj cas get sha256-abc");
        assert!(is_read_only_kj(&read));
        let write = plan_one("kj cas get sha256-abc --out /tmp/x");
        assert!(!is_read_only_kj(&write));
    }

    #[test]
    fn a_transport_list_is_read_only() {
        // No longer kept out of this module wholesale — the class puts it
        // on its own merits (`docs/kj-verb-class.md`, "Decisions carried
        // here").
        let cmd = plan_one("kj transport list");
        assert!(is_read_only_kj(&cmd));
    }

    // -- ${VAR} in verb position, and in a typed slot, fail closed ---------

    #[test]
    fn a_variable_reference_in_verb_position_fails_closed() {
        let cmd = plan_one("kj ${VERB} list");
        assert!(
            cmd.args.first().is_some_and(|a| matches!(
                a, PlannedValue::Plain(s) if s == "${VERB}"
            )),
            "test setup: expected the unexpanded literal ${{VERB}} as the first arg"
        );
        assert!(!is_read_only_kj(&cmd));
    }

    #[test]
    fn a_variable_reference_in_a_string_positional_on_a_read_only_verb_passes() {
        // `${ID}` can only ever stand for one argument (kaish does no word
        // splitting), and it sits in a `String` positional — the same
        // position `block read`'s handler would read whatever it resolves
        // to from.
        let cmd = plan_one("kj block read ${ID}");
        assert!(is_read_only_kj(&cmd));
    }

    #[test]
    fn a_variable_reference_in_a_typed_flag_fails_closed() {
        // `${T}` is not a `u64`, so `classify` cannot parse `--timeout`'s
        // value — even though `${CTX}`'s own `String` positional would have
        // been fine alone.
        let cmd = plan_one("kj wait ${CTX} --timeout ${T}");
        assert!(!is_read_only_kj(&cmd));
    }

    // -- command substitution: structural, not special-cased ---------------

    #[test]
    fn a_command_substitution_lands_as_its_own_command_and_never_rides_along() {
        let commands = plan_commands("kj block read $(rm -rf /)");
        assert_eq!(
            commands.len(),
            2,
            "expected the kj call and the substitution as two separate planned commands, got {}",
            commands.len()
        );
        let kj_cmd = commands
            .iter()
            .find(|c| c.name == "kj")
            .expect("a kj command among the planned commands");
        let sub_cmd = commands
            .iter()
            .find(|c| c.name != "kj")
            .expect("the substitution's own command");
        assert_eq!(sub_cmd.name, "rm");
        // The dangerous half is isolated in its own command and this
        // module refuses it outright — nothing here needs to special-case
        // "$(" text inside kj's own argv, because that text never reaches
        // kj's argv as anything but an inert, unexpanded literal.
        assert!(
            !is_read_only_kj(sub_cmd),
            "the substitution's own command must never read as read-only kj"
        );
        // kj's own command is genuinely read-only in isolation; it is the
        // hook's mixed-call rule (every command exempt or none is), not
        // this module, that must still refuse the call as a whole.
        assert!(is_read_only_kj(kj_cmd));
    }

    // -- gate exemption: the ledger's own verb, under the same five conditions

    /// The gate's answer path is exempt by construction: `kj ledger` never
    /// reaches a hook that can ask. Read-only verbs are exempt too.
    #[test]
    fn a_ledger_answer_is_gate_exempt() {
        assert!(is_gate_exempt_kj(&plan_one("kj ledger allow 01a0-abc")));
        assert!(is_gate_exempt_kj(&plan_one("kj ledger deny 01a0-abc")));
        assert!(is_gate_exempt_kj(&plan_one("kj ledger list --status abandoned")));
        assert!(is_gate_exempt_kj(&plan_one("kj block list")));
    }

    /// `kj ledger list` is read-only on its own merits, and `kj ledger
    /// allow` is not — but both are gate-exempt, because the exemption
    /// covers the whole verb regardless of what `classify` says about a
    /// given call.
    #[test]
    fn ledger_list_is_read_only_and_ledger_allow_is_not_but_both_are_exempt() {
        let list = plan_one("kj ledger list");
        assert!(is_read_only_kj(&list));
        assert!(is_gate_exempt_kj(&list));

        let allow = plan_one("kj ledger allow 019a2f3c");
        assert!(!is_read_only_kj(&allow));
        assert!(is_gate_exempt_kj(&allow));
    }

    /// `kj ledger` is exempt only under the structural conditions
    /// `is_read_only_kj` applies: a redirect, a background call, a
    /// substituted argument, or a mutating verb that is not `ledger` all
    /// refuse.
    #[test]
    fn a_ledger_answer_with_a_redirect_or_substitution_is_not_exempt() {
        assert!(!is_gate_exempt_kj(&plan_one("kj ledger allow 01a0 > /tmp/out")));
        assert!(!is_gate_exempt_kj(&plan_one("kj ledger allow 01a0 &")));
        assert!(!is_gate_exempt_kj(&plan_one("kj block create --role user --kind text")));
        // A substitution is planned as its own command beside the kj call,
        // so the kj half is exempt in isolation and the program rule is what
        // refuses the whole: the substituted command is not exempt.
        let program =
            kaish_kernel::ast::plan::plan_program("kj ledger allow $(cat /tmp/id)").unwrap();
        assert!(!program_is_gate_exempt(&program));
    }

    /// A whole program is exempt only when every command of every
    /// statement is: `kj ledger allow x; dd ...` must still be scored.
    #[test]
    fn a_program_is_exempt_only_when_every_command_is() {
        let only_answers = kaish_kernel::ast::plan::plan_program(
            "kj ledger show 01a0-abc; kj ledger allow 01a0-abc",
        )
        .unwrap();
        assert!(program_is_gate_exempt(&only_answers));
        let mixed = kaish_kernel::ast::plan::plan_program(
            "kj ledger allow 01a0-abc; dd if=/dev/zero of=/dev/sda",
        )
        .unwrap();
        assert!(!program_is_gate_exempt(&mixed));
        let empty = kaish_kernel::ast::plan::plan_program("").unwrap();
        assert!(!program_is_gate_exempt(&empty), "an empty program is not an exemption");
    }
}
