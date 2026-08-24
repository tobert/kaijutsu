//! `ps` — a sign pointing at `kj system ps`, never the roster itself.
//!
//! kaish ships a `ps` that enumerates host processes. Inside a kaijutsu
//! shell that is the wrong answer to the right question: a seat asking
//! "what is running?" means the kernel's own landscape, not every process
//! on the machine (Amy, 2026-08-24: *"drop the kaish ps in favor of system
//! ps when ps is needed for that, which generally kaijutsu agents don't
//! need to and shouldn't see"*).
//!
//! **This tool never renders the table.** `kj system ps` is the one door,
//! because it is the one that carries the capability check. If `ps` also
//! rendered the roster it would be a way around that check, which is
//! exactly the shape of mistake the gate exists to prevent. So `ps` always
//! fails, and its only job is to say what to do instead.
//!
//! Two messages, because the honest next step differs:
//!
//! - A shell that can reach `kj` is told the full path.
//! - A restricted shell — no host exec, no `kj system` — is told plainly
//!   that it cannot, with no alternative to chase.
//!
//! **Why a shadow rather than removal.** `ToolRegistry::register` is a
//! `HashMap::insert` keyed by `tool.name()`, and the registry has no
//! `remove`. A kaijutsu shell therefore cannot simply *lack* `ps`; leaving
//! it unregistered means falling through to kaish's host listing, which is
//! strictly worse than a refusal. `fg` in `vi_builtin.rs` is the same move
//! and the precedent for it. kaish's own `ps` is untouched for every other
//! embedder, which matters because that surface is a promise to them
//! (CLAUDE.md, "kaish and kaibo are not this").
//!
//! This is an ergonomic nudge, not a security control: every player is
//! still inside one trust boundary (`docs/instrument-design.md`, "Many
//! hands, one trust boundary"). The point is that a seat will not reason
//! about host processes by accident, not that it is being defended against.

use kaish_kernel::interpreter::ExecResult;
use kaish_kernel::tools::{ToolArgs, ToolCtx, ToolSchema};
use kaish_kernel::Tool;

pub struct PsBuiltin {
    /// Whether this shell can reach `kj` at all. A restricted shell gets the
    /// flat refusal; everything else gets pointed at `kj system ps`.
    has_kj: bool,
}

impl PsBuiltin {
    pub fn new(has_kj: bool) -> Self {
        Self { has_kj }
    }
}

#[async_trait::async_trait]
impl Tool for PsBuiltin {
    fn name(&self) -> &str {
        "ps"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "ps",
            "Not available here. This kernel's process table is `kj system ps`, \
             which needs the `system` authority.",
        )
    }

    async fn execute(&self, _args: ToolArgs, _ctx: &mut dyn ToolCtx) -> ExecResult {
        // Exit 1, never 0 with text: a caller that checks the status must
        // see this fail rather than read an empty roster as "nothing running".
        if self.has_kj {
            ExecResult::failure(
                1,
                "ps is not available in a kaijutsu shell — it would list the host's \
                 processes, not this kernel's. For the kernel's own turns and jobs, \
                 run `kj system ps` (needs the `system` authority; drift the question \
                 to a seat that holds it if yours does not)."
                    .to_string(),
            )
        } else {
            ExecResult::failure(
                1,
                "ps is not available in this shell.".to_string(),
            )
        }
    }
}
