//! One command outcome, projected into blocks, durable receipts, and kaish jobs.

use kaish_kernel::interpreter::ExecResult;
use kaijutsu_types::{OutputData, Status};
use kaijutsu_types::shell_envelope::{ShellEnvelope, ShellStatus};
use serde::{Deserialize, Serialize};

use crate::mcp::{KernelToolResult, ShellHookVerdict};
use super::command_result::{block_output_data, shell_hook_result_text, shell_result_to_envelope};

/// What execution produced before result hooks or persistence handling.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CommandExecution {
    NotRun,
    Completed(ExecResult),
    Rejected(String),
    Fault(String),
}

/// A hook may replace a result or stop its publication. Neither changes what ran.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CommandHookEffect {
    Replacement(KernelToolResult),
    Refused { reason: String, waiting: bool, ask_id: Option<String> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandOutcome {
    pub execution: CommandExecution,
    pub hook: Option<CommandHookEffect>,
    pub settlement_error: Option<String>,
    pub elapsed_ms: u64,
}

impl CommandOutcome {
    pub fn new(execution: CommandExecution, elapsed_ms: u64) -> Self {
        Self { execution, hook: None, settlement_error: None, elapsed_ms }
    }

    pub fn from_execution(result: Result<ExecResult, kaish_kernel::KernelError>, elapsed_ms: u64) -> Self {
        let execution = match result {
            Ok(result) => CommandExecution::Completed(result),
            Err(error) if error.is_rejected() => CommandExecution::Rejected(error.to_string()),
            Err(error) => CommandExecution::Fault(error.to_string()),
        };
        Self::new(execution, elapsed_ms)
    }

    pub fn apply_hook(&mut self, verdict: ShellHookVerdict) {
        self.hook = match verdict {
            ShellHookVerdict::Proceed => None,
            ShellHookVerdict::ShortCircuit(result) => Some(CommandHookEffect::Replacement(result)),
            ShellHookVerdict::Denied(error) => Some(CommandHookEffect::Refused {
                reason: error.to_string(),
                waiting: error.settled_block_status() == Status::Waiting,
                ask_id: error.as_refusal().and_then(|refusal| refusal.ask_id().map(str::to_owned)),
            }),
        };
    }

    /// The public result reports a physical exit only when it still describes
    /// the executed command. Hook replacements have their own status and data.
    pub fn envelope(&self) -> ShellEnvelope {
        let mut envelope = match &self.hook {
            Some(CommandHookEffect::Replacement(result)) => {
                let mut envelope = ShellEnvelope::new(if result.is_error { ShellStatus::Error } else { ShellStatus::Done });
                envelope.stdout = shell_hook_result_text(result);
                envelope.data = result.structured.clone();
                envelope.content_type = Some("text/plain".into());
                envelope.ephemeral = Some(false);
                envelope
            }
            Some(CommandHookEffect::Refused { reason, waiting, ask_id }) => {
                let mut envelope = ShellEnvelope::new(if *waiting { ShellStatus::Waiting } else { ShellStatus::Error });
                envelope.error = Some(reason.clone());
                envelope.ask_id = ask_id.clone();
                envelope
            }
            None => match &self.execution {
                CommandExecution::Completed(result) => shell_result_to_envelope(result.clone(), self.elapsed_ms),
                CommandExecution::NotRun => {
                    let mut envelope = ShellEnvelope::new(ShellStatus::Error);
                    envelope.error = Some("command was not run".into());
                    envelope
                }
                CommandExecution::Rejected(reason) | CommandExecution::Fault(reason) => {
                    let mut envelope = ShellEnvelope::new(if matches!(&self.execution, CommandExecution::Rejected(_)) {
                        ShellStatus::Rejected
                    } else { ShellStatus::Error });
                    envelope.error = Some(reason.clone());
                    envelope
                }
            },
        };
        envelope.elapsed_ms = Some(self.elapsed_ms);
        if let Some(error) = &self.settlement_error {
            envelope.status = ShellStatus::Error;
            envelope.error = Some(match envelope.error {
                Some(prior) => format!("{prior}\n{error}"),
                None => error.clone(),
            });
        }
        envelope
    }

    pub fn block_status(&self) -> Status {
        match self.envelope().status {
            ShellStatus::Done => Status::Done,
            ShellStatus::Running => Status::Running,
            ShellStatus::Waiting => Status::Waiting,
            _ => Status::Error,
        }
    }

    pub fn output_data(&self) -> Option<OutputData> {
        match &self.hook {
            Some(CommandHookEffect::Replacement(result)) => result.structured.clone()
                .map(|data| OutputData::new().with_rich_json(data)),
            Some(CommandHookEffect::Refused { .. }) => None,
            None => match &self.execution {
                CommandExecution::Completed(result) => block_output_data(result),
                _ => None,
            },
        }
    }

    /// Kaish jobs require an integer control-flow code. Preserve the executed
    /// result intact when applicable. Synthetic results use 0/1 for job control
    /// and identify themselves in baggage; receipts never claim a physical exit.
    pub fn job_result(&self) -> ExecResult {
        if self.hook.is_none() && self.settlement_error.is_none()
            && let CommandExecution::Completed(result) = &self.execution
        {
            return result.clone();
        }
        let envelope = self.envelope();
        let code = if envelope.is_error() { 1 } else { 0 };
        let mut result = ExecResult::success(envelope.stdout);
        result.code = code;
        result.err = envelope.stderr;
        if let Some(error) = envelope.error {
            if !result.err.is_empty() && !result.err.ends_with('\n') { result.err.push('\n'); }
            result.err.push_str(&error);
        }
        result.data = envelope.data.map(kaish_kernel::ast::Value::Json);
        result.content_type = envelope.content_type;
        result.baggage.insert("kaijutsu.synthetic".into(), "true".into());
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::ToolContent;

    #[test]
    fn spill_keeps_the_physical_exit_and_job_payload() {
        for exit in [0, 2, 3, 7] {
            let mut result = ExecResult::success_with_data("partial", kaish_kernel::ast::Value::Json(serde_json::json!([1, 2])));
            result.code = 3;
            result.original_code = Some(exit);
            result.did_spill = true;
            result.content_type = Some("text/markdown".into());
            result.baggage.insert("kaijutsu.ephemeral".into(), "true".into());
            let outcome = CommandOutcome::new(CommandExecution::Completed(result.clone()), 42);
            let envelope = outcome.envelope();
            assert_eq!(envelope.exit_code, Some(exit));
            assert_eq!(envelope.is_error(), exit != 0);
            assert_eq!(outcome.block_status(), if exit == 0 { Status::Done } else { Status::Error });
            assert_eq!(envelope.did_spill, Some(true));
            assert_eq!(envelope.data, Some(serde_json::json!([1, 2])));
            assert_eq!(envelope.content_type.as_deref(), Some("text/markdown"));
            assert_eq!(envelope.ephemeral, Some(true));
            assert_eq!(envelope.elapsed_ms, Some(42));
            assert_eq!(outcome.job_result(), result);
        }
    }

    #[test]
    fn replacements_keep_raw_execution_without_leaking_its_metadata() {
        let mut raw = ExecResult::failure(7, "raw diagnostic");
        raw.content_type = Some("text/markdown".into());
        raw.did_spill = true;
        raw.original_code = Some(7);
        raw.data = Some(kaish_kernel::ast::Value::Json(serde_json::json!({"old": true})));
        raw.baggage.insert("kaijutsu.ephemeral".into(), "true".into());
        let mut outcome = CommandOutcome::new(CommandExecution::Completed(raw.clone()), 9);
        let data = serde_json::json!({"new": true});
        outcome.apply_hook(ShellHookVerdict::ShortCircuit(KernelToolResult {
            is_error: false, content: vec![ToolContent::Json(data.clone())], structured: Some(data.clone()),
        }));
        let restored: CommandOutcome = serde_json::from_str(&serde_json::to_string(&outcome).unwrap()).unwrap();
        let CommandExecution::Completed(preserved) = &restored.execution else { panic!("raw execution was lost") };
        assert_eq!(preserved, &raw);
        let envelope = restored.envelope();
        assert_eq!(envelope.status, ShellStatus::Done);
        assert_eq!(envelope.exit_code, None);
        assert_eq!(envelope.did_spill, None);
        assert!(envelope.stderr.is_empty());
        assert_eq!(envelope.ephemeral, Some(false));
        assert_eq!(envelope.content_type.as_deref(), Some("text/plain"));
        assert_eq!(envelope.data, Some(data.clone()));
        assert_eq!(restored.output_data().unwrap().rich_json, Some(data.clone()));
        let job = restored.job_result();
        assert_eq!(job.code, 0);
        assert_eq!(job.data, Some(kaish_kernel::ast::Value::Json(data)));
        assert_eq!(job.baggage.get("kaijutsu.synthetic").map(String::as_str), Some("true"));
        assert!(job.err.is_empty());
    }

    #[test]
    fn rejection_and_execution_fault_have_no_physical_exit() {
        let rejected = CommandOutcome::new(CommandExecution::Rejected("invalid syntax".into()), 1);
        let fault = CommandOutcome::new(CommandExecution::Fault("execution failed".into()), 2);
        assert_eq!(rejected.envelope().status, ShellStatus::Rejected);
        assert_eq!(fault.envelope().status, ShellStatus::Error);
        for outcome in [rejected, fault] {
            assert_eq!(outcome.envelope().exit_code, None);
            assert_eq!(outcome.block_status(), Status::Error);
            assert!(outcome.job_result().code != 0);
        }
    }
}
