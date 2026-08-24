//! `ps` — kaijutsu's process table, shadowing kaish's host-listing one.
//!
//! kaish ships a `ps` that enumerates host processes. Inside a kaijutsu
//! shell that is the wrong answer to the right question: a seat asking
//! "what is running?" means the kernel's own landscape — turns in flight
//! and the child processes kaijutsu spawned — not every process on the
//! machine (Amy, 2026-08-24: *"drop the kaish ps in favor of system ps
//! when ps is needed for that, which generally kaijutsu agents don't need
//! to and shouldn't see"*).
//!
//! **How the shadow works, and why it needs no kaish change.**
//! `ToolRegistry::register` is a `HashMap::insert` keyed by `tool.name()`,
//! so registering a tool called `ps` replaces kaish's. `fg` in
//! `vi_builtin.rs` is the same move and the precedent for it. This is
//! shadowing, not removal: kaish's `ps` is untouched for every other
//! embedder, which matters because kaish's surface is a promise to its own
//! users (CLAUDE.md, "kaish and kaibo are not this").
//!
//! The narrowing is an ergonomic nudge, not a security control — a seat
//! that cannot list host processes is one that will not accidentally reason
//! about them. Every player is still inside one trust boundary
//! (`docs/instrument-design.md`, "Many hands, one trust boundary").
//!
//! One surface, two front doors: this and `kj system ps` render from the
//! same dispatcher method, so they cannot drift.

use std::sync::Arc;

use async_trait::async_trait;
use kaish_kernel::interpreter::ExecResult;
use kaish_kernel::tools::{ToolArgs, ToolCtx, ToolSchema};
use kaish_kernel::Tool;

use crate::kj::{KjDispatcher, KjResult};

pub struct PsBuiltin {
    dispatcher: Arc<KjDispatcher>,
}

impl PsBuiltin {
    pub fn new(dispatcher: Arc<KjDispatcher>) -> Self {
        Self { dispatcher }
    }
}

#[async_trait]
impl Tool for PsBuiltin {
    fn name(&self) -> &str {
        "ps"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "ps",
            "List this kernel's processes: turns in flight and the child \
             processes kaijutsu spawned, longest-running first. Not the \
             host's process table — use `kj system ps` for the same view.",
        )
        .example("What is this kernel doing right now", "ps")
    }

    async fn execute(&self, _args: ToolArgs, _ctx: &mut dyn ToolCtx) -> ExecResult {
        // Routed through the same `kj system ps` the verb runs, so the two
        // front doors cannot report different things.
        match self.dispatcher.system_ps_public() {
            KjResult::Ok { message, data, .. } => {
                let mut result = ExecResult::success(message);
                if let Some(d) = data {
                    result.data = Some(kaish_kernel::interpreter::json_to_value(d));
                }
                result
            }
            KjResult::Err(e) => ExecResult::failure(1, e),
            // `system ps` returns neither of the other KjResult shapes; if
            // that ever changes, say so rather than inventing an answer.
            other => ExecResult::failure(
                1,
                format!("ps: unexpected result shape from `kj system ps`: {other:?}"),
            ),
        }
    }
}
