//! Suspend result processing while retaining execution and its operation receipt.

use std::sync::Arc;
use kaijutsu_types::{AskRef, BlockId, ContextId, Status};
use crate::mcp::{McpError, McpResult};
use super::command_outcome::{CommandHookEffect, CommandOutcome};

pub(super) struct CommandResultReview {
    pub kernel: Arc<crate::Kernel>,
    pub context: ContextId,
    pub command: BlockId,
    pub output: BlockId,
    pub captured: CommandOutcome,
    pub cancel: tokio_util::sync::CancellationToken,
}

struct ReviewGuard<'a> {
    owner: &'a CommandResultReview,
    ask: &'a AskRef,
    armed: bool,
}

impl ReviewGuard<'_> {
    fn abandon(&self) -> Result<(), String> {
        use approval_ledger::types::ApprovalStatus;
        {
            let db = self.owner.kernel.kernel_db().lock();
            if let Some(row) = db.get_approval(&self.ask.request_id).map_err(|e| e.to_string())?
                && matches!(row.status, ApprovalStatus::Pending | ApprovalStatus::Claimed)
            {
                approval_ledger::decide::abandon(db.conn_for_ledger(), &self.ask.request_id,
                    Some("result review stopped; captured execution was not repeated")).map_err(|e| e.to_string())?;
            }
        }
        crate::kj::gate::announce_ledger_change(self.owner.kernel.kernel_db(), self.owner.kernel.ledger_flows());
        Ok(())
    }
}

impl Drop for ReviewGuard<'_> {
    fn drop(&mut self) {
        if !self.armed { return; }
        if let Err(error) = self.abandon() { tracing::error!("could not abandon interrupted result review: {error}"); }
        let mut outcome = self.owner.captured.clone();
        outcome.hook = Some(CommandHookEffect::Refused {
            reason: "Result review was interrupted; captured execution was not repeated.".into(),
            waiting: false, ask_id: Some(self.ask.request_id.clone()),
        });
        if let Err(error) = super::command::settle_outcome(&self.owner.kernel, self.owner.context,
            &self.owner.command, &self.owner.output, &outcome)
        {
            tracing::error!("could not settle interrupted result review: {error}");
        }
    }
}

#[async_trait::async_trait]
impl crate::mcp::broker::ResultReview for CommandResultReview {
    async fn wait_for_review(&self, ask: &AskRef) -> McpResult<()> {
        let mut guard = ReviewGuard { owner: self, ask, armed: true };
        let result = self.wait_inner(ask).await;
        guard.armed = false;
        if result.is_err() { guard.abandon().map_err(McpError::Protocol)?; }
        result
    }
}

impl CommandResultReview {
    async fn wait_inner(&self, ask: &AskRef) -> McpResult<()> {
        use approval_ledger::types::ApprovalStatus;
        let operation = self.kernel.shell_operations().get_by_output(&self.output, self.context)
            .map_err(McpError::Protocol)?.ok_or_else(|| McpError::Protocol("result review requires a durable operation receipt".into()))?;
        let mut waiting = self.captured.clone();
        waiting.hook = Some(CommandHookEffect::Refused {
            reason: "Captured execution awaits result review; approval continues processing without running source again.".into(),
            waiting: true, ask_id: Some(ask.request_id.clone()),
        });
        self.kernel.shell_operations().checkpoint_result_review(&operation.receipt.operation_id, &waiting)
            .map_err(McpError::Protocol)?;
        super::command::settle_outcome(&self.kernel, self.context, &self.command, &self.output, &waiting)
            .map_err(McpError::Protocol)?;
        let mut changes = self.kernel.ledger_flows().subscribe("ledger.changed");
        loop {
            let row = self.kernel.kernel_db().lock().get_approval(&ask.request_id)
                .map_err(|e| McpError::Protocol(e.to_string()))?
                .ok_or_else(|| McpError::Protocol("result review ask disappeared".into()))?;
            if !matches!(row.status, ApprovalStatus::Pending | ApprovalStatus::Claimed) {
                let redeemable = matches!(row.status, ApprovalStatus::Allowed | ApprovalStatus::Denied)
                    || (row.status == ApprovalStatus::Abandoned && row.decided_option.as_deref() == Some("cancel"));
                if redeemable && !self.kernel.kernel_db().lock().redeem_ask(&ask.request_id)
                    .map_err(|e| McpError::Protocol(e.to_string()))?
                {
                    return Err(McpError::Protocol("result review answer was already consumed".into()));
                }
                if row.status == ApprovalStatus::Allowed {
                    for block in [&self.output, &self.command] {
                        self.kernel.blocks().set_status(self.context, block, Status::Running)
                            .map_err(|e| McpError::Protocol(e.to_string()))?;
                    }
                    return Ok(());
                }
                return Err(McpError::refused_gate(kaijutsu_types::RefusalKind::Denied, "result review",
                    Some(crate::kj::gate::ask_ref(ask.request_id.clone(), row.status)),
                    "Result review did not approve publication; source was not run again."));
            }
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    let db = self.kernel.kernel_db().lock();
                    approval_ledger::decide::abandon(db.conn_for_ledger(), &ask.request_id,
                        Some("result review cancelled; captured execution was not repeated"))
                        .map_err(|e| McpError::Protocol(e.to_string()))?;
                }
                _ = changes.recv() => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::broker::ResultReview;
    use super::super::command_outcome::CommandExecution;
    use kaijutsu_types::PrincipalId;
    use std::future::Future;

    async fn fixture() -> (CommandResultReview, AskRef, String) {
        let kernel = Arc::new(crate::Kernel::new_ephemeral("result-review").await);
        let context = ContextId::new();
        let blocks = kernel.blocks();
        blocks.create_document(context, crate::block_store::DocumentKind::Conversation, None).unwrap();
        let command = blocks.insert_tool_call(context, None, None, "shell_write", serde_json::json!({}), None).unwrap();
        let output = blocks.insert_tool_result(context, &command, Some(&command), "waiting", false, None, None).unwrap();
        let actor = PrincipalId::system();
        let operation = kernel.shell_operations().register(context, actor, actor, command, output, "never-rerun", None).unwrap();
        let request_id = approval_ledger::ask::create_ask(kernel.kernel_db().lock().conn_for_ledger(),
            &approval_ledger::types::NewAsk {
                context_id: context.as_bytes().to_vec(), actor_id: actor.as_bytes().to_vec(),
                reviewer_id: PrincipalId::new().as_bytes().to_vec(), principal_id: actor.as_bytes().to_vec(),
                origin: approval_ledger::types::Origin::HookResult, instance: None, tool: None,
                hook_id: None, description: "Review captured result".into(), statements: vec![],
                authorized_label: None, rc_run_id: None, expires_at: None, options: vec![],
                signals: vec![], cwd: None, exec_source: None, exec_stdin: None,
                continuation_epoch: None, env: vec![],
            }).unwrap();
        let ask = crate::kj::gate::ask_ref(request_id, approval_ledger::types::ApprovalStatus::Pending);
        let review = CommandResultReview { kernel, context, command, output,
            captured: CommandOutcome::new(CommandExecution::Completed(
                kaish_kernel::interpreter::ExecResult::success("already ran")), 1),
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        (review, ask, operation.operation_id)
    }

    #[tokio::test]
    async fn dropping_a_review_settles_captured_execution_without_rerunning_it() {
        let (review, ask, operation) = fixture().await;
        let mut wait = Box::pin(review.wait_for_review(&ask));
        std::future::poll_fn(|cx| {
            assert!(wait.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        }).await;
        drop(wait);
        let state = review.kernel.shell_operations().get(&operation, review.context).unwrap().unwrap();
        assert_eq!(state.envelope.unwrap().status, kaijutsu_types::shell_envelope::ShellStatus::Error);
        let outcome = review.kernel.shell_operations().outcome(&operation, review.context).unwrap().unwrap();
        let CommandExecution::Completed(raw) = outcome.execution else { panic!("captured execution was lost") };
        assert_eq!(raw.text_out(), "already ran");
        assert_eq!(review.kernel.kernel_db().lock().get_approval(&ask.request_id).unwrap().unwrap().status,
            approval_ledger::types::ApprovalStatus::Abandoned);
        assert_eq!(review.kernel.blocks().get_block_snapshot(review.context, &review.output).unwrap().unwrap().status, Status::Error);
    }

    #[tokio::test]
    async fn cancelling_review_abandons_the_wait_without_authorizing_anything() {
        let (review, ask, _) = fixture().await;
        review.cancel.cancel();
        let error = review.wait_for_review(&ask).await.unwrap_err();
        assert!(error.as_refusal().is_some());
        assert_eq!(review.kernel.kernel_db().lock().get_approval(&ask.request_id).unwrap().unwrap().status,
            approval_ledger::types::ApprovalStatus::Abandoned);
        assert!(review.kernel.kernel_db().lock().redeem_ask(&ask.request_id).is_err());
    }

    #[tokio::test]
    async fn restart_preserves_execution_but_does_not_resume_an_interrupted_hook_snapshot() {
        let (review, ask, operation) = fixture().await;
        let mut checkpoint = review.captured.clone();
        checkpoint.hook = Some(CommandHookEffect::Refused { reason: "awaiting result review".into(),
            waiting: true, ask_id: Some(ask.request_id.clone()) });
        review.kernel.shell_operations().checkpoint_result_review(&operation, &checkpoint).unwrap();
        let db = review.kernel.kernel_db().clone();
        let principal = review.kernel.blocks().principal_id();
        let workspace = db.lock().get_or_create_default_workspace(principal).unwrap();
        let blocks = crate::block_store::shared_block_store_with_db(db.clone(), workspace, principal);
        let dir = tempfile::tempdir().unwrap();
        let recovered = crate::Kernel::new("recovered-review", dir.path(), blocks, db).await;
        let outcome = recovered.shell_operations().outcome(&operation, review.context).unwrap().unwrap();
        let CommandExecution::Completed(raw) = &outcome.execution else { panic!("lost executed outcome") };
        assert_eq!(raw.text_out(), "already ran");
        assert!(outcome.settlement_error.as_deref().unwrap().contains("restarted during result review"));
        assert_eq!(outcome.block_status(), Status::Error);
        assert_eq!(recovered.blocks().get_block_snapshot(review.context, &review.output).unwrap().unwrap().status, Status::Error);
        assert_eq!(recovered.kernel_db().lock().get_approval(&ask.request_id).unwrap().unwrap().status,
            approval_ledger::types::ApprovalStatus::Abandoned);
    }
}
