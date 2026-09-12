//! Running one kaish submission into a command/output block pair.
//!
//! Two callers reach the same code. `rpc::execute_shell_command` runs a
//! command a human typed, inside its connection's `spawn_local`. The
//! gate-resume driver runs an approved ask's stored source with no
//! connection at all, on its own thread. The pair, the ANSI ingest, the
//! stderr and output-data fields, the exit-code resolution and the
//! `PostCall`/`OnError` hook phases are identical in both — so they live
//! here once rather than being re-derived on the second path.
//!
//! What differs is what an in-shell context switch (`kj context switch`,
//! `kj fork`) is allowed to change: a connection has a session→context map
//! to update, and a detached run has nothing to tell. That is the
//! `on_context_switch` sink, and it is the only seam between the two.

use std::sync::Arc;

use kaijutsu_kernel::{
    block_store::SharedBlockStore, flows::SharedBlockFlowBus, kernel_db::KernelDb, Kernel,
};
use kaijutsu_kernel::runtime::embedded_kaish::EmbeddedKaish;
use kaijutsu_types::{BlockId, ContentType, ContextId, PrincipalId, Status};

use crate::rpc::{
    block_output_data, exec_result_to_hook_tool_result, overwrite_block_text, persist_shell_state,
    shell_hook_result_text, snapshot_shell_state,
};

/// Where an in-shell context switch is recorded.
///
/// `None` means nothing is listening: the switch still stops the durable
/// cwd/env write-back and still publishes `ContextSwitched`, it just has no
/// session map to update.
pub(crate) type ContextSwitchSink<'a> = Option<&'a dyn Fn(ContextId)>;

/// Detect an in-shell context switch, tell the sink about it, and report the
/// new context id. Mirrors `rpc::propagate_context_switch`, which is the
/// connection-bound form of the same check.
fn context_switched(
    kaish: &EmbeddedKaish,
    started_at: ContextId,
    sink: ContextSwitchSink<'_>,
) -> Option<ContextId> {
    match kaish.context_id() {
        Some(new_id) if new_id != started_at => {
            match sink {
                Some(record) => record(new_id),
                None => log::info!(
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
pub(crate) async fn run_into_blocks(
    kaish: &EmbeddedKaish,
    code: &str,
    context_id: ContextId,
    command_block_id: &BlockId,
    output_block_id: &BlockId,
    documents: &SharedBlockStore,
    block_flows: &SharedBlockFlowBus,
    kernel_db: &Arc<parking_lot::Mutex<KernelDb>>,
    kernel: &Arc<Kernel>,
    call_ctx: &kaijutsu_kernel::mcp::CallContext,
    on_context_switch: ContextSwitchSink<'_>,
) {
    // Yield to let the event loop flush BlockInserted events to clients
    // before we start producing text ops. Without this, fast commands
    // (like `ls`) can emit edit_text before the client has processed the
    // BlockInserted, causing DataMissing errors on the client side.
    tokio::task::yield_now().await;

    // Snapshot the shell's durable surface (cwd + exported env) so we can
    // persist whatever this command changes (`cd`, `export`) back to L1.
    let state_before = snapshot_shell_state(kaish).await;

    log::debug!(
        "shell_execute: executing code via EmbeddedKaish: {:?}",
        code
    );
    match kaish
        .execute_with_options(code, kaish_kernel::ExecuteOptions::default())
        .await
    {
        Ok(result) => {
            log::info!(
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
            let raw_out = kaijutsu_kernel::ansi_ingest::raw_stdout(&result);
            let projection = kaijutsu_kernel::ansi_ingest::project(&raw_out);
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
                log::error!("Failed to update shell output: {}", e);
            }
            // Strictly after the edit: `edit_text` clears style_spans.
            if let Some(p) = projection {
                kaijutsu_kernel::ansi_ingest::record(
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
                log::error!("Failed to set shell stderr: {}", e);
            }

            if let Some(output_data) = block_output_data(&result)
                && let Err(e) = documents.set_output(
                    context_id,
                    output_block_id,
                    Some(&output_data),
                )
            {
                log::error!("Failed to set output data: {}", e);
            }

            if let Some(ref ct_str) = result.content_type {
                let ct = ContentType::from_mime(ct_str);
                if ct != ContentType::Plain
                    && let Err(e) =
                        documents.set_content_type(context_id, output_block_id, ct)
                {
                    log::error!("Failed to set content_type: {}", e);
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
                        log::error!("Failed to set ephemeral on block: {}", e);
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
                log::error!("Failed to set output block exit_code: {}", e);
            }

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
                    log::info!(
                        "shell_execute: context switched {} → {}",
                        context_id,
                        new_context_id
                    );
                    block_flows.publish(kaijutsu_kernel::flows::BlockFlow::ContextSwitched {
                        context_id: new_context_id,
                    });
                }
                None => {
                    let state_after = snapshot_shell_state(kaish).await;
                    persist_shell_state(
                        kernel_db,
                        context_id,
                        &state_before,
                        &state_after,
                    );
                }
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
            let final_status = match real_code {
                0 | 2 | 3 => Status::Done,
                _ => Status::Error,
            };
            if let Err(e) =
                documents.set_status(context_id, output_block_id, final_status)
            {
                log::error!("Failed to set output block status: {}", e);
            }
            if let Err(e) =
                documents.set_status(context_id, command_block_id, final_status)
            {
                log::error!("Failed to set command block status: {}", e);
            }

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
                kaijutsu_kernel::mcp::ShellHookVerdict::Proceed => {}
                kaijutsu_kernel::mcp::ShellHookVerdict::ShortCircuit(sc_result) => {
                    let text = shell_hook_result_text(&sc_result);
                    let status = if sc_result.is_error { Status::Error } else { Status::Done };
                    if let Err(e) =
                        overwrite_block_text(documents, context_id, output_block_id, &text)
                    {
                        log::error!("Failed to write PostCall short-circuited shell output: {}", e);
                    }
                    let _ = documents.set_status(context_id, output_block_id, status);
                    let _ = documents.set_status(context_id, command_block_id, status);
                }
                kaijutsu_kernel::mcp::ShellHookVerdict::Denied(err) => {
                    // "on ...", not "denied by ...": the next line asks
                    // for a settled status precisely because this carries
                    // `GatePending` too, and a pending ask is not a no.
                    let reason = format!("on shell command result: {err}");
                    let settled = err.settled_block_status();
                    let _ = documents.set_stderr(context_id, output_block_id, Some(reason));
                    let _ = documents.set_status(context_id, output_block_id, settled);
                    let _ = documents.set_status(context_id, command_block_id, settled);
                }
            }
        }
        Err(e) => {
            let error_msg = format!("Error: {}", e);
            log::error!("Shell execution failed: {}", e);
            if let Err(e) = documents.edit_text_as(
                context_id,
                output_block_id,
                0,
                &error_msg,
                0,
                Some(PrincipalId::system()),
            ) {
                log::error!("Failed to update shell output with error: {}", e);
            }
            if let Err(e) =
                documents.set_status(context_id, output_block_id, Status::Error)
            {
                log::error!("Failed to set output block error status: {}", e);
            }
            if let Err(e) =
                documents.set_status(context_id, command_block_id, Status::Error)
            {
                log::error!("Failed to set command block error status: {}", e);
            }

            // OnError — hand the hook the real failure (mirrors
            // `call_tool`'s own OnError pinch point). `ShortCircuit` can
            // convert the failure into a synthetic success, same as
            // `call_tool`'s OnError; `Proceed`/`Deny` both leave the
            // real error above standing — a failed command is already
            // the terminal state a `Deny` would produce.
            let mcp_err = kaijutsu_kernel::mcp::McpError::Protocol(e.to_string());
            if let kaijutsu_kernel::mcp::ShellHookVerdict::ShortCircuit(sc_result) =
                kernel
                    .broker()
                    .shell_on_error_hooks(code, call_ctx, &mcp_err)
                    .await
            {
                let text = shell_hook_result_text(&sc_result);
                let status = if sc_result.is_error { Status::Error } else { Status::Done };
                if let Err(e) =
                    overwrite_block_text(documents, context_id, output_block_id, &text)
                {
                    log::error!("Failed to write OnError short-circuited shell output: {}", e);
                }
                let _ = documents.set_status(context_id, output_block_id, status);
                let _ = documents.set_status(context_id, command_block_id, status);
            }
        }
    }
}

#[cfg(test)]
mod fill_tests {
    use super::*;
    use kaijutsu_kernel::Kernel;
    use kaijutsu_kernel::block_store::DocumentKind;

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
            &documents,
            kernel.block_flows(),
            kernel.kernel_db(),
            &kernel,
            &kaijutsu_kernel::mcp::CallContext::new(
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
