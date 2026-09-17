//! Interactive shell admission and execution, independent of the submitting transport.

use std::sync::Arc;
use futures::FutureExt;
use tracing::Instrument;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use kaijutsu_types::{BlockId, PrincipalId, Refusal, Role, Status, ToolKind};
use crate::{Kernel, block_store::DraftSubmission};
use super::command::{self, CommandContextSwitch, CommandRunOptions, ContextSwitch};
use super::command_outcome::{CommandExecution, CommandHookEffect, CommandOutcome};
use super::context_shell::{ShellCwd, ShellIdentity, ShellPolicy};
use super::embedded_kaish::EmbeddedKaish;

pub enum ShellSource {
    Code(String),
    /// Consume this revision only after admission succeeds. Later typing survives.
    Draft(DraftSubmission),
}

pub struct ShellSubmission {
    pub command_block_id: BlockId,
    pub operation_id: String,
    pub refusal: Option<Refusal>,
}

/// Accepted work survives its caller. The receiver carries connection-local
/// context switches; dropping it does not cancel execution or durable settlement.
pub async fn submit(
    kernel: &Arc<Kernel>, identity: ShellIdentity, source: ShellSource, user_initiated: bool,
) -> Result<(ShellSubmission, mpsc::UnboundedReceiver<ContextSwitch>), String> {
    let (switches, receiver) = mpsc::unbounded_channel();
    let (reply, submitted) = oneshot::channel();
    let owner = kernel.clone();
    let trace_id = kernel.drift().read().trace_id_for_context(identity.context).unwrap_or([0u8; 16]);
    let span = kaijutsu_telemetry::context_root_span(&trace_id, "shell_execute");
    let depth = crate::mcp::broker::current_hook_depth();
    kernel.spawn_runtime_task(move |stop| crate::mcp::broker::inherit_hook_depth(depth, async move {
        match prepare(&owner, identity, source, user_initiated, &stop).await {
            Err(error) => {
                if let Err(Err(error)) = reply.send(Err(error)) {
                    tracing::error!("interactive admission failed after its caller departed: {error}");
                }
            }
            Ok((submission, execution)) => {
                let command = submission.command_block_id;
                let _ = reply.send(Ok(submission));
                if let Some((kaish, call_ctx, output, code)) = execution {
                    let record = |context| -> futures::future::LocalBoxFuture<'_, ()> {
                        Box::pin(command::send_context_switch(context, &switches, &stop))
                    };
                    if let Err(error) = command::run_into_blocks(&kaish, &code, identity.context,
                        &command, &output, &owner, &call_ctx,
                        CommandRunOptions { cancel: Some(stop.clone()),
                            context_switch: CommandContextSwitch::Publish(Some(&record)), ..Default::default() }).await
                    {
                        tracing::error!("interactive command settlement failed: {error}");
                    }
                }
            }
        }
    }.instrument(span)))?;
    let submission = submitted.await.map_err(|_| "interactive command task stopped before replying".to_string())??;
    Ok((submission, receiver))
}

type PreparedExecution = (EmbeddedKaish, crate::mcp::CallContext, BlockId, String);

async fn prepare(
    kernel: &Arc<Kernel>, identity: ShellIdentity, source: ShellSource, user_initiated: bool,
    stop: &CancellationToken,
) -> Result<(ShellSubmission, Option<PreparedExecution>), String> {
    let context = identity.context;
    let code = match &source {
        ShellSource::Code(code) => code.clone(),
        ShellSource::Draft(draft) => {
            let code = draft.content().trim().to_owned();
            if code.is_empty() { return Err("input is empty".into()); }
            code
        }
    };
    let kaish = tokio::select! {
        biased;
        _ = stop.cancelled() => return Err("kernel runtime shut down before interactive execution".into()),
        result = async {
            let dispatcher = kernel.broker().kj_dispatcher().await.ok_or("kj dispatcher is not registered")?;
            EmbeddedKaish::for_context(&dispatcher, "interactive", identity, ShellPolicy::Agent, ShellCwd::Context,
                dispatcher.semantic_index(), dispatcher.block_source()).await.map_err(|e| e.to_string())
        } => result?,
    };
    let documents = kernel.blocks();
    if documents.get(context).is_none() { return Err(format!("context {context} is not materialized")); }
    let last = documents.last_block_id(context);
    let command = documents.insert_tool_call_as(context, None, last.as_ref(), "shell",
        serde_json::json!({"code": code}), Some(ToolKind::Shell), Some(identity.performer), None,
        user_initiated.then_some(Role::User)).map_err(|e| e.to_string())?;
    let output = documents.insert_tool_result_as(context, &command, Some(&command), "", Status::Done, None,
        Some(ToolKind::Shell), Some(PrincipalId::system()), None).map_err(|e| e.to_string())?;
    let epoch = kernel.kernel_db().lock().continuation_epoch(context).map_err(|e| e.to_string())?;
    let receipt = kernel.shell_operations().register(context, identity.requester, identity.performer,
        command, output, &code, epoch)?;
    let mut submission = ShellSubmission { command_block_id: command,
        operation_id: receipt.operation_id.to_string(), refusal: None };
    let call_ctx = crate::mcp::CallContext::new(identity.requester, context, identity.session, kernel.id())
        .with_actor(identity.performer, identity.reviewer);
    // Preparation owns this exact pair until command settlement takes over.
    // Capture and result-hook unwinds belong to command::run_into_blocks.
    let mut settlement_started = false;
    let preparation = std::panic::AssertUnwindSafe(async {
        documents.set_status(context, &output, Status::Running).map_err(|e| e.to_string())?;
        if user_initiated {
            documents.set_excluded(context, &command, true).map_err(|e| e.to_string())?;
            documents.set_excluded(context, &output, true).map_err(|e| e.to_string())?;
        }
        let verdict = tokio::select! {
            biased;
            _ = stop.cancelled() => return Err("kernel runtime shut down before interactive execution".to_string()),
            verdict = kernel.broker().shell_pre_call_hooks(&code, &call_ctx) => verdict,
        };
        let execute = matches!(verdict, crate::mcp::ShellHookVerdict::Proceed);
        if !execute {
            let mut outcome = CommandOutcome::new(CommandExecution::NotRun, 0);
            outcome.apply_hook(verdict);
            settlement_started = true;
            command::settle_outcome(kernel, context, &command, &output, &outcome)?;
            if let Some(refusal) = outcome.refusal() {
                if let Some(ask) = refusal.ask_id() {
                    kernel.kernel_db().lock().link_ask_blocks(ask, &command, &output, crate::PairOwner::Session)
                        .map_err(|e| e.to_string())?;
                }
                submission.refusal = Some(refusal.clone());
            } else if let Some(CommandHookEffect::Refused { reason, .. }) = &outcome.hook {
                return Err(reason.clone());
            }
        }
        if submission.refusal.is_none() {
            if let ShellSource::Draft(draft) = &source {
                documents.consume_draft(draft).map_err(|e| format!("clear draft: {e}"))?;
            }
        }
        Ok(execute)
    }).catch_unwind().await;
    let execute = match preparation {
        Ok(Ok(execute)) => execute,
        failure => {
            let reason = match &failure {
                Ok(Err(error)) => error.as_str(),
                Err(_) => "Interactive pre-call preparation panicked; source was not run.",
                _ => unreachable!(),
            };
            // Settlement may have retained a refusal or replacement before a
            // projection failed. Its recovery record owns that outcome now.
            if !settlement_started {
                let mut outcome = CommandOutcome::new(CommandExecution::NotRun, 0);
                outcome.settlement_error = Some(reason.into());
                if let Err(error) = command::settle_outcome(kernel, context, &command, &output, &outcome) {
                    tracing::error!("could not settle interactive preparation failure: {error}");
                }
            }
            match failure {
                Ok(Err(error)) => return Err(error),
                Err(panic) => std::panic::resume_unwind(panic),
                _ => unreachable!(),
            }
        }
    };
    Ok((submission, execute.then_some((kaish, call_ctx, output, code))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{HookAction, HookBody, HookEntry, HookId};

    async fn fixture() -> (Arc<crate::kj::KjDispatcher>, ShellIdentity) {
        use crate::vfs::VfsOps;
        let dispatcher = Arc::new(crate::kj::test_helpers::test_dispatcher().await);
        dispatcher.set_self_arc();
        let kernel = dispatcher.kernel();
        kernel.broker().set_kj_dispatcher(&dispatcher).await;
        kernel.vfs().write_all(std::path::Path::new("/config/kernel/gate.toml"), b"[global]\n").await.unwrap();
        let requester = PrincipalId::new();
        let performer = PrincipalId::new();
        let context = crate::kj::test_helpers::register_context(&dispatcher, Some("interactive-owner"), None, requester);
        kernel.blocks().create_document(context, crate::DocumentKind::Conversation, None).unwrap();
        (dispatcher, ShellIdentity { requester, performer, reviewer: None,
            context, session: kaijutsu_types::SessionId::new() })
    }

    struct PausedHook {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        panic: bool,
    }

    #[async_trait::async_trait]
    impl crate::mcp::Hook for PausedHook {
        async fn invoke(&self, _: &crate::mcp::KernelCallParams, _: &crate::mcp::CallContext) -> crate::mcp::McpResult<()> {
            self.entered.notify_one();
            self.release.notified().await;
            assert!(!self.panic, "interactive pre-call panic sentinel");
            Ok(())
        }
    }

    #[tokio::test]
    async fn stopped_runtime_does_not_author_interactive_blocks() {
        let (dispatcher, identity) = fixture().await;
        let kernel = dispatcher.kernel();
        kernel.shutdown_runtime_worker().await.unwrap();
        let result = submit(kernel, identity, ShellSource::Code("echo never".into()), true).await;
        assert!(matches!(result, Err(ref error) if error.contains("shut down")));
        assert!(kernel.blocks().block_snapshots(identity.context).unwrap().is_empty());
    }

    #[tokio::test]
    async fn context_switch_is_applied_before_completion_but_cannot_hold_shutdown() {
        for shutdown in [false, true] {
            let (dispatcher, identity) = fixture().await;
            let kernel = dispatcher.kernel();
            kernel.mount("/scratch", crate::vfs::MemoryBackend::new()).await;
            kernel.kernel_db().lock().upsert_context_shell(&crate::kernel_db::ContextShellRow {
                context_id: identity.context, cwd: Some("/scratch".into()), updated_at: 0,
            }).unwrap();
            let target = crate::kj::test_helpers::register_context(&dispatcher, Some("switch-target"), None, identity.performer);
            kernel.blocks().create_document(target, crate::DocumentKind::Conversation, None).unwrap();
            let (submission, mut switches) = submit(kernel, identity,
                ShellSource::Code(format!("kj context switch {target}")), true).await.unwrap();
            let switch = tokio::time::timeout(std::time::Duration::from_secs(3), switches.recv()).await.unwrap().unwrap();
            assert_eq!(switch.context, target);
            assert!(kernel.shell_operations().get(&submission.operation_id, identity.context).unwrap().unwrap().completed_at.is_none());
            if !shutdown {
                switch.applied.send(()).unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(3), async {
                    loop {
                        let operation = kernel.shell_operations().get(&submission.operation_id, identity.context).unwrap().unwrap();
                        if operation.completed_at.is_some() {
                            assert!(!operation.envelope.unwrap().is_error());
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                }).await.expect("acknowledgement must release command completion");
            }
            tokio::time::timeout(std::time::Duration::from_secs(3), kernel.shutdown_runtime_worker())
                .await.expect("connection acknowledgement cannot prevent shutdown").unwrap();
            assert!(kernel.shell_operations().get(&submission.operation_id, identity.context).unwrap().unwrap().completed_at.is_some());
        }
    }

    #[tokio::test]
    async fn accepted_draft_survives_submitting_localset_and_consumes_only_its_revision() {
        for edit in [false, true] { paused_preparation(false, false, edit).await; }
    }

    #[tokio::test]
    async fn shutdown_before_pre_call_finishes_preserves_draft_and_settles_unrun_pair() {
        paused_preparation(true, false, false).await;
    }

    #[tokio::test]
    async fn pre_call_unwind_preserves_draft_and_settles_unrun_pair_before_failing_worker() {
        paused_preparation(false, true, false).await;
    }

    async fn paused_preparation(shutdown: bool, panic: bool, edit: bool) {
        let (dispatcher, identity) = fixture().await;
        let kernel = dispatcher.kernel().clone();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        kernel.broker().hooks().write().await.pre_call.entries.push(HookEntry {
            id: HookId("pause-interactive".into()), match_instance: None, match_tool: None,
            match_context: Some(identity.context), match_principal: None, priority: 0, kaish_script_id: None,
            action: HookAction::Invoke(HookBody::Builtin { name: "pause-interactive".into(),
                hook: Arc::new(PausedHook { entered: entered.clone(), release: release.clone(), panic }) }),
        });
        let code = "echo accepted-draft";
        kernel.blocks().edit_draft(identity.context, identity.performer, 0, code, 0).unwrap();
        let draft = kernel.blocks().draft_for_submission(identity.context, identity.performer).unwrap().unwrap();
        let local = tokio::task::LocalSet::new();
        let owner = kernel.clone();
        local.spawn_local(async move { submit(&owner, identity, ShellSource::Draft(draft), true).await });
        local.run_until(tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified()))
            .await.expect("interactive command must reach PreCall");
        drop(local);
        if edit {
            kernel.blocks().edit_draft(identity.context, identity.performer, code.len(), " later", 0).unwrap();
        }
        if shutdown {
            tokio::time::timeout(std::time::Duration::from_secs(3), kernel.shutdown_runtime_worker()).await.unwrap().unwrap();
        } else {
            release.notify_one();
        }
        let operation = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let operations = kernel.shell_operations().list_for_context(identity.context).unwrap();
                assert_eq!(operations.len(), 1);
                if operations[0].completed_at.is_some() { break operations.into_iter().next().unwrap(); }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        }).await.expect("interactive operation must settle independently of its caller");
        let draft = kernel.blocks().draft_block(identity.context, identity.performer).unwrap();
        assert_eq!(draft.is_some(), shutdown || panic || edit);
        if let Some(draft) = draft { assert_eq!(draft.content, if edit { "echo accepted-draft later" } else { code }); }
        let blocks = kernel.blocks().block_snapshots(identity.context).unwrap();
        let command = blocks.iter().find(|b| b.id == operation.receipt.command_block_id).unwrap();
        let output = blocks.iter().find(|b| b.id == operation.receipt.output_block_id).unwrap();
        assert_eq!(command.id.principal_id, identity.performer);
        assert!(command.excluded && output.excluded);
        assert_eq!(output.status, if shutdown || panic { Status::Error } else { Status::Done });
        assert_eq!(output.content, if shutdown || panic { "" } else { "accepted-draft\n" });
        let (requester, performer): (Vec<u8>, Vec<u8>) = kernel.kernel_db().lock().conn_for_ledger()
            .query_row("SELECT principal_id,actor_id FROM shell_operations WHERE command_block_id=?1",
                [command.id.to_key()], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(requester, identity.requester.as_bytes());
        assert_eq!(performer, identity.performer.as_bytes());
        assert_eq!(kernel.shutdown_runtime_worker().await.is_err(), panic);
    }
}
