//! Execute a contextual command into an existing block pair.
//!
//! Interactive submission and approval resume share this owner. A transport
//! may supply a context-switch callback; detached runs have no connection map.

use std::sync::Arc;

use crate::Kernel;
use crate::runtime::embedded_kaish::EmbeddedKaish;
use kaijutsu_types::{BlockId, ContentType, ContextId, PrincipalId, Status};

use super::command_result::exec_result_to_hook_tool_result;
use super::command_outcome::{CommandExecution, CommandOutcome};
use super::shell_state::{persist_shell_state, snapshot_shell_state};

/// Where an in-shell context switch is recorded.
///
/// `None` means nothing is listening: the switch still stops the durable
/// cwd/env write-back and still publishes `ContextSwitched`, it has no
/// session map to update.
pub type ContextSwitchSink<'a> = Option<&'a dyn Fn(ContextId)>;

/// Project the final outcome before publishing terminal block statuses.
/// A receipt commits the raw execution and hook result before either block
/// advertises completion. Projection failures reach the caller for recovery.
pub fn settle_outcome(
    kernel: &Kernel,
    context_id: ContextId,
    command_block_id: &BlockId,
    output_block_id: &BlockId,
    outcome: &CommandOutcome,
) -> Result<(), String> {
    let operation = kernel.shell_operations().get_by_output(output_block_id, context_id)?;
    if operation.as_ref().is_some_and(|operation| operation.receipt.command_block_id != *command_block_id) {
        return Err("shell operation command block does not match its receipt".into());
    }
    let documents = kernel.blocks();
    let envelope = outcome.envelope();
    let raw = match (&outcome.hook, &outcome.execution) {
        (None, CommandExecution::Completed(result)) => crate::ansi_ingest::raw_stdout(result),
        _ => std::borrow::Cow::Borrowed(envelope.stdout.as_bytes()),
    };
    let projection = crate::ansi_ingest::project(&raw);
    let text = projection.as_ref().map_or(envelope.stdout.as_str(), |p| p.text.as_str());
    documents.replace_text_as(context_id, output_block_id, text, Some(PrincipalId::system()))
        .map_err(|e| e.to_string())?;
    if let Some(projection) = projection {
        crate::ansi_ingest::record(documents, context_id, output_block_id, projection.spans, &raw);
    }
    let mut stderr = envelope.stderr.clone();
    if let Some(error) = &envelope.error {
        if !stderr.is_empty() && !stderr.ends_with('\n') { stderr.push('\n'); }
        stderr.push_str(error);
    }
    documents.set_stderr(context_id, output_block_id, if stderr.is_empty() { None } else { Some(stderr) })
        .map_err(|e| e.to_string())?;
    documents.set_output(context_id, output_block_id, outcome.output_data().as_ref())
        .map_err(|e| e.to_string())?;
    documents.set_content_type(context_id, output_block_id,
        envelope.content_type.as_deref().map_or(ContentType::Plain, ContentType::from_mime))
        .map_err(|e| e.to_string())?;
    documents.set_exit_code(context_id, output_block_id,
        envelope.exit_code.map(|code| code.clamp(i32::MIN as i64, i32::MAX as i64) as i32))
        .map_err(|e| e.to_string())?;
    for block in [command_block_id, output_block_id] {
        documents.set_ephemeral(context_id, block, envelope.ephemeral.unwrap_or(false))
            .map_err(|e| e.to_string())?;
    }
    let status = outcome.block_status();
    if let Some(operation) = operation {
        if status == Status::Waiting {
            let ask = envelope.ask_id.as_deref().ok_or("waiting command outcome has no ask")?;
            kernel.shell_operations().mark_waiting(&operation.receipt.operation_id, ask)?;
        } else {
            kernel.shell_operations().complete_outcome(&operation.receipt.operation_id, output_block_id, outcome)?;
        }
    }
    for block in [output_block_id, command_block_id] {
        documents.set_status(context_id, block, status).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Detect an in-shell context switch, tell the sink about it, and report the
/// new context id. The transport owns its connection-local map.
fn context_switched(
    kaish: &EmbeddedKaish,
    started_at: ContextId,
    sink: ContextSwitchSink<'_>,
) -> Option<ContextId> {
    match kaish.context_id() {
        Some(new_id) if new_id != started_at => {
            match sink {
                Some(record) => record(new_id),
                None => tracing::info!(
                    "shell run: the command switched to context {new_id} and there is no \
                     session map to move with it; the run stays reported against {started_at}"
                ),
            }
            Some(new_id)
        }
        _ => None,
    }
}

/// Run `code` in `kaish` and fill the already-authored `command_block_id` /
/// `output_block_id` pair with what it produced.
///
/// The pair must already exist. Hooks and durable shell state settle before
/// projections publish completion. A persistence failure is returned explicitly.
#[allow(clippy::too_many_arguments)]
pub async fn run_into_blocks(
    kaish: &EmbeddedKaish,
    code: &str,
    stdin: Option<String>,
    context_id: ContextId,
    command_block_id: &BlockId,
    output_block_id: &BlockId,
    kernel: &Arc<Kernel>,
    call_ctx: &crate::mcp::CallContext,
    on_context_switch: ContextSwitchSink<'_>,
) -> Result<(), String> {
    // Yield to let the event loop flush BlockInserted events to clients
    // before we start producing text ops. Without this, fast commands
    // (like `ls`) can emit edit_text before the client has processed the
    // BlockInserted, causing DataMissing errors on the client side.
    tokio::task::yield_now().await;

    let mut options = kaish_kernel::ExecuteOptions::default();
    if let Some(stdin) = stdin {
        options = options.with_stdin(stdin);
    }
    let tracked_job = match kernel.shell_operations().get_by_output(output_block_id, context_id) {
        Ok(Some(operation)) => {
            let manager = kernel.context_job_manager(context_id);
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let job = manager.register(code.to_owned(), receiver).await;
            let cancel = tokio_util::sync::CancellationToken::new();
            manager.set_cancel_token(job, cancel.clone()).await;
            options.cancel_token = Some(cancel);
            if let Err(error) = kernel.shell_operations().attach_job(&operation.receipt.operation_id, job, manager.clone()) {
                let mut outcome = CommandOutcome::new(CommandExecution::NotRun, 0);
                outcome.settlement_error = Some(error);
                let failure = outcome.job_result();
                let settled = settle_outcome(kernel, context_id, command_block_id, output_block_id, &outcome);
                manager.finalize_streams(job, &failure).await;
                let _ = sender.send(failure);
                return settled;
            }
            Some((manager, job, sender))
        }
        Ok(None) => None,
        Err(error) => return Err(error),
    };

    // Persist only this invocation's cwd/export changes to the context.
    let state_before = snapshot_shell_state(kaish).await;

    let started = std::time::Instant::now();
    let result = kaish.execute_with_options(code, options).await;
    let verdict = match &result {
        Ok(result) => kernel.broker().shell_post_call_hooks(
            code, call_ctx, &exec_result_to_hook_tool_result(result)).await,
        Err(error) => kernel.broker().shell_on_error_hooks(
            code, call_ctx, &crate::mcp::McpError::Protocol(error.to_string())).await,
    };
    let mut outcome = CommandOutcome::from_execution(result, started.elapsed().as_millis() as u64);
    outcome.apply_hook(verdict);

    // A context switch saves the outgoing state itself. Its snapshots span
    // two contexts, so only runs that stayed put write back this diff.
    match context_switched(kaish, context_id, on_context_switch) {
        Some(new_context_id) => {
            tracing::info!(
                "shell_execute: context switched {} → {}",
                context_id,
                new_context_id
            );
            kernel.block_flows().publish(crate::flows::BlockFlow::ContextSwitched {
                context_id: new_context_id,
            });
        }
        None => {
            let state_after = snapshot_shell_state(kaish).await;
            if let Err(error) = persist_shell_state(kernel.kernel_db(), context_id, &state_before, &state_after) {
                tracing::error!("shell state write failed: {error}");
                outcome.settlement_error = Some(error);
            }
        }
    }

    outcome.elapsed_ms = started.elapsed().as_millis() as u64;
    let settled = settle_outcome(kernel, context_id, command_block_id, output_block_id, &outcome);
    if let Some((manager, job, sender)) = tracked_job {
        if let Err(error) = &settled { outcome.settlement_error = Some(error.clone()); }
        let result = outcome.job_result();
        manager.finalize_streams(job, &result).await;
        let _ = sender.send(result);
    }
    settled
}

#[cfg(test)]
mod fill_tests {
    use super::*;
    use crate::Kernel;
    use crate::block_store::DocumentKind;

    struct PausedHook {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl crate::mcp::Hook for PausedHook {
        async fn invoke(&self, _: &crate::mcp::KernelCallParams,
            _: &crate::mcp::CallContext) -> crate::mcp::McpResult<()> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn terminal_blocks_wait_for_result_hooks() {
        use crate::mcp::{HookAction, HookBody, HookEntry, HookId, KernelToolResult};
        for code in ["echo real-output", "echo '"] {
            let kernel = Arc::new(Kernel::new_ephemeral("wait-for-result-hooks").await);
            let documents = kernel.blocks().clone();
            let ctx = ContextId::new();
            documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
            let command = documents.insert_tool_call(ctx, None, None, "shell_write", serde_json::json!({}), None).unwrap();
            let output = documents.insert_tool_result(ctx, &command, Some(&command), "waiting", false, None, None).unwrap();
            for block in [&command, &output] { documents.set_status(ctx, block, Status::Running).unwrap(); }
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let mut hooks = kernel.broker().hooks().write().await;
            let table = if code == "echo real-output" { &mut hooks.post_call } else { &mut hooks.on_error };
            for (name, action) in [
                ("pause", HookAction::Invoke(HookBody::Builtin { name: "pause".into(),
                    hook: Arc::new(PausedHook { entered: entered.clone(), release: release.clone() }) })),
                ("replace", HookAction::ShortCircuit(KernelToolResult::text("synthetic output"))),
            ] {
                table.entries.push(HookEntry { id: HookId(name.into()), match_instance: None,
                    match_tool: None, match_context: None, match_principal: None, action,
                    priority: 0, kaish_script_id: None });
            }
            drop(hooks);
            let kaish = EmbeddedKaish::new("result-hook", documents.clone(), kernel.clone(), None).unwrap();
            kaish.set_context_id(ctx);
            let call_ctx = crate::mcp::CallContext::new(
                PrincipalId::system(), ctx, kaijutsu_types::SessionId::new(), kernel.id());
            let run = run_into_blocks(&kaish, code, None, ctx, &command, &output,
                &kernel, &call_ctx, None);
            let observe = async {
                tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified()).await.unwrap();
                let command_status = documents.get_block_snapshot(ctx, &command).unwrap().unwrap().status;
                let output_status = documents.get_block_snapshot(ctx, &output).unwrap().unwrap().status;
                release.notify_one();
                (command_status, output_status)
            };
            let (settled, statuses) = tokio::join!(run, observe);
            settled.unwrap();
            assert_eq!(statuses, (Status::Running, Status::Running), "{code}: hook result is not final yet");
            let final_output = documents.get_block_snapshot(ctx, &output).unwrap().unwrap();
            assert_eq!(final_output.status, Status::Done);
            assert_eq!(final_output.content, "synthetic output");
        }
    }

    #[tokio::test]
    async fn replacement_clears_command_metadata_in_every_projection() {
        use crate::mcp::{HookAction, HookEntry, HookId, KernelToolResult, ToolContent};
        let kernel = Arc::new(Kernel::new_ephemeral("replacement-projections").await);
        let documents = kernel.blocks().clone();
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let command = documents.insert_tool_call(ctx, None, None, "shell_write", serde_json::json!({}), None).unwrap();
        let output = documents.insert_tool_result(ctx, &command, Some(&command), "waiting", false, None, None).unwrap();
        let receipt = kernel.shell_operations().register(ctx, PrincipalId::system(), PrincipalId::system(),
            command, output, "echo warning >&2; false", None).unwrap();
        let payload = serde_json::json!({"replacement": true});
        kernel.broker().hooks().write().await.post_call.entries.push(HookEntry {
            id: HookId("replace".into()), match_instance: None, match_tool: None,
            match_context: None, match_principal: None, priority: 0, kaish_script_id: None,
            action: HookAction::ShortCircuit(KernelToolResult {
                is_error: false,
                content: vec![ToolContent::Text("replacement".into()), ToolContent::Json(payload.clone())],
                structured: Some(payload.clone()),
            }),
        });
        let kaish = EmbeddedKaish::new("replacement-projections", documents.clone(), kernel.clone(), None).unwrap();
        kaish.set_context_id(ctx);
        run_into_blocks(&kaish, "echo warning >&2; false", None, ctx, &command, &output,
            &kernel,
            &crate::mcp::CallContext::new(PrincipalId::system(), ctx, kaijutsu_types::SessionId::new(), kernel.id()), None).await.unwrap();
        let block = documents.get_block_snapshot(ctx, &output).unwrap().unwrap();
        assert_eq!(block.status, Status::Done);
        assert_eq!(block.exit_code, None, "a replacement has no command exit code");
        assert!(block.stderr.is_none(), "the replaced command's diagnostic must not leak");
        assert_eq!(block.content, "replacement\n{\"replacement\":true}");
        assert_eq!(block.output.unwrap().rich_json, Some(payload.clone()));
        let state = kernel.shell_operations().get(&receipt.operation_id, ctx).unwrap().unwrap();
        let raw_outcome = kernel.shell_operations().outcome(&receipt.operation_id, ctx).unwrap().unwrap();
        let CommandExecution::Completed(raw) = &raw_outcome.execution else { panic!("missing raw command") };
        assert_eq!(raw.code, 1);
        assert_eq!(raw.err, "warning\n");
        let jobs = kernel.context_job_manager(ctx);
        let job = jobs.list().await.into_iter().find(|job| Some(job.id.to_string()) == state.receipt.job_id).unwrap();
        let job_result = jobs.wait(job.id).await.unwrap();
        assert_eq!(job_result.code, 0);
        assert_eq!(job_result.data, Some(kaish_kernel::ast::Value::Json(payload.clone())));
        assert!(job_result.err.is_empty());
        let envelope = state.envelope.unwrap();
        assert!(!envelope.is_error());
        assert_eq!(envelope.exit_code, None);
        assert_eq!(envelope.data, Some(payload));
        assert_eq!(envelope.stdout, block.content);
        assert!(envelope.stderr.is_empty());
    }

    #[tokio::test]
    async fn real_nonzero_exits_are_errors_in_blocks_and_receipts() {
        for exit in [2, 3] {
            let kernel = Arc::new(Kernel::new_ephemeral("real-exit").await);
            let documents = kernel.blocks().clone();
            let ctx = ContextId::new();
            documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
            let command = documents.insert_tool_call(ctx, None, None, "shell_write", serde_json::json!({}), None).unwrap();
            let output = documents.insert_tool_result(ctx, &command, Some(&command), "waiting", false, None, None).unwrap();
            let code = format!("exit {exit}");
            let receipt = kernel.shell_operations().register(ctx, PrincipalId::system(), PrincipalId::system(),
                command, output, &code, None).unwrap();
            let kaish = EmbeddedKaish::new("real-exit", documents.clone(), kernel.clone(), None).unwrap();
            kaish.set_context_id(ctx);
            run_into_blocks(&kaish, &code, None, ctx, &command, &output,
                &kernel,
                &crate::mcp::CallContext::new(PrincipalId::system(), ctx, kaijutsu_types::SessionId::new(), kernel.id()), None).await.unwrap();
            let block = documents.get_block_snapshot(ctx, &output).unwrap().unwrap();
            assert_eq!(block.status, Status::Error, "exit {exit} is a command failure");
            let envelope = kernel.shell_operations().get(&receipt.operation_id, ctx).unwrap().unwrap().envelope.unwrap();
            assert!(envelope.is_error());
            assert_eq!(envelope.exit_code, Some(exit));
        }
    }

    #[tokio::test]
    async fn failed_receipt_commit_keeps_blocks_running_until_settlement_retries() {
        let kernel = Arc::new(Kernel::new_ephemeral("receipt-commit-failure").await);
        let documents = kernel.blocks();
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let command = documents.insert_tool_call(ctx, None, None, "shell_write", serde_json::json!({}), None).unwrap();
        let output = documents.insert_tool_result(ctx, &command, Some(&command), "waiting", false, None, None).unwrap();
        for block in [&command, &output] { documents.set_status(ctx, block, Status::Running).unwrap(); }
        let receipt = kernel.shell_operations().register(ctx, PrincipalId::system(), PrincipalId::system(),
            command, output, "echo executed-once", None).unwrap();
        let outcome = CommandOutcome::new(CommandExecution::Completed(
            kaish_kernel::interpreter::ExecResult::success("executed-once")), 3);
        kernel.kernel_db().lock().conn_for_ledger().execute_batch(
            "CREATE TRIGGER fail_completion BEFORE UPDATE OF completed_at ON shell_operations BEGIN
             SELECT RAISE(ABORT, 'receipt write failed'); END;"
        ).unwrap();
        let error = settle_outcome(&kernel, ctx, &command, &output, &outcome).unwrap_err();
        assert!(error.contains("receipt write failed"));
        for block in [&command, &output] {
            assert_eq!(documents.get_block_snapshot(ctx, block).unwrap().unwrap().status, Status::Running);
        }
        assert!(kernel.shell_operations().get(&receipt.operation_id, ctx).unwrap().unwrap().completed_at.is_none());
        kernel.kernel_db().lock().conn_for_ledger().execute_batch("DROP TRIGGER fail_completion;").unwrap();
        settle_outcome(&kernel, ctx, &command, &output, &outcome).unwrap();
        settle_outcome(&kernel, ctx, &command, &output, &outcome).unwrap();
        assert_eq!(documents.get_block_snapshot(ctx, &output).unwrap().unwrap().status, Status::Done);
        assert_eq!(kernel.shell_operations().get(&receipt.operation_id, ctx).unwrap().unwrap().envelope.unwrap().stdout, "executed-once");
    }

    #[tokio::test]
    async fn rejected_program_clears_waiting_text_and_preserves_rejection() {
        let kernel = Arc::new(Kernel::new_ephemeral("rejected-command").await);
        let documents = kernel.blocks().clone();
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let command = documents.insert_tool_call(ctx, None, None, "shell_write", serde_json::json!({}), None).unwrap();
        let output = documents.insert_tool_result(ctx, &command, Some(&command), "waiting placeholder", false, None, None).unwrap();
        let receipt = kernel.shell_operations().register(ctx, PrincipalId::system(), PrincipalId::system(),
            command, output, "echo '", None).unwrap();
        let kaish = EmbeddedKaish::new("rejected-command", documents.clone(), kernel.clone(), None).unwrap();
        kaish.set_context_id(ctx);
        run_into_blocks(&kaish, "echo '", None, ctx, &command, &output, &kernel,
            &crate::mcp::CallContext::new(PrincipalId::system(), ctx, kaijutsu_types::SessionId::new(), kernel.id()), None).await.unwrap();
        let block = documents.get_block_snapshot(ctx, &output).unwrap().unwrap();
        assert_eq!(block.status, Status::Error);
        assert!(block.content.is_empty());
        assert!(block.stderr.is_some());
        assert_eq!(block.exit_code, None);
        let envelope = kernel.shell_operations().get(&receipt.operation_id, ctx).unwrap().unwrap().envelope.unwrap();
        assert_eq!(envelope.status, kaijutsu_types::shell_envelope::ShellStatus::Rejected);
        assert_eq!(envelope.exit_code, None);
        assert!(matches!(kernel.shell_operations().outcome(&receipt.operation_id, ctx).unwrap().unwrap().execution,
            CommandExecution::Rejected(_)));
    }

    /// An approved ask fills the pair the gate left `Waiting`. That result
    /// block already carries the gate's placeholder text, and the output
    /// must replace it: a placeholder that survives beside the real output
    /// tells the next turn nothing ran.
    #[tokio::test]
    async fn a_waiting_result_is_replaced_by_the_output_not_prefixed_to_it() {
        let kernel = Arc::new(Kernel::new_ephemeral("fill-waiting").await);
        let documents = kernel.blocks().clone();
        let ctx = ContextId::new();
        documents
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();
        let call = documents
            .insert_tool_call(ctx, None, None, "shell_write", serde_json::json!({}), None)
            .unwrap();
        let placeholder = "gate for shell_write is waiting on a human: nothing was run.";
        let result = documents
            .insert_tool_result(ctx, &call, Some(&call), placeholder, true, None, None)
            .unwrap();
        documents.set_status(ctx, &result, Status::Waiting).unwrap();

        let kaish = EmbeddedKaish::new("fill-waiting", documents.clone(), kernel.clone(), None)
            .expect("EmbeddedKaish::new failed");
        kaish.set_context_id(ctx);
        run_into_blocks(
            &kaish,
            "echo replaced",
            None,
            ctx,
            &call,
            &result,
            &kernel,
            &crate::mcp::CallContext::new(
                PrincipalId::system(), ctx, kaijutsu_types::SessionId::new(), kernel.id(),
            ),
            None,
        )
        .await.unwrap();

        let filled = documents
            .get_block_snapshot(ctx, &result)
            .unwrap()
            .expect("the result block still exists");
        assert_eq!(
            filled.content.trim(),
            "replaced",
            "the output must replace the placeholder, not sit beside it"
        );
        assert_eq!(filled.status, Status::Done);
    }
}
