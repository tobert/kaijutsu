//! Streaming shell execution with caller cancellation and kernel-owned settlement.

use std::sync::Arc;
use tracing::Instrument;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use kaijutsu_types::Refusal;
use crate::Kernel;
use super::command::{self, CommandContextSwitch, CommandRunOptions, ContextSwitch, ShellStateWriteBack};
use super::command_outcome::{CommandExecution, CommandOutcome};
use super::context_shell::{ShellCwd, ShellIdentity, ShellPolicy};
use super::embedded_kaish::EmbeddedKaish;

/// The adapter applies and acknowledges switches while waiting for completion.
/// It owns its execution ID, output subscribers, and cancellation on disconnect.
pub struct StreamingExecution {
    pub switches: mpsc::UnboundedReceiver<ContextSwitch>,
    pub completed: oneshot::Receiver<Result<CommandOutcome, String>>,
}

/// Prepare one streaming invocation on the kernel worker. PreCall refusals
/// return before an execution ID is returned; replacements follow completion.
/// The caller token interrupts preparation, execution and review. Kernel shutdown
/// cancels accepted work and joins settlement even if the caller has departed.
/// `policy` is the shell the seat's facade chose
/// ([`crate::mcp::Broker::check_shell_facade`]); a read-only one asks as the
/// read-only `shell` and leaves the context's cwd and env unchanged.
pub async fn execute(
    kernel: &Arc<Kernel>, identity: ShellIdentity, policy: ShellPolicy, code: String, cancel: CancellationToken,
) -> Result<Result<StreamingExecution, Refusal>, String> {
    let read_only = matches!(policy, ShellPolicy::ReadOnly);
    let (ready, admitted) = oneshot::channel();
    let (reply, completed) = oneshot::channel();
    let (switches, receiver) = mpsc::unbounded_channel();
    let owner = kernel.clone();
    let cancel = cancel.child_token();
    let depth = crate::mcp::broker::current_hook_depth();
    let span = tracing::Span::current();
    kernel.spawn_context_task(identity.context, move |admission, shutdown| crate::mcp::broker::inherit_hook_depth(depth, async move {
        let run = async {
            let (kaish, call, replacement, judged) = match prepare(&owner, admission, identity, policy, &code, &cancel).await {
                Ok(Ok(prepared)) => prepared,
                Ok(Err(refusal)) => { let _ = ready.send(Ok(Err(refusal))); return; }
                Err(error) => { let _ = ready.send(Err(error)); return; }
            };
            if ready.send(Ok(Ok(()))).is_err() { cancel.cancel(); }
            let record = |context| -> futures::future::LocalBoxFuture<'_, ()> {
                Box::pin(command::send_context_switch(context, &switches, &cancel))
            };
            let outcome = match replacement {
                Some(outcome) => Ok(outcome),
                None => command::run_without_blocks(&kaish, &code, &owner, &call,
                    CommandRunOptions {
                        cancel: Some(cancel.clone()), context_switch: CommandContextSwitch::Publish(Some(&record)),
                        state_writeback: if read_only { ShellStateWriteBack::Discard } else { ShellStateWriteBack::Persist },
                        judged,
                        ..Default::default()
                    }).await,
            };
            if let Err(Err(error)) = reply.send(outcome) {
                tracing::error!("streaming settlement failed after its caller departed: {error}");
            }
        };
        tokio::pin!(run);
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => { cancel.cancel(); run.await; }
            _ = &mut run => {}
        }
    }.instrument(span)))?;
    match admitted.await.map_err(|_| "streaming command task stopped during preparation".to_string())?? {
        Ok(()) => Ok(Ok(StreamingExecution { switches: receiver, completed })),
        Err(refusal) => Ok(Err(refusal)),
    }
}

/// The shell, the call, a hook's replacement outcome when one stands in
/// for the command, and the program files the council judged.
type Prepared = (EmbeddedKaish, crate::mcp::CallContext, Option<CommandOutcome>, Vec<crate::council::programs::JudgedFile>);

async fn prepare(
    kernel: &Arc<Kernel>, admission: super::admission::ContextAdmission, identity: ShellIdentity, policy: ShellPolicy,
    code: &str, cancel: &CancellationToken,
) -> Result<Result<Prepared, Refusal>, String> {
    debug_assert_eq!(admission.context(), identity.context);
    let read_only = matches!(policy, ShellPolicy::ReadOnly);
    let kaish = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err("streaming command cancelled before execution".into()),
        result = async {
            let dispatcher = kernel.broker().kj_dispatcher().await.ok_or("kj dispatcher is not registered")?;
            EmbeddedKaish::for_context(&dispatcher, "streaming", identity, policy, ShellCwd::Context,
                dispatcher.semantic_index(), dispatcher.block_source()).await.map_err(|e| e.to_string())
        } => result?,
    };
    let call = crate::mcp::CallContext::new(identity.requester, identity.context, identity.session, kernel.id())
        .with_actor(identity.performer, identity.reviewer);
    let verdict = if read_only {
        kernel.broker().read_only_shell_pre_call_hooks(code, &call, cancel).await
    } else {
        kernel.broker().shell_pre_call_hooks(code, &call, cancel).await
    };
    let mut judged = Vec::new();
    let replacement = match verdict {
        crate::mcp::ShellHookVerdict::Proceed(files) => {
            judged = files;
            None
        }
        crate::mcp::ShellHookVerdict::Denied(error) => return match error.as_refusal() {
            Some(refusal) => Ok(Err(refusal)),
            None => Err(format!("execute: {error}")),
        },
        verdict => {
            let mut outcome = CommandOutcome::new(CommandExecution::NotRun, 0);
            outcome.apply_hook(verdict);
            Some(outcome)
        }
    };
    Ok(Ok((kaish, call, replacement, judged)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{HookAction, HookBody, HookEntry, HookId};
    use kaijutsu_types::PrincipalId;

    async fn fixture() -> (Arc<crate::kj::KjDispatcher>, ShellIdentity) {
        use crate::vfs::VfsOps;
        let dispatcher = Arc::new(crate::kj::test_helpers::test_dispatcher().await);
        dispatcher.set_self_arc();
        let kernel = dispatcher.kernel();
        kernel.broker().set_kj_dispatcher(&dispatcher).await;
        kernel.vfs().write_all(std::path::Path::new("/config/kernel/gate.toml"), b"[global]\n").await.unwrap();
        let requester = PrincipalId::new();
        let context = crate::kj::test_helpers::register_context(&dispatcher, Some("streaming-owner"), None, requester);
        kernel.blocks().create_document(context, crate::DocumentKind::Conversation, None).unwrap();
        let performer = crate::kj::test_helpers::root_character(&dispatcher);
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
        async fn invoke(&self, _: &crate::mcp::KernelCallParams, _: &crate::mcp::CallContext, cancel: &tokio_util::sync::CancellationToken) -> crate::mcp::McpResult<()> {
            self.entered.notify_one();
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(crate::mcp::McpError::Cancelled),
                _ = self.release.notified() => {}
            }
            assert!(!self.panic, "streaming hook panic sentinel");
            Ok(())
        }
    }

    async fn pause(kernel: &Kernel, identity: ShellIdentity, pre_call: bool, panic: bool)
        -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)
    {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut hooks = kernel.broker().hooks().write().await;
        let phase = if pre_call { &mut hooks.pre_call } else { &mut hooks.post_call };
        phase.entries.push(HookEntry {
            id: HookId("pause-streaming".into()), match_instance: None, match_tool: None,
            match_context: Some(identity.context), match_principal: None, priority: 0, kaish_script_id: None,
            action: HookAction::Invoke(HookBody::Builtin { name: "pause-streaming".into(),
                hook: Arc::new(PausedHook { entered: entered.clone(), release: release.clone(), panic }) }),
        });
        (entered, release)
    }

    /// A read-only streaming command asks as the read-only `shell`, so an
    /// approval resumes it read-only; a writable one asks as `shell_write`.
    #[tokio::test]
    async fn an_ask_names_the_shell_the_policy_chose() {
        for (policy, instance, tool) in [(ShellPolicy::ReadOnly, "builtin.shell", "shell"),
            (ShellPolicy::Agent, "builtin.shell_write", "shell_write")] {
            let (dispatcher, mut identity) = fixture().await;
            identity.reviewer = Some(crate::kj::test_helpers::test_reviewer_principal());
            let kernel = dispatcher.kernel();
            let context = identity.context;
            kernel.kernel_db().lock().update_character_root(identity.performer, false).unwrap();
            kernel.kernel_db().lock().update_context_review(context, Some(identity.performer), identity.reviewer).unwrap();
            kernel.broker().hooks().write().await.pre_call.entries.push(HookEntry {
                id: HookId("ask-every-shell".into()), match_instance: None, match_tool: None,
                match_context: Some(context), match_principal: None, priority: 0, kaish_script_id: None,
                action: HookAction::Ask(crate::mcp::AskSpec { description: Some("ask every shell".into()) }),
            });
            let prepared = prepare(kernel, kernel.admit_context(context).unwrap(), identity, policy,
                "echo probe > /probe", &CancellationToken::new()).await.unwrap();
            assert!(!matches!(prepared, Ok((_, _, None, _))), "{tool}: the ask holds the command");
            let row = approval_ledger::ask::list_pending(kernel.kernel_db().lock().conn_for_ledger()).unwrap()
                .into_iter().find(|row| row.context_id == context.as_bytes()).expect("PreCall must leave its ask");
            assert_eq!((row.instance.as_deref(), row.tool.as_deref()), (Some(instance), Some(tool)));
        }
    }

    #[tokio::test]
    async fn retained_streaming_work_uses_caller_cancellation_after_submitting_localset_drops() {
        for cancel_result in [false, true] {
            let (dispatcher, identity) = fixture().await;
            let kernel = dispatcher.kernel();
            let (entered, release) = pause(kernel, identity, false, false).await;
            let cancel = CancellationToken::new();
            let local = tokio::task::LocalSet::new();
            let execution = local.run_until(execute(kernel, identity, ShellPolicy::Agent,
                "kj block create --role user --kind text --content streaming-once".into(), cancel.clone()))
                .await.unwrap().unwrap();
            local.run_until(tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified())).await.unwrap();
            drop(local);
            if cancel_result { cancel.cancel(); } else { release.notify_one(); }
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), execution.completed).await.unwrap().unwrap().unwrap();
            assert_eq!(outcome.envelope().is_error(), cancel_result);
            assert!(matches!(outcome.execution, CommandExecution::Completed(_)));
            let blocks = kernel.blocks().block_snapshots(identity.context).unwrap();
            assert_eq!(blocks.len(), 1, "streaming must author only the requested text, once");
            assert_eq!(blocks[0].content, "streaming-once");
            assert_eq!(blocks[0].id.principal_id, identity.performer);
            kernel.shutdown_runtime_worker().await.unwrap();
        }
    }

    #[tokio::test]
    async fn shutdown_or_caller_cancellation_interrupts_streaming_pre_call_without_running_source() {
        for shutdown in [false, true] {
            let (dispatcher, identity) = fixture().await;
            let kernel = dispatcher.kernel();
            let (entered, _) = pause(kernel, identity, true, false).await;
            let cancel = CancellationToken::new();
            let submit = execute(kernel, identity, ShellPolicy::Agent,
                "kj block create --role user --kind text --content must-not-run".into(), cancel.clone());
            let stop = async {
                tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified()).await.unwrap();
                if shutdown { kernel.shutdown_runtime_worker().await.unwrap(); } else { cancel.cancel(); }
            };
            let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(3), async { tokio::join!(submit, stop) }).await.unwrap();
            assert!(matches!(result, Err(ref error) if error.contains("cancelled")));
            assert!(kernel.blocks().block_snapshots(identity.context).unwrap().is_empty());
            kernel.shutdown_runtime_worker().await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_streaming_hook_panic_closes_the_reply_and_fails_worker_shutdown() {
        for pre_call in [false, true] {
            let (dispatcher, identity) = fixture().await;
            let kernel = dispatcher.kernel();
            let (_, release) = pause(kernel, identity, pre_call, true).await;
            release.notify_one();
            let result = execute(kernel, identity, ShellPolicy::Agent, "echo captured".into(), CancellationToken::new()).await;
            if pre_call {
                assert!(result.is_err());
            } else {
                let execution = result.unwrap().unwrap();
                assert!(tokio::time::timeout(std::time::Duration::from_secs(3), execution.completed).await.unwrap().is_err());
            }
            assert!(kernel.shutdown_runtime_worker().await.is_err());
        }
    }
}
