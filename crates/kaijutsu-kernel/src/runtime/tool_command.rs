//! Retained shell-tool execution. The broker admits a call; this owner applies
//! result hooks when execution finishes and settles its optional receipt.

use std::sync::Arc;
use kaijutsu_types::{ContentType, PrincipalId, Role, BlockKind, Status};
use kaijutsu_types::shell_envelope::{ShellEnvelope, ShellStatus};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use crate::mcp::{Broker, CallContext, KernelCallParams, KernelToolResult, McpError, McpResult};
use crate::shell_operations::ShellOperationReceipt;
use super::command::{self, CommandContextSwitch, CommandHooks, CommandJobOutput, CommandRunOptions, ShellStateWriteBack};
use super::command_outcome::{CommandExecution, CommandOutcome};
use super::command_result::shell_envelope_to_tool_result;
use super::embedded_kaish::EmbeddedKaish;

pub(crate) fn create_operation(
    kernel: &crate::Kernel, call: &CallContext, source: &str, status: Status,
) -> Result<ShellOperationReceipt, String> {
    let blocks = kernel.blocks();
    let command = blocks.insert_block_as(call.context_id, None, None, Role::Tool, BlockKind::ToolCall,
        source, status, ContentType::Plain, Some(call.actor_id)).map_err(|e| e.to_string())?;
    let output = blocks.insert_block_as(call.context_id, Some(&command), Some(&command), Role::Tool,
        BlockKind::ToolResult, String::new(), status, ContentType::Plain, Some(PrincipalId::system()))
        .map_err(|e| e.to_string())?;
    for block in [&command, &output] { blocks.set_excluded(call.context_id, block, true).map_err(|e| e.to_string())?; }
    let epoch = kernel.kernel_db().lock().continuation_epoch(call.context_id).map_err(|e| e.to_string())?;
    kernel.shell_operations().register(call.context_id, call.principal_id, call.actor_id, command, output, source, epoch)
}

pub(crate) struct ToolCommand {
    pub kernel: Arc<crate::Kernel>,
    pub broker: Arc<Broker>,
    pub kaish: EmbeddedKaish,
    pub params: KernelCallParams,
    pub call: CallContext,
    pub code: String,
    pub stdin: Option<String>,
    pub foreground: bool,
    pub read_only: bool,
}

impl ToolCommand {
    pub(crate) async fn execute(self, cancel: CancellationToken) -> McpResult<KernelToolResult> {
        let receipt = if self.foreground { None } else {
            Some(create_operation(&self.kernel, &self.call, &self.code, Status::Running).map_err(McpError::Protocol)?)
        };
        let policy = self.broker.policy_of(&self.params.instance).await.unwrap_or_default();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (reply, completed) = tokio::sync::oneshot::channel();
        let (notices, mut reviews) = tokio::sync::mpsc::unbounded_channel();
        let completion_receipt = receipt.clone();
        let failure_kernel = self.kernel.clone();
        let context = self.call.context_id;
        let foreground = self.foreground;
        let task_cancel = if foreground { cancel.child_token() } else { CancellationToken::new() };
        let cancel_guard = task_cancel.clone().drop_guard();
        let span = tracing::Span::current();
        let hook_depth = crate::mcp::broker::current_hook_depth();
        let host = self.kernel.clone();
        let started = host.spawn_command(move |shutdown| crate::mcp::broker::inherit_hook_depth(hook_depth, async move {
                let stop_command = task_cancel.clone();
                let run = CommandRunOptions { stdin: self.stdin,
                    context_switch: CommandContextSwitch::Pinned,
                    hooks: Some(CommandHooks { broker: &self.broker, params: &self.params, max_result_bytes: policy.max_result_bytes }),
                    state_writeback: if self.read_only { ShellStateWriteBack::Discard } else { ShellStateWriteBack::Persist },
                    job_output: if foreground { CommandJobOutput::Settled } else { CommandJobOutput::LiveExecution },
                    cancel: Some(task_cancel), job_ready: Some(ready_tx), review_notices: Some(notices),
                };
                let execute = async { match &completion_receipt {
                    Some(receipt) => command::run_into_blocks(&self.kaish, &self.code, context,
                        &receipt.command_block_id, &receipt.output_block_id, &self.kernel, &self.call, run).await,
                    None => command::run_without_blocks(&self.kaish, &self.code, &self.kernel, &self.call,
                        kaish_kernel::ExecuteOptions::default(), run).await,
                } };
                tokio::pin!(execute);
                let outcome = tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => {
                        stop_command.cancel();
                        execute.await
                    }
                    outcome = &mut execute => outcome,
                };
                if let Some(receipt) = &completion_receipt {
                    match &outcome {
                        Ok(_) => if let Err(error) = self.kernel.notify_async_shell_completion(
                            &receipt.operation_id, context, self.call.principal_id, self.call.actor_id).await {
                            tracing::error!("shell completion notification failed: {error}");
                        },
                        Err(error) => tracing::error!("shell operation {} settlement failed: {error}", receipt.operation_id),
                    }
                }
                let _ = reply.send(outcome);
            }.instrument(span)));
        if let Err(error) = started {
            if let Some(receipt) = &receipt {
                let mut outcome = CommandOutcome::new(CommandExecution::NotRun, 0);
                outcome.settlement_error = Some(format!("shell worker could not start: {error}"));
                command::settle_outcome(&failure_kernel, context, &receipt.command_block_id, &receipt.output_block_id, &outcome)
                    .map_err(McpError::Protocol)?;
            }
            return Err(McpError::Protocol(format!("shell worker could not start: {error}")));
        }
        if let Some(receipt) = receipt {
            if ready_rx.await.is_err() {
                let outcome = completed.await.map_err(|_| McpError::Protocol("shell worker stopped before admission".into()))?
                    .map_err(McpError::Protocol)?;
                let mut envelope = outcome.envelope();
                envelope.operation_id = Some(receipt.operation_id);
                envelope.block_id = Some(receipt.output_block_id.to_key());
                return Ok(shell_envelope_to_tool_result(envelope));
            }
            cancel_guard.disarm();
            let mut envelope = ShellEnvelope::new(ShellStatus::Running);
            envelope.operation_id = Some(receipt.operation_id);
            envelope.block_id = Some(receipt.output_block_id.to_key());
            return Ok(shell_envelope_to_tool_result(envelope));
        }
        tokio::select! {
            Some(refusal) = reviews.recv() => {
                cancel_guard.disarm();
                Err(McpError::Refused(refusal))
            }
            result = completed => {
                let outcome = result.map_err(|_| McpError::Protocol("shell worker stopped before completion".into()))?
                    .map_err(McpError::Protocol)?;
                if let Some(refusal) = outcome.refusal() { return Err(McpError::Refused(refusal.clone())); }
                Ok(shell_envelope_to_tool_result(outcome.envelope()))
            }
        }
    }
}
