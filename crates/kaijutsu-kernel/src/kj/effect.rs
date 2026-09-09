//! The `kj` verb class: every verb declares its effect where it is
//! declared, and that declaration is the only place the question is
//! answered. The devlog chapter "The file that answered a question the
//! code already knew" carries the reasoning.
//!
//! Each domain module implements [`Classify`] for its `*Args` struct with an
//! exhaustive match over the parsed value, so a new variant with no arm is
//! a compile error, and an argument that changes the effect (`block cat
//! --out <path>`) is visible to the arm that classifies it.

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use serde::{Deserialize, Serialize};

/// What running a `kj` verb does to the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Effect {
    /// No side effect anywhere: not the kernel database, not the host
    /// filesystem, not a remote, not a peer. Skips the shell gate and the
    /// classifier by construction.
    Read,
    /// Changes state that can be changed back. Meets the gate like any
    /// other statement.
    Write,
    /// Permanent, or takes something down that cannot be brought back.
    /// The dispatcher refuses it without `--confirm`, always.
    Destroy,
}

impl Effect {
    pub fn as_str(self) -> &'static str {
        match self {
            Effect::Read => "read",
            Effect::Write => "write",
            Effect::Destroy => "destroy",
        }
    }
}

impl std::fmt::Display for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Implemented by every `kj` domain's `*Args` struct and by each subcommand
/// enum beneath it. A struct with a subcommand field delegates to it; a
/// verb with no subcommand answers on the struct directly.
pub trait Classify {
    fn effect(&self) -> Effect;
}

/// The whole `kj` argv as one parse: the two root flags and every domain
/// subcommand. `kj_command()` reflects from this, and [`classify`] parses
/// through it, so the leaf set a client sees and the leaf set the class
/// covers are the same by construction.
///
/// The two root flags are not `global`: kaish's binder already merges root
/// params onto every leaf so the trailing `… retag a b --confirm` form
/// binds, and reflecting them per leaf would change the published schema.
/// The builtin strips both before dispatch (`runtime/kj_builtin.rs`), and
/// [`classify`] strips them the same way, so plan-time argv that still
/// carries them parses too.
///
/// `--confirm` is a bare flag: presence is the confirmation. `--json` is
/// owned by kaish's output formatting and is declared here only so the
/// root argv surface is complete; the schema reflection excludes it.
#[derive(Parser, Debug)]
#[command(
    name = "kj",
    about = "Kernel command interface. Run `kj help` or `kj <command> help` for detailed workflows.",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct KjArgs {
    /// Confirm a destructive operation
    #[arg(long)]
    pub(crate) confirm: bool,
    /// Emit output as JSON
    #[arg(long)]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) command: KjCommand,
}

/// One variant per domain, in the order `kj help` lists them.
#[derive(Subcommand, Debug)]
pub(crate) enum KjCommand {
    Context(super::context::ContextArgs),
    Workspace(super::workspace::WorkspaceArgs),
    Preset(super::preset::PresetArgs),
    Backend(super::backend::BackendArgs),
    Cast(super::cast::CastArgs),
    Character(super::character::CharacterArgs),
    Handoff(super::handoff::HandoffArgs),
    Alias(super::alias::AliasArgs),
    Cas(super::cas::CasArgs),
    Cc(super::cc::CcArgs),
    Ledger(super::ledger::LedgerArgs),
    Db(super::db::DbArgs),
    Audio(super::audio::AudioArgs),
    Midi(super::midi::MidiArgs),
    Roster(super::roster::RosterArgs),
    Cp(super::cp::CpArgs),
    Play(super::play::PlayArgs),
    Rc(super::rc::RcArgs),
    Editor(super::editor::EditorArgs),
    Swap(super::swap::SwapArgs),
    System(super::system::SystemArgs),
    Config(super::config::ConfigArgs),
    Block(super::block::BlockArgs),
    Binding(super::binding::BindingArgs),
    Policy(super::policy::PolicyArgs),
    Mcp(super::mcp::McpArgs),
    Hook(super::hook::HookArgs),
    Search(super::search::SearchArgs),
    Doc(super::doc::DocArgs),
    Attach(super::attach::AttachArgs),
    Transport(super::transport::TransportArgs),
    Models(super::model::ModelsArgs),
    Model(super::model::ModelArgs),
    Kaish(super::kaish::KaishArgs),
    Fork(super::fork::ForkArgs),
    Drive(super::drive::DriveArgs),
    Wait(super::wait::WaitArgs),
    Stage(super::stage::StageArgs),
    Drift(super::drift::DriftArgs),
    Cache(super::cache::CacheArgs),
    Vfs(super::vfs::VfsArgs),
    Diff(super::diff::DiffArgs),
}

impl Classify for KjCommand {
    fn effect(&self) -> Effect {
        match self {
            KjCommand::Context(a) => a.effect(),
            KjCommand::Workspace(a) => a.effect(),
            KjCommand::Preset(a) => a.effect(),
            KjCommand::Backend(a) => a.effect(),
            KjCommand::Cast(a) => a.effect(),
            KjCommand::Character(a) => a.effect(),
            KjCommand::Handoff(a) => a.effect(),
            KjCommand::Alias(a) => a.effect(),
            KjCommand::Cas(a) => a.effect(),
            KjCommand::Cc(a) => a.effect(),
            KjCommand::Ledger(a) => a.effect(),
            KjCommand::Db(a) => a.effect(),
            KjCommand::Audio(a) => a.effect(),
            KjCommand::Midi(a) => a.effect(),
            KjCommand::Roster(a) => a.effect(),
            KjCommand::Cp(a) => a.effect(),
            KjCommand::Play(a) => a.effect(),
            KjCommand::Rc(a) => a.effect(),
            KjCommand::Editor(a) => a.effect(),
            KjCommand::Swap(a) => a.effect(),
            KjCommand::System(a) => a.effect(),
            KjCommand::Config(a) => a.effect(),
            KjCommand::Block(a) => a.effect(),
            KjCommand::Binding(a) => a.effect(),
            KjCommand::Policy(a) => a.effect(),
            KjCommand::Mcp(a) => a.effect(),
            KjCommand::Hook(a) => a.effect(),
            KjCommand::Search(a) => a.effect(),
            KjCommand::Doc(a) => a.effect(),
            KjCommand::Attach(a) => a.effect(),
            KjCommand::Transport(a) => a.effect(),
            KjCommand::Models(a) => a.effect(),
            KjCommand::Model(a) => a.effect(),
            KjCommand::Kaish(a) => a.effect(),
            KjCommand::Fork(a) => a.effect(),
            KjCommand::Drive(a) => a.effect(),
            KjCommand::Wait(a) => a.effect(),
            KjCommand::Stage(a) => a.effect(),
            KjCommand::Drift(a) => a.effect(),
            KjCommand::Cache(a) => a.effect(),
            KjCommand::Vfs(a) => a.effect(),
            KjCommand::Diff(a) => a.effect(),
        }
    }
}

/// Why an argv could not be classified. A parse failure is the only way:
/// the argv names no live leaf, or an argument does not fit its slot
/// (a `${VAR}` in a numeric flag at plan time, say). The caller decides
/// what that means; the shell gate treats it as "not read-only".
#[derive(Debug, thiserror::Error)]
pub enum ClassifyError {
    #[error("kj argv does not classify: {0}")]
    Parse(#[from] clap::Error),
}

/// The effect of running `kj <argv>`, from the verb's own declaration.
///
/// `argv` is the kj argv without the leading `kj`, with or without the root
/// `--confirm`/`--json` flags (both are stripped before the parse, the way
/// the builtin strips them before dispatch). Runs no handler and touches no
/// kernel state.
pub fn classify(argv: &[String]) -> Result<Effect, ClassifyError> {
    let mut argv = argv.to_vec();
    super::parse::strip_flag(&mut argv, &["--confirm", "--json"]);
    let matches = cached_kj_args_command().try_get_matches_from(argv)?;
    let parsed = KjArgs::from_arg_matches(&matches)?;
    Ok(parsed.command.effect())
}

/// A clone of the built `kj` clap tree, built once. Constructing it from
/// scratch chains one builder call per domain — around 40 of them, several
/// nested another level deep — into a single expression; in a debug build
/// that one call's own stack frame is large enough to matter next to a
/// deeply rc-nested `kj` invocation (`kj fork` re-entering kaish for the
/// new context's create lifecycle, say), where `classify` now runs at every
/// re-entry. [`crate::kj::kj_command`] and this module's own parsing both
/// clone this instead of rebuilding it.
pub(crate) fn cached_kj_args_command() -> clap::Command {
    static COMMAND: std::sync::OnceLock<clap::Command> = std::sync::OnceLock::new();
    COMMAND.get_or_init(KjArgs::command).clone()
}

/// The canonical leaf path an argv reaches, and the first positional after
/// it — used to name a `Destroy` latch by the leaf the caller reached
/// rather than the tokens they typed (`kj ctx archive` names itself
/// `kj context archive`, aliases resolved for free).
///
/// Walks `kj_command()` one token at a time, matching each against the
/// current node's subcommands (`Command::find_subcommand`, which checks
/// name and aliases both). The walk stops at the first token that is not a
/// subcommand of the current node; the path so far is the leaf, and the
/// first remaining token that does not start with `-` is the target.
///
/// Returns `None` when no subcommand matches at all — an unrecognized verb.
pub fn leaf_path(argv: &[String]) -> Option<(String, Option<String>)> {
    let mut argv = argv.to_vec();
    super::parse::strip_flag(&mut argv, &["--confirm", "--json"]);

    let mut current = super::kj_command();
    let mut path = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        match current.find_subcommand(argv[i].as_str()).cloned() {
            Some(sub) => {
                path.push(sub.get_name().to_string());
                current = sub;
                i += 1;
            }
            None => break,
        }
    }
    if path.is_empty() {
        return None;
    }
    let target = argv[i..].iter().find(|t| !t.starts_with('-')).cloned();
    Some((path.join(" "), target))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use std::collections::BTreeSet;

    /// Split a clause into argv the way this module's tests need it —
    /// `kj/reflect.rs` owns the definition (`every_live_leaf_classifies`
    /// lives there now too), aliased so call sites here read the same as
    /// before the move.
    use super::super::reflect::clause_argv as argv;

    /// Every `path -> [aliases]` leaf under a command, path space-joined
    /// without the root name.
    fn leaves(cmd: &clap::Command) -> BTreeSet<(String, Vec<String>)> {
        fn walk(cmd: &clap::Command, prefix: &[String], out: &mut BTreeSet<(String, Vec<String>)>) {
            for sub in cmd.get_subcommands() {
                let mut path = prefix.to_vec();
                path.push(sub.get_name().to_string());
                if sub.has_subcommands() {
                    walk(sub, &path, out);
                } else {
                    let mut aliases: Vec<String> = sub.get_all_aliases().map(str::to_string).collect();
                    aliases.sort();
                    out.insert((path.join(" "), aliases));
                }
            }
        }
        let mut out = BTreeSet::new();
        walk(cmd, &[], &mut out);
        out
    }

    #[test]
    fn root_flags_are_stripped_before_the_parse() {
        let with = classify(&argv("kj preset list --json")).unwrap();
        let trailing = classify(&argv("kj preset rm x --confirm")).unwrap();
        assert_eq!(with, Effect::Read);
        assert_eq!(trailing, Effect::Destroy);
    }

    #[test]
    fn an_unknown_verb_does_not_classify() {
        assert!(classify(&argv("kj frobnicate now")).is_err());
        assert!(classify(&argv("kj ${VERB} list")).is_err());
    }

    #[test]
    fn an_argument_can_change_the_effect() {
        assert_eq!(classify(&argv("kj block cat 019a2f3c")).unwrap(), Effect::Read);
        assert_eq!(classify(&argv("kj block cat 019a2f3c --out /tmp/x")).unwrap(), Effect::Write);
    }

    #[test]
    fn leaf_path_resolves_an_alias_to_its_canonical_name() {
        assert_eq!(
            leaf_path(&argv("kj ctx archive abc123")),
            Some(("context archive".to_string(), Some("abc123".to_string())))
        );
    }

    #[test]
    fn leaf_path_walks_a_nested_subcommand() {
        assert_eq!(
            leaf_path(&argv("kj backend model set myback gpt-4")),
            Some(("backend model set".to_string(), Some("myback".to_string())))
        );
    }

    #[test]
    fn leaf_path_with_no_positional_after_the_leaf() {
        assert_eq!(
            leaf_path(&argv("kj preset list")),
            Some(("preset list".to_string(), None))
        );
    }

    #[test]
    fn root_flags_stay_on_the_root_and_never_propagate_to_leaves() {
        let cmd = KjArgs::command();
        let root: Vec<_> = cmd.get_arguments().map(|a| a.get_id().as_str()).collect();
        assert!(root.contains(&"confirm") && root.contains(&"json"), "root args: {root:?}");
        for a in cmd.get_arguments() {
            assert!(!a.is_global_set(), "`--{}` must not be global: the schema reflects per leaf", a.get_id());
        }
        assert!(!leaves(&cmd).is_empty());
    }
}
