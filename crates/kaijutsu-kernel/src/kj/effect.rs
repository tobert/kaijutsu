//! The `kj` verb class: every verb declares its effect where it is
//! declared, and that declaration is the only place the question is
//! answered. `docs/kj-verb-class.md` carries the build plan.
//!
//! Each domain module implements [`Classify`] for its `*Args` struct with an
//! exhaustive match over the parsed value, so a new variant with no arm is
//! a compile error, and an argument that changes the effect (`block cat
//! --out <path>`) is visible to the arm that classifies it.

use clap::{Parser, Subcommand};
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use std::collections::BTreeSet;

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
