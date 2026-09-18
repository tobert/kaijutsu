//! Addressed `kj` commands with structured arguments and optional transcript output.

use std::sync::Arc;
use futures::FutureExt;
use tracing::Instrument;
use tokio_util::sync::CancellationToken;
use kaijutsu_types::{BlockId, Refusal, Role, ToolKind};
#[cfg(test)]
use kaijutsu_types::{PrincipalId, Status};
use crate::Kernel;
use super::command::{self, CommandContextSwitch, CommandRunOptions};
use super::command_outcome::{CommandExecution, CommandHookEffect, CommandOutcome};
use super::context_shell::{ShellCwd, ShellIdentity, ShellPolicy};
use super::embedded_kaish::EmbeddedKaish;

pub struct ExecutedKj {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub command_block_id: Option<BlockId>,
    pub latch: Option<super::kj_builtin::KjLatchInfo>,
    pub data: Option<serde_json::Value>,
}

// Source execution supplies per-call cancellation; kaish's execute_argv has
// no ExecuteOptions argument. Backticks are literal inside kaish double quotes.
fn kaish_quote(word: &str) -> String {
    let escaped = word.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$");
    format!("\"{}\"", escaped)
}

/// Admit one structured `kj` invocation to the kernel worker. Accepted work
/// survives its caller; a result review returns Pending while execution remains
/// retained. Structured argv stays literal and the addressed context stays pinned.
/// Quiet calls author no command/output pair or operation receipt.
pub async fn execute_kj(
    kernel: &Arc<Kernel>, identity: ShellIdentity, argv: &[String], quiet: bool,
) -> Result<Result<ExecutedKj, Refusal>, String> {
    let (notices, mut reviews) = tokio::sync::mpsc::unbounded_channel();
    let (reply, completed) = tokio::sync::oneshot::channel();
    let owner = kernel.clone();
    let argv = argv.to_vec();
    let span = tracing::Span::current();
    let hook_depth = crate::mcp::broker::current_hook_depth();
    kernel.spawn_runtime_task(move |stop| crate::mcp::broker::inherit_hook_depth(hook_depth, async move {
        let result = run_kj(&owner, identity, &argv, quiet, notices, stop).await;
        if let Err(Err(error)) = reply.send(result) {
            tracing::error!(context = %identity.context,
                "structured command settlement failed after its caller departed: {error}");
        }
    }.instrument(span)))?;
    tokio::select! {
        Some(refusal) = reviews.recv() => Ok(Err(refusal)),
        result = completed => result.map_err(|_| "structured command task stopped before replying".to_string())?,
    }
}

async fn run_kj(
    kernel: &Arc<Kernel>, identity: ShellIdentity, argv: &[String], quiet: bool,
    notices: tokio::sync::mpsc::UnboundedSender<Refusal>, stop: CancellationToken,
) -> Result<Result<ExecutedKj, Refusal>, String> {
    if stop.is_cancelled() { return Err("kernel runtime shut down before structured execution".into()); }
    let context = identity.context;
    if kernel.kernel_db().lock().get_context(context).map_err(|e| e.to_string())?.is_none() {
        return Err("context not found".into());
    }
    let kaish = tokio::select! {
        biased;
        _ = stop.cancelled() => return Err("kernel runtime shut down during structured preparation".into()),
        result = async {
            let dispatcher = kernel.broker().kj_dispatcher().await.ok_or("kj dispatcher is not registered")?;
            EmbeddedKaish::for_context(&dispatcher, "structured-kj", identity, ShellPolicy::Agent, ShellCwd::Context,
                dispatcher.semantic_index(), dispatcher.block_source()).await.map_err(|e| e.to_string())
        } => result?,
    };
    let documents = kernel.blocks();
    if documents.get(context).is_none() { return Err(format!("context {context} is not materialized")); }
    let mut code = String::from("kj");
    for arg in argv { code.push(' '); code.push_str(&kaish_quote(arg)); }
    let receipt = if quiet { None } else {
        let receipt = documents.start_shell_operation(crate::shell_operations::ShellOperationStart {
            notify: false,
            context, principal: identity.requester, actor: identity.performer, source: &code,
            tool: "kj", input: serde_json::json!({"argv": argv}), kind: ToolKind::Builtin,
            role: Role::User, excluded: false, status: kaijutsu_types::Status::Running, ask: None,
        }).map_err(|error| error.to_string())?;
        Some(receipt)
    };
    let mut call_ctx = crate::mcp::CallContext::new(identity.requester, context, identity.session, kernel.id())
        .with_actor(identity.performer, identity.reviewer);
    call_ctx.publishes_pair = receipt.is_some();
    let verdict = tokio::select! {
        biased;
        _ = stop.cancelled() => {
            let reason = "kernel runtime shut down before structured execution";
            settle_unrun(kernel, receipt.as_ref(), reason)?;
            return Err(reason.into());
        },
        result = std::panic::AssertUnwindSafe(kernel.broker().shell_pre_call_hooks(&code, &call_ctx)).catch_unwind() => match result {
            Ok(verdict) => verdict,
            Err(panic) => {
                if let Err(error) = settle_unrun(kernel, receipt.as_ref(), "Structured pre-call hook panicked; source was not run.") {
                    tracing::error!("could not settle structured pre-call panic: {error}");
                }
                std::panic::resume_unwind(panic);
            }
        },
    };
    let outcome = match verdict {
        crate::mcp::ShellHookVerdict::Proceed => match &receipt {
            Some(receipt) => command::run_into_blocks(&kaish, &code, receipt,
                kernel, &call_ctx, CommandRunOptions { stdin: None,
                    context_switch: CommandContextSwitch::Pinned, review_notices: Some(notices),
                    cancel: Some(stop), ..Default::default() }).await?,
            None => command::run_without_blocks(&kaish, &code, kernel, &call_ctx,
                kaish_kernel::ExecuteOptions::default(), CommandRunOptions { stdin: None,
                    context_switch: CommandContextSwitch::Pinned, review_notices: Some(notices),
                    cancel: Some(stop), ..Default::default() }).await?,
        },
        verdict => {
            let mut outcome = CommandOutcome::new(CommandExecution::NotRun, 0);
            outcome.apply_hook(verdict);
            if let Some(receipt) = &receipt {
                command::settle_operation(kernel, receipt, &outcome, Some(crate::PairOwner::Session))?;
            }
            outcome
        }
    };
    if let Some(error) = &outcome.settlement_error { return Err(error.clone()); }
    if let Some(refusal) = outcome.refusal() { return Ok(Err(refusal.clone())); }
    if let Some(CommandHookEffect::Refused { reason, .. }) = &outcome.hook { return Err(reason.clone()); }
    let result = outcome.exec_result();
    let raw = crate::ansi_ingest::raw_stdout(&result);
    let stdout = crate::ansi_ingest::project(&raw).map_or_else(|| result.text_out().into_owned(), |p| p.text);
    Ok(Ok(ExecutedKj {
        exit_code: result.code.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        stdout, stderr: result.err.clone(), command_block_id: receipt.map(|receipt| receipt.command_block_id),
        latch: super::kj_builtin::latch_from_result(&result),
        data: outcome.output_data().and_then(|output| output.rich_json),
    }))
}

fn settle_unrun(
    kernel: &Kernel, receipt: Option<&crate::shell_operations::ShellOperationReceipt>, reason: &str,
) -> Result<(), String> {
    if let Some(receipt) = receipt {
        let mut outcome = CommandOutcome::new(CommandExecution::NotRun, 0);
        outcome.settlement_error = Some(reason.into());
        command::settle_operation(kernel, receipt, &outcome, None)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (Arc<crate::kj::KjDispatcher>, ShellIdentity) {
        use crate::vfs::VfsOps;
        let dispatcher = Arc::new(crate::kj::test_helpers::test_dispatcher_persistent().await);
        dispatcher.set_self_arc();
        let kernel = dispatcher.kernel();
        kernel.broker().set_kj_dispatcher(&dispatcher).await;
        // Let this fixture exercise PreCall; the shipped allow tier skips it.
        kernel.vfs().write_all(std::path::Path::new("/config/kernel/gate.toml"), b"[global]\n").await.unwrap();
        let requester = PrincipalId::new();
        let context = crate::kj::test_helpers::register_context(&dispatcher, Some("structured-lifetime"), None, requester);
        kernel.blocks().create_document(context, crate::DocumentKind::Conversation, None).unwrap();
        (dispatcher, ShellIdentity { requester, performer: requester, reviewer: None,
            context, session: kaijutsu_types::SessionId::new() })
    }

    struct PausedHook {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl crate::mcp::Hook for PausedHook {
        async fn invoke(&self, _: &crate::mcp::KernelCallParams, _: &crate::mcp::CallContext) -> crate::mcp::McpResult<()> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn stopped_runtime_refuses_structured_execution() {
        let (dispatcher, identity) = fixture().await;
        let kernel = dispatcher.kernel();
        kernel.shutdown_runtime_worker().await.unwrap();
        let result = execute_kj(kernel, identity, &["help".into()], true).await;
        assert!(matches!(result, Err(ref error) if error.contains("shut down")),
            "structured execution ignored stopped runtime admission");
        assert!(kernel.blocks().block_snapshots(identity.context).unwrap().is_empty());
    }

    #[tokio::test]
    async fn accepted_structured_execution_survives_its_submitting_localset() {
        paused_execution(false, false).await;
    }

    #[tokio::test]
    async fn shutdown_settles_a_structured_command_paused_in_result_hooks() {
        paused_execution(true, false).await;
    }

    #[tokio::test]
    async fn shutdown_settles_a_structured_command_before_pre_call_hooks_finish() {
        paused_execution(true, true).await;
    }

    struct PanicHook;

    #[async_trait::async_trait]
    impl crate::mcp::Hook for PanicHook {
        async fn invoke(&self, _: &crate::mcp::KernelCallParams, _: &crate::mcp::CallContext) -> crate::mcp::McpResult<()> {
            panic!("structured pre-call panic sentinel");
        }
    }

    #[tokio::test]
    async fn pre_call_panic_settles_without_running_source_and_fails_the_worker() {
        use crate::mcp::{HookAction, HookBody, HookEntry, HookId};
        let (dispatcher, identity) = fixture().await;
        let kernel = dispatcher.kernel();
        kernel.broker().hooks().write().await.pre_call.entries.push(HookEntry {
            id: HookId("panic-structured".into()), match_instance: None, match_tool: None,
            match_context: Some(identity.context), match_principal: None, priority: 0, kaish_script_id: None,
            action: HookAction::Invoke(HookBody::Builtin { name: "panic-structured".into(), hook: Arc::new(PanicHook) }),
        });
        let argv = ["block", "create", "--role", "user", "--kind", "text", "--content", "must-not-run"]
            .into_iter().map(str::to_owned).collect::<Vec<_>>();
        assert!(execute_kj(kernel, identity, &argv, false).await.is_err());
        assert!(kernel.shutdown_runtime_worker().await.is_err());
        let operations = kernel.shell_operations().list_for_context(identity.context).unwrap();
        assert_eq!(operations.len(), 1);
        assert!(operations[0].completed_at.is_some());
        let blocks = kernel.blocks().block_snapshots(identity.context).unwrap();
        assert_eq!(blocks.len(), 2);
        assert!(blocks.iter().all(|block| block.status == Status::Error));
        assert!(blocks.iter().all(|block| block.content != "must-not-run"));
    }

    async fn paused_execution(shutdown: bool, pre_call: bool) {
        use crate::mcp::{HookAction, HookBody, HookEntry, HookId};
        let (dispatcher, identity) = fixture().await;
        let kernel = dispatcher.kernel().clone();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut hooks = kernel.broker().hooks().write().await;
        let phase = if pre_call { &mut hooks.pre_call } else { &mut hooks.post_call };
        phase.entries.push(HookEntry {
            id: HookId("pause-structured".into()), match_instance: None, match_tool: None,
            match_context: Some(identity.context), match_principal: None, priority: 0, kaish_script_id: None,
            action: HookAction::Invoke(HookBody::Builtin { name: "pause-structured".into(),
                hook: Arc::new(PausedHook { entered: entered.clone(), release: release.clone() }) }),
        });
        drop(hooks);
        let local = tokio::task::LocalSet::new();
        let submitted = kernel.clone();
        local.spawn_local(async move {
            let argv = ["block", "create", "--role", "user", "--kind", "text", "--content", "structured-once"]
                .into_iter().map(str::to_owned).collect::<Vec<_>>();
            execute_kj(&submitted, identity, &argv, false).await
        });
        local.run_until(tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified()))
            .await.expect("structured execution must reach its result hook");
        if shutdown {
            tokio::time::timeout(std::time::Duration::from_secs(3), kernel.shutdown_runtime_worker())
                .await.expect("shutdown must cancel result hooks").unwrap();
        } else {
            drop(local);
            release.notify_one();
        }
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let operations = kernel.shell_operations().list_for_context(identity.context).unwrap();
                assert_eq!(operations.len(), 1);
                if operations[0].completed_at.is_some() { break; }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        }).await.expect("accepted structured execution must settle independently of its submitter");
        let blocks = kernel.blocks().block_snapshots(identity.context).unwrap();
        assert_eq!(blocks.iter().filter(|block| block.content == "structured-once").count(), if pre_call { 0 } else { 1 });
        kernel.shutdown_runtime_worker().await.unwrap();
    }

    #[tokio::test]
    async fn structured_execution_preserves_literal_arguments_and_distinct_identities() {
        let dispatcher = Arc::new(crate::kj::test_helpers::test_dispatcher_persistent().await);
        dispatcher.set_self_arc();
        let kernel = dispatcher.kernel();
        kernel.broker().set_kj_dispatcher(&dispatcher).await;
        let requester = PrincipalId::new();
        let performer = PrincipalId::new();
        let context = crate::kj::test_helpers::register_context(&dispatcher, Some("structured"), None, requester);
        kernel.blocks().create_document(context, crate::block_store::DocumentKind::Conversation, None).unwrap();
        let content = "literal $(echo unexpected); $HOME `echo unexpected` \"quotes\" \\ end";
        let argv: Vec<String> = ["block", "create", "--role", "user", "--kind", "text", "--content", content]
            .into_iter().map(str::to_owned).collect();
        let reply = execute_kj(kernel, ShellIdentity { requester, performer, reviewer: None,
            context, session: kaijutsu_types::SessionId::new() }, &argv, false).await.unwrap().unwrap();
        assert_eq!(reply.exit_code, 0, "{}", reply.stderr);
        let command = reply.command_block_id.unwrap();
        assert_eq!(command.principal_id, performer);
        let authored = kernel.blocks().block_snapshots(context).unwrap().into_iter()
            .find(|block| block.kind == kaijutsu_types::BlockKind::Text).unwrap();
        assert_eq!(authored.content, content, "arguments must remain literal");
        assert_eq!(authored.id.principal_id, performer);
        let (stored_requester, stored_performer): (Vec<u8>, Vec<u8>) = kernel.kernel_db().lock().conn_for_ledger()
            .query_row("SELECT principal_id,actor_id FROM shell_operations WHERE command_block_id=?1",
                [command.to_key()], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(stored_requester, requester.as_bytes());
        assert_eq!(stored_performer, performer.as_bytes());
    }

}
