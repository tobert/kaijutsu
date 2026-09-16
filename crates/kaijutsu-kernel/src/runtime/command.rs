//! Execute contextual commands and settle their results through shared hooks.
//!
//! Callers choose an existing transcript pair or no transcript output. A
//! transport may supply a context-switch callback and review notices.

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

/// A structured command stays addressed to its context; interactive commands
/// publish an in-shell switch and may update a connection's session map.
pub enum CommandContextSwitch<'a> {
    Publish(ContextSwitchSink<'a>),
    Pinned,
}

/// Tool commands retain their original invocation and broker policy. Direct
/// interactive commands use the normalized shell_write invocation instead.
pub struct CommandHooks<'a> {
    pub broker: &'a crate::mcp::Broker,
    pub params: &'a crate::mcp::KernelCallParams,
    pub max_result_bytes: usize,
}

#[derive(Clone, Copy)]
pub enum ShellStateWriteBack { Persist, Discard }

/// Live job streams are raw execution observations. Final job results and
/// receipts carry the hook-processed outcome instead.
#[derive(Clone, Copy)]
pub enum CommandJobOutput { Settled, LiveExecution }

pub struct CommandRunOptions<'a> {
    pub stdin: Option<String>,
    pub hooks: Option<CommandHooks<'a>>,
    pub state_writeback: ShellStateWriteBack,
    pub job_output: CommandJobOutput,
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    pub job_ready: Option<tokio::sync::oneshot::Sender<()>>,
    pub context_switch: CommandContextSwitch<'a>,
    pub review_notices: Option<tokio::sync::mpsc::UnboundedSender<kaijutsu_types::Refusal>>,
}

impl Default for CommandRunOptions<'_> {
    fn default() -> Self {
        Self { stdin: None, hooks: None, state_writeback: ShellStateWriteBack::Persist, job_output: CommandJobOutput::Settled, cancel: None, job_ready: None, context_switch: CommandContextSwitch::Publish(None), review_notices: None }
    }
}

/// Retain the outcome before projection and commit its receipt before terminal
/// block publication. Failed projections keep their recovery marker and return
/// an error; startup finishes them without executing the command again.
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
    let status = outcome.block_status();
    if let Some(operation) = &operation
        && status != Status::Waiting
    {
        kernel.shell_operations().prepare_settlement(&operation.receipt.operation_id, outcome)?;
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
    if let Some(operation) = &operation {
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
    if let Some(operation) = &operation
        && status != Status::Waiting
    {
        kernel.shell_operations().finish_projection(&operation.receipt.operation_id)?;
    }
    Ok(())
}

/// Finish retained projections at startup without invoking kaish or hooks.
/// A committed receipt plus terminal output proves that output was projected;
/// preserve any subsequent user edits.
pub(crate) fn recover_settlements(kernel: &Kernel) -> Result<usize, String> {
    let pending = kernel.shell_operations().pending_projections()?;
    for operation in &pending {
        let receipt = &operation.receipt;
        let context = receipt.context_id;
        let outcome = kernel.shell_operations().outcome(&receipt.operation_id, context)?
            .ok_or_else(|| format!("shell operation {} lost its pending outcome", receipt.operation_id))?;
        let blocks = kernel.blocks();
        if !blocks.contains(context) { blocks.load_one_from_db(context).map_err(|e| e.to_string())?; }
        let output = blocks.get_block_snapshot(context, &receipt.output_block_id).map_err(|e| e.to_string())?
            .ok_or_else(|| format!("shell operation {} lost its output block", receipt.operation_id))?;
        if operation.completed_at.is_some() && matches!(output.status, Status::Done | Status::Error) {
            kernel.shell_operations().complete_outcome(&receipt.operation_id, &receipt.output_block_id, &outcome)?;
            let command = blocks.get_block_snapshot(context, &receipt.command_block_id).map_err(|e| e.to_string())?
                .ok_or_else(|| format!("shell operation {} lost its command block", receipt.operation_id))?;
            if !matches!(command.status, Status::Done | Status::Error) {
                blocks.set_status(context, &receipt.command_block_id, outcome.block_status()).map_err(|e| e.to_string())?;
            }
            kernel.shell_operations().finish_projection(&receipt.operation_id)?;
        } else {
            settle_outcome(kernel, context, &receipt.command_block_id, &receipt.output_block_id, &outcome)?;
        }
    }
    Ok(pending.len())
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
    context_id: ContextId,
    command_block_id: &BlockId,
    output_block_id: &BlockId,
    kernel: &Arc<Kernel>,
    call_ctx: &crate::mcp::CallContext,
    run: CommandRunOptions<'_>,
) -> Result<CommandOutcome, String> {
    // Yield to let the event loop flush BlockInserted events to clients
    // before we start producing text ops. Without this, fast commands
    // (like `ls`) can emit edit_text before the client has processed the
    // BlockInserted, causing DataMissing errors on the client side.
    tokio::task::yield_now().await;

    let mut options = kaish_kernel::ExecuteOptions::default();
    options.cancel_token = run.cancel.clone();
    if let Some(stdin) = run.stdin {
        options = options.with_stdin(stdin);
    }
    let tracked_job = match kernel.shell_operations().get_by_output(output_block_id, context_id) {
        Ok(Some(operation)) => {
            let manager = kernel.context_job_manager(context_id);
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let job = manager.register(code.to_owned(), receiver).await;
            let cancel = run.cancel.clone().unwrap_or_default();
            manager.set_cancel_token(job, cancel.clone()).await;
            options.cancel_token = Some(cancel);
            if let Err(error) = kernel.shell_operations().attach_job(&operation.receipt.operation_id, job, manager.clone()) {
                let mut outcome = CommandOutcome::new(CommandExecution::NotRun, 0);
                outcome.settlement_error = Some(error);
                let failure = outcome.exec_result();
                let settled = settle_outcome(kernel, context_id, command_block_id, output_block_id, &outcome);
                manager.finalize_streams(job, &failure).await;
                let _ = sender.send(failure);
                return settled.map(|()| outcome);
            }
            Some((manager, job, sender))
        }
        Ok(None) => None,
        Err(error) => return Err(error),
    };

    let streams = if matches!(run.job_output, CommandJobOutput::LiveExecution) {
        let (manager, job, _) = tracked_job.as_ref().ok_or("live command output requires a tracked job")?;
        Some(manager.streams(*job).await.ok_or("tracked command lost its output streams")?)
    } else { None };
    if let Some(ready) = run.job_ready { let _ = ready.send(()); }
    let started = std::time::Instant::now();
    let review_cancel = options.cancel_token.clone().unwrap_or_default();
    let mut outcome = capture_command(kaish, code, options, kernel, context_id, run.context_switch, run.state_writeback, streams).await;

    let review = super::result_review::CommandResultReview::new(kernel.clone(), call_ctx.clone(),
        Some((*command_block_id, *output_block_id)), outcome.clone(), review_cancel, run.review_notices);
    if finish_result_hooks(&mut outcome, code, kernel, call_ctx, run.hooks, &review).await? {
        outcome.elapsed_ms = started.elapsed().as_millis() as u64;
    }
    let settled = settle_outcome(kernel, context_id, command_block_id, output_block_id, &outcome);
    if let Some((manager, job, sender)) = tracked_job {
        if let Err(error) = &settled { outcome.settlement_error = Some(error.clone()); }
        let result = outcome.exec_result();
        let streams_result = match run.job_output {
            CommandJobOutput::LiveExecution => CommandOutcome::new(outcome.execution.clone(), outcome.elapsed_ms).exec_result(),
            CommandJobOutput::Settled => result.clone(),
        };
        manager.finalize_streams(job, &streams_result).await;
        let _ = sender.send(result);
    }
    settled.map(|()| outcome)
}

/// Execute without transcript blocks. A result review retains an audit record
/// only when it opens an ask; ordinary calls create no review record.
pub async fn run_without_blocks(
    kaish: &EmbeddedKaish,
    code: &str,
    kernel: &Arc<Kernel>,
    call_ctx: &crate::mcp::CallContext,
    mut options: kaish_kernel::ExecuteOptions,
    run: CommandRunOptions<'_>,
) -> Result<CommandOutcome, String> {
    let started = std::time::Instant::now();
    if let Some(stdin) = run.stdin { options = options.with_stdin(stdin); }
    if let Some(cancel) = run.cancel { options.cancel_token = Some(cancel); }
    let cancel = options.cancel_token.clone().unwrap_or_default();
    let mut outcome = capture_command(kaish, code, options,
        kernel, call_ctx.context_id, run.context_switch, run.state_writeback, None).await;
    let review = super::result_review::CommandResultReview::new(kernel.clone(), call_ctx.clone(),
        None, outcome.clone(), cancel, run.review_notices);
    if finish_result_hooks(&mut outcome, code, kernel, call_ctx, run.hooks, &review).await? {
        outcome.elapsed_ms = started.elapsed().as_millis() as u64;
    }
    review.settle(&outcome)?;
    Ok(outcome)
}

async fn capture_command(
    kaish: &EmbeddedKaish,
    code: &str,
    options: kaish_kernel::ExecuteOptions,
    kernel: &Arc<Kernel>,
    context_id: ContextId,
    context_switch: CommandContextSwitch<'_>,
    state_writeback: ShellStateWriteBack,
    streams: Option<kaish_kernel::scheduler::JobStreams>,
) -> CommandOutcome {
    if options.cancel_token.as_ref().is_some_and(|cancel| cancel.is_cancelled()) {
        let mut outcome = CommandOutcome::new(CommandExecution::NotRun, 0);
        outcome.settlement_error = Some("Command was cancelled before execution.".into());
        return outcome;
    }
    // Persist only this invocation's cwd/export changes to the context.
    let state_before = snapshot_shell_state(kaish).await;

    let started = std::time::Instant::now();
    let result = match streams {
        Some(streams) => Box::pin(execute_with_job_output(kaish, code, options, streams)).await,
        None => kaish.execute_with_options(code, options).await,
    };
    let mut outcome = CommandOutcome::from_execution(result, started.elapsed().as_millis() as u64);

    // A context switch saves the outgoing state itself. Its snapshots span
    // two contexts, so only runs that stayed put write back this diff.
    match if let CommandContextSwitch::Publish(sink) = context_switch {
        context_switched(kaish, context_id, sink)
    } else { kaish.context_id().filter(|id| *id != context_id) } {
        Some(new_context_id) => {
            tracing::info!(
                "shell_execute: context switched {} → {}",
                context_id,
                new_context_id
            );
            if matches!(context_switch, CommandContextSwitch::Publish(_)) {
                kernel.block_flows().publish(crate::flows::BlockFlow::ContextSwitched { context_id: new_context_id });
            }
        }
        None if matches!(state_writeback, ShellStateWriteBack::Persist) => {
            let state_after = snapshot_shell_state(kaish).await;
            if let Err(error) = persist_shell_state(kernel.kernel_db(), context_id, &state_before, &state_after) {
                tracing::error!("shell state write failed: {error}");
                outcome.settlement_error = Some(error);
            }
        }
        None => {}
    }

    outcome
}

async fn execute_with_job_output(
    kaish: &EmbeddedKaish, code: &str, options: kaish_kernel::ExecuteOptions,
    streams: kaish_kernel::scheduler::JobStreams,
) -> Result<kaish_kernel::interpreter::ExecResult, kaish_kernel::KernelError> {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<kaish_kernel::interpreter::ExecResult>();
    let mut on_output = move |result: &kaish_kernel::interpreter::ExecResult| {
        sender.send(result.clone()).expect("command output receiver lives through execution");
    };
    let execute = async {
        let result = kaish.execute_with_options_streaming(code, options, &mut on_output).await;
        drop(on_output);
        result
    };
    let write = async {
        while let Some(result) = receiver.recv().await {
            match result.out_bytes() {
                Some(bytes) => streams.stdout.write(bytes).await,
                None => streams.stdout.write(result.text_out().as_bytes()).await,
            }
            streams.stderr.write(result.err.as_bytes()).await;
        }
    };
    let (result, ()) = tokio::join!(execute, write);
    if let Ok(result) = &result {
        match result.code {
            124 => streams.stderr.write(b"background job timed out\n").await,
            130 => streams.stderr.write(b"background job cancelled\n").await,
            _ => {}
        }
    }
    result
}

async fn finish_result_hooks(
    outcome: &mut CommandOutcome, code: &str, kernel: &Kernel,
    call: &crate::mcp::CallContext, invocation: Option<CommandHooks<'_>>,
    review: &super::result_review::CommandResultReview,
) -> Result<bool, String> {
    if review.cancel.is_cancelled() && match &outcome.execution {
        CommandExecution::NotRun => true,
        CommandExecution::Completed(result) => result.original_code.unwrap_or(result.code) == 130,
        _ => false,
    } {
        // Cancellation already settled execution. Do not invent a hook refusal
        // or start result hooks after their owner has stopped.
        return Ok(true);
    }
    let completed = tokio::select! {
        biased;
        _ = review.cancel.cancelled() => false,
        _ = Box::pin(apply_result_hooks(outcome, code, kernel, call, invocation, Some(review))) => true,
    };
    if !completed { *outcome = review.interrupted_outcome()?; }
    Ok(completed)
}

async fn apply_result_hooks(
    outcome: &mut CommandOutcome,
    code: &str,
    kernel: &Kernel,
    call_ctx: &crate::mcp::CallContext,
    invocation: Option<CommandHooks<'_>>,
    review: Option<&dyn crate::mcp::broker::ResultReview>,
) {
    let direct = crate::mcp::Broker::shell_write_hook_params(code);
    let (broker, params) = invocation.as_ref().map_or((kernel.broker().as_ref(), &direct), |hook| (hook.broker, hook.params));
    let verdict = match &outcome.execution {
        CommandExecution::Completed(result) => {
            let result = if let Some(hook) = &invocation {
                let mut result = super::command_result::shell_envelope_to_tool_result(outcome.envelope());
                if crate::mcp::broker::estimate_result_size(&result) > hook.max_result_bytes {
                    crate::mcp::broker::truncate_result_to_budget(&mut result, hook.max_result_bytes);
                }
                result
            } else { exec_result_to_hook_tool_result(result) };
            broker.result_post_call_hooks(params, call_ctx, &result, review).await
        }
        CommandExecution::Rejected(error) | CommandExecution::Fault(error) => broker.result_error_hooks(
            params, call_ctx, &crate::mcp::McpError::Protocol(error.clone()), review).await,
        CommandExecution::NotRun => unreachable!("a completed invocation has an execution outcome"),
    };
    outcome.apply_hook(verdict);
}

#[cfg(test)]
mod fill_tests {
    use super::*;
    use crate::Kernel;
    use crate::block_store::DocumentKind;

    #[tokio::test]
    async fn a_cancelled_admission_does_not_enter_kaish() {
        let kernel = Arc::new(Kernel::new_ephemeral("cancelled-admission").await);
        let ctx = ContextId::new();
        let kaish = EmbeddedKaish::new("cancelled-admission", kernel.blocks().clone(), kernel.clone(), None).unwrap();
        kaish.set_context_id(ctx);
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let call = crate::mcp::CallContext::new(PrincipalId::system(), ctx, kaijutsu_types::SessionId::new(), kernel.id());
        let outcome = run_without_blocks(&kaish, "export SHOULD_NOT_RUN=yes", &kernel, &call,
            kaish_kernel::ExecuteOptions::default(), CommandRunOptions { cancel: Some(cancel), ..Default::default() })
            .await.unwrap();
        assert!(matches!(outcome.execution, CommandExecution::NotRun), "cancelled admission must not start execution");
        assert!(outcome.envelope().is_error());
        assert!(kaish.get_var("SHOULD_NOT_RUN").await.is_none());

        kernel.blocks().create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let command = kernel.blocks().insert_tool_call(ctx, None, None, "shell_write", serde_json::json!({}), None).unwrap();
        let output = kernel.blocks().insert_tool_result(ctx, &command, Some(&command), "", false, None, None).unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let outcome = run_into_blocks(&kaish, "echo should-not-run", ctx, &command, &output,
            &kernel, &call, CommandRunOptions { cancel: Some(cancel), ..Default::default() }).await.unwrap();
        assert!(matches!(outcome.execution, CommandExecution::NotRun), "a pair without a receipt must still honor cancellation");
        assert_eq!(kernel.blocks().get_block_snapshot(ctx, &output).unwrap().unwrap().status, Status::Error);
    }

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
            let run = run_into_blocks(&kaish, code, ctx, &command, &output,
                &kernel, &call_ctx, CommandRunOptions::default());
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
        run_into_blocks(&kaish, "echo warning >&2; false", ctx, &command, &output,
            &kernel,
            &crate::mcp::CallContext::new(PrincipalId::system(), ctx, kaijutsu_types::SessionId::new(), kernel.id()), CommandRunOptions::default()).await.unwrap();
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
            run_into_blocks(&kaish, &code, ctx, &command, &output,
                &kernel,
                &crate::mcp::CallContext::new(PrincipalId::system(), ctx, kaijutsu_types::SessionId::new(), kernel.id()), CommandRunOptions::default()).await.unwrap();
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

    async fn restart_after_settlement_failure(after_receipt: bool) {
        let kernel = Arc::new(Kernel::new_ephemeral("settlement-recovery").await);
        let documents = kernel.blocks();
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let command = documents.insert_tool_call(ctx, None, None, "shell_write", serde_json::json!({}), None).unwrap();
        let output = documents.insert_tool_result(ctx, &command, Some(&command), "waiting", false, None, None).unwrap();
        for block in [&command, &output] { documents.set_status(ctx, block, Status::Running).unwrap(); }
        let receipt = kernel.shell_operations().register(ctx, PrincipalId::system(), PrincipalId::system(),
            command, output, "never-execute-this-source-on-recovery", None).unwrap();
        let outcome = CommandOutcome::new(CommandExecution::Completed(
            kaish_kernel::interpreter::ExecResult::success("captured before failure")), 13);
        let db = kernel.kernel_db().clone();
        db.lock().conn_for_ledger().execute_batch(if after_receipt {
            "CREATE TRIGGER fail_settlement BEFORE INSERT ON oplog
             WHEN EXISTS(SELECT 1 FROM shell_operations WHERE completed_at IS NOT NULL)
             BEGIN SELECT RAISE(ABORT, 'terminal block write failed'); END;"
        } else {
            "CREATE TRIGGER fail_settlement BEFORE UPDATE OF completed_at ON shell_operations
             BEGIN SELECT RAISE(ABORT, 'receipt write failed'); END;"
        }).unwrap();
        assert!(settle_outcome(&kernel, ctx, &command, &output, &outcome).is_err());
        assert!(kernel.shell_operations().outcome(&receipt.operation_id, ctx).unwrap().is_some(),
            "a failed receipt write must not discard the captured outcome");
        db.lock().conn_for_ledger().execute_batch("DROP TRIGGER fail_settlement;").unwrap();
        let principal = documents.principal_id();
        let workspace = db.lock().get_or_create_default_workspace(principal).unwrap();
        let reloaded = crate::block_store::shared_block_store_with_db(db.clone(), workspace, principal);
        let dir = tempfile::tempdir().unwrap();
        let recovered = Kernel::new("recovered-settlement", dir.path(), reloaded, db).await;
        recovered.blocks().load_one_from_db(ctx).unwrap();
        for block in [&command, &output] {
            assert_eq!(recovered.blocks().get_block_snapshot(ctx, block).unwrap().unwrap().status, Status::Done);
        }
        assert_eq!(recovered.blocks().get_block_snapshot(ctx, &output).unwrap().unwrap().content, "captured before failure");
        let saved = recovered.shell_operations().get(&receipt.operation_id, ctx).unwrap().unwrap();
        assert_eq!(saved.envelope.unwrap().stdout, "captured before failure");
    }

    #[tokio::test]
    async fn restart_retains_outcome_after_failed_receipt_commit() {
        restart_after_settlement_failure(false).await;
    }

    #[tokio::test]
    async fn restart_repairs_blocks_after_receipt_committed() {
        restart_after_settlement_failure(true).await;
    }

    #[tokio::test]
    async fn recovery_does_not_mistake_an_old_terminal_block_for_a_finished_projection() {
        let kernel = Arc::new(Kernel::new_ephemeral("unprojected-outcome").await);
        let documents = kernel.blocks();
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let command = documents.insert_tool_call(ctx, None, None, "shell_write", serde_json::json!({}), None).unwrap();
        let output = documents.insert_tool_result(ctx, &command, Some(&command), "old placeholder", false, None, None).unwrap();
        assert_eq!(documents.get_block_snapshot(ctx, &output).unwrap().unwrap().status, Status::Done);
        let receipt = kernel.shell_operations().register(ctx, PrincipalId::system(), PrincipalId::system(),
            command, output, "do-not-execute", None).unwrap();
        let outcome = CommandOutcome::new(CommandExecution::Completed(
            kaish_kernel::interpreter::ExecResult::success("retained result")), 1);
        kernel.shell_operations().prepare_settlement(&receipt.operation_id, &outcome).unwrap();
        assert_eq!(recover_settlements(&kernel).unwrap(), 1);
        assert_eq!(documents.get_block_snapshot(ctx, &output).unwrap().unwrap().content, "retained result");
        assert_eq!(recover_settlements(&kernel).unwrap(), 0);
    }

    #[tokio::test]
    async fn recovery_preserves_edits_after_terminal_publication() {
        let kernel = Arc::new(Kernel::new_ephemeral("edited-after-settlement").await);
        let documents = kernel.blocks();
        let ctx = ContextId::new();
        documents.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let command = documents.insert_tool_call(ctx, None, None, "shell_write", serde_json::json!({}), None).unwrap();
        let output = documents.insert_tool_result(ctx, &command, Some(&command), "waiting", false, None, None).unwrap();
        let receipt = kernel.shell_operations().register(ctx, PrincipalId::system(), PrincipalId::system(),
            command, output, "do-not-execute", None).unwrap();
        let outcome = CommandOutcome::new(CommandExecution::Completed(
            kaish_kernel::interpreter::ExecResult::success("captured result")), 1);
        kernel.kernel_db().lock().conn_for_ledger().execute_batch(
            "CREATE TRIGGER fail_projection_cleanup BEFORE DELETE ON shell_operation_projections
             BEGIN SELECT RAISE(ABORT, 'cleanup failed'); END;"
        ).unwrap();
        assert!(settle_outcome(&kernel, ctx, &command, &output, &outcome).is_err());
        assert_eq!(documents.get_block_snapshot(ctx, &output).unwrap().unwrap().status, Status::Done);
        documents.replace_text_as(ctx, &output, "later edit", Some(PrincipalId::system())).unwrap();
        kernel.kernel_db().lock().conn_for_ledger().execute_batch("DROP TRIGGER fail_projection_cleanup;").unwrap();
        assert_eq!(recover_settlements(&kernel).unwrap(), 1);
        assert_eq!(documents.get_block_snapshot(ctx, &output).unwrap().unwrap().content, "later edit");
        assert_eq!(kernel.shell_operations().get(&receipt.operation_id, ctx).unwrap().unwrap().envelope.unwrap().stdout, "captured result");
        assert_eq!(recover_settlements(&kernel).unwrap(), 0);
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
        run_into_blocks(&kaish, "echo '", ctx, &command, &output, &kernel,
            &crate::mcp::CallContext::new(PrincipalId::system(), ctx, kaijutsu_types::SessionId::new(), kernel.id()), CommandRunOptions::default()).await.unwrap();
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
            ctx,
            &call,
            &result,
            &kernel,
            &crate::mcp::CallContext::new(
                PrincipalId::system(), ctx, kaijutsu_types::SessionId::new(), kernel.id(),
            ),
            CommandRunOptions::default(),
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
