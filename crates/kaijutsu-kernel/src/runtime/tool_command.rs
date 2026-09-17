//! Retained shell-tool execution. The broker admits a call; this owner applies
//! result hooks when execution finishes and settles its optional receipt.

use std::sync::Arc;
use kaijutsu_types::Role;
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
    kernel: &crate::Kernel, call: &CallContext, source: &str, ask: Option<&str>,
) -> Result<ShellOperationReceipt, String> {
    kernel.blocks().start_shell_operation(crate::shell_operations::ShellOperationStart {
        context: call.context_id, principal: call.principal_id, actor: call.actor_id,
        source, tool: "shell", input: serde_json::json!({"command": source}),
        kind: kaijutsu_types::ToolKind::Shell, role: Role::Tool, excluded: true,
        status: if ask.is_some() { kaijutsu_types::Status::Waiting } else { kaijutsu_types::Status::Running },
        ask: ask.map(|ask| (ask, crate::PairOwner::Turn)),
    }).map_err(|error| error.to_string())
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
            Some(create_operation(&self.kernel, &self.call, &self.code, None).map_err(McpError::Protocol)?)
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
        let started = host.spawn_runtime_task(move |shutdown| crate::mcp::broker::inherit_hook_depth(hook_depth, async move {
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
                outcome.settlement_error = Some(format!("runtime worker could not start: {error}"));
                command::settle_outcome(&failure_kernel, context, &receipt.command_block_id, &receipt.output_block_id, &outcome)
                    .map_err(McpError::Protocol)?;
            }
            return Err(McpError::Protocol(format!("runtime worker could not start: {error}")));
        }
        if let Some(receipt) = receipt {
            if ready_rx.await.is_err() {
                let outcome = completed.await.map_err(|_| McpError::Protocol("runtime worker stopped before admission".into()))?
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
                let outcome = result.map_err(|_| McpError::Protocol("runtime worker stopped before completion".into()))?
                    .map_err(McpError::Protocol)?;
                if let Some(refusal) = outcome.refusal() { return Err(McpError::Refused(refusal.clone())); }
                Ok(shell_envelope_to_tool_result(outcome.envelope()))
            }
        }
    }
}

#[cfg(test)]
mod setup_tests {
    use super::*;
    use kaijutsu_types::{ContextId, PrincipalId, Status};

    async fn fixture() -> (tempfile::TempDir, crate::Kernel, ContextId) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(parking_lot::Mutex::new(crate::KernelDb::open(dir.path().join("kernel.db")).unwrap()));
        let workspace = db.lock().get_or_create_default_workspace(PrincipalId::system()).unwrap();
        let flows = Arc::new(crate::flows::FlowBus::new(64));
        let blocks = Arc::new(crate::block_store::BlockStore::with_db_and_flows(db.clone(), workspace, PrincipalId::system(), flows.clone()));
        let kernel = crate::Kernel::with_flows(kaijutsu_types::KernelId::new(), "operation-setup", flows, dir.path(), blocks, db).await;
        let context = ContextId::new();
        kernel.blocks().create_document(context, crate::DocumentKind::Conversation, None).unwrap();
        (dir, kernel, context)
    }

    #[tokio::test]
    async fn operation_registration_failure_publishes_no_partial_pair() {
        let (_dir, kernel, context) = fixture().await;
        let mut events = kernel.block_flows().subscribe("block.*");
        kernel.kernel_db().lock().conn_for_ledger().execute_batch(
            "CREATE TRIGGER reject_operation BEFORE INSERT ON shell_operations
             BEGIN SELECT RAISE(FAIL, 'injected operation registration fault'); END;"
        ).unwrap();
        let call = CallContext::new(PrincipalId::new(), context, kaijutsu_types::SessionId::new(), kernel.id());
        let error = create_operation(&kernel, &call, "echo never-run", None).unwrap_err();
        assert!(error.contains("injected operation registration fault"), "{error}");
        assert!(events.try_recv().is_none(), "registration failure must not publish a partial pair");
        assert!(kernel.shell_operations().list_for_context(context).unwrap().is_empty());
        let db = kernel.blocks().db().unwrap().clone();
        let workspace = db.lock().get_or_create_default_workspace(PrincipalId::system()).unwrap();
        let restored = crate::block_store::BlockStore::with_db(db, workspace, PrincipalId::system());
        restored.load_from_db().unwrap();
        assert!(restored.block_snapshots(context).unwrap().is_empty(), "failed setup must not survive replay");
    }
    #[tokio::test]
    async fn operation_pair_receipt_and_ask_link_commit_together() {
        use approval_ledger::ask::create_ask;
        use approval_ledger::types::{NewAsk, Origin};
        for fault in [None, Some("link"), Some("journal")] {
            let (_dir, kernel, context) = fixture().await;
            let requester = PrincipalId::new();
            let actor = PrincipalId::new();
            let reviewer = PrincipalId::new();
            let call = CallContext::new(requester, context, kaijutsu_types::SessionId::new(), kernel.id())
                .with_actor(actor, Some(reviewer));
            let ask = create_ask(kernel.kernel_db().lock().conn_for_ledger(), &NewAsk {
                context_id: context.as_bytes().to_vec(), actor_id: actor.as_bytes().to_vec(),
                reviewer_id: reviewer.as_bytes().to_vec(), principal_id: requester.as_bytes().to_vec(),
                origin: Origin::ShellGate, instance: None, tool: None, hook_id: None,
                description: "captured command".into(), statements: vec![], authorized_label: None,
                rc_run_id: None, expires_at: None, options: vec![], signals: vec![], cwd: None,
                exec_source: Some("echo captured".into()), exec_stdin: None, continuation_epoch: None, env: vec![],
            }).unwrap();
            if let Some(fault) = fault {
                let sql = match fault {
                    "link" => "CREATE TRIGGER reject_link BEFORE UPDATE OF command_block_id ON approvals BEGIN SELECT RAISE(FAIL, 'injected link fault'); END;",
                    _ => "CREATE TRIGGER reject_journal BEFORE INSERT ON oplog BEGIN SELECT RAISE(FAIL, 'injected journal fault'); END;",
                };
                kernel.kernel_db().lock().conn_for_ledger().execute_batch(sql).unwrap();
            }
            let mut events = kernel.block_flows().subscribe("block.*");
            let result = create_operation(&kernel, &call, "echo captured", Some(&ask));
            let db = kernel.blocks().db().unwrap().clone();
            let workspace = db.lock().get_or_create_default_workspace(PrincipalId::system()).unwrap();
            let restored = crate::block_store::BlockStore::with_db(db, workspace, PrincipalId::system());
            restored.load_from_db().unwrap();
            let blocks = restored.block_snapshots(context).unwrap();
            let linked: (Option<String>, Option<String>, Option<String>) = kernel.kernel_db().lock().conn_for_ledger()
                .query_row("SELECT command_block_id,output_block_id,pair_owner FROM approvals WHERE request_id=?1",
                    [&ask], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap();
            if let Some(fault) = fault {
                assert!(result.unwrap_err().contains(&format!("injected {fault} fault")));
                assert!(events.try_recv().is_none());
                assert!(blocks.is_empty());
                assert!(kernel.shell_operations().list_for_context(context).unwrap().is_empty());
                assert_eq!(linked, (None, None, None));
            } else {
                let receipt = result.unwrap();
                assert_eq!(linked, (Some(receipt.command_block_id.to_key()), Some(receipt.output_block_id.to_key()), Some("turn".into())));
                assert_eq!(blocks.len(), 2);
                assert!(blocks.iter().all(|block| block.status == Status::Waiting && block.excluded));
                let command = blocks.iter().find(|block| block.id == receipt.command_block_id).unwrap();
                let output = blocks.iter().find(|block| block.id == receipt.output_block_id).unwrap();
                assert_eq!(command.id.principal_id, actor);
                assert_eq!(output.id.principal_id, PrincipalId::system());
                assert_eq!(output.tool_call_id, Some(command.id));
                let saved = kernel.shell_operations().get_by_ask(&ask, context).unwrap().unwrap();
                assert_eq!(saved.receipt.operation_id, receipt.operation_id);
                assert_eq!(saved.source, "echo captured");
                let crate::flows::BlockFlow::Inserted { version: first, block, .. } = events.try_recv().unwrap().payload else { panic!("command insert") };
                assert_eq!(block.status, Status::Waiting);
                let crate::flows::BlockFlow::Inserted { version: second, block, .. } = events.try_recv().unwrap().payload else { panic!("result insert") };
                assert_eq!(block.status, Status::Waiting);
                assert_eq!(first, second);
                assert!(events.try_recv().is_none(), "pair is published once with its final setup metadata");
                let repeated = create_operation(&kernel, &call, "echo captured", Some(&ask)).unwrap();
                assert_eq!(repeated.operation_id, receipt.operation_id, "retry must retain its original owner");
                assert!(events.try_recv().is_none());
                assert!(create_operation(&kernel, &call, "echo changed", Some(&ask)).is_err());
                assert_eq!(kernel.blocks().block_snapshots(context).unwrap().len(), 2,
                    "conflicting setup must reject before mutating or poisoning the document");
            }
        }
    }

}
