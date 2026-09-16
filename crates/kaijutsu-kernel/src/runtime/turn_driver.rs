//! Headless turn requests submitted to the kernel's shared model runtime.
//!
//! One subscription handles each request once. Dedicated driver thread
//! shutdown remains part of the runtime migration; see `docs/kaish-integration.md`.

use std::sync::Arc;
use crate::Kernel;
use crate::flows::{TurnFlow, TurnOrigin};
use super::llm_stream::spawn_llm_for_prompt;
use super::shell_state::context_cwd;

/// Run one headless request subscription for this kernel.
///
/// Each request resolves startup state here and hands accepted model work to
/// the shared worker. Startup failures publish a visible error and a failed
/// turn event. See `docs/kaish-integration.md` for remaining thread ownership.
pub fn spawn_turn_driver(kernel: Arc<Kernel>) {
    // The turn driver runs tool calls through kaish, and nested `kj`
    // commands overflow the default stack — see `spawn_kaish_thread`.
    if let Err(e) = crate::spawn_kaish_thread("turn-driver", move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!("turn-driver: failed to build runtime: {e}");
                return;
            }
        };
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            let kernel = &kernel;
            let mut sub = kernel.turn_flows().subscribe("turn.requested");
            tracing::info!("Turn driver online");
            while let Some(msg) = sub.recv().await {
                let TurnFlow::Requested {
                    context_id,
                    after_block_id,
                    // The user block is already in the log (anchored by
                    // `after_block_id`); hydration reads it from there.
                    content: _,
                    principal_id,
                    model,
                    continuation_epoch,
                } = msg.payload
                else {
                    // Completion and failure have their own topics.
                    continue;
                };
                // Headless turns use durable cwd. Preserve an unset cwd so
                // path-based tools can refuse instead of walking a fake root.
                let tool_ctx = match context_cwd(kernel, context_id) {
                    Some(cwd) => crate::ExecContext::new(
                        principal_id,
                        context_id,
                        cwd,
                        kaijutsu_types::SessionId::new(),
                        kernel.id(),
                    ),
                    None => crate::ExecContext::new_without_cwd(
                        principal_id,
                        context_id,
                        kaijutsu_types::SessionId::new(),
                        kernel.id(),
                    ),
                };
                match spawn_llm_for_prompt(
                    kernel,
                    context_id,
                    model.as_deref(),
                    &after_block_id,
                    tool_ctx,
                    principal_id,
                    // Origin lets consumers distinguish requested model work
                    // from interactive turns without silencing either.
                    TurnOrigin::Autonomous,
                    continuation_epoch,
                )
                .await
                {
                    // Spawn succeeded — the stream owns the terminal Completed/Failed
                    // publish at its end. Nothing to publish here; doing so would
                    // double-announce (and at the wrong time, with no output id).
                    Ok(()) => {}
                    Err(e) => {
                        let err = e.to_string();
                        tracing::warn!(
                            "turn.requested: failed to drive turn for {context_id}: {err}"
                        );
                        // Surface the failure as a visible Error block in the
                        // child context — same `insert_error_block_as` error-block
                        // API the LLM stream uses (llm_stream.rs report_llm_error),
                        // anchored at the turn's `after_block_id` — so the dropped
                        // turn isn't silently invisible.
                        let payload = kaijutsu_types::ErrorPayload {
                            category: kaijutsu_types::ErrorCategory::Stream,
                            severity: kaijutsu_types::ErrorSeverity::Error,
                            code: None,
                            detail: Some(format!(
                                "autonomous turn failed to run for this context: {err}"
                            )),
                            span: None,
                            source_kind: None,
                        };
                        let summary = payload.summary_line();
                        if let Err(insert_err) = kernel.blocks().insert_error_block_as(
                            context_id,
                            &after_block_id,
                            &payload,
                            summary,
                            Some(principal_id),
                        ) {
                            tracing::warn!(
                                "turn.requested: failed to insert error block for {context_id}: {insert_err}"
                            );
                        }
                        kernel.turn_flows().publish(TurnFlow::Failed {
                            context_id,
                            principal_id,
                            error: err,
                            origin: TurnOrigin::Autonomous,
                        });
                        // spawn_llm_for_prompt failed before it could mark
                        // the turn begun itself (see its doc comment) — but
                        // publish_turn_request already marked it when this
                        // Requested was published, and no process_llm_stream
                        // will ever run to clear it. Clear it here so the
                        // context doesn't read "in flight" forever.
                        kernel.mark_turn_ended(context_id);
                    }
                }
            }
            tracing::warn!("Turn driver: turn bus closed, driver exiting");
        });
    }) {
        tracing::error!("Failed to spawn turn-driver thread: {e}");
    }
}
