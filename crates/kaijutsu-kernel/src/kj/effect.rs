//! The `kj` verb class: every verb declares its effect where it is
//! declared, and that declaration is the only place the question is
//! answered. `docs/kj-verb-class.md` carries the build plan.
//!
//! Each domain module implements [`Classify`] for its `*Args` struct with an
//! exhaustive match over the parsed value, so a new variant with no arm is
//! a compile error, and an argument that changes the effect (`block cat
//! --out <path>`) is visible to the arm that classifies it.

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
