//! Retain execution through result review, with or without a transcript pair.
//! The command owner cancels the wait; terminal retention closes unanswered asks.

use std::sync::Arc;
use kaijutsu_types::{AskRef, BlockId, Status};
use crate::mcp::{McpError, McpResult};
use super::command_outcome::{CommandHookEffect, CommandOutcome};

pub(super) struct CommandResultReview {
    pub kernel: Arc<crate::Kernel>,
    pub review_id: String,
    pub call: crate::mcp::CallContext,
    pub pair: Option<(BlockId, BlockId)>,
    pub captured: CommandOutcome,
    operation: Option<crate::shell_operations::ShellOperationState>,
    interrupted: parking_lot::Mutex<Option<CommandOutcome>>,
    pub cancel: tokio_util::sync::CancellationToken,
    pub notices: Option<tokio::sync::mpsc::UnboundedSender<kaijutsu_types::Refusal>>,
}

struct ReviewGuard<'a> {
    owner: &'a CommandResultReview,
    ask: &'a AskRef,
    armed: bool,
}

impl Drop for ReviewGuard<'_> {
    fn drop(&mut self) {
        if !self.armed { return; }
        let outcome = self.owner.interrupt(
            "Result review was interrupted; captured execution was not repeated.", Some(&self.ask.request_id));
        if let Err(error) = self.owner.settle(&outcome)
        {
            tracing::error!("could not settle interrupted result review: {error}");
        }
    }
}

#[async_trait::async_trait]
impl crate::mcp::broker::ResultReview for CommandResultReview {
    fn record_ask(&self, conn: &rusqlite::Connection, request: &str) -> crate::kernel_db::KernelDbResult<()> {
        use crate::kernel_db::KernelDbError;
        let operation = match (self.pair, &self.operation) {
            (Some((command, output)), Some(operation))
                if operation.receipt.context_id == self.call.context_id
                    && operation.receipt.command_block_id == command && operation.receipt.output_block_id == output => Some(operation.receipt.operation_id.as_str()),
            (None, None) => None,
            _ => return Err(KernelDbError::Validation("result review pair has no matching admission receipt".into())),
        };
        crate::shell_operations::ShellOperationRegistry::checkpoint_result_review_in(
            conn, &self.review_id, operation, &self.call, &self.waiting_outcome(request))
    }

    async fn wait_for_review(&self, ask: &AskRef) -> McpResult<()> {
        let mut guard = ReviewGuard { owner: self, ask, armed: true };
        let result = self.wait_inner(ask).await;
        guard.armed = false;
        result
    }
}

impl CommandResultReview {
    pub(super) fn new(
        kernel: Arc<crate::Kernel>, call: crate::mcp::CallContext, pair: Option<(BlockId, BlockId)>,
        operation: Option<crate::shell_operations::ShellOperationState>,
        captured: CommandOutcome, cancel: tokio_util::sync::CancellationToken,
        notices: Option<tokio::sync::mpsc::UnboundedSender<kaijutsu_types::Refusal>>,
    ) -> Self {
        Self { kernel, review_id: uuid::Uuid::now_v7().to_string(), call, pair, captured, operation,
            interrupted: parking_lot::Mutex::new(None), cancel, notices }
    }

    pub(super) fn settle(&self, outcome: &CommandOutcome) -> Result<(), String> {
        match self.pair {
            Some((command, output)) => super::command::settle_known_outcome(&self.kernel, self.call.context_id, &command, &output, outcome, None, self.operation.as_ref()),
            None => self.kernel.shell_operations().finish_result_review(&self.review_id, outcome).map(|changed| {
                if changed { crate::kj::gate::announce_ledger_change(self.kernel.kernel_db(), self.kernel.ledger_flows()); }
            }),
        }
    }

    /// Cancellation and unwinding share the first interrupted outcome, even
    /// before storage accepts it. Recovery never needs a successful DB read.
    pub(super) fn interrupted_outcome(&self, reason: &str) -> CommandOutcome {
        self.interrupt(reason, None)
    }

    fn interrupt(&self, reason: &str, ask: Option<&str>) -> CommandOutcome {
        self.interrupted.lock().get_or_insert_with(|| {
            let mut outcome = self.captured.clone();
            outcome.hook = Some(CommandHookEffect::Refused {
                reason: reason.into(), refusal: None, waiting: false, ask_id: ask.map(str::to_owned),
            });
            outcome
        }).clone()
    }

    fn waiting_outcome(&self, request: &str) -> CommandOutcome {
        let mut waiting = self.captured.clone();
        waiting.hook = Some(CommandHookEffect::Refused {
            reason: "Captured execution awaits result review; approval continues processing without running source again.".into(),
            refusal: None, waiting: true, ask_id: Some(request.into()),
        });
        waiting
    }

    async fn wait_inner(&self, ask: &AskRef) -> McpResult<()> {
        use approval_ledger::types::ApprovalStatus;
        let waiting = self.waiting_outcome(&ask.request_id);
        if self.pair.is_some() { self.settle(&waiting).map_err(McpError::Protocol)?; }
        if let Some(notices) = &self.notices {
            let refusal = McpError::gate_pending(None, Some(ask.clone()),
                "Captured execution awaits result review; approval continues processing without running source again.".into())
                .as_refusal().expect("a pending gate is a refusal");
            // The command keeps its owner when its RPC receiver has departed.
            let _ = notices.send(refusal);
        }
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
                    if let Some((command, output)) = self.pair {
                        for block in [output, command] {
                            self.kernel.blocks().set_status(self.call.context_id, &block, Status::Running)
                                .map_err(|e| McpError::Protocol(e.to_string()))?;
                        }
                    }
                    return Ok(());
                }
                return Err(McpError::refused_gate(kaijutsu_types::RefusalKind::Denied, "result review",
                    Some(crate::kj::gate::ask_ref(ask.request_id.clone(), row.status)),
                    "Result review did not approve publication; source was not run again."));
            }
            tokio::select! {
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
    use kaijutsu_types::{ContextId, PrincipalId};
    use std::future::Future;

    async fn fixture(authored: bool) -> (CommandResultReview, AskRef, Option<String>) {
        let kernel = Arc::new(crate::Kernel::new_ephemeral("result-review").await);
        let context = ContextId::new();
        let blocks = kernel.blocks();
        blocks.create_document(context, crate::block_store::DocumentKind::Conversation, None).unwrap();
        let actor = PrincipalId::system();
        let (pair, operation) = if authored {
            let command = blocks.insert_tool_call(context, None, None, "shell_write", serde_json::json!({}), None).unwrap();
            let output = blocks.insert_tool_result(context, &command, Some(&command), "waiting", false, None, None).unwrap();
            let operation = kernel.shell_operations().register(context, actor, actor, command, output, "never-rerun", None).unwrap();
            (Some((command, output)), Some(operation.operation_id))
        } else { (None, None) };
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
        let call = crate::mcp::CallContext::new(actor, context, kaijutsu_types::SessionId::new(), kernel.id());
        let admitted = operation.as_ref().map(|id| kernel.shell_operations().get(id, context).unwrap().unwrap());
        let review = CommandResultReview::new(kernel, call, pair, admitted,
            CommandOutcome::new(CommandExecution::Completed(
                kaish_kernel::interpreter::ExecResult::success("already ran")), 1),
            tokio_util::sync::CancellationToken::new(), None);
        (review, ask, operation)
    }

    #[tokio::test]
    async fn failed_review_abandonment_retries_with_terminal_retention() {
        for (authored, event_fault) in [(true, false), (false, false), (true, true), (false, true)] {
            let (review, ask, operation) = fixture(authored).await;
            review.kernel.kernel_db().lock().in_transaction(|db| review.record_ask(db.conn_for_ledger(), &ask.request_id)).unwrap();
            let mut wait = Box::pin(review.wait_for_review(&ask));
            std::future::poll_fn(|cx| {
                assert!(wait.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            }).await;
            {
                let db = review.kernel.kernel_db().lock();
                if event_fault { approval_ledger::claim::claim(db.conn_for_ledger(), &ask.request_id, PrincipalId::new().as_bytes()).unwrap(); }
                db.conn_for_ledger().execute_batch(if event_fault {
                    "CREATE TRIGGER fail_review_abandon BEFORE INSERT ON approval_events WHEN NEW.kind='abandoned'
                     BEGIN SELECT RAISE(ABORT, 'injected abandonment fault'); END;"
                } else {
                    "CREATE TRIGGER fail_review_abandon BEFORE UPDATE OF status ON approvals WHEN NEW.status='abandoned'
                     BEGIN SELECT RAISE(ABORT, 'injected abandonment fault'); END;"
                }).unwrap();
            }
            drop(wait);
            let store = review.kernel.shell_operations();
            assert!(store.result_review_for_ask(&ask.request_id, review.call.context_id).unwrap().unwrap().settled.is_none(),
                "terminal result must not commit while its unanswered ask remains actionable");
            assert_eq!(review.kernel.kernel_db().lock().get_approval(&ask.request_id).unwrap().unwrap().status,
                if event_fault { approval_ledger::types::ApprovalStatus::Claimed } else { approval_ledger::types::ApprovalStatus::Pending });
            assert_eq!(store.pending_retention(10).unwrap().len(), 1, "ledger storage faults must keep the live retry owner");
            if let Some(operation) = &operation { assert!(store.outcome(operation, review.call.context_id).unwrap().is_none()); }
            assert!(review.kernel.shutdown_runtime_worker().await.unwrap_err().contains("injected abandonment fault"));
            review.kernel.kernel_db().lock().conn_for_ledger().execute_batch("DROP TRIGGER fail_review_abandon").unwrap();
            let mut changes = review.kernel.ledger_flows().subscribe("ledger.changed");
            review.kernel.shutdown_runtime_worker().await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(1), changes.recv()).await.expect("retention retry must announce its ask closure").unwrap();
            assert_eq!(review.kernel.kernel_db().lock().get_approval(&ask.request_id).unwrap().unwrap().status,
                approval_ledger::types::ApprovalStatus::Abandoned);
            let saved = store.result_review_for_ask(&ask.request_id, review.call.context_id).unwrap().unwrap().settled.unwrap();
            assert_eq!(saved.exec_result(), review.interrupted_outcome("repeat").exec_result());
            assert!(store.retention_failures().is_empty());
            review.settle(&saved).unwrap();
            assert!(changes.try_recv().is_none(), "a repeat settlement must not announce another transition");
        }
    }

    #[tokio::test]
    async fn terminal_review_retention_preserves_existing_decisions() {
        use approval_ledger::types::ApprovalStatus;
        for authored in [true, false] {
            for status in [ApprovalStatus::Allowed, ApprovalStatus::Denied, ApprovalStatus::Expired] {
                let (review, ask, _) = fixture(authored).await;
                let before = {
                    let db = review.kernel.kernel_db().lock();
                    db.in_transaction(|db| review.record_ask(db.conn_for_ledger(), &ask.request_id)).unwrap();
                    if status == ApprovalStatus::Expired { approval_ledger::decide::expire(db.conn_for_ledger(), &ask.request_id).unwrap(); }
                    else {
                        approval_ledger::decide::decide(db.conn_for_ledger(), &ask.request_id,
                            approval_ledger::decide::DecideInput { allow: status == ApprovalStatus::Allowed,
                                auto_reason: Some("test rule"), ..Default::default() }).unwrap();
                    }
                    approval_ledger::ask::list_events(db.conn_for_ledger(), &ask.request_id).unwrap().len()
                };
                review.settle(&review.interrupted_outcome("stopped after decision")).unwrap();
                let db = review.kernel.kernel_db().lock();
                assert_eq!(db.get_approval(&ask.request_id).unwrap().unwrap().status, status);
                assert_eq!(approval_ledger::ask::list_events(db.conn_for_ledger(), &ask.request_id).unwrap().len(), before);
            }
        }
    }

    #[tokio::test]
    async fn interrupted_review_keeps_one_outcome_through_storage_faults() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        for authored in [true, false] {
            for read_fault in [false, true] {
                let (review, ask, operation) = fixture(authored).await;
                review.kernel.kernel_db().lock().in_transaction(|db| review.record_ask(db.conn_for_ledger(), &ask.request_id)).unwrap();
                let mut wait = Box::pin(review.wait_for_review(&ask));
                std::future::poll_fn(|cx| {
                    assert!(wait.as_mut().poll(cx).is_pending());
                    std::task::Poll::Ready(())
                }).await;
                {
                    let db = review.kernel.kernel_db().lock();
                    if read_fault {
                        db.conn_for_ledger().authorizer(Some(|ctx: AuthContext<'_>| match ctx.action {
                            AuthAction::Read { table_name: "shell_operations" | "shell_result_reviews", .. } => Authorization::Deny,
                            _ => Authorization::Allow,
                        })).unwrap();
                    } else {
                        db.conn_for_ledger().execute_batch(if authored {
                            "CREATE TRIGGER fail_interrupt BEFORE INSERT ON shell_operation_outcomes BEGIN SELECT RAISE(ABORT, 'interrupted retention fault'); END;"
                        } else {
                            "CREATE TRIGGER fail_interrupt BEFORE UPDATE OF final_json ON shell_result_reviews BEGIN SELECT RAISE(ABORT, 'interrupted retention fault'); END;"
                        }).unwrap();
                    }
                }
                drop(wait);
                let pending = review.kernel.shell_operations().pending_retention(10).unwrap();
                assert_eq!(pending.len(), 1, "dropped review must retain its result even when receipt reads fail");
                let interrupted = review.interrupted_outcome("outer cancellation");
                assert_eq!(serde_json::to_value(&interrupted).unwrap(), serde_json::to_value(&pending[0].1).unwrap(),
                    "cancellation must keep the exact interrupted result already owned by retention");
                {
                    let db = review.kernel.kernel_db().lock();
                    db.conn_for_ledger().authorizer(None::<fn(AuthContext<'_>) -> Authorization>).unwrap();
                    if !read_fault { db.conn_for_ledger().execute_batch("DROP TRIGGER fail_interrupt").unwrap(); }
                }
                review.settle(&interrupted).unwrap();
                super::super::command::retry_retained_outcomes(&review.kernel, 10).unwrap();
                assert!(review.kernel.shell_operations().retention_failures().is_empty());
                let saved = review.kernel.shell_operations().result_review_for_ask(&ask.request_id, review.call.context_id).unwrap().unwrap();
                assert_eq!(saved.operation_id, operation);
                assert_eq!(serde_json::to_value(saved.settled.unwrap()).unwrap(), serde_json::to_value(interrupted).unwrap());
                review.kernel.shutdown_runtime_worker().await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn dropping_a_review_settles_captured_execution_without_rerunning_it() {
        let (review, ask, operation) = fixture(true).await;
        let operation = operation.unwrap();
        review.kernel.kernel_db().lock().in_transaction(|db| review.record_ask(db.conn_for_ledger(), &ask.request_id)).unwrap();
        let mut wait = Box::pin(review.wait_for_review(&ask));
        std::future::poll_fn(|cx| {
            assert!(wait.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        }).await;
        drop(wait);
        let state = review.kernel.shell_operations().get(&operation, review.call.context_id).unwrap().unwrap();
        assert_eq!(state.envelope.unwrap().status, kaijutsu_types::shell_envelope::ShellStatus::Error);
        let outcome = review.kernel.shell_operations().outcome(&operation, review.call.context_id).unwrap().unwrap();
        let CommandExecution::Completed(raw) = outcome.execution else { panic!("captured execution was lost") };
        assert_eq!(raw.text_out(), "already ran");
        assert_eq!(review.kernel.kernel_db().lock().get_approval(&ask.request_id).unwrap().unwrap().status,
            approval_ledger::types::ApprovalStatus::Abandoned);
        assert_eq!(review.kernel.blocks().get_block_snapshot(review.call.context_id, &review.pair.unwrap().1).unwrap().unwrap().status, Status::Error);
    }

    #[tokio::test]
    async fn original_execution_ask_keeps_its_receipt_after_result_review() {
        let (review, ask, operation) = fixture(true).await;
        let operation = operation.unwrap();
        let (command, output) = review.pair.unwrap();
        let context = review.call.context_id;
        let original = {
            let db = review.kernel.kernel_db().lock();
            let original = approval_ledger::ask::create_ask(db.conn_for_ledger(), &approval_ledger::types::NewAsk {
                context_id: context.as_bytes().to_vec(), actor_id: review.call.actor_id.as_bytes().to_vec(),
                principal_id: review.call.principal_id.as_bytes().to_vec(), reviewer_id: PrincipalId::new().as_bytes().to_vec(),
                origin: approval_ledger::types::Origin::ShellGate, instance: None, tool: None,
                hook_id: None, description: "execute captured source".into(), statements: vec![],
                authorized_label: None, rc_run_id: None, expires_at: None, options: vec![],
                signals: vec![], cwd: None, exec_source: Some("never-rerun".into()), exec_stdin: None,
                continuation_epoch: None, env: vec![],
            }).unwrap();
            db.link_ask_blocks(&original, &command, &output, crate::PairOwner::Turn).unwrap();
            original
        };
        let store = review.kernel.shell_operations();
        assert_eq!(store.get_by_ask(&original, context).unwrap().unwrap().receipt.operation_id, operation);
        let mut checkpoint = review.captured.clone();
        checkpoint.hook = Some(CommandHookEffect::Refused { reason: "review".into(), refusal: None,
            waiting: true, ask_id: Some(ask.request_id.clone()) });
        store.checkpoint_result_review(&review.review_id, Some(&operation), &review.call, &checkpoint).unwrap();
        for request in [&original, &ask.request_id] {
            let state = store.get_by_ask(request, context).unwrap().expect("every ask retains its original receipt");
            assert_eq!(state.receipt.operation_id, operation);
            assert_eq!(state.receipt.ask_id.as_deref(), Some(ask.request_id.as_str()), "lookup preserves the most recent review ask");
            assert!(store.get_by_ask(request, ContextId::new()).unwrap().is_none());
        }
        review.kernel.kernel_db().lock().link_ask_blocks(&original, &command, &output, crate::PairOwner::Turn).unwrap();
        let reused = crate::runtime::tool_command::create_operation(&review.kernel, &review.call, "never-rerun", Some(&original)).unwrap();
        assert_eq!(reused.operation_id, operation, "setup retries retain the original pair during result review");
        assert_eq!(reused.ask_id.as_deref(), Some(ask.request_id.as_str()));
        review.settle(&review.captured).unwrap();
        for request in [&original, &ask.request_id] {
            let state = store.get_by_ask(request, context).unwrap().unwrap();
            assert_eq!(state.envelope.unwrap().stdout, "already ran");
        }
        assert_eq!(store.list_for_context(context).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn receipt_free_review_retries_terminal_retention_after_shutdown_refusal() {
        let (review, ask, _) = fixture(false).await;
        let store = review.kernel.shell_operations();
        let mut checkpoint = review.captured.clone();
        checkpoint.hook = Some(CommandHookEffect::Refused { reason: "review".into(), refusal: None,
            waiting: true, ask_id: Some(ask.request_id.clone()) });
        store.checkpoint_result_review(&review.review_id, None, &review.call, &checkpoint).unwrap();
        review.kernel.kernel_db().lock().conn_for_ledger().execute_batch(
            "CREATE TRIGGER fail_review_retention BEFORE UPDATE OF final_json ON shell_result_reviews
             BEGIN SELECT RAISE(ABORT, 'injected review retention fault'); END;"
        ).unwrap();
        assert!(store.finish_result_review(&review.review_id, &review.captured).is_err());
        let mut different = review.captured.clone();
        different.elapsed_ms += 1;
        assert!(store.finish_result_review(&review.review_id, &different).unwrap_err().contains("different live retention owner"));
        assert!(review.kernel.shutdown_runtime_worker().await.unwrap_err().contains("injected review retention fault"));
        assert!(store.result_review_for_ask(&ask.request_id, review.call.context_id).unwrap().unwrap().settled.is_none());
        review.kernel.kernel_db().lock().conn_for_ledger().execute_batch("DROP TRIGGER fail_review_retention").unwrap();
        review.kernel.shutdown_runtime_worker().await.unwrap();
        let saved = store.result_review_for_ask(&ask.request_id, review.call.context_id).unwrap().unwrap().settled.unwrap();
        assert_eq!(saved.exec_result(), review.captured.exec_result());
        assert_eq!(saved.elapsed_ms, review.captured.elapsed_ms);
        assert!(store.retention_failures().is_empty());
    }

    #[tokio::test]
    async fn review_storage_preserves_captured_execution_and_terminal_results() {
        let (review, ask, _) = fixture(false).await;
        let mut checkpoint = review.captured.clone();
        checkpoint.hook = Some(CommandHookEffect::Refused { reason: "review".into(), refusal: None,
            waiting: true, ask_id: Some(ask.request_id.clone()) });
        let store = review.kernel.shell_operations();
        store.checkpoint_result_review(&review.review_id, None, &review.call, &checkpoint).unwrap();
        let mut changed = checkpoint.clone();
        changed.execution = CommandExecution::Completed(kaish_kernel::interpreter::ExecResult::success("different execution"));
        assert!(store.checkpoint_result_review(&review.review_id, None, &review.call, &changed).is_err(),
            "another checkpoint cannot rewrite execution");
        store.finish_result_review(&review.review_id, &review.captured).unwrap();
        store.finish_result_review(&review.review_id, &review.captured).unwrap();
        assert!(store.checkpoint_result_review(&review.review_id, None, &review.call, &checkpoint).is_err());
        changed.hook = None;
        assert!(store.finish_result_review(&review.review_id, &changed).is_err());
        let record = store.result_review_for_ask(&ask.request_id, review.call.context_id).unwrap().unwrap();
        let CommandExecution::Completed(raw) = record.settled.unwrap().execution else { panic!("lost execution") };
        assert_eq!(raw.text_out(), "already ran");
    }

    #[tokio::test]
    async fn review_storage_requires_receipt_preparation_for_tracked_completion() {
        let (review, ask, operation) = fixture(true).await;
        let mut checkpoint = review.captured.clone();
        checkpoint.hook = Some(CommandHookEffect::Refused { reason: "review".into(), refusal: None,
            waiting: true, ask_id: Some(ask.request_id.clone()) });
        let store = review.kernel.shell_operations();
        store.checkpoint_result_review(&review.review_id, operation.as_deref(), &review.call, &checkpoint).unwrap();
        assert!(store.finish_result_review(&review.review_id, &review.captured).is_err(),
            "tracked review completion must be atomic with operation outcome preparation");
        assert!(store.result_review_for_ask(&ask.request_id, review.call.context_id).unwrap().unwrap().settled.is_none());
        review.settle(&review.captured).unwrap();
        assert!(store.result_review_for_ask(&ask.request_id, review.call.context_id).unwrap().unwrap().settled.is_some());
    }

    #[tokio::test]
    async fn quiet_review_drop_retains_execution_without_creating_blocks() {
        let (review, ask, operation) = fixture(false).await;
        assert!(operation.is_none());
        review.kernel.kernel_db().lock().in_transaction(|db| review.record_ask(db.conn_for_ledger(), &ask.request_id)).unwrap();
        let mut wait = Box::pin(review.wait_for_review(&ask));
        std::future::poll_fn(|cx| {
            assert!(wait.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        }).await;
        drop(wait);
        let record = review.kernel.shell_operations().result_review_for_ask(&ask.request_id, review.call.context_id).unwrap().unwrap();
        assert!(record.operation_id.is_none());
        let final_result = record.settled.unwrap();
        assert_eq!(final_result.block_status(), Status::Error);
        let CommandExecution::Completed(raw) = final_result.execution else { panic!("lost raw execution") };
        assert_eq!(raw.text_out(), "already ran");
        assert!(review.kernel.blocks().block_snapshots(review.call.context_id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn restart_preserves_execution_but_does_not_resume_an_interrupted_hook_snapshot() {
        for (authored, legacy) in [(true, false), (false, false), (true, true)] {
            let (review, ask, operation) = fixture(authored).await;
            let mut checkpoint = review.captured.clone();
            checkpoint.hook = Some(CommandHookEffect::Refused { reason: "awaiting result review".into(),
                refusal: None, waiting: true, ask_id: Some(ask.request_id.clone()) });
            if legacy {
                let db = review.kernel.kernel_db().lock();
                db.conn_for_ledger().execute_batch("DROP TABLE shell_result_review_asks; DROP TABLE shell_result_reviews;
                    CREATE TABLE shell_result_reviews(operation_id TEXT PRIMARY KEY REFERENCES shell_operations(operation_id), outcome_json TEXT NOT NULL);")
                    .unwrap();
                db.conn_for_ledger().execute("INSERT INTO shell_result_reviews VALUES(?1,?2)",
                    rusqlite::params![operation.as_ref().unwrap(), serde_json::to_string(&checkpoint).unwrap()]).unwrap();
            } else {
                review.kernel.shell_operations().checkpoint_result_review(&review.review_id, operation.as_deref(), &review.call, &checkpoint).unwrap();
            }
            let db = review.kernel.kernel_db().clone();
            let principal = review.kernel.blocks().principal_id();
            let workspace = db.lock().get_or_create_default_workspace(principal).unwrap();
            let blocks = crate::block_store::shared_block_store_with_db(db.clone(), workspace, principal);
            let dir = tempfile::tempdir().unwrap();
            let recovered = crate::Kernel::new("recovered-review", dir.path(), blocks, db).await;
            let record = recovered.shell_operations().result_review_for_ask(&ask.request_id, review.call.context_id).unwrap().unwrap();
            assert_eq!(record.operation_id, operation);
            let outcome = record.settled.unwrap();
            let CommandExecution::Completed(raw) = &outcome.execution else { panic!("lost executed outcome") };
            assert_eq!(raw.text_out(), "already ran");
            assert!(outcome.settlement_error.as_deref().unwrap().contains("restarted during result review"));
            assert_eq!(outcome.block_status(), Status::Error);
            if let Some((_, output)) = review.pair {
                assert_eq!(recovered.blocks().get_block_snapshot(review.call.context_id, &output).unwrap().unwrap().status, Status::Error);
                assert_eq!(recovered.shell_operations().get_by_ask(&ask.request_id, review.call.context_id).unwrap().unwrap().receipt.operation_id,
                    operation.unwrap());
            }
            assert_eq!(recovered.kernel_db().lock().get_approval(&ask.request_id).unwrap().unwrap().status,
                approval_ledger::types::ApprovalStatus::Abandoned);
        }
    }
}
