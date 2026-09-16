//! Execute a contextual command into an existing block pair.
//!
//! Interactive submission and approval resume share this owner. A transport
//! may supply a context-switch callback; detached runs have no connection map.

use std::sync::Arc;

use crate::{
    block_store::SharedBlockStore, flows::SharedBlockFlowBus, kernel_db::KernelDb, Kernel,
};
use crate::runtime::embedded_kaish::EmbeddedKaish;
use kaijutsu_types::{BlockId, ContentType, ContextId, PrincipalId, Status};

use super::command_result::{block_output_data, exec_result_to_hook_tool_result, shell_hook_result_text};
use super::shell_state::{persist_shell_state, snapshot_shell_state};

/// Where an in-shell context switch is recorded.
///
/// `None` means nothing is listening: the switch still stops the durable
/// cwd/env write-back and still publishes `ContextSwitched`, it has no
/// session map to update.
pub type ContextSwitchSink<'a> = Option<&'a dyn Fn(ContextId)>;

/// Persist a registered operation's final result after its block pair settles.
// TODO: Project blocks, receipts, and job results from one settled shell outcome.
// Reconstructing the receipt here duplicates the MCP shell completion policy.
// Preserve caller-specific hooks, cwd/env persistence, and context switching.
// See docs/issues.md, "Turn execution and shell settlement".
pub fn complete_operation_from_blocks(
    kernel: &Kernel,
    context_id: ContextId,
    output_block_id: &BlockId,
) -> Result<(), String> {
    use kaijutsu_types::shell_envelope::{ShellEnvelope, ShellStatus};

    let Some(operation) = kernel.shell_operations()
        .get_by_output(output_block_id, context_id).map_err(|e| e.to_string())?
    else {
        return Ok(());
    };
    let block = kernel.blocks().get_block_snapshot(context_id, output_block_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("shell operation output {output_block_id} is missing"))?;
    if matches!(block.status, Status::Running | Status::Waiting) {
        return Err(format!("shell operation output {output_block_id} has not settled"));
    }
    let status = if block.status == Status::Error {
        ShellStatus::Error
    } else {
        block.exit_code.map_or(ShellStatus::Done, |code| ShellEnvelope::status_for_exit(i64::from(code)))
    };
    let mut envelope = ShellEnvelope::new(status);
    envelope.stdout = block.content;
    envelope.stderr = block.stderr.unwrap_or_default();
    envelope.exit_code = block.exit_code.map(i64::from);
    envelope.block_id = Some(output_block_id.to_key());
    envelope.operation_id = Some(operation.receipt.operation_id.to_string());
    envelope.data = block.output.as_ref().map(|output| output.to_json());
    envelope.content_type = Some(block.content_type.as_mime().to_owned());
    kernel.shell_operations().complete(&operation.receipt.operation_id.to_string(), envelope)
        .map(|_| ())
        .map_err(|e| e.to_string())
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
/// The pair must already exist; this never authors blocks. It settles both
/// to a terminal status on every path, including a kaish fault.
#[allow(clippy::too_many_arguments)]
pub async fn run_into_blocks(
    kaish: &EmbeddedKaish,
    code: &str,
    stdin: Option<String>,
    context_id: ContextId,
    command_block_id: &BlockId,
    output_block_id: &BlockId,
    documents: &SharedBlockStore,
    block_flows: &SharedBlockFlowBus,
    kernel_db: &Arc<parking_lot::Mutex<KernelDb>>,
    kernel: &Arc<Kernel>,
    call_ctx: &crate::mcp::CallContext,
    on_context_switch: ContextSwitchSink<'_>,
) {
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
                let failure = kaish_kernel::interpreter::ExecResult::failure(1, error.clone());
                manager.finalize_streams(job, &failure).await;
                let _ = sender.send(failure);
                let _ = documents.set_stderr(context_id, output_block_id, Some(error));
                let _ = documents.set_status(context_id, output_block_id, Status::Error);
                let _ = documents.set_status(context_id, command_block_id, Status::Error);
                if let Err(error) = complete_operation_from_blocks(kernel, context_id, output_block_id) {
                    tracing::error!("could not settle unstarted shell operation: {error}");
                }
                return;
            }
            Some((manager, job, sender))
        }
        Ok(None) => None,
        Err(error) => {
            let _ = documents.set_stderr(context_id, output_block_id, Some(error));
            let _ = documents.set_status(context_id, output_block_id, Status::Error);
            let _ = documents.set_status(context_id, command_block_id, Status::Error);
            return;
        }
    };

    // Snapshot the shell's durable surface (cwd + exported env) so we can
    // persist whatever this command changes (`cd`, `export`) back to L1.
    let state_before = snapshot_shell_state(kaish).await;

    tracing::debug!(
        "shell_execute: executing code via EmbeddedKaish: {:?}",
        code
    );
    let mut final_status = match kaish
        .execute_with_options(code, options)
        .await
    {
        Ok(result) => {
            tracing::info!(
                "shell_execute: kaish returned code={} original_code={:?} did_spill={} out_len={} err_len={}",
                result.code,
                result.original_code,
                result.did_spill,
                result.text_out().len(),
                result.err.len()
            );

            // stdout → block content (DTE-tracked, app-observable, streams).
            // stderr → its own metadata field so callers can tell them apart
            // (a successful-with-warnings command carries stderr + exit 0).
            // The LLM still sees both: hydration merges stderr back into the
            // tool_result content (see hydrate.rs).
            //
            // ANSI ingest (docs/ansi-and-beyond.md): the raw bytes are the
            // provenance, the projection is the block. `raw_stdout` reads
            // `result.out` directly because `text_out()` is already lossy
            // on the `Bytes` arm — storing a lossy "original" would defeat
            // the whole point of keeping one.
            let raw_out = crate::ansi_ingest::raw_stdout(&result);
            let projection = crate::ansi_ingest::project(&raw_out);
            let out_text = match projection {
                Some(ref p) => p.text.clone(),
                None => result.text_out().into_owned(),
            };
            // Replace, never insert: an output block authored for this run
            // is empty, but a `Waiting` tool result an approval fills already
            // carries the gate's placeholder text.
            if let Err(e) = documents.replace_text_as(
                context_id,
                output_block_id,
                &out_text,
                Some(PrincipalId::system()),
            ) {
                tracing::error!("Failed to update shell output: {}", e);
            }
            // Strictly after the edit: `edit_text` clears style_spans.
            if let Some(p) = projection {
                crate::ansi_ingest::record(
                    documents,
                    context_id,
                    output_block_id,
                    p.spans,
                    &raw_out,
                );
            }

            if !result.err.is_empty()
                && let Err(e) = documents.set_stderr(
                    context_id,
                    output_block_id,
                    Some(result.err.clone()),
                )
            {
                tracing::error!("Failed to set shell stderr: {}", e);
            }

            if let Some(output_data) = block_output_data(&result)
                && let Err(e) = documents.set_output(
                    context_id,
                    output_block_id,
                    Some(&output_data),
                )
            {
                tracing::error!("Failed to set output data: {}", e);
            }

            if let Some(ref ct_str) = result.content_type {
                let ct = ContentType::from_mime(ct_str);
                if ct != ContentType::Plain
                    && let Err(e) =
                        documents.set_content_type(context_id, output_block_id, ct)
                {
                    tracing::error!("Failed to set content_type: {}", e);
                }
            }

            // Read baggage: mark blocks ephemeral if tool signaled it
            if result
                .baggage
                .get("kaijutsu.ephemeral")
                .map(|v| v == "true")
                .unwrap_or(false)
            {
                for bid in [command_block_id, output_block_id] {
                    if let Err(e) = documents.set_ephemeral(context_id, bid, true) {
                        tracing::error!("Failed to set ephemeral on block: {}", e);
                    }
                }
            }

            // Persist the real kaish exit code on the ToolResult block
            // before flipping status. Consumers (MCP context_shell return,
            // BRP introspection, history views) read this to distinguish
            // exit codes that all map to the same Status::Error.
            //
            // `result.code` is the code kaish hands back for `$?` inside a
            // script — and on this `OutputProfile::Agent` shell, a capped
            // command (`did_spill`) has that field FORCIBLY remapped to 3,
            // with the command's actual exit stashed in `original_code`
            // (kaish-kernel's `output_limit` module doc). That remap is a
            // deliberate, loud signal for a script's own control flow — but
            // this durable field is not control flow, it's the permanent
            // record. Resolving through `original_code` here is the same
            // move `mcp/servers/shell.rs`'s `shell_result_to_envelope` makes
            // ("truncation is not failure") — a command that exited 0 and
            // merely printed a lot must not read back as exit_code=3
            // forever because it once got captured over 8 KB.
            // Clamp to i32 — POSIX exit codes are 0-255; saturating cast
            // covers the i64-to-i32 narrowing without surprise.
            let real_code = result.original_code.unwrap_or(result.code);
            let exit_code_i32: i32 = real_code.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            if let Err(e) = documents.set_exit_code(
                context_id,
                output_block_id,
                Some(exit_code_i32),
            ) {
                tracing::error!("Failed to set output block exit_code: {}", e);
            }

            // Exit 2: latch gate (rm/trash) — confirmation message shown, not a failure
            // Exit 3: truncation (did_spill) OR a command's own genuine exit 3 — neither is a failure
            //
            // Matched on `real_code`, not `result.code`: kaish's did_spill
            // remap is unconditional — a command that FAILED and also
            // spilled >8KB of output gets `code = 3` with the real failing
            // code stashed in `original_code` (kaish-kernel's `output_limit`
            // remap in `Kernel::run`/`spill_if_needed`, unconditional on the
            // pre-spill exit). Matching on the raw code would fold that
            // failure into the `3 => Done` arm and misreport it as success.
            // `real_code` collapses back to `result.code` whenever
            // `original_code` is `None` (no spill), so a command that
            // genuinely exits 2 or 3 on its own is unaffected — this only
            // changes classification for the spilled-and-failed case.
            let mut final_status = match real_code {
                0 | 2 | 3 => Status::Done,
                _ => Status::Error,
            };
            // PostCall — hand the hook the real result this command
            // produced (docs/gate-and-shell-split.md, "The three rpc.rs
            // shell paths take the hook path"): mirrors `Broker::
            // call_tool`'s own PostCall pinch point. `Proceed` changes
            // nothing — the real output above already stands.
            // `ShortCircuit` overrides it the same way `call_tool`'s
            // PostCall can override a real server result; `Deny` settles
            // both blocks to `Error` with the hook's reason, same as a
            // `call_tool` caller getting `Denied` instead of the result
            // it actually produced.
            let hook_result = exec_result_to_hook_tool_result(&result);
            match kernel
                .broker()
                .shell_post_call_hooks(code, call_ctx, &hook_result)
                .await
            {
                crate::mcp::ShellHookVerdict::Proceed => {}
                crate::mcp::ShellHookVerdict::ShortCircuit(sc_result) => {
                    let text = shell_hook_result_text(&sc_result);
                    let status = if sc_result.is_error { Status::Error } else { Status::Done };
                    if let Err(e) =
                        documents.replace_text_as(context_id, output_block_id, &text, Some(PrincipalId::system()))
                    {
                        tracing::error!("Failed to write PostCall short-circuited shell output: {}", e);
                    }
                    final_status = status;
                }
                crate::mcp::ShellHookVerdict::Denied(err) => {
                    // "on ...", not "denied by ...": the next line asks
                    // for a settled status precisely because this carries
                    // `GatePending` too, and a pending ask is not a no.
                    let reason = format!("on shell command result: {err}");
                    let settled = err.settled_block_status();
                    let _ = documents.set_stderr(context_id, output_block_id, Some(reason));
                    final_status = settled;
                }
            }
            final_status
        }
        Err(e) => {
            let mut final_status = Status::Error;
            let error_msg = format!("Error: {}", e);
            tracing::error!("Shell execution failed: {}", e);
            if let Err(e) = documents.edit_text_as(
                context_id,
                output_block_id,
                0,
                &error_msg,
                0,
                Some(PrincipalId::system()),
            ) {
                tracing::error!("Failed to update shell output with error: {}", e);
            }
            // OnError — hand the hook the real failure (mirrors
            // `call_tool`'s own OnError pinch point). `ShortCircuit` can
            // convert the failure into a synthetic success, same as
            // `call_tool`'s OnError; `Proceed`/`Deny` both leave the
            // real error above standing — a failed command is already
            // the terminal state a `Deny` would produce.
            let mcp_err = crate::mcp::McpError::Protocol(e.to_string());
            if let crate::mcp::ShellHookVerdict::ShortCircuit(sc_result) =
                kernel
                    .broker()
                    .shell_on_error_hooks(code, call_ctx, &mcp_err)
                    .await
            {
                let text = shell_hook_result_text(&sc_result);
                let status = if sc_result.is_error { Status::Error } else { Status::Done };
                if let Err(e) =
                    documents.replace_text_as(context_id, output_block_id, &text, Some(PrincipalId::system()))
                {
                    tracing::error!("Failed to write OnError short-circuited shell output: {}", e);
                }
                final_status = status;
            }
            final_status
        }
    };
    // Settle durable context state *before* flipping status to a
    // terminal value: clients (and our own e2e harness) treat the
    // ToolResult reaching Done/Error as "command finished" and may
    // fire their next command immediately. If we persisted after, a
    // back-to-back `cd /x` then `pwd` could re-materialize the shell
    // off stale L1. Detect an in-shell context switch (kj fork /
    // context switch) and propagate it to the connection's shared
    // map; otherwise persist this command's cwd/export changes to the
    // context it ran in. (On a switch the snapshots straddle two
    // contexts and the outgoing cwd is already saved inside kaish, so
    // we skip the write-back.)
    match context_switched(kaish, context_id, on_context_switch) {
        Some(new_context_id) => {
            tracing::info!(
                "shell_execute: context switched {} → {}",
                context_id,
                new_context_id
            );
            block_flows.publish(crate::flows::BlockFlow::ContextSwitched {
                context_id: new_context_id,
            });
        }
        None => {
            let state_after = snapshot_shell_state(kaish).await;
            if let Err(error) = persist_shell_state(kernel_db, context_id, &state_before, &state_after) {
                tracing::error!("shell state write failed: {error}");
                let _ = documents.set_stderr(context_id, output_block_id, Some(error));
                final_status = Status::Error;
            }
        }
    }

    // Terminal publication follows hooks and durable shell state. Observers
    // may submit the next command as soon as they see either block settle.
    for block in [output_block_id, command_block_id] {
        if let Err(error) = documents.set_status(context_id, block, final_status) {
            tracing::error!("Failed to settle shell block: {error}");
        }
    }
    if let Some((manager, job, sender)) = tracked_job {
        let result = match documents.get_block_snapshot(context_id, output_block_id) {
            Ok(Some(block)) => {
                let mut result = kaish_kernel::interpreter::ExecResult::success(block.content);
                result.err = block.stderr.unwrap_or_default();
                result.code = block.exit_code.map(i64::from)
                    .unwrap_or(if block.status == Status::Error { 1 } else { 0 });
                result
            }
            Ok(None) => kaish_kernel::interpreter::ExecResult::failure(1, "shell output block is missing"),
            Err(error) => kaish_kernel::interpreter::ExecResult::failure(1, error.to_string()),
        };
        manager.finalize_streams(job, &result).await;
        let _ = sender.send(result);
    }
    if let Err(error) = complete_operation_from_blocks(kernel, context_id, output_block_id) {
        tracing::error!("could not complete shell operation: {error}");
    }
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
                &documents, kernel.block_flows(), kernel.kernel_db(), &kernel, &call_ctx, None);
            let observe = async {
                tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified()).await.unwrap();
                let command_status = documents.get_block_snapshot(ctx, &command).unwrap().unwrap().status;
                let output_status = documents.get_block_snapshot(ctx, &output).unwrap().unwrap().status;
                release.notify_one();
                (command_status, output_status)
            };
            let (_, statuses) = tokio::join!(run, observe);
            assert_eq!(statuses, (Status::Running, Status::Running), "{code}: hook result is not final yet");
            let final_output = documents.get_block_snapshot(ctx, &output).unwrap().unwrap();
            assert_eq!(final_output.status, Status::Done);
            assert_eq!(final_output.content, "synthetic output");
        }
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
            &documents,
            kernel.block_flows(),
            kernel.kernel_db(),
            &kernel,
            &crate::mcp::CallContext::new(
                PrincipalId::system(), ctx, kaijutsu_types::SessionId::new(), kernel.id(),
            ),
            None,
        )
        .await;

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
