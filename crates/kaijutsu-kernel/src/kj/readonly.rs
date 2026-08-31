//! Static read-only classification for a `kj` invocation reaching the shell.
//!
//! `lfm2d-advisory` (`assets/defaults/rc/lib/hooks/lfm2d.kai`) scores every
//! `shell_write` clause through a classifier that over-escalates on ordinary
//! reads — `kj mcp tools exa` scored 0.385 and asked a human about a
//! read-only command. This module is Amy's fix: a static pass list for
//! read-only `kj` verbs, consulted before the classifier ever runs, so a
//! call this module accepts skips scoring entirely (`KJ_TOOL_PLAN`'s
//! `kj_readonly` field, wired in `mcp/broker.rs`).
//!
//! [`is_read_only_kj`] takes one [`PlannedCommand`] — kaish's own plan
//! projection, never the raw argv text — and returns `true` only when every
//! one of six conditions holds. Each is independently necessary; getting any
//! one wrong turns this into a bypass, so read every doc comment on
//! [`is_read_only_kj`] before touching it.
//!
//! ## `${VAR}` is safe in a fixed subcommand's flags, and only there
//!
//! kaish renders an unexpanded `${VAR}` reference as the literal text
//! `${VAR}` inside a [`PlannedValue::Plain`] — the plan is parse
//! information, built before any substitution runs (`kaish_types::plan`'s
//! module doc). kaish also does no word splitting: an expansion can never
//! inject additional argv entries, so `${VAR}` can only ever stand for
//! *one* argument, never a flag plus a value plus a trailing clause.
//!
//! That makes `${VAR}` harmless in a value position on an already-resolved
//! read-only subcommand — `kj block read ${ID}` still runs `block read`,
//! whatever `${ID}` turns out to be, and `block read` takes no flag that
//! writes. It is NOT safe in the verb or subcommand position: `kj ${VERB}
//! list` cannot be looked up in [`READ_ONLY_TABLE`] (the literal text
//! `${VERB}` matches no known command name), so [`is_read_only_kj`] simply
//! fails to resolve it and returns `false` — condition 6 refuses it before
//! any question of expansion-time danger even arises. No special case is
//! needed for either direction: the table lookup itself is what makes the
//! verb/subcommand position safe, and the argument semantics above are what
//! make the flag position safe.
//!
//! ## The redirect hole this closes
//!
//! A redirect turns any command into a filesystem write — `kj block list >
//! ~/.bashrc` must never classify as read-only, no matter how inert the
//! command itself is. The existing hook exemptions (`--help`, `kj ledger`)
//! do not check for one; this was already a narrow real hole in the hook
//! before this module existed (see `assets/defaults/rc/lib/hooks/lfm2d.kai`'s
//! own exemption filter — it matches on clause text alone). This module
//! closes it for every command it accepts, but the two pre-existing
//! exemptions are unchanged and unrelated: this module and its
//! `kj_readonly` field are additive, and closing that pre-existing hole for
//! `--help`/`ledger` (if ever wanted) is a change to the hook itself, not to
//! this module.
//!
//! ## Two-level tables only — a nested subcommand is excluded wholesale
//!
//! [`READ_ONLY_TABLE`] pairs a command with exactly one subcommand token,
//! because that is as deep as [`is_read_only_kj`] ever looks. Several `kj`
//! verbs nest a third level under one subcommand — `kj backend default
//! show` (read) vs. `kj backend default set` (write), `kj cast slot set`,
//! `kj drift edge rm` — and this module cannot tell those apart: `backend`
//! `default` is one table entry regardless of what follows it. So a
//! subcommand with a mixed-safety third level is never entered in
//! [`READ_ONLY_TABLE`] at all, not even for its read half. Nothing is lost
//! when every branch beneath it already mutates (`cast slot`, `drift
//! edge`); `backend default show` is the one case that gives up a real
//! read-only path for safety — recorded in the per-entry table below.
//!
//! ## `kj ledger` is out of scope here, on purpose
//!
//! `kj ledger list`/`show`/`rules`/`runs` are genuinely read-only by this
//! module's own rules, but none of `kj ledger`'s subcommands appear in
//! [`READ_ONLY_TABLE`] — the whole verb stays classified mutating in
//! [`MUTATING_TABLE`] (for [`every_kj_subcommand_is_classified`]'s
//! exhaustiveness only) and is left to the hook's own `kj ledger`
//! exemption, which exists for a reason specific to the gate's answer path
//! (self-approval is impossible by a context check, not a score — see the
//! hook's own comment). Folding ledger into this table would duplicate that
//! reasoning in a second place for no benefit.

use kaish_types::plan::{PlannedCommand, PlannedValue};

/// `(command, subcommand)` pairs whose ENTIRE flag surface is incapable of
/// a write — to the kernel database, the filesystem, a context, or a
/// remote. Verified by reading each subcommand's clap definition (not
/// inferred from its name): see the module doc for the two-level and
/// ledger exclusions, and [`NO_SUBCOMMAND_READ_ONLY`] for verbs with no
/// subcommand at all.
///
/// Every entry below carries a one-line justification in its own comment.
/// The full accounting — every mutating pair, and the two verbs excluded
/// only out of caution — lives in the task report, not here (this file
/// follows the project's own rule against historical prose in comments).
pub(crate) const READ_ONLY_TABLE: &[(&str, &str)] = &[
    // -- block: metadata/content reads. `cat`/`original` are excluded
    // despite reading a block, because both accept `--out <path>`, which
    // writes to the filesystem; `status`/`edit`/`create`/`append`/
    // `reproject` mutate the block itself.
    ("block", "list"),
    ("block", "inspect"),
    ("block", "count"),
    ("block", "read"),
    ("block", "history"),
    ("block", "diff"),
    // `render` engraves an ABC block to SVG on stdout and stores nothing.
    // Unlike `cat` it has no `--out`, which is the only reason `cat` is
    // classified mutating.
    ("block", "render"),
    // -- context: metadata reads. `switch` moves the session's active
    // context (a session-row write) and every other variant mutates a
    // context, so neither is here.
    ("context", "list"),
    ("context", "info"),
    ("context", "prompt"),
    ("context", "current"),
    ("context", "log"),
    // -- workspace / preset / backend / cast / alias: `list`/`show` read a
    // row; every other verb inserts, updates, or deletes one. `backend
    // model`/`backend default`/`cast slot` nest a third level (see module
    // doc) and are excluded wholesale.
    ("workspace", "list"),
    ("workspace", "show"),
    ("preset", "list"),
    ("preset", "show"),
    ("backend", "list"),
    ("backend", "show"),
    ("cast", "list"),
    ("cast", "show"),
    ("alias", "list"),
    // -- cas: `ls`/`info` read the store's metadata. `get` is excluded
    // despite reading an object, because it accepts `--out <path>`.
    ("cas", "ls"),
    ("cas", "info"),
    // -- cc: the session roster. `send` delivers a message unless
    // `--dry-run` is also given, and this module classifies on the
    // subcommand alone, never a flag's presence — see the module doc.
    ("cc", "list"),
    // -- audio: `beats` is pure offline analysis of a caller-given file —
    // no kernel/db/context/filesystem write.
    ("audio", "beats"),
    // -- midi: `list`/`show` read the device-profile tree; `send`/
    // `identify`/`panic` emit real MIDI (a write to attached hardware).
    ("midi", "list"),
    ("midi", "show"),
    // -- roster: `list` reads the live roster; `status` posts to it.
    ("roster", "list"),
    // -- rc: `list`/`show` read script files and the seed comparison
    // ("Indicator only — it never writes anything", rc.rs's own doc);
    // `add`/`rm` write the tree.
    ("rc", "list"),
    ("rc", "show"),
    // -- editor: `list` and `state` read session metadata/buffer; `open`
    // allocates a new session, `keys`/`save`/`quit` mutate one.
    ("editor", "list"),
    ("editor", "state"),
    // -- swap: `list` reads the unflushed-buffer table; `ack`/`discard`
    // resolve one.
    ("swap", "list"),
    // -- system: `status`/`ps` report kernel state; `quiesce`/`resume`
    // flip the durable quiesce flag.
    ("system", "status"),
    ("system", "ps"),
    // -- config: `list`/`show` read a config file; `reset` overwrites one
    // with its embedded default.
    ("config", "list"),
    ("config", "show"),
    // -- binding: `show` reads a context's capability allow-set;
    // `allow`/`revoke`/`reset` all write it.
    ("binding", "show"),
    // -- policy: `show` reads an instance's QoS policy; `set` writes it.
    ("policy", "show"),
    // -- mcp: `list` compares configured-vs-running servers; `reload`
    // reconciles the broker's live registrations.
    ("mcp", "list"),
    // -- hook: `list`/`show` read the broker's hook tables; `add`/`remove`
    // write them.
    ("hook", "list"),
    ("hook", "show"),
    // -- doc: `list`/`tree` read document metadata/structure; `create`/
    // `delete` mutate.
    ("doc", "list"),
    ("doc", "tree"),
    // -- kaish: `primer` composes onboarding text from `kaish-help` with no
    // kernel/db/filesystem write.
    ("kaish", "primer"),
    // -- stage: `status` reads staging state; `commit`/`include`/`exclude`
    // all mutate it.
    ("stage", "status"),
    // -- drift: `queue`/`history` read staged/flushed drift state; every
    // other verb (`push`/`pull`/`merge`/`flush`/`cancel`/`edge`) mutates.
    ("drift", "queue"),
    ("drift", "history"),
    // -- cache: `list` reads breakpoints; `add`/`clear` mutate them.
    ("cache", "list"),
    // -- vfs: both verbs are documented pure-read discovery ("Pure read
    // discovery, no capability gate" — vfs.rs's own module doc); neither
    // mutates.
    ("vfs", "snapshot"),
    ("vfs", "activity"),
];

/// Top-level `kj` verbs with no `#[command(subcommand)]` at all — every
/// argument is a flag or positional on the verb itself — whose entire flag
/// surface is read-only. Paired with [`MUTATING_NO_SUBCOMMAND`] so
/// [`every_kj_subcommand_is_classified`] can hold every no-subcommand verb
/// to the same exhaustiveness this module holds two-level ones to.
pub(crate) const READ_ONLY_NO_SUBCOMMAND: &[&str] = &[
    // `kj models` / `kj model`: pure LLM-registry discovery, no capability
    // gate ("discovery is not escalation" — model.rs's own module doc).
    "models",
    "model",
    // `kj search`: a regex scan over block content already in the kernel.
    "search",
    // `kj diff`: reads two kernel-held versions of a file and renders a
    // unified diff; never writes either side.
    "diff",
    // `kj wait`: subscribes to the turn-completion bus and re-reads the
    // block log in a loop; no write in the dispatch path.
    "wait",
];

/// Every OTHER `(command, subcommand)` pair `kj_command()` declares,
/// classified mutating. Exists only so
/// [`every_kj_subcommand_is_classified`] can assert every pair lands in
/// exactly one of this table or [`READ_ONLY_TABLE`] — a new `kj` verb with
/// no entry in either fails that test until someone classifies it.
///
/// `kj ledger`'s subcommands are listed here even though `list`/`show`/
/// `rules`/`runs` are themselves reads — see the module doc, "`kj ledger`
/// is out of scope here, on purpose". `kj transport`'s subcommands are
/// listed here in full, `list` included, even though its own doc comment
/// calls `list` read-only — excluded on Amy's explicit instruction to keep
/// the whole verb out of this module for now, not out of a per-verb
/// mutation finding; revisit if `kj transport list` needs the bypass later.
#[cfg(test)]
const MUTATING_TABLE: &[(&str, &str)] = &[
    ("context", "switch"),
    ("context", "create"),
    ("context", "scratch"),
    ("context", "rebind"),
    ("context", "set"),
    ("context", "unset"),
    ("context", "move"),
    ("context", "rename"),
    ("context", "archive"),
    ("context", "conclude"),
    ("context", "promote"),
    ("context", "demote"),
    ("context", "pause"),
    ("context", "resume"),
    ("context", "remove"),
    ("context", "retag"),
    ("context", "hydrate"),
    ("workspace", "create"),
    ("workspace", "add"),
    ("workspace", "bind"),
    ("workspace", "remove"),
    ("preset", "save"),
    ("preset", "remove"),
    ("preset", "reseed"),
    ("backend", "set"),
    ("backend", "remove"),
    ("backend", "model"),
    ("backend", "default"),
    ("backend", "reseed"),
    ("cast", "create"),
    ("cast", "remove"),
    ("cast", "set"),
    ("cast", "slot"),
    ("alias", "set"),
    ("alias", "remove"),
    ("cas", "put"),
    ("cas", "get"),
    ("cas", "rm"),
    ("cc", "send"),
    ("ledger", "list"),
    ("ledger", "show"),
    ("ledger", "allow"),
    ("ledger", "deny"),
    ("ledger", "rules"),
    ("ledger", "forget"),
    ("ledger", "runs"),
    ("ledger", "signal"),
    ("db", "backup"),
    ("db", "checkpoint"),
    ("midi", "send"),
    ("midi", "identify"),
    ("midi", "panic"),
    ("roster", "status"),
    ("rc", "add"),
    ("rc", "rm"),
    ("editor", "open"),
    ("editor", "keys"),
    ("editor", "save"),
    ("editor", "quit"),
    ("swap", "ack"),
    ("swap", "discard"),
    ("system", "quiesce"),
    ("system", "resume"),
    ("config", "reset"),
    ("block", "cat"),
    ("block", "original"),
    ("block", "reproject"),
    ("block", "append"),
    ("block", "status"),
    ("block", "edit"),
    ("block", "create"),
    ("binding", "allow"),
    ("binding", "revoke"),
    ("binding", "reset"),
    ("policy", "set"),
    ("mcp", "reload"),
    ("hook", "remove"),
    ("hook", "add"),
    ("doc", "create"),
    ("doc", "delete"),
    ("transport", "attach"),
    ("transport", "detach"),
    ("transport", "play"),
    ("transport", "pause"),
    ("transport", "stop"),
    ("transport", "tempo"),
    ("transport", "ooda"),
    ("transport", "clock"),
    ("transport", "rotate"),
    ("transport", "delete"),
    ("transport", "list"),
    ("stage", "commit"),
    ("stage", "include"),
    ("stage", "exclude"),
    ("drift", "push"),
    ("drift", "pull"),
    ("drift", "merge"),
    ("drift", "flush"),
    ("drift", "cancel"),
    ("drift", "edge"),
    ("cache", "add"),
    ("cache", "clear"),
];

/// Top-level `kj` verbs with no subcommand at all, classified mutating —
/// the [`MUTATING_TABLE`] counterpart of [`READ_ONLY_NO_SUBCOMMAND`].
#[cfg(test)]
const MUTATING_NO_SUBCOMMAND: &[&str] = &["cp", "play", "attach", "fork", "drive"];

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
/// 5. Every argument is [`PlannedValue::Plain`]. A [`PlannedValue::Redacted`]
///    argument means kaish's own confirm-key convention judged something
///    here a credential — never something to wave through unscored.
/// 6. The plain arguments resolve, verb then subcommand, to a pair this
///    module's tables cover: either a no-subcommand verb in
///    [`READ_ONLY_NO_SUBCOMMAND`], or a `(command, subcommand)` pair in
///    [`READ_ONLY_TABLE`] with no argument before the subcommand starting
///    with `-`. An unresolvable pair — an unknown verb, an unknown
///    subcommand, or a flag sitting where the subcommand belongs — fails
///    closed: this function returns `false`, not an error, on anything it
///    cannot positively place in the tables.
pub(crate) fn is_read_only_kj(cmd: &PlannedCommand) -> bool {
    if cmd.name != "kj" {
        return false;
    }
    if !cmd.redirects.is_empty() {
        return false;
    }
    if cmd.background {
        return false;
    }
    if !cmd.heredocs.is_empty() {
        return false;
    }

    let mut plain_args: Vec<&str> = Vec::with_capacity(cmd.args.len());
    for arg in &cmd.args {
        match arg {
            PlannedValue::Plain(s) => plain_args.push(s.as_str()),
            // `Redacted`, and any variant added later: never read-only.
            _ => return false,
        }
    }

    let Some(&verb) = plain_args.first() else {
        return false;
    };
    if verb.starts_with('-') {
        return false;
    }
    if READ_ONLY_NO_SUBCOMMAND.contains(&verb) {
        return true;
    }

    let Some(&subcommand) = plain_args.get(1) else {
        return false;
    };
    if subcommand.starts_with('-') {
        return false;
    }
    READ_ONLY_TABLE.contains(&(verb, subcommand))
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

    // -- condition 5: a redacted argument refuses --------------------------

    #[test]
    fn a_presented_confirm_key_refuses() {
        let cmd = plan_one("kj block list --confirm=deadbeef");
        assert!(
            cmd.args.iter().any(|a| a.is_redacted()),
            "test setup: expected kaish to redact the confirm key"
        );
        assert!(!is_read_only_kj(&cmd));
    }

    // -- condition 6: table resolution, and the dash-before-subcommand
    // guard specifically -------------------------------------------------

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
        // `--json` sits where `list` would; the table lookup can't see past
        // it, so this must fail closed rather than skip to the next word.
        let cmd = plan_one("kj block --json list");
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
    fn cas_get_is_excluded_despite_reading_because_of_its_out_flag() {
        // `kj cas get <hash> --out <path>` writes to the filesystem, and
        // this module has no per-flag logic — so `cas get` is never in
        // READ_ONLY_TABLE at all, even for a call with no --out.
        let cmd = plan_one("kj cas get deadbeef");
        assert!(!is_read_only_kj(&cmd));
    }

    // -- ${VAR} in verb position fails closed; in a flag value it doesn't --

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
    fn a_variable_reference_in_a_flag_value_on_a_resolved_read_only_pair_passes() {
        // `${ID}` can only ever stand for one argument (kaish does no word
        // splitting), and it sits in a value position on an
        // already-resolved read-only pair — it cannot smuggle in a flag.
        let cmd = plan_one("kj block read ${ID}");
        assert!(is_read_only_kj(&cmd));
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

    // -- exhaustiveness: every kj_command() leaf is classified -------------

    #[test]
    fn every_kj_subcommand_is_classified() {
        let root = super::super::kj_command();
        let mut unclassified = Vec::new();

        for verb in root.get_subcommands() {
            let verb_name = verb.get_name();
            if verb_name == "help" {
                // clap's own auto-generated help pseudo-subcommand — not a
                // kj verb, nothing to classify.
                continue;
            }
            let subs: Vec<&str> = verb
                .get_subcommands()
                .map(|s| s.get_name())
                .filter(|&n| n != "help")
                .collect();

            if subs.is_empty() {
                let in_ro = READ_ONLY_NO_SUBCOMMAND.contains(&verb_name);
                let in_mut = MUTATING_NO_SUBCOMMAND.contains(&verb_name);
                if in_ro == in_mut {
                    unclassified.push(format!("kj {verb_name} (no subcommand)"));
                }
                continue;
            }

            for sub_name in subs {
                let pair = (verb_name, sub_name);
                let in_ro = READ_ONLY_TABLE.contains(&pair);
                let in_mut = MUTATING_TABLE.contains(&pair);
                if in_ro == in_mut {
                    unclassified.push(format!("kj {verb_name} {sub_name}"));
                }
            }
        }

        assert!(
            unclassified.is_empty(),
            "every kj subcommand must be classified in exactly one of \
             READ_ONLY_TABLE/READ_ONLY_NO_SUBCOMMAND or \
             MUTATING_TABLE/MUTATING_NO_SUBCOMMAND — unclassified (or \
             double-classified): {unclassified:#?}"
        );
    }
}
