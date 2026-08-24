//! System subcommand: what the kernel is doing right now.
//!
//! The operator's first question during an incident is "what is running?",
//! and until this existed the only way to answer it was to read the
//! journal. That is the wrong instrument: the log says what happened, not
//! what is happening, and it costs a search under time pressure.
//!
//! `status` answers it in one line; `ps` gives the roster. Both are
//! read-only, and deliberately land before the stopping verbs
//! (`quiesce`, `resume`, `seppuku` — `docs/system-verbs.md`), because
//! those are only safe to use if you can see whether they took effect.
//!
//! ## Kaijutsu's processes, not the host's
//!
//! `kj system ps` reports the kernel's own landscape: turns in flight and
//! the child processes kaijutsu spawned. It is not `ps(1)` and will never
//! grow into it — a seat inside the kernel has no business enumerating the
//! host, and kaijutsu shells shadow kaish's host-listing `ps` with this
//! view for exactly that reason.
//!
//! ## What "in flight" means
//!
//! A turn, not a request. The roster is the kernel's turn-liveness
//! registry — the same one `kj wait` reads — which covers a whole agentic
//! turn rather than any single model call. A context sitting in the gap
//! between a tool result and the next model block is still in flight, and
//! that is the honest answer: it is about to make another provider call.
//!
//! Elapsed runs from the FIRST mark, so an autonomous turn's clock starts
//! when it was requested rather than when a driver picked it up. Queue
//! time is delay worth seeing, not noise to hide.

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
    /// Summarize what the kernel is working on: how many turns are in
    /// flight and how many asks are waiting on a human.
    Status,
    /// List the kernel's own processes: turns in flight and the child
    /// processes kaijutsu spawned, longest-running first. Not the host's
    /// process table.
    Ps,
}

/// Render a duration the way a process table should: short, fixed-ish
/// width, and never more precision than the reader can use.
fn elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}.{}s", secs, d.subsec_millis() / 100)
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
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

        // The whole noun is gated, `ps` and `status` included. A seat doing
        // ordinary work has no business reading the kernel's process table,
        // and one that needs an answer drifts the question to a seat that
        // holds this (docs/system-verbs.md, "Who holds `kj system`").
        if let Err(denied) = self.require_cap(caller, crate::mcp::Capability::System, "system") {
            return denied;
        }

        match parsed.command {
            SystemCommand::Status => self.system_status(caller),
            SystemCommand::Ps => self.system_ps(caller),
        }
    }

    /// Label and seat type for a context, for display only. A context the
    /// DB cannot describe is shown rather than hidden: a turn running on
    /// one is exactly the shape of a bug.
    fn describe_context(&self, id: kaijutsu_types::ContextId) -> (String, String) {
        let db = self.kernel_db().lock();
        match db.get_context(id) {
            Ok(Some(row)) => (row.label.unwrap_or_else(|| id.short()), row.context_type),
            _ => (id.short(), "?".to_string()),
        }
    }

    fn system_status(&self, _caller: &KjCaller) -> KjResult {
        let turns = self.kernel().turns_in_flight();
        let pending = {
            let db = self.kernel_db().lock();
            db.list_pending_asks().unwrap_or_default()
        };

        // Turns and asks are counted separately, never summed: a turn is
        // bounded by machine time and will end on its own, an ask is
        // bounded by human time and will not.
        let lines = vec![
            format!("turns in flight: {}", turns.len()),
            format!("asks waiting on a human: {}", pending.len()),
        ];
        let data = serde_json::json!({
            "turns_in_flight": turns.len(),
            "asks_pending": pending.len(),
        });
        KjResult::ok_ephemeral_with_data(lines.join("\n"), ContentType::Plain, data)
    }

    /// The same roster `kj system ps` renders, for the `ps` shell builtin
    /// (`runtime/ps_builtin.rs`). One implementation, two front doors.
    pub fn system_ps_public(&self) -> KjResult {
        self.system_ps_inner()
    }

    fn system_ps(&self, _caller: &KjCaller) -> KjResult {
        self.system_ps_inner()
    }

    fn system_ps_inner(&self) -> KjResult {
        let mut lines = vec![format!(
            "{:<6} {:<10} {:<20} {:<9} {:>8}  {}",
            "KIND", "ID", "CONTEXT", "TYPE", "ELAPSED", "DETAIL"
        )];
        let mut rows: Vec<serde_json::Value> = Vec::new();

        for (id, age) in self.kernel().turns_in_flight() {
            let (label, ctype) = self.describe_context(id);
            lines.push(format!(
                "{:<6} {:<10} {:<20} {:<9} {:>8}  {}",
                "turn",
                id.short(),
                label,
                ctype,
                elapsed(age),
                "-"
            ));
            rows.push(serde_json::json!({
                "kind": "turn",
                "id": id.to_string(),
                "context": label,
                "context_type": ctype,
                "elapsed_ms": age.as_millis() as u64,
            }));
        }

        // Child processes kaijutsu spawned. `summary_by_context` is the one
        // kernel-wide read the registry offers, so it names the contexts and
        // `list_for_context` supplies each one's per-process detail.
        let summaries = self.kernel().background_processes().summary_by_context();
        for context_id in summaries.keys() {
            let (label, ctype) = self.describe_context(*context_id);
            for job in self
                .kernel()
                .background_processes()
                .list_for_context(*context_id)
            {
                if job.status != "running" {
                    continue;
                }
                let age = std::time::Duration::from_millis(
                    now_unix_ms().saturating_sub(job.started_at_unix_ms),
                );
                lines.push(format!(
                    "{:<6} {:<10} {:<20} {:<9} {:>8}  pid {}  {}",
                    "job",
                    &job.id,
                    label,
                    ctype,
                    elapsed(age),
                    job.pid,
                    job.command
                ));
                rows.push(serde_json::json!({
                    "kind": "job",
                    "id": job.id,
                    "pid": job.pid,
                    "context": label,
                    "context_type": ctype,
                    "elapsed_ms": age.as_millis() as u64,
                    "command": job.command,
                }));
            }
        }

        if rows.is_empty() {
            lines.push("(nothing running)".to_string());
        }

        KjResult::ok_ephemeral_with_data(
            lines.join("\n"),
            ContentType::Plain,
            serde_json::Value::Array(rows),
        )
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
