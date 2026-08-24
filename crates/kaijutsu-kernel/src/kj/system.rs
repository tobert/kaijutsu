//! System subcommand: what the kernel is doing right now.
//!
//! The operator's first question during an incident is "what is running?",
//! and until this existed the only way to answer it was to read the
//! journal. That is a poor instrument: the log tells you what happened,
//! not what is happening, and it costs a grep under time pressure.
//!
//! Background jobs are NOT reported yet: `background_exec` exposes only
//! `list_for_context`, so a kernel-wide roster means walking every live
//! context. Left out rather than half-reported — see `docs/issues.md`.
//!
//! `status` is deliberately read-only and deliberately first. The stopping
//! verbs that will join it here (`quiesce`, `resume`, and a terminal exit)
//! are only safe to use if you can see whether they took effect, so the
//! observation lands before the levers do.
//!
//! ## What "in flight" means
//!
//! A turn, not a request. The roster is the kernel's turn-liveness
//! registry — the same one `kj wait` reads — which covers a whole agentic
//! turn rather than any single model call. A context sitting in the gap
//! between a tool result and the next model block is still in flight, and
//! that is the honest answer: it is going to make another provider call.

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;

use super::{KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "system",
    about = "Inspect and control the running kernel.",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct SystemArgs {
    #[command(subcommand)]
    command: SystemCommand,
}

#[derive(Subcommand, Debug)]
enum SystemCommand {
    /// Report what the kernel is working on: turns in flight, and asks
    /// waiting on a human.
    Status,
}

impl KjDispatcher {
    pub(crate) async fn dispatch_system(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let parsed = match SystemArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj system: {e}"));
            }
        };

        match parsed.command {
            SystemCommand::Status => self.system_status(caller),
        }
    }

    fn system_status(&self, _caller: &KjCaller) -> KjResult {
        let turns = self.kernel().turns_in_flight();

        // Label and type make the roster readable — an operator deciding
        // what to stop needs to know a runaway coder from the musician
        // keeping a beat.
        let mut rows: Vec<serde_json::Value> = Vec::new();
        let mut lines: Vec<String> = Vec::new();

        lines.push(format!("turns in flight: {}", turns.len()));
        for id in &turns {
            let (label, ctype) = {
                let db = self.kernel_db().lock();
                match db.get_context(*id) {
                    Ok(Some(row)) => (
                        row.label.unwrap_or_else(|| id.short()),
                        row.context_type,
                    ),
                    // A turn on a context the DB cannot describe is worth
                    // showing, not hiding: it is exactly the shape of a bug.
                    _ => (id.short(), "?".to_string()),
                }
            };
            lines.push(format!("  {}  {:<20} {}", id.short(), label, ctype));
            rows.push(serde_json::json!({
                "context_id": id.to_string(),
                "label": label,
                "context_type": ctype,
            }));
        }

        // Asks are bounded by human time, not machine time, so they are
        // reported next to the turns rather than mixed in: a pending ask is
        // not work the kernel can finish.
        let pending = {
            let db = self.kernel_db().lock();
            db.list_pending_asks().unwrap_or_default()
        };
        lines.push(format!("asks waiting on a human: {}", pending.len()));

        let data = serde_json::json!({
            "turns_in_flight": rows,
            "asks_pending": pending.len(),
        });

        KjResult::ok_ephemeral_with_data(lines.join("\n"), ContentType::Plain, data)
    }
}
