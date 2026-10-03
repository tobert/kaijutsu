//! `ps` — this context's own shell operations, never the host's processes.
//!
//! kaish ships a `ps` that enumerates host processes. Inside a kaijutsu
//! shell that is the wrong answer to the right question: a seat asking
//! "what is running?" means its own work, not every process on the machine,
//! which kaijutsu agents generally do not need and should not see.
//!
//! So `ps` lists the shell operations this context started — background
//! calls, calls waiting for approval, and with `-a` the finished ones — from
//! the same registry `list_shell_operations` and `kj wait --operation` read.
//! It needs no `system` authority because it never leaves the context. The
//! kernel-wide roster stays `kj system ps`, which carries that check.
//!
//! It does not list host processes a context's commands started. Nothing
//! tracks those yet (`docs/issues.md`, "Nothing tracks the OS processes a
//! context starts").
//!
//! **Why a shadow rather than removal.** `ToolRegistry::register` is a
//! `HashMap::insert` keyed by `tool.name()`, and the registry has no
//! `remove`. Leaving `ps` unregistered would fall through to kaish's host
//! listing. kaish's own `ps` is untouched for every other embedder.

use std::sync::Arc;

use kaijutsu_types::ContextId;
use kaish_kernel::interpreter::ExecResult;
use kaish_kernel::tools::{ToolArgs, ToolCtx, ToolSchema};
use kaish_kernel::Tool;

use crate::shell_operations::{ShellOperationRegistry, ShellOperationState};

pub struct PsBuiltin {
    operations: Arc<ShellOperationRegistry>,
    context: ContextId,
}

impl PsBuiltin {
    pub fn new(operations: Arc<ShellOperationRegistry>, context: ContextId) -> Self {
        Self { operations, context }
    }
}

const USAGE: &str = "usage: ps [-a]. ps lists this context's unfinished shell operations; -a adds finished ones.";

#[async_trait::async_trait]
impl Tool for PsBuiltin {
    fn name(&self) -> &str {
        "ps"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "ps",
            "List this context's shell operations: background calls and calls waiting \
             for approval. -a also lists finished ones. Host processes are not listed.",
        )
    }

    async fn execute(&self, args: ToolArgs, _ctx: &mut dyn ToolCtx) -> ExecResult {
        let argv = match args.to_argv() {
            Ok(argv) => argv,
            Err(e) => return ExecResult::failure(2, format!("ps: {e}. {USAGE}")),
        };
        let mut all = false;
        for arg in &argv {
            match arg.as_str() {
                "-a" | "--all" => all = true,
                other => return ExecResult::failure(2, format!("ps: unknown argument '{other}'. {USAGE}")),
            }
        }
        match self.operations.list_for_context(self.context) {
            Ok(entries) => ExecResult::success(render(&entries, all, now_ms())),
            Err(e) => ExecResult::failure(1, format!("ps: could not read this context's shell operations: {e}")),
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The status `list_shell_operations` reports for an operation.
fn status(entry: &ShellOperationState) -> &'static str {
    match &entry.envelope {
        Some(envelope) => envelope.status.as_str(),
        None if entry.receipt.ask_id.is_some() && entry.receipt.job_id.is_none() => "waiting",
        None => "running",
    }
}

fn elapsed(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

fn render(entries: &[ShellOperationState], all: bool, now: i64) -> String {
    if entries.is_empty() {
        return "no shell operations in this context\n".to_string();
    }
    let finished = entries.iter().filter(|e| e.completed_at.is_some()).count();
    let shown: Vec<&ShellOperationState> = entries.iter().filter(|e| all || e.completed_at.is_none()).collect();
    let mut out = String::new();
    if !shown.is_empty() {
        out.push_str(&format!("{:<36}  {:<8}  {:>7}  {:>4}  COMMAND\n", "OPERATION", "STATUS", "ELAPSED", "EXIT"));
        for entry in shown {
            let end = entry.completed_at.unwrap_or(now);
            let exit = entry.envelope.as_ref().and_then(|e| e.exit_code).map_or("-".to_string(), |c| c.to_string());
            let command = entry.source.lines().next().unwrap_or("");
            out.push_str(&format!(
                "{:<36}  {:<8}  {:>7}  {:>4}  {}\n",
                entry.receipt.operation_id, status(entry), elapsed(end - entry.created_at), exit, command
            ));
        }
    } else {
        out.push_str("no unfinished shell operations in this context\n");
    }
    if !all && finished > 0 {
        out.push_str(&format!("{finished} finished operation(s) not shown; ps -a lists them\n"));
    }
    out
}
