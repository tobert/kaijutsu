//! Model streaming and the tool-call loop.
//!
//! Resolve a turn's identity, provider, tools, and instructions, then stream
//! provider events into blocks and dispatch tool calls through the broker.
//! Interactive and headless callers share `spawn_llm_for_prompt`.
//! Conversation locks and interrupts belong to the kernel's `TurnState`;
//! the kernel worker owns accepted tasks and joins them during shutdown.
//! See `docs/kaish-integration.md`.

#[cfg(test)]
use std::collections::HashMap;
use std::sync::Arc;
use futures::FutureExt;

use kaijutsu_types::shell_envelope::ShellEnvelope;
use kaijutsu_types::{BlockKind, ContentType, Role, Status, summarize_thinking};
use crate::flows::{TurnFlow, TurnOrigin, TurnStopReason};
use crate::kernel_db::KernelDb;
use crate::llm::stream::{
    BuildOpts, CacheTarget, InlineToolResult, StreamEvent, apply_slot_tunables,
    longest_cache_ttl_secs,
};
use crate::llm::SlotTunables;
use crate::llm::{ContentBlock, LlmError, ToolDefinition};
use crate::mcp::{McpError, PolicyError};
use crate::{Kernel, LlmMessage, Provider, SharedBlockStore};
use kaijutsu_types::ToolKind as TypesToolKind;
use kaijutsu_types::{ConsentMode, ContextId, PrincipalId};

use crate::runtime::interrupt::ContextInterruptState;
use super::turn_state::{ConversationCache, TurnLease};

#[derive(Debug, thiserror::Error)]
enum StreamFailure {
    #[error("Could not persist model output: {0}")]
    Persistence(#[from] crate::block_store::BlockStoreError),
    #[error("Invalid provider stream: {0}")]
    Protocol(String),
}

#[derive(Clone, Copy)]
enum OpenContent {
    Thinking(kaijutsu_types::BlockId),
    Text(kaijutsu_types::BlockId),
}

impl OpenContent {
    fn id(self) -> kaijutsu_types::BlockId {
        match self { Self::Thinking(id) | Self::Text(id) => id }
    }
}

/// Check framing before accepting bytes or dispatching a provider's tool call.
fn validate_content_event(open: Option<OpenContent>, event: &StreamEvent) -> Result<(), StreamFailure> {
    let valid = match event {
        StreamEvent::ThinkingDelta(_) | StreamEvent::ThinkingEnd { .. } => matches!(open, Some(OpenContent::Thinking(_))),
        StreamEvent::TextDelta(_) | StreamEvent::TextEnd => matches!(open, Some(OpenContent::Text(_))),
        StreamEvent::ThinkingStart | StreamEvent::TextStart | StreamEvent::ToolUse { .. }
            | StreamEvent::ToolUseInvalid { .. } | StreamEvent::InlineToolUse { .. }
            | StreamEvent::Done { .. } => open.is_none(),
        StreamEvent::ToolResult { .. } => return Err(StreamFailure::Protocol("provider emitted ToolResult; only the runtime authors tool results".into())),
        StreamEvent::Error(_) => true,
    };
    if valid { return Ok(()); }
    let event_name = match event {
        StreamEvent::ThinkingStart => "ThinkingStart", StreamEvent::ThinkingDelta(_) => "ThinkingDelta",
        StreamEvent::ThinkingEnd { .. } => "ThinkingEnd", StreamEvent::TextStart => "TextStart",
        StreamEvent::TextDelta(_) => "TextDelta", StreamEvent::TextEnd => "TextEnd",
        StreamEvent::ToolUse { .. } => "ToolUse", StreamEvent::ToolUseInvalid { .. } => "ToolUseInvalid",
        StreamEvent::InlineToolUse { .. } => "InlineToolUse",
        StreamEvent::ToolResult { .. } => "ToolResult", StreamEvent::Done { .. } => "Done", StreamEvent::Error(_) => "Error",
    };
    let state = match open { Some(OpenContent::Thinking(_)) => "an open thinking block",
        Some(OpenContent::Text(_)) => "an open text block", None => "no open content block" };
    Err(StreamFailure::Protocol(format!("{event_name} is invalid with {state}; providers must bracket matching content and close it before tools or Done")))
}

/// Record a startup failure after the prompt so readers see why no turn ran.
fn insert_pre_stream_error_block(
    documents: &SharedBlockStore,
    context_id: ContextId,
    after_block_id: &kaijutsu_types::BlockId,
    detail: &str,
) {
    let payload = kaijutsu_types::ErrorPayload {
        category: kaijutsu_types::ErrorCategory::Stream,
        severity: kaijutsu_types::ErrorSeverity::Error,
        code: None,
        detail: Some(detail.to_string()),
        span: None,
        source_kind: None,
    };
    let summary = payload.summary_line();
    if let Err(e) = documents.insert_error_block_as(
        context_id,
        after_block_id,
        &payload,
        summary,
        Some(PrincipalId::system()),
    ) {
        tracing::warn!("Failed to insert pre-stream error block: {}", e);
    }
}

/// Fraction of a model's context window at which the pre-flight size
/// estimate (`crate::estimate_tokens`) triggers a visible warning.
/// 0.9, not 1.0: the estimator's bytes/4 conversion undercounts code-heavy
/// text (real BPE tokenizers run denser on structured content than plain
/// prose), and a non-blocking warning is most useful *before* the provider's
/// own hard rejection fires — so this fires with headroom, not at the edge.
const CONTEXT_WARNING_THRESHOLD: f64 = 0.9;

/// Pre-flight, non-blocking size check: warn — never refuse or trim — when a
/// turn's *estimated* size is at or above `CONTEXT_WARNING_THRESHOLD` of the
/// resolved model's context window.
///
/// This is a WARN-AND-SEND path, not a correctness gate. Auto-compaction was
/// deliberately removed from kaijutsu and the kernel never silently refuses
/// or trims a turn on this model's behalf; the provider's own overflow
/// rejection remains the hard backstop. `window` is `None` when the model's
/// context window is unconfigured/unresolvable (`LlmRegistry::context_window_
/// for_live`) — in that case there is nothing honest to compare against, so
/// no check runs at all. Never fabricate a denominator (same house rule as
/// `context_used_pct`, kernel_db.rs:360).
///
/// Emits **unconditionally** per over-threshold turn — a growing
/// conversation will warn again on every subsequent turn, with no
/// once-per-context latch. That's a deliberate, noted simplification: a
/// latch would need to track "have we warned since the last exclude/fork",
/// state this advisory path doesn't otherwise carry. `kj context info`
/// remains the quiet, on-demand gauge for anyone who wants a single check
/// instead of a running warning.
fn warn_if_near_context_window(
    documents: &SharedBlockStore,
    context_id: ContextId,
    after_block_id: &kaijutsu_types::BlockId,
    messages: &[LlmMessage],
    provider_name: &str,
    model_name: &str,
    window: Option<u64>,
) {
    let Some(window) = window else {
        // Unknown window: nothing to compare against. Never invent a
        // fabricated denominator just to produce a number.
        return;
    };
    let estimate = crate::estimate_tokens(messages);
    let threshold = (window as f64 * CONTEXT_WARNING_THRESHOLD) as u64;
    if estimate < threshold {
        return;
    }
    let pct = if window == 0 {
        // A configured window of 0 is a config error, not a real model —
        // still never panic this advisory path over a division by zero.
        100.0
    } else {
        (estimate as f64 / window as f64) * 100.0
    };
    let detail = format!(
        "conversation is an estimated ~{estimate} tokens against {model_name}'s {window}-token \
         window (~{pct:.0}%). The turn was sent anyway; the provider is the hard limit. \
         Remedies: exclude large blocks then fork; `kj fork --compact`; if this model's window \
         is wrong, re-pin it with \
         `kj backend model set {provider_name} {model_name} --context-window <N>`."
    );
    tracing::warn!("{detail}");

    // Telemetry-only insert: unlike the loud-failure paths elsewhere in this
    // file, a failed Trace insert here must never fail or stall the turn —
    // the `tracing::warn!` above already surfaced the warning operator-side, and
    // this is a WARN-AND-SEND path by design, exempt from the fail-loudly-
    // and-stop pattern that governs the hydration/provider-resolution/tool
    // failures above. The in-conversation copy is best-effort.
    if let Err(e) = documents.insert_block_as(
        context_id,
        None,
        Some(after_block_id),
        kaijutsu_types::Role::System,
        kaijutsu_types::BlockKind::Trace,
        detail,
        kaijutsu_types::Status::Done,
        kaijutsu_types::ContentType::Plain,
        Some(PrincipalId::system()),
    ) {
        tracing::warn!("Failed to insert context-size warning Trace block: {e}");
    }
}

/// Hydrate the live conversation session for one turn.
///
/// Catches the `mailbox` up against the current block log and returns the
/// wire-history snapshot the LLM should see. On a block-read failure this does
/// **not** fall back to a partial/empty session — that would silently hand the
/// model an amnesiac request (no-silent-fallbacks directive). Instead it
/// surfaces a `BlockKind::Error` block anchored at the user's message and
/// returns `Err(())`, which the caller turns into an early return so the turn
/// fails loudly with operator-visible feedback.
///
/// `block_snapshots` only fails with `DocumentNotFound`, which means the
/// context's document is genuinely gone (e.g. evicted/deleted in the async
/// window between the user-block insert and this task running). In that case
/// the error-block insert below will itself fail (no document to anchor in);
/// we log that loudly and still fail the turn — never proceed to the LLM.
fn hydrate_messages(
    documents: &SharedBlockStore,
    context_id: ContextId,
    after_block_id: &kaijutsu_types::BlockId,
    mailbox: &mut crate::ConversationMailbox,
    // The hydration window policy `(marker, window)`, or `None` to hydrate the
    // whole history (the default; every non-musician context). When set, the
    // turn hydrates only `[0, marker] ∪ last-window` — the cost guard for
    // endless musician logs (design: docs/chameleon.md).
    policy: Option<(kaijutsu_types::BlockId, u32)>,
) -> Result<Vec<LlmMessage>, ()> {
    let read = documents.block_snapshots(context_id);
    handle_hydration_outcome(documents, context_id, after_block_id, read, mailbox, policy)
}

/// Turn a block-log read into the wire-history snapshot, or fail the turn.
///
/// Split out from [`hydrate_messages`] so the failure branch is exercisable in
/// tests with a real (present) document — `BlockStore::block_snapshots` itself
/// can only fail with `DocumentNotFound`, which would also make the error-block
/// anchor unreachable, so the read result is injected here.
fn handle_hydration_outcome(
    documents: &SharedBlockStore,
    context_id: ContextId,
    after_block_id: &kaijutsu_types::BlockId,
    read: crate::BlockStoreResult<Vec<kaijutsu_types::BlockSnapshot>>,
    mailbox: &mut crate::ConversationMailbox,
    policy: Option<(kaijutsu_types::BlockId, u32)>,
) -> Result<Vec<LlmMessage>, ()> {
    match read {
        Ok(blocks) => {
            match policy {
                // Windowed context: rebuild `[0, marker] ∪ last-window` each turn
                // (a sliding tail can drop a block, which the append-only
                // catch_up can't express). Applies on cold start too — this is
                // the same path a restart re-hydrates through, so the marker
                // bounds cold-start hydration as well as steady state.
                Some((marker, window)) => {
                    mailbox.rehydrate_windowed(&blocks, marker, window as usize);
                    let snapshot = mailbox.snapshot();
                    tracing::debug!(
                        "Mailbox windowed-rehydrated (marker {marker}, window {window}): \
                         {} blocks in log → {} messages on the wire for context {context_id}",
                        blocks.len(),
                        snapshot.len(),
                    );
                    Ok(snapshot)
                }
                None => {
                    let new_blocks = mailbox.catch_up(&blocks);
                    let snapshot = mailbox.snapshot();
                    tracing::debug!(
                        "Mailbox caught up: +{} new blocks, {} messages on the wire for context {}",
                        new_blocks,
                        snapshot.len(),
                        context_id
                    );
                    Ok(snapshot)
                }
            }
        }
        Err(e) => {
            // Hydration failed. Do NOT fall back to the mailbox snapshot +
            // appended user message — an empty/stale session means the model
            // sees no history and responds out of nowhere. Surface the failure
            // and fail the turn.
            tracing::error!(
                "Hydration failed for context {}: {} — failing the turn loudly",
                context_id,
                e
            );
            let detail = format!(
                "Could not read conversation history for this context: {e}. \
                 The turn was stopped instead of sending the model an empty session."
            );
            insert_pre_stream_error_block(documents, context_id, after_block_id, &detail);
            Err(())
        }
    }
}

async fn build_tool_definitions(
    kernel: &Arc<Kernel>,
    context_id: ContextId,
    principal_id: PrincipalId,
) -> Result<Vec<ToolDefinition>, crate::mcp::McpError> {
    Ok(kernel
        .list_tool_defs_via_broker(context_id, principal_id)
        .await?
        .into_iter()
        .map(|(name, schema, description)| ToolDefinition {
            name,
            description: description.unwrap_or_default(),
            input_schema: schema,
        })
        .collect())
}

/// Prepare an admitted turn and transfer its existing lease to inference.
/// Startup ownership and cancellation belong to `turn_request::queue_startup`.
pub(super) async fn spawn_admitted_turn(
    kernel: &Arc<Kernel>,
    context_id: ContextId,
    model: Option<&str>,
    after_block_id: &kaijutsu_types::BlockId,
    tool_ctx: crate::ExecContext,
    user_principal_id: PrincipalId,
    origin: TurnOrigin,
    continuation_epoch: Option<i64>,
    turn_lease: TurnLease,
    context_admission: super::admission::ContextAdmission,
) -> Result<(), String> {
    debug_assert_eq!(context_admission.context(), context_id);
    let documents = kernel.blocks().clone();
    let kernel_arc = kernel.clone();
    let kernel_db = kernel.kernel_db().clone();
    let conversation_cache = kernel.turns().conversations().clone();

    let (actor, director) = {
        let db = kernel_db.lock();
        let row = db.get_context(context_id)
            .map_err(|e| format!("Could not read performer assignment: {e}"))?
            .ok_or_else(|| format!("No such context: {context_id}"))?;
        (row.played_by, row.director_id)
    };
    let review = kernel_arc
        .resolve_context_review(context_id)
        .await?;
    let identity = {
        let db = kernel_db.lock();
        super::turn_identity::resolve(&db, actor, review.reviewer.principal_id)
    };
    let identity = match identity {
        Ok(identity) => identity,
        Err(detail) => {
            insert_pre_stream_error_block(&documents, context_id, after_block_id, &detail);
            return Err(detail);
        }
    };
    let performer = {
        let db = kernel_db.lock();
        let character = |principal_id: PrincipalId| -> Result<crate::CharacterIdentity, String> {
            let sheet = db.get_character(principal_id)
                .map_err(|e| format!("Could not read assigned character: {e}"))?
                .ok_or_else(|| format!("Assigned character {principal_id} has no sheet"))?;
            if sheet.retired_at.is_some() {
                return Err(format!("Assigned character {} is retired", sheet.name));
            }
            Ok(crate::CharacterIdentity { principal_id, name: sheet.name })
        };
        character(identity.actor)?
    };
    let reviewer = review.reviewer;
    let tool_ctx = tool_ctx.with_actor(identity.actor, Some(identity.reviewer));

    // Every turn checks the durable quiesce flag before provider work. Refuse
    // unreadable state. Admission already owns a lease; startup failure
    // drops it while retaining the durable seed. Running turns are unaffected.
    let quiesce = {
        let db = kernel_db.lock();
        db.quiesce_state()
    };
    let quiesced = match quiesce {
        Ok(q) => q,
        Err(e) => {
            tracing::error!("quiesce flag read failed: {e}; refusing the turn");
            return Err(format!(
                "cannot read the quiesce flag, so no turn starts: {e}"
            ));
        }
    };
    if let Some(state) = quiesced {
        let reason = match state.reason.as_deref() {
            Some(r) => format!(" Reason: {r}."),
            None => String::new(),
        };
        // Writes land while quiesced, so this explanation reaches the
        // conversation even though the turn does not run.
        let _ = documents
            .insert_block_as(
                context_id,
                None,
                Some(after_block_id),
                kaijutsu_types::Role::System,
                kaijutsu_types::BlockKind::Text,
                &format!(
                    "The kernel is quiesced, so no turn starts.{reason} Writes still \
                     land — anything you or anyone else writes is saved and will be \
                     here when turns resume. `kj system status` shows the flag; \
                     `kj system resume` clears it."
                ),
                kaijutsu_types::Status::Done,
                kaijutsu_types::ContentType::Plain,
                Some(PrincipalId::system()),
            )
            .and_then(|bid| documents.set_ephemeral(context_id, &bid, true));
        return Err(
            "the kernel is quiesced — no turn starts; `kj system resume` clears it".into());
    }

    // Read per-context model from DriftRouter (quick read, release lock).
    // Capture label/state alongside for the situational system-prompt addendum.
    let (ctx_model, ctx_provider_name, ctx_label, ctx_state) = {
        let drift = kernel_arc.drift().read();
        // Guard: block LLM invocation while context is in Staging state
        if let Some(h) = drift.get(context_id)
            && h.state == kaijutsu_types::ContextState::Staging
        {
            // Insert an ephemeral system block explaining why the prompt was rejected
            let _ = documents
                .insert_block_as(
                    context_id,
                    None,
                    Some(after_block_id),
                    kaijutsu_types::Role::System,
                    kaijutsu_types::BlockKind::Text,
                    "Context is in staging mode. Use `kj stage commit` to go live.",
                    kaijutsu_types::Status::Done,
                    kaijutsu_types::ContentType::Plain,
                    Some(PrincipalId::system()),
                )
                .and_then(|bid| documents.set_ephemeral(context_id, &bid, true));
            return Err(
                "context is in staging mode — commit to enable LLM prompts".into());
        }
        match drift.get(context_id) {
            Some(h) => (
                h.model.clone(),
                h.provider.clone(),
                h.label.clone(),
                Some(h.state),
            ),
            None => (None, None, None, None),
        }
    };

    // Resolve provider + model through `resolve_context_model` (the one
    // function that answers "what model does this context play?").
    // Priority: explicit param > per-context (DriftRouter) > cast slot on
    // this context's context_type > registry default. The context_type and
    // cast label are read once per turn here; the pure resolution itself
    // lives in `crate::model_resolution` where it's unit-tested.
    let (context_type, cast_label) = {
        let db = kernel_db.lock();
        match db.get_context(context_id) {
            Ok(Some(row)) => {
                let label = row.cast_id.and_then(|id| match db.get_cast(id) {
                    Ok(Some(cast)) => Some(cast.label),
                    // A dangling cast_id (cast removed; FK is SET NULL so
                    // this is a race at worst) falls through to the
                    // default — resolution handles None; log it so the
                    // fallthrough is observable.
                    Ok(None) => {
                        tracing::warn!(
                            "context {context_id} carries cast_id {id} with no cast row; \
                             falling through to default resolution"
                        );
                        None
                    }
                    Err(e) => {
                        tracing::warn!("cast lookup for context {context_id} failed: {e}");
                        None
                    }
                });
                (row.context_type, label)
            }
            // No row / read failure: resolution still works (no cast, no
            // type-matched slot). "coder" is not assumed — an empty type
            // matches no slot role, which is the honest answer.
            Ok(None) => (String::new(), None),
            Err(e) => {
                tracing::warn!("context row lookup for {context_id} failed: {e}");
                (String::new(), None)
            }
        }
    };
    let provider_resolution: Result<(Arc<Provider>, String, u64, Option<SlotTunables>, StreamTimeouts), String> = {
        let registry = kernel_arc.llm().read().await;
        let max_tokens = registry.max_output_tokens();

        // Explicit RPC param outranks the per-context override; both are
        // "the context row's own model" as far as resolution is concerned.
        let effective_model = model.or(ctx_model.as_deref());

        match crate::resolve_context_model(
            &context_type,
            ctx_provider_name.as_deref(),
            effective_model,
            cast_label.as_deref(),
            &registry,
        ) {
            Some(resolved) => match registry.get(&resolved.backend) {
                Some(p) => {
                    tracing::debug!(
                        "model resolution for {context_id}: {}/{} via {:?}",
                        resolved.backend,
                        resolved.model,
                        resolved.source
                    );
                    let timeouts = StreamTimeouts::resolve(
                        kernel_arc.timeouts(),
                        registry.backend_config(&resolved.backend),
                    );
                    Ok((p, resolved.model, max_tokens, resolved.tunables, timeouts))
                }
                // A resolved backend must be registered; falling back would
                // send the pinned model name to a different provider.
                None => Err(format!(
                    "backend '{}' for this context is not registered (missing key? \
                     see `kj backend list` / `kj backend show {}`)",
                    resolved.backend, resolved.backend
                )),
            },
            None => Err("No LLM backend configured (see `kj backend list`)".to_string()),
        }
    };
    let (provider, model_name, max_output_tokens, slot_tunables, stream_timeouts) = match provider_resolution {
        Ok(v) => v,
        Err(detail) => {
            tracing::error!("LLM resolution failed for context {context_id}: {detail}");
            insert_pre_stream_error_block(&documents, context_id, after_block_id, &detail);
            return Err(detail);
        }
    };

    // The broker applies bindings and ListTools hooks. Refuse the turn on
    // failure so a configuration error cannot silently remove its tools.
    let tools = match build_tool_definitions(&kernel_arc, context_id, user_principal_id).await {
        Ok(tools) => tools,
        Err(e) => {
            tracing::error!("Failed to build tool definitions for context {context_id}: {e}");
            let detail = format!(
                "Could not resolve this context's tool bindings: {e}. \
                 The turn was stopped instead of running the model with no tools."
            );
            insert_pre_stream_error_block(&documents, context_id, after_block_id, &detail);
            return Err(detail);
        }
    };

    // Assemble the system prompt from the sections this context's rc
    // lifecycle selected, plus per-call facts about its current state and
    // tool inventory. The kernel owns facts, never a mandatory base body.
    //
    // rc sections come from `(Role::System, BlockKind::Text)` blocks in the
    // conversation — typically dropped in by rc-on-create/-on-fork. They
    // land before the `<situation>` addendum.
    let situational = crate::SituationalContext {
        context_id: Some(context_id),
        context_label: ctx_label,
        context_state: ctx_state,
        provider: ctx_provider_name.clone(),
        model: Some(model_name.clone()),
        performer: Some(performer.clone()),
        reviewer: Some(reviewer.clone()),
        tool_names: tools.iter().map(|t| t.name.clone()).collect(),
    };
    let rc_sections = match crate::read_system_prompt_sections(&documents, context_id) {
        Ok(sections) => sections,
        Err(e) => {
            let detail = format!(
                "Could not read this context's instruction blocks: {e}. The turn was stopped."
            );
            tracing::error!("System prompt read failed for context {context_id}: {e}");
            insert_pre_stream_error_block(&documents, context_id, after_block_id, &detail);
            return Err(detail);
        }
    };
    let system_prompt = crate::build_system_prompt(&situational, &rc_sections);

    tracing::info!(
        "Spawning LLM stream: context={}, model={}",
        context_id,
        model_name
    );

    let after_block_id = *after_block_id;

    // Automatic continuation claims its epoch before starting inference.
    let continuation_epoch = match continuation_epoch {
        Some(epoch) => {
            let window = kernel_arc.gate_resume_window().await?;
            let window_ms = i64::try_from(window.as_millis())
                .map_err(|error| error.to_string())?;
            if !kernel_db.lock().claim_automatic_resume(
                context_id, epoch, kaijutsu_types::now_millis() as i64, window_ms,
            ).map_err(|error| error.to_string())? {
                return Err("automatic continuation window closed before startup".into());
            }
            Some(epoch)
        },
        None => {
            let db = kernel_db.lock();
            let current = db.get_context(context_id).map_err(|error| error.to_string())?
                .ok_or("context disappeared during turn preparation")?;
            if current.played_by != Some(identity.actor) {
                return Err("performer changed during turn preparation; submit a new drive".into());
            }
            Some(db.begin_continuation(context_id, kaijutsu_types::now_millis() as i64)
                .map_err(|error| format!("Could not open continuation window: {error}"))?.epoch)
        },
    };

    assert_eq!(turn_lease.context(), context_id, "turn lease belongs to its request");
    let interrupt = turn_lease.interrupt();

    // Queue admission and this function's return must stay in the same poll.
    // Startup owns refusal; the queued stream owns its terminal event.
    let accepted = kernel.spawn_runtime_task(move |stop| async move {
        let cancel = interrupt.clone();
        let run = process_llm_stream(
            provider, documents, context_id, model_name, kernel_arc, kernel_db,
            tools, after_block_id, system_prompt, max_output_tokens, stream_timeouts,
            slot_tunables, conversation_cache, user_principal_id,
            Some(TurnSpanIdentity { performer, reviewer, director, review_source: review.source }),
            tool_ctx, interrupt, turn_lease, origin,
            continuation_epoch,
        );
        tokio::pin!(run);
        tokio::select! {
            biased;
            _ = stop.cancelled() => { cancel.hard(); run.await; }
            _ = &mut run => {}
        }
    });
    if let Err(error) = accepted {
        return Err(error);
    }

    Ok(())
}

/// The two guards on one streaming completion, resolved per backend: the
/// total wall-clock cap and the per-chunk idle limit. A backend's own
/// `request_timeout_secs` / `idle_timeout_secs` (`kj backend set`) win;
/// the kernel-wide `TimeoutPolicy` fills whichever it leaves unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamTimeouts {
    pub request: std::time::Duration,
    pub idle: std::time::Duration,
}

impl StreamTimeouts {
    pub fn resolve(
        policy: &kaijutsu_types::TimeoutPolicy,
        backend: Option<&crate::BackendConfig>,
    ) -> Self {
        let secs = |v: Option<u64>, fallback: std::time::Duration| {
            v.map(std::time::Duration::from_secs).unwrap_or(fallback)
        };
        Self {
            request: secs(backend.and_then(|b| b.request_timeout_secs), policy.llm_request_timeout),
            idle: secs(backend.and_then(|b| b.idle_timeout_secs), policy.llm_idle_timeout),
        }
    }

    /// The kernel-wide defaults alone — a caller with no backend in hand.
    pub fn from_policy(policy: &kaijutsu_types::TimeoutPolicy) -> Self {
        Self::resolve(policy, None)
    }
}

/// Agentic-loop iteration cap by consent mode (M1-A6).
///
/// Both modes leave enough headroom for real chained tool work; the cap
/// is a runaway guard, not a checkpoint. Autonomous gets more rope.
const COLLABORATIVE_MAX_ITERATIONS: u32 = 50;
const AUTONOMOUS_MAX_ITERATIONS: u32 = 100;

fn iteration_cap_for_consent(mode: ConsentMode) -> u32 {
    match mode {
        ConsentMode::Collaborative => COLLABORATIVE_MAX_ITERATIONS,
        ConsentMode::Autonomous => AUTONOMOUS_MAX_ITERATIONS,
    }
}

/// How many times one turn answers an output-ceiling stop with a notice and
/// another inference. Counted over the whole turn, not per run of consecutive
/// stops. The turn then ends with the provider's own `max_tokens` reason.
///
/// Small on purpose: a model that spends the whole ceiling on reasoning tends
/// to do it again, and each continuation costs another ceiling of output
/// tokens. Each continuation also spends an agentic iteration, so the
/// iteration cap still bounds the turn.
const MAX_OUTPUT_CEILING_CONTINUATIONS: u32 = 3;

/// What the turn knows at an output-ceiling stop, for [`CeilingStop::continues`].
#[derive(Clone, Copy, Debug)]
struct CeilingStop {
    /// The provider stopped this inference at the output ceiling.
    ceiling: bool,
    /// Continuations this turn has already spent.
    continuations_spent: u32,
    /// A beat is waiting on this turn's output.
    timed_delivery: bool,
    /// Another pass fits under the agentic-loop iteration cap.
    iterations_left: bool,
    cancelled: bool,
    stopping_after_turn: bool,
}

impl CeilingStop {
    /// Whether the turn takes another inference.
    ///
    /// It continues only when the ceiling is what stopped it, the turn has
    /// continuations left, no beat is waiting on it, another pass fits under
    /// the iteration cap, and no interrupt is pending. Every other ending is
    /// somebody else's to report, and the caller writes no notice for an
    /// inference that will not run — a durable notice for a response that no
    /// longer exists would instruct the next turn instead.
    fn continues(self) -> bool {
        self.ceiling
            && self.continuations_spent < MAX_OUTPUT_CEILING_CONTINUATIONS
            && !self.timed_delivery
            && self.iterations_left
            && !self.cancelled
            && !self.stopping_after_turn
    }
}

/// Reasoning entries a provider may receive back: those carrying a continuity
/// signature. This is the hydrator's rule (`llm/hydrate.rs`), applied to the
/// live loop so one turn serializes the same way live and rehydrated. An
/// unsigned block is dropped — Anthropic refuses one echoed back without its
/// signature, and the next hydration would not replay it either.
fn replayable_reasoning(
    reasoning: Vec<(String, Option<String>)>,
) -> Vec<(String, Option<String>)> {
    reasoning
        .into_iter()
        .filter(|(text, signature)| signature.is_some() && !text.is_empty())
        .collect()
}

/// The assistant message replayed for an output-ceiling continuation, or
/// `None` when the truncated response cannot stand as one.
///
/// A replayed assistant message needs text. Reasoning alone is not an
/// assistant turn — the API requires accompanying text or a tool call, and
/// `llm/hydrate.rs` (`flush_assistant`) drops that shape, which is the common
/// truncation: the whole ceiling goes to reasoning before any text arrives.
/// The notice then follows the previous message instead. The shape matches
/// `flush_assistant` too: plain text when no reasoning survives, blocks
/// otherwise.
fn ceiling_continuation_assistant(
    reasoning: Vec<(String, Option<String>)>,
    text: &str,
) -> Option<LlmMessage> {
    if text.trim().is_empty() {
        return None;
    }
    let reasoning = replayable_reasoning(reasoning);
    Some(if reasoning.is_empty() {
        LlmMessage::assistant(text)
    } else {
        LlmMessage::with_reasoning_text_and_tool_uses(
            reasoning,
            Some(text.to_string()),
            Vec::new(),
        )
    })
}

#[cfg(test)]
mod ceiling_decision_tests {
    use super::*;

    /// A ceiling stop with everything clear continues.
    fn clear() -> CeilingStop {
        CeilingStop {
            ceiling: true,
            continuations_spent: 0,
            timed_delivery: false,
            iterations_left: true,
            cancelled: false,
            stopping_after_turn: false,
        }
    }

    #[test]
    fn a_clear_ceiling_stop_continues() {
        assert!(clear().continues());
    }

    #[test]
    fn an_ordinary_ending_does_not_continue() {
        assert!(!CeilingStop { ceiling: false, ..clear() }.continues());
    }

    #[test]
    fn the_budget_is_spent_at_the_bound() {
        assert!(
            CeilingStop {
                continuations_spent: MAX_OUTPUT_CEILING_CONTINUATIONS - 1,
                ..clear()
            }
            .continues()
        );
        assert!(
            !CeilingStop {
                continuations_spent: MAX_OUTPUT_CEILING_CONTINUATIONS,
                ..clear()
            }
            .continues()
        );
    }

    /// A beat waiting on this turn gets the truncated output now, not a better
    /// one late: extra inferences would put slow work on the beat path.
    #[test]
    fn a_turn_a_beat_waits_on_does_not_continue() {
        assert!(!CeilingStop { timed_delivery: true, ..clear() }.continues());
    }

    /// The next pass would halt at the iteration cap, so the notice would
    /// describe an inference that never ran.
    #[test]
    fn the_iteration_cap_stops_the_continuation() {
        assert!(!CeilingStop { iterations_left: false, ..clear() }.continues());
    }

    #[test]
    fn a_pending_interrupt_stops_the_continuation() {
        assert!(!CeilingStop { cancelled: true, ..clear() }.continues());
        assert!(!CeilingStop { stopping_after_turn: true, ..clear() }.continues());
    }
}

#[cfg(test)]
mod continuation_replay_tests {
    use super::*;
    use crate::llm::{ContentBlock, MessageContent};

    fn signed(text: &str) -> (String, Option<String>) {
        (text.to_string(), Some("signed".to_string()))
    }

    fn blocks(message: &LlmMessage) -> Vec<&ContentBlock> {
        match &message.content {
            MessageContent::Blocks(blocks) => blocks.iter().collect(),
            MessageContent::Text(_) => Vec::new(),
        }
    }

    /// Text alone replays as the model wrote it.
    #[test]
    fn text_only_replays_as_an_assistant_message() {
        let message = ceiling_continuation_assistant(Vec::new(), "half a plan")
            .expect("text can stand as an assistant message");
        assert_eq!(message.as_text(), Some("half a plan"));
    }

    /// Signed reasoning rides with the text it belongs to.
    #[test]
    fn signed_reasoning_rides_with_text() {
        let message = ceiling_continuation_assistant(vec![signed("thinking")], "half a plan")
            .expect("text can stand as an assistant message");
        let blocks = blocks(&message);
        assert!(
            matches!(blocks.first(), Some(ContentBlock::Reasoning { text, .. }) if text == "thinking"),
            "{blocks:?}"
        );
        assert!(
            matches!(blocks.get(1), Some(ContentBlock::Text { text }) if text == "half a plan"),
            "{blocks:?}"
        );
    }

    /// The benchmark's own shape: the ceiling went entirely to reasoning. A
    /// lone Reasoning block is not an assistant turn — the API requires
    /// accompanying text or a tool call, and `llm/hydrate.rs`
    /// (`flush_assistant`) drops the same shape, so replaying it would send
    /// the provider something the next hydration could never reproduce.
    #[test]
    fn reasoning_without_text_replays_nothing() {
        assert!(ceiling_continuation_assistant(vec![signed("thinking")], "").is_none());
    }

    /// Unsigned reasoning is dropped, so it cannot carry an otherwise empty
    /// message either.
    #[test]
    fn unsigned_reasoning_without_text_replays_nothing() {
        assert!(ceiling_continuation_assistant(vec![("thinking".into(), None)], "").is_none());
    }

    /// Nothing arrived before the ceiling: no message at all. An assistant
    /// message with empty content is refused by the providers.
    #[test]
    fn an_empty_response_replays_nothing() {
        assert!(ceiling_continuation_assistant(Vec::new(), "").is_none());
    }

    /// A signed but empty thinking block is skipped by the message builder
    /// (`llm/mod.rs`), so guarding on the pre-build list would produce an
    /// assistant message with no content blocks at all.
    #[test]
    fn signed_but_empty_reasoning_replays_nothing() {
        assert!(ceiling_continuation_assistant(vec![signed("")], "").is_none());
    }

    /// Whitespace is not text: it cannot carry a message either.
    #[test]
    fn whitespace_only_text_replays_nothing() {
        assert!(ceiling_continuation_assistant(vec![signed("thinking")], "  \n").is_none());
    }
}

#[cfg(test)]
mod stream_timeout_tests {
    use super::*;

    /// A backend's own timeouts win; the policy fills what it leaves unset.
    #[test]
    fn stream_timeouts_take_the_backends_own_values_over_the_policy() {
        use std::time::Duration;
        let policy = kaijutsu_types::TimeoutPolicy::default();
        let none = StreamTimeouts::from_policy(&policy);
        assert_eq!(none.idle, policy.llm_idle_timeout);
        assert_eq!(none.request, policy.llm_request_timeout);
        let mut slow = crate::BackendConfig::new("tenchi", crate::BackendKind::OpenAi);
        slow.idle_timeout_secs = Some(600);
        let t = StreamTimeouts::resolve(&policy, Some(&slow));
        assert_eq!(t.idle, Duration::from_secs(600), "the backend's idle limit");
        assert_eq!(t.request, policy.llm_request_timeout, "request stays the policy's when unset");
        slow.request_timeout_secs = Some(1200);
        let t = StreamTimeouts::resolve(&policy, Some(&slow));
        assert_eq!(t.request, Duration::from_secs(1200));
    }
}

#[cfg(test)]
mod consent_tests {
    use super::*;

    #[test]
    fn collaborative_caps_at_fifty_iterations() {
        assert_eq!(iteration_cap_for_consent(ConsentMode::Collaborative), 50);
    }

    #[test]
    fn autonomous_caps_at_one_hundred_iterations() {
        assert_eq!(iteration_cap_for_consent(ConsentMode::Autonomous), 100);
    }
}

#[cfg(test)]
mod hydration_tests {
    use super::*;
    use crate::{BlockStoreError, DocumentKind, shared_block_store};

    /// When the block-log read fails during mailbox catch-up, the turn must
    /// fail loudly: a `BlockKind::Error` block lands in the conversation and the
    /// caller gets `Err(())` (so it returns early and never sends the LLM an
    /// empty/partial session). Regression for the silent-fallback that pushed
    /// `LlmMessage::user(content)` onto an empty mailbox snapshot.
    #[test]
    fn hydration_read_failure_surfaces_error_block_and_no_messages() {
        let documents = shared_block_store(PrincipalId::new());
        let context_id = ContextId::new();
        documents
            .create_document(context_id, DocumentKind::Conversation, None)
            .expect("create document");

        // Anchor block: the user's just-inserted prompt.
        let user_block_id = documents
            .insert_block_as(
                context_id,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "hello",
                Status::Done,
                ContentType::Plain,
                Some(PrincipalId::new()),
            )
            .expect("insert user block");

        let mut mailbox = crate::ConversationMailbox::new();

        // Inject a failing read (the real failure mode: DocumentNotFound).
        let read = Err(BlockStoreError::DocumentNotFound(context_id));
        let result = handle_hydration_outcome(
            &documents,
            context_id,
            &user_block_id,
            read,
            &mut mailbox,
            None,
        );

        // (b) No messages produced — the caller returns early, so the LLM is
        // never called with an empty message list.
        assert!(
            result.is_err(),
            "hydration failure must fail the turn, not yield a (possibly empty) message list"
        );

        // (a) A visible Error block lands in the conversation.
        let blocks = documents
            .block_snapshots(context_id)
            .expect("read blocks after error insert");
        assert!(
            blocks.iter().any(|b| b.kind == BlockKind::Error),
            "expected a BlockKind::Error block to surface the hydration failure, got: {:?}",
            blocks.iter().map(|b| b.kind).collect::<Vec<_>>()
        );
        // And no model/assistant text block was fabricated.
        assert!(
            !blocks
                .iter()
                .any(|b| b.role == Role::Model && b.kind == BlockKind::Text),
            "no model turn should have been produced on a failed hydration"
        );
    }

    /// A successful read hydrates the mailbox and returns the wire snapshot —
    /// the happy path still works after the refactor.
    #[test]
    fn hydration_read_success_returns_snapshot() {
        let documents = shared_block_store(PrincipalId::new());
        let context_id = ContextId::new();
        documents
            .create_document(context_id, DocumentKind::Conversation, None)
            .expect("create document");
        let user_block_id = documents
            .insert_block_as(
                context_id,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "hello",
                Status::Done,
                ContentType::Plain,
                Some(PrincipalId::new()),
            )
            .expect("insert user block");

        let mut mailbox = crate::ConversationMailbox::new();
        let result =
            hydrate_messages(&documents, context_id, &user_block_id, &mut mailbox, None);
        let messages = result.expect("successful hydration");
        assert!(
            !messages.is_empty(),
            "a conversation with a user block must hydrate at least one message"
        );
    }

    /// With a hydration policy `Some((marker, window))`, the turn hydrates only
    /// `[0, marker] ∪ last-window` — the archived middle never reaches the wire.
    /// Pins the windowed branch of the hydrate path end to end (read → window →
    /// snapshot), where a mis-wire (e.g. always passing None) would hide.
    #[test]
    fn windowed_policy_hydrates_prefix_and_tail_skips_middle() {
        let documents = shared_block_store(PrincipalId::new());
        let context_id = ContextId::new();
        documents
            .create_document(context_id, DocumentKind::Conversation, None)
            .expect("create document");
        let p = PrincipalId::new();
        let insert = |role, content: &str| {
            documents
                .insert_block_as(
                    context_id,
                    None,
                    None,
                    role,
                    BlockKind::Text,
                    content,
                    Status::Done,
                    ContentType::Plain,
                    Some(p),
                )
                .expect("insert")
        };
        insert(Role::User, "q0");
        let marker = insert(Role::Model, "a0"); // prefix end = [q0, a0]
        insert(Role::User, "q1-ARCHIVED");
        insert(Role::Model, "a1-ARCHIVED");
        insert(Role::User, "q2");
        let last = insert(Role::Model, "a2"); // tail (window 2) = [q2, a2]

        let mut mailbox = crate::ConversationMailbox::new();
        let messages =
            hydrate_messages(&documents, context_id, &last, &mut mailbox, Some((marker, 2)))
                .expect("windowed hydration");
        let wire: String = messages
            .iter()
            .filter_map(|m| m.as_text().map(str::to_string))
            .collect::<Vec<_>>()
            .join("\n");
        for kept in ["q0", "a0", "q2", "a2"] {
            assert!(wire.contains(kept), "windowed wire must keep {kept}; got: {wire}");
        }
        assert!(
            !wire.contains("ARCHIVED"),
            "the archived middle must not reach the wire; got: {wire}"
        );
    }
}

#[cfg(test)]
mod context_window_warning_tests {
    use super::*;
    use crate::{DocumentKind, shared_block_store};

    /// When the estimated turn size is at/above `CONTEXT_WARNING_THRESHOLD`
    /// of a *known* model window, a visible `BlockKind::Trace` block lands in
    /// the conversation and the turn is never blocked — `warn_if_near_
    /// context_window` returns `()` unconditionally and the already-hydrated
    /// `messages` the caller holds are untouched, so the stream proceeds to
    /// send them regardless of the warning.
    #[test]
    fn over_threshold_with_known_window_inserts_trace_and_does_not_block_turn() {
        let documents = shared_block_store(PrincipalId::new());
        let context_id = ContextId::new();
        documents
            .create_document(context_id, DocumentKind::Conversation, None)
            .expect("create document");
        let user_block_id = documents
            .insert_block_as(
                context_id,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "hello",
                Status::Done,
                ContentType::Plain,
                Some(PrincipalId::new()),
            )
            .expect("insert user block");

        // ~400 bytes of content -> ~100 estimated tokens (400/4), comfortably
        // over 90% of a 100-token window.
        let messages = vec![LlmMessage::user("x".repeat(400))];
        let window = Some(100u64);

        // The turn's message list is what actually gets sent; prove the
        // warning path doesn't consume or replace it.
        let messages_len_before = messages.len();

        warn_if_near_context_window(
            &documents,
            context_id,
            &user_block_id,
            &messages,
            "anthropic",
            "claude-opus-4-8",
            window,
        );

        assert_eq!(
            messages.len(),
            messages_len_before,
            "the warning path must never mutate the outgoing message list"
        );

        let blocks = documents
            .block_snapshots(context_id)
            .expect("read blocks after warning check");
        let trace = blocks
            .iter()
            .find(|b| b.kind == BlockKind::Trace)
            .unwrap_or_else(|| {
                panic!(
                    "expected a BlockKind::Trace warning block, got kinds: {:?}",
                    blocks.iter().map(|b| b.kind).collect::<Vec<_>>()
                )
            });
        assert!(
            trace.content.contains("claude-opus-4-8"),
            "trace content should name the model: {}",
            trace.content
        );
        assert!(
            trace.content.contains("100-token"),
            "trace content should name the window size: {}",
            trace.content
        );
        assert!(
            trace.content.contains("sent anyway"),
            "trace content should say the turn was sent anyway: {}",
            trace.content
        );
    }

    /// Small conversation, well under the threshold of a known window: no
    /// Trace block appears.
    #[test]
    fn under_threshold_with_known_window_inserts_no_trace() {
        let documents = shared_block_store(PrincipalId::new());
        let context_id = ContextId::new();
        documents
            .create_document(context_id, DocumentKind::Conversation, None)
            .expect("create document");
        let user_block_id = documents
            .insert_block_as(
                context_id,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "hello",
                Status::Done,
                ContentType::Plain,
                Some(PrincipalId::new()),
            )
            .expect("insert user block");

        let messages = vec![LlmMessage::user("hello")];
        let window = Some(100_000u64);

        warn_if_near_context_window(
            &documents,
            context_id,
            &user_block_id,
            &messages,
            "anthropic",
            "claude-opus-4-8",
            window,
        );

        let blocks = documents
            .block_snapshots(context_id)
            .expect("read blocks after warning check");
        assert!(
            !blocks.iter().any(|b| b.kind == BlockKind::Trace),
            "no warning should fire under threshold; got kinds: {:?}",
            blocks.iter().map(|b| b.kind).collect::<Vec<_>>()
        );
    }

    /// An unknown/unconfigured window (`None`) must never be treated as a
    /// fabricated denominator — no check runs at all, even for a huge
    /// conversation that would clearly be "close to the edge" under any real
    /// window.
    #[test]
    fn unknown_window_inserts_no_trace_even_for_huge_input() {
        let documents = shared_block_store(PrincipalId::new());
        let context_id = ContextId::new();
        documents
            .create_document(context_id, DocumentKind::Conversation, None)
            .expect("create document");
        let user_block_id = documents
            .insert_block_as(
                context_id,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "hello",
                Status::Done,
                ContentType::Plain,
                Some(PrincipalId::new()),
            )
            .expect("insert user block");

        let messages = vec![LlmMessage::user("x".repeat(1_000_000))];

        warn_if_near_context_window(
            &documents,
            context_id,
            &user_block_id,
            &messages,
            "anthropic",
            "claude-opus-4-8",
            None,
        );

        let blocks = documents
            .block_snapshots(context_id)
            .expect("read blocks after warning check");
        assert!(
            !blocks.iter().any(|b| b.kind == BlockKind::Trace),
            "an unknown window must never fabricate a denominator; got kinds: {:?}",
            blocks.iter().map(|b| b.kind).collect::<Vec<_>>()
        );
    }
}

/// One tool dispatch, projected into the conversation and its block pair.
///
/// `is_error` tells the model about a refusal; `status` records whether the
/// pair is settled. An unanswered gate leaves the pair `Waiting`.
struct ToolDispatch {
    content: String,
    is_error: bool,
    status: Status,
    /// Error child for a terminal refusal. A pending ask uses only its pair,
    /// which the approval owner can settle in place.
    payload: Option<kaijutsu_types::ErrorPayload>,
    /// Link this ask atomically before publishing the pair as Waiting.
    ask_id: Option<String>,
}

fn map_tool_dispatch_result(
    tool_name: &str,
    result: Result<crate::ExecResult, McpError>,
) -> ToolDispatch {
    match result {
        Ok(r) if r.success => {
            tracing::debug!("Tool {} succeeded: {}", tool_name, r.stdout);
            ToolDispatch {
                content: r.stdout,
                is_error: false,
                status: Status::Done,
                payload: None,
                ask_id: None,
            }
        }
        Ok(r) => {
            // A tool that wrote a body already said what went wrong, and that
            // body is passed through untouched — prefixing "Error: " onto a
            // structured body (the shell envelope, `docs/shell-envelope.md`)
            // makes it unparseable and reports the failure twice. The prefix
            // is for a tool that failed with nothing but stderr to show.
            let content = if r.stdout.is_empty() {
                format!("Error: {}", r.stderr)
            } else {
                r.stdout.clone()
            };
            tracing::warn!("Tool {} failed: {}", tool_name, content);
            let payload = kaijutsu_types::ErrorPayload {
                category: kaijutsu_types::ErrorCategory::Tool,
                severity: kaijutsu_types::ErrorSeverity::Error,
                code: None,
                detail: Some(content.clone()),
                span: None,
                source_kind: Some(kaijutsu_types::BlockKind::ToolResult),
            };
            ToolDispatch {
                content,
                is_error: true,
                status: Status::Error,
                payload: Some(payload),
                ask_id: None,
            }
        }
        // A refusal is a verdict the machinery reached, not an execution
        // failure — "Execution error" below would teach a model that an
        // unanswered ask was a crash, and the retry that follows mints
        // another durable ask. The kind carries a stable code, and
        // `settled_block_status` decides the blocks the same way every
        // shell path does.
        Err(e) if e.as_refusal().is_some() => {
            let status = e.settled_block_status();
            let refusal = e.as_refusal().expect("guarded by the arm above");
            tracing::info!("Tool {} refused: {}", tool_name, refusal);
            let code = match refusal.kind {
                kaijutsu_types::RefusalKind::Denied => "gate.denied",
                kaijutsu_types::RefusalKind::Pending => "gate.pending",
                kaijutsu_types::RefusalKind::GateUnavailable => "gate.unavailable",
                kaijutsu_types::RefusalKind::CapabilityDenied => "capability.denied",
                kaijutsu_types::RefusalKind::FacadeDenied => "capability.facade",
                kaijutsu_types::RefusalKind::LoadoutDenied => "capability.loadout",
            };
            let payload = kaijutsu_types::ErrorPayload {
                category: kaijutsu_types::ErrorCategory::Tool,
                severity: kaijutsu_types::ErrorSeverity::Error,
                code: Some(code.into()),
                detail: Some(refusal.to_string()),
                span: None,
                source_kind: Some(kaijutsu_types::BlockKind::ToolResult),
            };
            // The refusal's own text names the state, the ask and the
            // remedy; a prefix here would restate one of them.
            //
            // `Waiting` withholds the payload rather than authoring it and
            // relying on the two insert sites to skip it: `payload` here is
            // the ONLY input either site reads to decide whether to author a
            // `system/error` child, so deciding once, here, is the whole
            // fix rather than a rule two call sites must both remember.
            let payload = (status != Status::Waiting).then_some(payload);
            ToolDispatch {
                content: refusal.to_string(),
                is_error: true,
                status,
                payload,
                ask_id: refusal.ask_id().map(str::to_owned),
            }
        }
        Err(McpError::Policy(PolicyError::Timeout { timeout_ms, .. })) => {
            tracing::error!(
                "Tool {} timed out after {}ms (per-instance policy call_timeout)",
                tool_name,
                timeout_ms
            );
            let secs = timeout_ms as f64 / 1000.0;
            let payload = kaijutsu_types::ErrorPayload {
                category: kaijutsu_types::ErrorCategory::Tool,
                severity: kaijutsu_types::ErrorSeverity::Error,
                code: Some("tool.timeout".into()),
                detail: Some(format!("Tool '{}' timed out after {:.1}s", tool_name, secs)),
                span: None,
                source_kind: Some(kaijutsu_types::BlockKind::ToolResult),
            };
            ToolDispatch {
                content: format!("Error: tool '{}' timed out after {:.1}s", tool_name, secs),
                is_error: true,
                status: Status::Error,
                payload: Some(payload),
                ask_id: None,
            }
        }
        Err(e) => {
            tracing::error!("Tool {} execution error: {}", tool_name, e);
            let payload = kaijutsu_types::ErrorPayload {
                category: kaijutsu_types::ErrorCategory::Tool,
                severity: kaijutsu_types::ErrorSeverity::Error,
                code: None,
                detail: Some(e.to_string()),
                span: None,
                source_kind: Some(kaijutsu_types::BlockKind::ToolResult),
            };
            ToolDispatch {
                content: format!("Execution error: {}", e),
                is_error: true,
                status: Status::Error,
                payload: Some(payload),
                ask_id: None,
            }
        }
    }
}

/// Dispatch one tool call through the broker and map the outcome for the
/// conversation.
///
/// The broker enforces the instance's live `call_timeout` policy and races
/// it against cancellation. A hard interrupt wins when both are ready.
async fn dispatch_and_map_tool_result(
    kernel: &Arc<Kernel>,
    tool_name: &str,
    params: &str,
    tool_ctx: &crate::ExecContext,
    cancel: tokio_util::sync::CancellationToken,
) -> ToolDispatch {
    let result = kernel
        .dispatch_tool_via_broker_with_cancel(tool_name, params, tool_ctx, cancel)
        .await;
    map_tool_dispatch_result(tool_name, result)
}

fn pending_shell_operation_receipt(
    kernel: &Arc<Kernel>,
    documents: &SharedBlockStore,
    context_id: ContextId,
    tool_ctx: &crate::ExecContext,
    source: &str,
    ask_id: &str,
) -> Result<String, String> {
    if let Some(existing) = kernel.shell_operations().get_by_ask(ask_id, context_id)? {
        return Ok(existing.receipt.operation_id);
    }
    let arguments: serde_json::Value = serde_json::from_str(source).map_err(|error| error.to_string())?;
    let command_source = arguments.get("command").and_then(serde_json::Value::as_str)
        .ok_or_else(|| "pending shell call has no command".to_string())?;
    let receipt = documents.start_shell_operation(crate::shell_operations::ShellOperationStart {
            notify: false,
        context: context_id, principal: tool_ctx.principal_id, actor: tool_ctx.actor_id,
        source: command_source, tool: "shell", input: serde_json::json!({"command": command_source}),
        kind: kaijutsu_types::ToolKind::Shell, role: Role::Model, excluded: true, status: Status::Waiting,
        ask: Some((ask_id, crate::PairOwner::Turn)),
    }).map_err(|error| error.to_string())?;
    Ok(receipt.operation_id)
}

fn make_pending_shell_receipt(
    kernel: &Arc<Kernel>, documents: &SharedBlockStore, context_id: ContextId,
    tool_ctx: &crate::ExecContext, tool_name: &str, params: &str,
    ask_id: Option<&str>, content: &mut String, is_error: &mut bool, status: &mut Status,
) {
    if !matches!(tool_name, "shell" | "shell_write") || *status != Status::Waiting {
        return;
    }
    let Some(ask_id) = ask_id else { return; };
    if serde_json::from_str::<serde_json::Value>(params).ok()
        .and_then(|value| value.get("foreground").and_then(serde_json::Value::as_bool)) == Some(true)
    {
        return;
    }
    match pending_shell_operation_receipt(kernel, documents, context_id, tool_ctx, params, ask_id) {
        Ok(operation_id) => {
            let mut receipt = ShellEnvelope::new(kaijutsu_types::shell_envelope::ShellStatus::Waiting);
            receipt.stderr = format!("operation {operation_id} is waiting for ask {ask_id}; the command has not run");
            receipt.operation_id = Some(operation_id);
            receipt.ask_id = Some(ask_id.to_owned());
            *content = receipt.to_value().to_string();
            *is_error = false;
            *status = Status::Done;
        }
        Err(error) => {
            *content = format!("Could not create receipt for pending ask {ask_id}: {error}");
            *is_error = true;
            *status = Status::Error;
        }
    }
}

/// Execute a recorded model call only after its Running result exists. Both
/// ordinary and inline providers use this content projection and settlement.
async fn dispatch_recorded_tool_result(
    documents: &SharedBlockStore, context_id: ContextId, kernel: &Arc<Kernel>,
    tool_name: &str, input: &serde_json::Value, tool_ctx: &crate::ExecContext,
    cancel: tokio_util::sync::CancellationToken, tool_use_id: &str,
    call: kaijutsu_types::BlockId, turn_lease: &TurnLease,
) -> crate::block_store::BlockStoreResult<(InlineToolResult, kaijutsu_types::BlockId)> {
    let result = documents.insert_tool_result_as(context_id, &call, Some(&call), "", Status::Running,
        None, Some(TypesToolKind::Builtin), Some(PrincipalId::system()), Some(tool_use_id.to_owned()))?;
    turn_lease.track_block(result);
    tokio::task::yield_now().await;

    let mut tool_ctx = tool_ctx.clone();
    tool_ctx.publishes_pair = true;
    let tool_ctx = &tool_ctx;
    let params = input.to_string();
    let ToolDispatch { mut content, mut is_error, status: mut settled_status, payload, ask_id } =
        dispatch_and_map_tool_result(kernel, tool_name, &params, tool_ctx, cancel).await;
    make_pending_shell_receipt(kernel, documents, context_id, tool_ctx, tool_name, &params,
        ask_id.as_deref(), &mut content, &mut is_error, &mut settled_status);

    // The model receives the shell envelope; people and hydration read its
    // output. Project the actual output once so both readers get clean text.
    let envelope = ShellEnvelope::from_tool_result(&content);
    let source = envelope.as_ref().map_or_else(|| content.clone(), |env| env.readable_output());
    let ansi = crate::ansi_ingest::project(source.as_bytes());
    let block_content = ansi.as_ref().map_or(source.as_str(), |projection| projection.text.as_str());
    let model_content = match envelope {
        Some(env) => env.with_clean_output(block_content).to_value().to_string(),
        None => block_content.to_owned(),
    };
    let styles = ansi.as_ref().map(|projection| (projection.spans.clone(), source.as_bytes()));
    documents.settle_tool_result_as(context_id, &call, &result, block_content,
        settled_status, is_error, PrincipalId::system(), styles, ask_id.as_deref())?;
    if ask_id.is_some() { crate::kj::gate::announce_ledger_change(kernel.kernel_db(), kernel.ledger_flows()); }
    let anchor = if let Some(payload) = payload {
        documents.insert_error_block_as(context_id, &result, &payload, payload.summary_line(), Some(PrincipalId::system()))?
    } else { result };
    Ok((InlineToolResult { content: model_content, is_error }, anchor))
}

/// An inline provider waits for this callback. Reply only after durable
/// settlement and retain the final result/error block as the next anchor.
async fn dispatch_inline_tool_result(
    documents: &SharedBlockStore,
    context_id: ContextId,
    last_block_id: &mut kaijutsu_types::BlockId,
    kernel: &Arc<Kernel>,
    tool_name: &str,
    input: serde_json::Value,
    tool_ctx: &crate::ExecContext,
    cancel: tokio_util::sync::CancellationToken,
    tool_use_id: &str,
    actor_principal: PrincipalId,
    turn_lease: &TurnLease,
) -> crate::block_store::BlockStoreResult<InlineToolResult> {
    let call = documents.insert_tool_call_as(context_id, None, Some(last_block_id), tool_name,
        input.clone(), Some(TypesToolKind::Builtin), Some(actor_principal), Some(tool_use_id.to_owned()), None)?;
    turn_lease.track_block(call);
    *last_block_id = call;
    let (result, anchor) = dispatch_recorded_tool_result(documents, context_id, kernel, tool_name, &input,
        tool_ctx, cancel, tool_use_id, call, turn_lease).await?;
    *last_block_id = anchor;
    Ok(result)
}

/// The `llm.turn` span's usage fields, recorded exactly once per turn.
///
/// Each `Done` event replaces `last`; the record happens when the guard drops
/// at turn exit, on every path out of `process_llm_stream`. Recording per
/// call is wrong twice: `tracing_subscriber`'s fmt layer appends every
/// `Span::record` to a span's formatted fields instead of replacing it, so
/// every log line under the span grew by one usage block per iteration; and
/// the span is one turn, so its usage is one gauge. The final call's numbers
/// are the gauge `ContextUsageRow` keeps: each call resends the whole
/// history, so a sum would multiply-count it.
#[must_use = "the record happens when the guard drops"]
struct TurnUsageOnSpan {
    span: tracing::Span,
    last: Option<TurnUsage>,
}

struct TurnUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: u64,
    stop_reason: Option<String>,
}

impl Drop for TurnUsageOnSpan {
    fn drop(&mut self) {
        let Some(usage) = self.last.take() else {
            return;
        };
        self.span.record("llm.usage.input_tokens", usage.input_tokens);
        self.span.record("llm.usage.output_tokens", usage.output_tokens);
        self.span.record("llm.usage.cache_read_tokens", usage.cache_read_tokens);
        self.span.record("llm.usage.cache_write_tokens", usage.cache_write_tokens);
        self.span.record("llm.usage.reasoning_tokens", usage.reasoning_tokens);
        if let Some(stop_reason) = usage.stop_reason {
            self.span.record("llm.response.stop_reason", stop_reason.as_str());
        }
    }
}

/// Process LLM streaming in a background task with agentic loop.
///
/// Handles all stream events, executes tools, and loops until the model signals
/// completion or the context interrupt fires. Block events are broadcast via
/// FlowBus (BlockStore emits BlockFlow events).
///
/// `after_block_id` is the starting point for block ordering — all streaming
/// blocks will be inserted after this block (typically the user's message).
///
/// The span carries `llm.*` fields (matching the kernel's span namespace;
/// metrics live under `gen_ai.*`). Usage fields are declared empty and
/// recorded once at turn exit by [`TurnUsageOnSpan`], holding the final
/// `Done` event's numbers, so token/cache/reasoning accounting lands on the
/// trace, not just the metrics meter.
///
/// `turn.*` is the outcome namespace, recorded at the publish site: how the turn
/// ended (`turn.stop_reason`) and who asked for it (`turn.origin`) — the same two
/// facts the `TurnEvents` wire push carries, so a trace and a subscriber tell the
/// same story. Turn/LLM spans are 100%-sampled (`docs/telemetry.md`), so these
/// land on every turn.
#[derive(Debug, Clone)]
struct TurnSpanIdentity {
    performer: crate::CharacterIdentity,
    reviewer: crate::CharacterIdentity,
    director: Option<PrincipalId>,
    review_source: crate::approval_identity::ReviewSource,
}

// The full turn context (provider, target block, interrupt/cache/kernel
// handles) has to reach the stream loop somehow; a params struct would just
// relocate these 17 fields without changing the shape of the problem.
fn record_continuation_yield(
    kernel_db: &parking_lot::Mutex<KernelDb>,
    context_id: ContextId,
    continuation_epoch: Option<i64>,
) {
    let Some(epoch) = continuation_epoch else {
        return;
    };
    if let Err(error) = kernel_db
        .lock()
        .record_continuation_yield(context_id, epoch, kaijutsu_types::now_millis() as i64)
    {
        tracing::error!(
            "Could not record continuation yield for {context_id} epoch {epoch}: {error}"
        );
    }
}

#[tracing::instrument(
    name = "llm.turn",
    skip_all,
    fields(
        llm.provider = provider.name(),
        llm.model = %model_name,
        context.id = %context_id,
        principal.id = %user_principal_id,
        actor.id = tracing::field::Empty,
        reviewer.id = tracing::field::Empty,
        director.id = tracing::field::Empty,
        review.source = tracing::field::Empty,
        actor.name = tracing::field::Empty,
        reviewer.name = tracing::field::Empty,
        llm.usage.input_tokens = tracing::field::Empty,
        llm.usage.output_tokens = tracing::field::Empty,
        llm.usage.cache_read_tokens = tracing::field::Empty,
        llm.usage.cache_write_tokens = tracing::field::Empty,
        llm.usage.reasoning_tokens = tracing::field::Empty,
        llm.response.stop_reason = tracing::field::Empty,
        turn.id = %turn_lease.id(),
        turn.stop_reason = tracing::field::Empty,
        turn.origin = tracing::field::Empty,
    )
)]
#[allow(clippy::too_many_arguments)]
async fn process_llm_stream(
    provider: Arc<Provider>,
    documents: SharedBlockStore,
    context_id: ContextId,
    model_name: String,
    kernel: Arc<Kernel>,
    kernel_db: Arc<parking_lot::Mutex<KernelDb>>,
    tools: Vec<ToolDefinition>,
    after_block_id: kaijutsu_types::BlockId,
    system_prompt: String,
    max_output_tokens: u64,
    // This backend's total and idle guards (`StreamTimeouts::resolve`).
    stream_timeouts: StreamTimeouts,
    // The context's resolved cast-seat tunables (`resolve_context_model`),
    // already cascaded onto `llm_defaults`; `None` when no cast seat answered
    // (the floor then applies at the `apply_slot_tunables` seam below).
    slot_tunables: Option<SlotTunables>,
    conversation_cache: Arc<ConversationCache>,
    // The requester authors the TurnFlow outcome event; provider blocks
    // use the performing character carried by tool_ctx.
    user_principal_id: PrincipalId,
    // Resolved once when the turn begins. Tool calls receive IDs in their
    // CallContext, but character names belong on the turn span only: no
    // per-tool character database reads.
    span_identity: Option<TurnSpanIdentity>,
    tool_ctx: crate::ExecContext,
    interrupt: Arc<ContextInterruptState>,
    turn_lease: TurnLease,
    // Who asked for the turn (see `spawn_llm_for_prompt`). Rides onto every
    // terminal `TurnFlow` this stream publishes; the publish itself is
    // unconditional. It fires at actual stream end with the real output block
    // id, not at spawn racing the model.
    origin: TurnOrigin,
    continuation_epoch: Option<i64>,
) {
    let turn_id = turn_lease.id();
    let panic_anchor = after_block_id.clone();
    // The mailbox lock keeps conversation exclusion through terminal publication.
    // The lease releases liveness first so observers see that this turn ended.
    // A queued cancellation does not need to acquire the mailbox lock.
    let session = conversation_cache.get_or_create(context_id);
    let mut mailbox = tokio::select! {
        biased;
        _ = interrupt.cancel.cancelled() => None,
        mailbox = session.lock() => Some(mailbox),
    };
    let outcome = if let Some(mailbox) = mailbox.as_mut() {
        std::panic::AssertUnwindSafe(run_llm_stream(
            provider, documents.clone(), context_id, model_name, kernel.clone(), kernel_db.clone(),
            tools, after_block_id, system_prompt, max_output_tokens, stream_timeouts, slot_tunables,
            conversation_cache, user_principal_id, span_identity, tool_ctx, interrupt, origin,
            continuation_epoch, mailbox, &turn_lease,
        )).catch_unwind().await
    } else {
        Ok(Ok(TurnFlow::Completed {
            turn_id, context_id, principal_id: user_principal_id, output_block_id: None,
            reason: TurnStopReason::Cancelled { immediate: true }, origin,
        }))
    };
    let mut stream_failure = false;
    let outcome = outcome.map(|result| result.unwrap_or_else(|error| {
        stream_failure = true;
        turn_lease.interrupt().hard();
        TurnFlow::Failed { turn_id, context_id, principal_id: user_principal_id,
            error: error.to_string(), origin }
    }));

    if outcome.is_err() { turn_lease.interrupt().hard(); }
    let (cleanup, mut cleanup_panic) = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| turn_lease.settle_blocks(&documents))) {
        Ok(result) => (result, None),
        Err(panic) => (Err("block cleanup panicked; the document may require recovery".into()), Some(panic)),
    };
    let can_record_diagnostic = cleanup.is_ok();
    record_continuation_yield(&kernel_db, context_id, continuation_epoch);
    match outcome {
        Ok(mut event) => {
            let cleanup_error = match cleanup {
                Err(error) => Some(format!("Model turn block cleanup failed: {error}")),
                Ok(count) if count > 0 && matches!(event, TurnFlow::Completed { reason, .. } if reason.output_is_complete()) =>
                    Some(format!("Model turn ended with {count} unfinished block(s); they were marked Error.")),
                _ => None,
            };
            let needs_diagnostic = stream_failure || cleanup_error.is_some();
            if let Some(mut error) = cleanup_error {
                if let TurnFlow::Failed { error: original, .. } = &event {
                    error = format!("{original}; {error}");
                }
                tracing::error!(%context_id, %error);
                event = TurnFlow::Failed { turn_id, context_id, principal_id: user_principal_id, error, origin };
            }
            // Do not re-enter a document whose cleanup failed. Terminal
            // publication remains possible when its journal is unavailable.
            if needs_diagnostic && can_record_diagnostic {
                let TurnFlow::Failed { error, .. } = &event else { unreachable!("write and cleanup faults fail the turn") };
                if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
                    insert_pre_stream_error_block(&documents, context_id, &panic_anchor, error))) {
                    tracing::error!(%context_id, "recording the turn diagnostic panicked");
                    cleanup_panic = Some(panic);
                }
            }
            turn_lease.finish(&event).await;
            kernel.turn_flows().publish(event);
            if let Some(panic) = cleanup_panic { std::panic::resume_unwind(panic); }
        }
        Err(panic) => {
            let mut error = "Model turn panicked; execution stopped and side effects may be incomplete.".to_string();
            if let Err(cleanup_error) = cleanup { error.push_str(&format!(" Block cleanup failed: {cleanup_error}")); }
            if can_record_diagnostic && std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
                insert_pre_stream_error_block(&documents, context_id, &panic_anchor, &error))).is_err() {
                tracing::error!(%context_id, "recording the panic diagnostic also panicked");
            }
            let event = TurnFlow::Failed {
                turn_id, context_id, principal_id: user_principal_id, error: error.into(), origin,
            };
            turn_lease.finish(&event).await;
            kernel.turn_flows().publish(event);
            std::panic::resume_unwind(panic);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_llm_stream(
    provider: Arc<Provider>,
    documents: SharedBlockStore,
    context_id: ContextId,
    model_name: String,
    kernel: Arc<Kernel>,
    kernel_db: Arc<parking_lot::Mutex<KernelDb>>,
    tools: Vec<ToolDefinition>,
    after_block_id: kaijutsu_types::BlockId,
    system_prompt: String,
    max_output_tokens: u64,
    // This backend's total and idle guards (`StreamTimeouts::resolve`).
    stream_timeouts: StreamTimeouts,
    // The context's resolved cast-seat tunables (`resolve_context_model`),
    // already cascaded onto `llm_defaults`; `None` when no cast seat answered
    // (the floor then applies at the `apply_slot_tunables` seam below).
    slot_tunables: Option<SlotTunables>,
    conversation_cache: Arc<ConversationCache>,
    // The requester authors the TurnFlow outcome event; provider blocks
    // use the performing character carried by tool_ctx.
    user_principal_id: PrincipalId,
    // Resolved once when the turn begins. Tool calls receive IDs in their
    // CallContext, but character names belong on the turn span only: no
    // per-tool character database reads.
    span_identity: Option<TurnSpanIdentity>,
    tool_ctx: crate::ExecContext,
    interrupt: Arc<ContextInterruptState>,
    // Who asked for the turn (see `spawn_llm_for_prompt`). Rides onto every
    // terminal `TurnFlow` this stream publishes; the publish itself is
    // unconditional. It fires at actual stream end with the real output block
    // id, not at spawn racing the model.
    origin: TurnOrigin,
    continuation_epoch: Option<i64>,
    mailbox: &mut crate::ConversationMailbox,
    turn_lease: &TurnLease,
) -> Result<TurnFlow, StreamFailure> {
    let turn_id = turn_lease.id();
    if let Some(identity) = span_identity {
        let span = tracing::Span::current();
        span.record("actor.id", tracing::field::display(identity.performer.principal_id));
        span.record("reviewer.id", tracing::field::display(identity.reviewer.principal_id));
        if let Some(director) = identity.director {
            span.record("director.id", tracing::field::display(director));
        }
        span.record("review.source", identity.review_source.as_str());
        span.record("actor.name", identity.performer.name.as_str());
        span.record("reviewer.name", identity.reviewer.name.as_str());
    }

    // Records the final call's usage on the `llm.turn` span when this
    // function exits, whichever path it takes.
    let mut usage_on_span = TurnUsageOnSpan {
        span: tracing::Span::current(),
        last: None,
    };

    // The principal stamped on blocks this stream authors. Two provenance
    // categories share the turn: PROVIDER-OUTPUT is content the LLM itself
    // produced — the streamed Thinking and Text blocks and their appends,
    // and a ToolCall block, whether it arrives as an ordinary tool-use event
    // or an inline provider callback — and stamps `actor_principal` at its
    // insert and at every append/edit that extends the same block.
    // KERNEL-OUTPUT is everything the kernel itself authors about the turn —
    // tool results, structured tool errors, the max-iterations halt,
    // interrupt/quiesce/staging notices, warnings — and stamps
    // `PrincipalId::system()` directly at each site, never this binding.
    // The turn's performer is resolved before dispatch and also accompanies
    // every tool call. A model change does not change this identity.
    let actor_principal = tool_ctx.actor_id;

    // Catch the mailbox up against the current block log — folds in
    // any blocks that landed since the last turn (the user prompt
    // that triggered this call, plus shell commands, MCP tool calls,
    // drift, etc. from sibling writers). Blocks already folded in
    // are skipped, so this is O(new blocks), not O(history).
    // block_snapshots() reads from in-memory DashMap; sub-millisecond
    // for typical conversations.
    // Read the per-context hydration window policy. A read failure (DB error) or
    // a corrupt stored policy (unparseable marker / bad window) is a LOUD failure,
    // not a silent degrade: quietly hydrating full history would disable the cost
    // guard on a context driving at tempo (unbounded spend) — a silent fallback on
    // a safety mechanism. Fail the turn like any other hydration failure; an
    // announced turn must still publish exactly one terminal event (§7).
    let hydration_policy = match kernel_db.lock().get_hydration_policy(context_id) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(
                "Hydration policy read failed for context {context_id}: {e}; failing the turn"
            );
            return Ok(TurnFlow::Failed {
                turn_id, context_id,
                principal_id: user_principal_id,
                error: format!("hydration policy unreadable: {e}"),
                origin,
            });
        }
    };
    let mut messages = match hydrate_messages(
        &documents,
        context_id,
        &after_block_id,
        mailbox,
        hydration_policy,
    ) {
        Ok(messages) => messages,
        // Hydration failed and surfaced a visible Error block; fail the turn
        // loudly rather than streaming against an empty/partial session. This is a
        // terminal path: return Failed for the finalization owner to publish.
        Err(()) => {
            return Ok(TurnFlow::Failed {
                turn_id, context_id,
                principal_id: user_principal_id,
                error: "hydration failed: could not read conversation history".to_string(),
                origin,
            });
        }
    };
    // mailbox lock is held through the rest of the stream — same
    // semantics as the previous MutexGuard<Vec<LlmMessage>>: only one
    // prompt per context proceeds at a time (Fix D+E). `messages` is
    // a local Vec — the agentic loop appends to it freely, but those
    // appends don't write through to the mailbox. The next turn's
    // catch_up picks up the assistant blocks via the block log.

    // Resolve any (Asset, Text, Image) blocks against CAS so
    // vision-capable providers receive the actual bytes. CAS reads
    // are blocking std::fs; the resolver delegates each to
    // spawn_blocking so the runtime stays responsive on stacks of
    // images. Unresolved hashes fall back to a text marker via
    // to_rig_request — never panic.
    //
    // The per-hash image cache (owned by ConversationCache) skips
    // disk + base64 work for hashes already resolved this session,
    // so a 20-image conversation doesn't re-encode every turn.
    {
        let cas: std::sync::Arc<dyn crate::ContentStore> = kernel.cas().clone();
        crate::resolve_image_blocks_from_cas(
            &mut messages,
            cas,
            Some(conversation_cache.image_cache()),
        )
        .await;
    }

    // Pre-flight context-size warning (WARN AND SEND — never block the
    // turn). Runs against the FINAL message list about to be sent, after CAS
    // image resolution — the same `messages` the provider receives below.
    // See `warn_if_near_context_window` for the design rationale.
    {
        let context_window = {
            let registry = kernel.llm().read().await;
            registry
                .context_window_for_live(provider.name(), &model_name)
                .await
        };
        warn_if_near_context_window(
            &documents,
            context_id,
            &after_block_id,
            &messages,
            provider.name(),
            &model_name,
            context_window,
        );
    }

    tracing::debug!(
        "Sending {} messages for context {}",
        messages.len(),
        context_id
    );

    // Resolve the iteration cap once per stream so the guard and halt message
    // agree. Both modes allow chained tool work; this is a runaway limit.
    // TODO: Resolve consent from this context or retire the per-context setting.
    // `kj context set --consent` writes ContextRow, but this reads kernel state.
    // Do not copy context configuration into the shared kernel-wide value.
    // See docs/issues.md, "Consent setting ownership".
    let consent = kernel.consent_mode().await;
    let max_iterations = iteration_cap_for_consent(consent);
    let mut iteration: u32 = 0;
    // Output-ceiling continuations spent by this turn, against
    // `MAX_OUTPUT_CEILING_CONTINUATIONS`.
    let mut ceiling_continuations: u32 = 0;
    // Max retries for transient LLM provider failures (network blips, rate limits)
    const MAX_LLM_RETRIES: u32 = 2;

    // Track last inserted block for ordering - each new block goes after the previous
    let mut last_block_id = after_block_id;
    // Exact final model text for the turn event and any admitted score handoff.
    // A tool-only turn has no text output; consumers must not substitute the tail.
    let mut output_block_id: Option<kaijutsu_types::BlockId> = None;
    // How this turn ends, as the structured reason the tail publishes on
    // `TurnFlow::Completed`. Every `break` out of the agentic loop sets it
    // first — a cancel, an iteration cap, and a token-ceiling truncation are
    // three different endings, and reporting them as one (or as a bare error
    // string, which is what a cancel used to be) is exactly the ambiguity the
    // `TurnEvents` subscribers exist to escape. Genuine breakage still returns
    // early with `TurnFlow::Failed` and never reaches the tail, so a turn
    // publishes exactly one terminal event either way.
    let mut stop_reason_out = TurnStopReason::EndTurn;

    // Continue until completion, cancellation, or the iteration limit.
    'agentic: loop {
        if interrupt.cancel.is_cancelled() {
            stop_reason_out = TurnStopReason::Cancelled { immediate: true };
            break;
        }
        iteration += 1;
        if iteration > max_iterations {
            // Consent-aware halt message (M1-A6): in Collaborative mode the
            // cap is intentional, not a runaway. Tell the user how to
            // resume rather than just signaling an alarm.
            let halt_msg = match consent {
                ConsentMode::Collaborative => format!(
                    "Paused after {max_iterations} agentic iteration(s) (consent: collaborative). \
                     Send a follow-up to continue, or switch to autonomous to extend chains."
                ),
                ConsentMode::Autonomous => format!(
                    "⚠️ Maximum tool iterations reached ({max_iterations})."
                ),
            };
            tracing::warn!(
                "Agentic loop hit max iterations ({}, consent={}), stopping",
                max_iterations,
                consent,
            );
            let _ = documents.insert_block_as(
                context_id,
                None,
                Some(&last_block_id),
                Role::Model,
                BlockKind::Text,
                &halt_msg,
                Status::Done,
                ContentType::Plain,
                Some(PrincipalId::system()),
            );
            stop_reason_out = TurnStopReason::MaxIterations;
            break;
        }

        // Soft interrupt: stop before the next LLM call. The in-flight model
        // call already finished, so the output block is a whole phrase — that
        // is the difference `immediate: false` records.
        if interrupt
            .stop_after_turn
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            tracing::info!(
                "Soft interrupt requested for {}, stopping agentic loop",
                context_id
            );
            stop_reason_out = TurnStopReason::Cancelled { immediate: false };
            break;
        }

        tracing::debug!(
            "Agentic loop iteration {} with {} messages, {} tools",
            iteration,
            messages.len(),
            tools.len()
        );

        // Build shared-knob options. Provider-specific knobs (Claude
        // extended thinking, Gemini grounding) live as typed builder
        // methods on the provider's native request — applied inside
        // its `Client::stream()` based on configuration and context
        // state. The cache_breakpoints carrier is the one exception:
        // it's a Claude-specific policy populated per-context by rc
        // lifecycle scripts via `kj cache` (see
        // `project_cache_breakpoint_policy`). A DB read failure here
        // is non-fatal — we log and proceed without caching, since
        // prompt caching is an optimization, not a correctness
        // requirement.
        let cache_breakpoints: Vec<CacheTarget> = {
            let db = kernel_db.lock();
            match db.list_cache_breakpoints(context_id) {
                Ok(bps) => bps,
                Err(e) => {
                    tracing::warn!(
                        "Failed to read cache breakpoints for {context_id}: {e} — proceeding without caching"
                    );
                    Vec::new()
                }
            }
        };

        // Overlay the LLM tunables cascade (max_tokens/temperature/top_p/
        // effort/thinking_budget/thinking_style) onto the request:
        // `slot_tunables` is the context's resolved cast seat (already
        // cascaded onto `llm_defaults` by `resolve_context_model`); when no
        // seat answered, `floor` (the bare `llm_defaults` row) applies, so a
        // kernel with no casts configured still gets its defaults on every
        // request.
        let default_tunables = {
            let registry = kernel.llm().read().await;
            registry.default_tunables().clone()
        };
        let build_opts = apply_slot_tunables(
            BuildOpts::new(&model_name)
                .with_system(&system_prompt)
                .with_max_tokens(max_output_tokens)
                .with_tools(tools.clone())
                .with_cache_breakpoints(cache_breakpoints),
            slot_tunables.as_ref(),
            &default_tunables,
        );

        // Invalid requests cannot recover by retrying the same history.
        // Other startup failures keep the bounded backoff; mid-stream errors
        // are not retried to avoid duplicate kernel blocks.
        let mut stream = {
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                let stamp = continuation_epoch.map(|epoch| kernel_db.lock().admit_inference_request(
                    context_id, epoch, kaijutsu_types::now_millis() as i64,
                )).transpose();
                let started = match stamp {
                    Ok(Some(false)) => Err(LlmError::InvalidRequest(
                        "The continuation closed after performer reassignment; this inference request was not started.".into()
                    )),
                    Ok(_) => tokio::select! {
                        biased;
                        _ = interrupt.cancel.cancelled() => {
                            stop_reason_out = TurnStopReason::Cancelled { immediate: true };
                            break 'agentic;
                        }
                        result = provider.stream(build_opts.clone(), messages.clone()) => result,
                    },
                    Err(error) => Err(LlmError::InvalidRequest(format!(
                        "Could not record inference request: {error}"
                    ))),
                };
                match started {
                    Ok(s) => {
                        if attempt > 1 {
                            tracing::debug!("LLM stream started on attempt {}", attempt);
                        } else {
                            tracing::debug!("LLM stream started successfully");
                        }
                        break s;
                    }
                    Err(e) if attempt <= MAX_LLM_RETRIES && !matches!(&e, LlmError::InvalidRequest(_)) => {
                        let delay_secs = attempt as u64;
                        tracing::warn!(
                            "LLM stream failed (attempt {}/{}): {}, retrying in {}s",
                            attempt,
                            MAX_LLM_RETRIES + 1,
                            e,
                            delay_secs
                        );
                        tokio::select! {
                            biased;
                            _ = interrupt.cancel.cancelled() => {
                                stop_reason_out = TurnStopReason::Cancelled { immediate: true };
                                break 'agentic;
                            }
                            _ = tokio::time::sleep(std::time::Duration::from_secs(delay_secs)) => {}
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            "Failed to start LLM stream after {} attempts: {}",
                            attempt,
                            e
                        );
                        let payload = kaijutsu_types::ErrorPayload {
                            category: kaijutsu_types::ErrorCategory::Stream,
                            severity: kaijutsu_types::ErrorSeverity::Error,
                            code: None,
                            detail: Some(format!(
                                "Failed after {} attempts: {}",
                                attempt, e
                            )),
                            span: None,
                            source_kind: None,
                        };
                        let _ = documents.insert_error_block_as(
                            context_id,
                            &last_block_id,
                            &payload,
                            payload.summary_line(),
                            Some(PrincipalId::system()),
                        );
                        // Finalization publishes this failure after cleanup.
                        return Ok(TurnFlow::Failed {
                            turn_id, context_id,
                            principal_id: user_principal_id,
                            error: format!("LLM stream failed to start: {e}"),
                            origin,
                        });
                    }
                }
            }
        };

        // Process stream events
        let mut open_content: Option<OpenContent> = None;
        // Each pending invocation already has a durable call block.
        let mut tool_calls: Vec<(String, String, serde_json::Value, kaijutsu_types::BlockId)> = vec![];
        // Calls whose arguments did not parse: `(id, name, recorded input,
        // the error result already written)`. They dispatch nothing; they ride
        // the same assistant/tool-result pairing so the model reads its own
        // call and the answer to it.
        let mut invalid_tool_calls: Vec<(String, String, serde_json::Value, String)> = vec![];
        // Collect text output for conversation history
        let mut assistant_text = String::new();
        // Collect thinking output for in-call continuity (A3), one
        // `(text, signature)` entry **per** thinking block (ThinkingStart opens
        // a new entry; deltas append to it; ThinkingEnd stamps its signature).
        // Kept separate — never merged — because Anthropic verifies each
        // `signature_delta` against its own block's text; a later turn echoes
        // them back unmodified and in order. Reset per agentic-loop iteration so
        // it holds *this* turn's reasoning. This is the same per-block shape the
        // hydrator reconstructs from block history, so live and rehydrated turns
        // serialize identically.
        let mut assistant_reasoning: Vec<(String, Option<String>)> = Vec::new();
        // The provider stopped this inference at the output ceiling. Set by
        // the terminal `Done`; read at the bottom of the loop, where the turn
        // either continues with a notice or ends as `MaxTokens`.
        let mut output_ceiling_hit = false;

        tracing::debug!("Entering stream event loop");
        let mut cancel_deadline = None;
        // Two-layer timeout: total wall-clock cap on the entire completion,
        // and a per-chunk idle guard for providers that open the connection
        // but stop sending tokens — both the backend's own when it set them.
        let idle_timeout = stream_timeouts.idle;
        let request_timeout = stream_timeouts.request;
        let total_deadline =
            tokio::time::sleep(request_timeout);
        tokio::pin!(total_deadline);
        let received_done = 'stream: loop {
            // Cancellation retains accepted output and may still report usage.
            // One absolute drain deadline prevents a trickling provider from
            // extending cancellation indefinitely.
            let event = if let Some(deadline) = cancel_deadline {
                match tokio::time::timeout_at(deadline, stream.next_event()).await {
                    Ok(Some(event)) => event,
                    Ok(None) => break 'stream false,
                    Err(_) => {
                        tracing::warn!(%context_id, "provider did not confirm cancellation before the drain deadline");
                        break 'stream false;
                    }
                }
            } else {
                tokio::select! {
                    biased;
                    _ = interrupt.cancel.cancelled() => {
                        tracing::info!("Hard interrupt: cancelling LLM stream for {}", context_id);
                        stream.cancel();
                        cancel_deadline = Some(tokio::time::Instant::now() + idle_timeout);
                        continue 'stream;
                    }
                    _ = &mut total_deadline => {
                        tracing::warn!(
                            "LLM stream exceeded total request timeout {:?} ({})",
                            request_timeout, context_id
                        );
                        stream.cancel();
                        StreamEvent::Error(format!(
                            "LLM request timed out after {:?}", request_timeout
                        ))
                    }
                    r = tokio::time::timeout(idle_timeout, stream.next_event()) => {
                        match r {
                            Ok(Some(ev)) => ev,
                            Ok(None) => break 'stream false,
                            Err(_) => {
                                tracing::warn!(
                                    "LLM stream idle for {:?} ({})",
                                    idle_timeout, context_id
                                );
                                stream.cancel();
                                StreamEvent::Error(format!(
                                    "LLM stream idle for {:?}", idle_timeout
                                ))
                            }
                        }
                    }
                }
            };
            tracing::debug!("Received stream event: {:?}", event);
            if cancel_deadline.is_some() {
                match &event {
                    StreamEvent::Done { .. } => {},
                    StreamEvent::Error(error) => {
                        tracing::debug!(%context_id, %error, "provider stopped during cancellation");
                        break 'stream false;
                    }
                    _ => continue 'stream,
                }
            } else {
                validate_content_event(open_content, &event)?;
            }
            match event {
                StreamEvent::ThinkingStart => {
                    // Open a fresh reasoning entry for this block (its deltas
                    // append here; ThinkingEnd stamps its signature).
                    assistant_reasoning.push((String::new(), None));
                    let block_id = documents.insert_block_as(
                        context_id,
                        None,
                        Some(&last_block_id),
                        Role::Model,
                        BlockKind::Thinking,
                        "",
                        Status::Running,
                        ContentType::Plain,
                        Some(actor_principal),
                    )?;
                    turn_lease.track_block(block_id);
                    last_block_id = block_id;
                    open_content = Some(OpenContent::Thinking(block_id));
                }

                StreamEvent::ThinkingDelta(text) => {
                    let block_id = open_content.expect("validated thinking delta").id();
                    documents.append_text_as(context_id, &block_id, &text, Some(actor_principal))?;
                    assistant_reasoning.last_mut().expect("ThinkingStart opened reasoning").0.push_str(&text);
                }

                StreamEvent::ThinkingEnd { signature } => {
                    let block_id = open_content.take().expect("validated thinking end").id();
                    // Hydration needs the same bytes and verifier as this call.
                    if let Some(signature) = signature.filter(|signature| !signature.is_empty()) {
                        documents.set_signature(context_id, &block_id, Some(signature.clone()))?;
                        assistant_reasoning.last_mut().expect("ThinkingStart opened reasoning").1 = Some(signature);
                    }
                    // Display-only summary precedes the completion status.
                    if let Some(summary) = assistant_reasoning.last().and_then(|(text, _)| summarize_thinking(text))
                        && let Err(error) = documents.set_summary(context_id, &block_id, summary) {
                        tracing::warn!(%error, "Failed to persist thinking summary");
                    }
                    documents.set_status(context_id, &block_id, Status::Done)?;
                }

                StreamEvent::TextStart => {
                    let block_id = documents.insert_block_as(
                        context_id,
                        None,
                        Some(&last_block_id),
                        Role::Model,
                        BlockKind::Text,
                        "",
                        Status::Running,
                        ContentType::Plain,
                        Some(actor_principal),
                    )?;
                    turn_lease.track_block(block_id);
                    last_block_id = block_id;
                    open_content = Some(OpenContent::Text(block_id));
                    // This is the turn's model-text output. A later text
                    // block in the same turn supersedes it — Completed
                    // carries the LAST one, the model's final say.
                    output_block_id = Some(block_id);
                }

                StreamEvent::TextDelta(text) => {
                    let block_id = open_content.expect("validated text delta").id();
                    documents.append_text_as(context_id, &block_id, &text, Some(actor_principal))?;
                    assistant_text.push_str(&text);
                }

                StreamEvent::TextEnd => {
                    let block_id = open_content.take().expect("validated text end").id();
                    documents.set_status(context_id, &block_id, Status::Done)?;
                }

                StreamEvent::ToolUse { id, name, input } => {
                    let call = documents.insert_tool_call_as(context_id, None, Some(&last_block_id), &name,
                        input.clone(), Some(TypesToolKind::Builtin), Some(actor_principal), Some(id.clone()), None)?;
                    turn_lease.track_block(call);
                    last_block_id = call;
                    tool_calls.push((id, name, input, call));
                }

                StreamEvent::ToolUseInvalid { id, name, arguments, error } => {
                    // The model made this call and waits on it. Record it with
                    // the arguments as they arrived and answer it with an error
                    // result: failing the turn tells the model nothing, and it
                    // ends the work with the call still unanswered.
                    let detail = format!(
                        "This call did not run: its arguments were not valid JSON. They stop \
                         after {} bytes ({error}), most likely cut off at the output limit. \
                         Make the call again in smaller pieces — write a large file in several \
                         appends rather than one call.",
                        arguments.len(),
                    );
                    tracing::warn!(
                        %context_id, tool = %name, call = %id, bytes = arguments.len(), %error,
                        "Tool call arguments did not parse; answering the model with an error result"
                    );
                    // The raw text rides as one JSON field: the wire needs a
                    // valid object on both the call and its result, and this
                    // keeps what the model wrote where it can read it back.
                    let input = serde_json::json!({ "truncated_arguments": arguments });
                    let call = documents.insert_tool_call_as(context_id, None, Some(&last_block_id),
                        &name, input.clone(), Some(TypesToolKind::Builtin), Some(actor_principal),
                        Some(id.clone()), None)?;
                    turn_lease.track_block(call);
                    let answer = documents.insert_tool_result_as(context_id, &call, Some(&call),
                        "", Status::Running, None, Some(TypesToolKind::Builtin),
                        Some(PrincipalId::system()), Some(id.clone()))?;
                    turn_lease.track_block(answer);
                    // Settle the pair rather than writing the result alone:
                    // the call block reaches a final status the same way every
                    // other dispatched call does.
                    documents.settle_tool_result_as(context_id, &call, &answer, &detail,
                        Status::Error, true, PrincipalId::system(), None, None)?;
                    last_block_id = answer;
                    invalid_tool_calls.push((id, name, input, detail));
                }

                StreamEvent::InlineToolUse { id, name, input } => {
                    let result = dispatch_inline_tool_result(
                        &documents,
                        context_id,
                        &mut last_block_id,
                        &kernel,
                        &name,
                        input,
                        &tool_ctx,
                        interrupt.cancel.clone(),
                        &id,
                        actor_principal,
                        turn_lease,
                    )
                    .await?;

                    // Reply only after the durable blocks have reached their
                    // final states.  A failed reply is fatal: continuing to
                    // consume this stream would leave the provider waiting on
                    // a callback it never received.
                    if let Err(error) = stream.respond_inline_tool(&id, result).await {
                        let detail = format!(
                            "failed to return inline tool result for {name} ({id}): {error}"
                        );
                        tracing::error!("{}", detail);
                        let payload = kaijutsu_types::ErrorPayload {
                            category: kaijutsu_types::ErrorCategory::Stream,
                            severity: kaijutsu_types::ErrorSeverity::Error,
                            code: Some("stream.inline_tool_reply".into()),
                            detail: Some(detail.clone()),
                            span: None,
                            source_kind: Some(BlockKind::ToolResult),
                        };
                        let _ = documents.insert_error_block_as(
                            context_id,
                            &last_block_id,
                            &payload,
                            payload.summary_line(),
                            Some(PrincipalId::system()),
                        );
                        return Ok(TurnFlow::Failed {
                            turn_id, context_id,
                            principal_id: user_principal_id,
                            error: detail,
                            origin,
                        });
                    }
                }

                StreamEvent::ToolResult { .. } => {
                    unreachable!("provider results are rejected by content validation");
                }

                StreamEvent::Done {
                    stop_reason,
                    input_tokens,
                    output_tokens,
                    extra,
                } => {
                    // Map the provider-specific usage extra into the shared
                    // TokenCounts shape. DeepSeek reports an automatic-cache
                    // hit/miss split + reasoning tokens; Anthropic reports
                    // cache read/creation. Unknown / absent → zeros.
                    use crate::llm::UsageExtra;
                    let (cache_read, cache_write, reasoning) = match &extra {
                        Some(UsageExtra::OpenAiCompat(d)) => {
                            (d.prompt_cache_hit_tokens, 0, d.reasoning_tokens)
                        }
                        Some(UsageExtra::Claude(c)) => (
                            c.cache_read_input_tokens,
                            c.cache_creation_input_tokens,
                            0,
                        ),
                        None => (0, 0, 0),
                    };

                    // The turn span gets this call's usage at turn exit, once
                    // (`TurnUsageOnSpan`); a later call replaces it.
                    usage_on_span.last = Some(TurnUsage {
                        input_tokens: input_tokens.unwrap_or(0),
                        output_tokens: output_tokens.unwrap_or(0),
                        cache_read_tokens: cache_read,
                        cache_write_tokens: cache_write,
                        reasoning_tokens: reasoning,
                        stop_reason: stop_reason.clone(),
                    });

                    // Record token usage to the global meter (no-op until OTel
                    // is enabled). Both the completed and cancelled paths spend
                    // tokens, so record before branching. cache_creation maps
                    // from the provider extra (Anthropic cache writes); reasoning
                    // rides as its own gen_ai.token.type.
                    kaijutsu_telemetry::record_llm_usage(
                        provider.name(),
                        &model_name,
                        kaijutsu_telemetry::TokenCounts {
                            input: input_tokens.unwrap_or(0),
                            output: output_tokens.unwrap_or(0),
                            cache_read,
                            cache_creation: cache_write,
                            reasoning,
                        },
                    );

                    // Context-usage gauge: persist a SNAPSHOT of this call's
                    // usage as "how full is this context right now" — NOT a
                    // running sum (see `ContextUsageRow` / the `context_usage`
                    // table doc comment). Every `Done` event overwrites the
                    // previous value: each provider call resends the ENTIRE
                    // growing conversation as input (the agentic loop below
                    // appends tool results and re-sends `messages` in full on
                    // every iteration), so the LAST `Done` event this turn
                    // produces already reflects the whole history — summing
                    // per-iteration `input_tokens` across a multi-tool-call
                    // turn would multiply-count the same resent history N
                    // times. Recorded before the cancel branch, same as
                    // telemetry above: a cancelled call still spent tokens and
                    // still tells us how full the context is.
                    //
                    // `total_input_tokens` normalizes a provider quirk:
                    // Anthropic's `input_tokens` EXCLUDES cache_read/
                    // cache_creation tokens (billed and reported separately
                    // even though they were part of what was sent), so the
                    // actual prompt size is input_tokens + cache_read +
                    // cache_creation. DeepSeek/OpenAI-compatible
                    // `prompt_tokens` already includes the cache hit/miss
                    // split (hit + miss == prompt_tokens — see
                    // `llm/openai/stream.rs` tests), so adding `cache_read`
                    // there would double-count.
                    // "The provider never told us" is NOT "the provider said
                    // zero" — and the write below is an unconditional upsert
                    // over the context's single row, so conflating the two
                    // DESTROYS a good snapshot. The OpenAI-compatible path
                    // only learns usage from a final chunk that never arrives
                    // if the stream is cancelled first (`llm/openai/stream.rs`
                    // yields `None` for both counts in that case), so a hard
                    // interrupt would otherwise overwrite a real "234k/1M"
                    // with "0" and leave the gauge quietly lying until the
                    // next completed call. Claude reports `input_tokens` at
                    // `message_start`, so a cancel there still carries real
                    // numbers and still records — which is the behavior we
                    // want and the reason this guard checks for the total
                    // absence of data rather than keying off cancellation.
                    if input_tokens.is_none() && output_tokens.is_none() {
                        tracing::debug!(
                            "context usage not recorded for {context_id}: provider \
                             reported no token counts (stream ended before usage \
                             arrived); keeping the previous snapshot"
                        );
                    } else {
                        let is_claude = matches!(extra, Some(UsageExtra::Claude(_)));
                        let total_input_tokens = input_tokens.unwrap_or(0)
                            + if is_claude { cache_read + cache_write } else { 0 };
                        // The TTL actually applied to this request's cache
                        // breakpoints. Only Claude's `build()` (`llm/claude/
                        // build.rs`) turns `BuildOpts::cache_breakpoints` into
                        // a real `cache_control` on the wire — every other
                        // provider's `build()` ignores the carrier, so a
                        // breakpoint sitting unused in `build_opts` there
                        // would be a TTL claim the request never made.
                        let cache_ttl_secs = if is_claude {
                            longest_cache_ttl_secs(&build_opts.cache_breakpoints)
                        } else {
                            0
                        };
                        let usage_row = crate::ContextUsageRow {
                            context_id,
                            provider: provider.name().to_string(),
                            model: model_name.clone(),
                            input_tokens: total_input_tokens as i64,
                            output_tokens: output_tokens.unwrap_or(0) as i64,
                            cache_read_tokens: cache_read as i64,
                            cache_write_tokens: cache_write as i64,
                            reasoning_tokens: reasoning as i64,
                            cache_ttl_secs,
                            updated_at: kaijutsu_types::now_millis() as i64,
                        };
                        if let Err(e) = kernel_db.lock().set_context_usage(&usage_row) {
                            // Non-fatal: the turn itself succeeded; only the
                            // usage gauge failed to persist. Loud log (not
                            // silent) so a persistently-failing write is
                            // observable, but the conversation must not fail
                            // because a telemetry-adjacent counter didn't write.
                            tracing::warn!(
                                "Failed to persist context usage for {context_id}: {e}"
                            );
                        }
                    }

                    if cancel_deadline.is_some() {
                        // Usage may arrive during cancellation, but its notice
                        // is a kernel fact and must not hydrate as model text.
                        let _ = documents
                            .insert_block_as(
                                context_id,
                                None,
                                Some(&last_block_id),
                                Role::System,
                                BlockKind::Text,
                                "⛔ Interrupted",
                                Status::Done,
                                ContentType::Plain,
                                Some(PrincipalId::system()),
                            )
                            .and_then(|bid| documents.set_ephemeral(context_id, &bid, true));
                        break 'stream true;
                    }
                    // Preserve truncation. Each completion replaces the previous
                    // iteration's reason; refusal and stop_sequence use EndTurn
                    // until the public stop-reason type distinguishes them.
                    match stop_reason.as_deref() {
                        Some("refusal") => {
                            tracing::warn!(
                                "LLM stream ended with stop_reason=refusal for {context_id} \
                                 — the model declined to answer; reported to the turn \
                                 outcome as EndTurn until the wire has a dedicated \
                                 Refusal stop reason"
                            );
                        }
                        Some("stop_sequence") => {
                            tracing::info!(
                                "LLM stream ended with stop_reason=stop_sequence for \
                                 {context_id}; reported to the turn outcome as EndTurn"
                            );
                        }
                        _ => {}
                    }
                    stop_reason_out = match stop_reason.as_deref() {
                        Some("max_tokens") | Some("length") => TurnStopReason::MaxTokens,
                        _ => TurnStopReason::EndTurn,
                    };
                    output_ceiling_hit = stop_reason_out == TurnStopReason::MaxTokens;
                    tracing::info!(
                        "LLM stream completed: stop_reason={:?}, tokens_in={:?}, tokens_out={:?}",
                        stop_reason,
                        input_tokens,
                        output_tokens
                    );
                    break 'stream true;
                }

                StreamEvent::Error(err) => {
                    tracing::error!("LLM stream error: {}", err);
                    let payload = kaijutsu_types::ErrorPayload {
                        category: kaijutsu_types::ErrorCategory::Stream,
                        severity: kaijutsu_types::ErrorSeverity::Error,
                        code: None,
                        detail: Some(err.clone()),
                        span: None,
                        source_kind: None,
                    };
                    let _ = documents.insert_error_block_as(
                        context_id,
                        &last_block_id,
                        &payload,
                        payload.summary_line(),
                        Some(PrincipalId::system()),
                    );
                    // The outer owner settles open blocks before publishing failure.
                    return Ok(TurnFlow::Failed {
                        turn_id, context_id,
                        principal_id: user_principal_id,
                        error: format!("LLM stream error: {err}"),
                        origin,
                    });
                }
            }
        };
        if !received_done && cancel_deadline.is_none() {
            return Err(StreamFailure::Protocol("stream ended before Done".into()));
        }

        // Hard cancellation ends with a retained fragment, never a score delivery.
        if cancel_deadline.is_some() {
            stop_reason_out = TurnStopReason::Cancelled { immediate: true };
            break;
        }

        // Check if we need to execute tools.
        // rig doesn't expose stop_reason through FinalCompletionResponse — its own
        // agent uses the presence of tool calls as the continuation signal (see
        // rig-core streaming.rs did_call_tool pattern). This is reliable because
        // the API only emits ToolCall content blocks when stop_reason is "tool_use".
        // A call whose arguments did not parse counts here: it dispatches
        // nothing, but its error result is the model's turn back, and it is
        // the one message about a response the output limit cut off.
        if tool_calls.is_empty() && invalid_tool_calls.is_empty() {
            // The output ceiling cut this inference off before the model
            // reached a tool call, so the turn has nothing to act on and
            // nobody to ask — a driven worker has no human to say "continue".
            // Tell the model what happened and take another inference, up to
            // `MAX_OUTPUT_CEILING_CONTINUATIONS` times per turn. Past that the
            // turn ends with the provider's own `max_tokens` reason.
            //
            // Decide the whole continuation before writing anything durable: a
            // notice the model never receives still hydrates into the next
            // turn as an instruction about a response that no longer exists.
            // The conditions are the ones the top of this loop applies to the
            // next pass, plus the two that belong to a ceiling stop: the
            // continuation budget, and a turn a beat is waiting on, which ends
            // at its first ceiling stop rather than spending inferences the
            // caller did not budget for (`docs/tracks.md`).
            let stop = CeilingStop {
                ceiling: output_ceiling_hit,
                continuations_spent: ceiling_continuations,
                timed_delivery: turn_lease.owes_timed_delivery(),
                iterations_left: iteration < max_iterations,
                cancelled: interrupt.cancel.is_cancelled(),
                stopping_after_turn: interrupt
                    .stop_after_turn
                    .load(std::sync::atomic::Ordering::Relaxed),
            };
            if stop.continues() {
                ceiling_continuations += 1;
                // Replay what arrived so the model can continue from it.
                if let Some(assistant) = ceiling_continuation_assistant(
                    std::mem::take(&mut assistant_reasoning),
                    &assistant_text,
                ) {
                    messages.push(assistant);
                }
                let notice = format!(
                    "Your last response stopped at the output limit of {} tokens before you \
                     finished, and no tool call in it was complete. Continue from where you \
                     stopped. Keep your reasoning brief and act with a tool call.",
                    build_opts.max_tokens,
                );
                tracing::warn!(
                    %context_id,
                    continuation = ceiling_continuations,
                    limit = MAX_OUTPUT_CEILING_CONTINUATIONS,
                    "Inference stopped at the output ceiling; continuing the turn with a notice"
                );
                // A `(System, Notification)` block is the durable, visible
                // carrier the model reads as a user message, live and on the
                // next hydration alike (`llm/hydrate.rs`).
                //
                // A write fault fails the turn, like every other block this
                // stream authors for the model (`?` on the provider content
                // writes above): the notice and the message pushed below are
                // one fact, and continuing with only the wire copy would send
                // an instruction no later hydration can reproduce.
                let notice_block = documents.insert_block_as(
                    context_id,
                    None,
                    Some(&last_block_id),
                    Role::System,
                    BlockKind::Notification,
                    &notice,
                    Status::Done,
                    ContentType::Plain,
                    Some(PrincipalId::system()),
                )?;
                last_block_id = notice_block;
                messages.push(LlmMessage::user(notice));
                continue 'agentic;
            }
            if output_ceiling_hit {
                // The turn ends here. Report why it did not continue, and let
                // a requested interrupt name the ending it caused — the same
                // reason the top of the loop would have published.
                if stop.cancelled {
                    stop_reason_out = TurnStopReason::Cancelled { immediate: true };
                } else if stop.stopping_after_turn {
                    stop_reason_out = TurnStopReason::Cancelled { immediate: false };
                }
                tracing::warn!(
                    %context_id,
                    decision = ?stop,
                    "Output ceiling reached and the turn does not continue"
                );
            }
            // Add final assistant message to history before saving
            if !assistant_text.is_empty() {
                messages.push(LlmMessage::assistant(&assistant_text));
            }
            tracing::debug!("Agentic loop complete - no tool calls this iteration");
            break;
        }

        // Execute tools concurrently — the kernel sequences concurrent block inserts
        tracing::debug!("Executing {} tool calls concurrently", tool_calls.len());

        // Build assistant tool uses (for conversation history). A call whose
        // arguments did not parse is one of them: the provider sent it, so the
        // assistant turn carries it and the reply below answers it.
        let assistant_tool_uses: Vec<ContentBlock> = tool_calls
            .iter()
            .map(|(id, name, input, _)| ContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            })
            .chain(invalid_tool_calls.iter().map(|(id, name, input, _)| ContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            }))
            .collect();

        // Signal cancellation on the first persistence fault, but join every
        // admitted call before terminal cleanup. Dropping siblings here would
        // leave side effects without their final result.
        let futures = tool_calls.into_iter().map(|(tool_use_id, tool_name, input, call)| {
            let kernel = kernel.clone();
            let documents = documents.clone();
            let tool_ctx = tool_ctx.clone();
            let interrupt = interrupt.clone();
            async move {
                let result = dispatch_recorded_tool_result(&documents, context_id, &kernel, &tool_name, &input,
                    &tool_ctx, interrupt.cancel.clone(), &tool_use_id, call, turn_lease).await;
                match result {
                    Ok((result, anchor)) => Ok((ContentBlock::ToolResult {
                        tool_use_id, content: result.content, is_error: result.is_error,
                    }, anchor)),
                    Err(error) => {
                        tracing::error!(%tool_name, %tool_use_id, %error, "tool persistence failed");
                        interrupt.hard();
                        Err(error)
                    }
                }
            }
        });
        let results = futures::future::join_all(futures).await;
        let mut tool_results = Vec::new();
        for result in results {
            let (content, anchor) = result?;
            tool_results.push(content);
            last_block_id = anchor;
        }
        // The error results for calls that never dispatched, in the same order
        // as their tool uses above.
        for (tool_use_id, _, _, detail) in std::mem::take(&mut invalid_tool_calls) {
            tool_results.push(ContentBlock::ToolResult {
                tool_use_id,
                content: detail,
                is_error: true,
            });
        }

        // Add assistant message with tool uses to conversation. Preserve
        // accumulated thinking (A3), one Reasoning block per thinking block, so
        // multi-step tool turns keep the model's chain-of-thought intact within
        // this `process_llm_stream` invocation. Each signature comes from the
        // provider via `StreamEvent::ThinkingEnd.signature` — load-bearing for
        // Anthropic when extended thinking is enabled and tool_use is in the
        // same turn. `replayable_reasoning` applies the one rule both paths
        // out of this loop share, the hydrator's: signed entries only.
        let reasoning = replayable_reasoning(std::mem::take(&mut assistant_reasoning));
        let text = (!assistant_text.is_empty()).then(|| std::mem::take(&mut assistant_text));
        messages.push(LlmMessage::with_reasoning_text_and_tool_uses(
            reasoning,
            text,
            assistant_tool_uses,
        ));

        // Add user message with tool results
        messages.push(LlmMessage::tool_results(tool_results));

        // Each mutation is now journaled via journal_op — no explicit checkpoint needed.

        // Loop continues - re-prompt with tool results
    }

    // Conversation history is already persisted in the per-context lock.
    // The MutexGuard drops when this function returns.
    tracing::debug!(
        "Conversation cache updated: {} messages for cell {}",
        messages.len(),
        context_id
    );

    let turn_span = tracing::Span::current();
    turn_span.record("turn.stop_reason", stop_reason_out.as_str());
    turn_span.record("turn.origin", origin.as_str());
    Ok(TurnFlow::Completed {
        turn_id, context_id,
        principal_id: user_principal_id,
        output_block_id,
        reason: stop_reason_out,
        origin,
    })
}

#[cfg(test)]
mod publish_tests {
    //! The turn-outcome publish site: at stream *end* (not spawn — that raced
    //! the model, T15 / design-chameleon-batch1-f2-notation §7/§16), carrying
    //! the model's output block id, the structured stop reason, and the turn's
    //! origin.
    //!
    //! EVERY turn publishes now, interactive included. The old producer-side
    //! gate silenced interactive turns because the beat scheduler must not
    //! crystallize them; that filter moved onto the event (`origin`) so wire
    //! subscribers can see the turns a UI cares about most. These tests pin
    //! both halves: everyone announces, and the announcement says enough to be
    //! filtered correctly.
    //!
    //! This is the smallest honest test of the publish site (the design allows
    //! it over the heavy mock-provider SSH e2e harness, which the project's
    //! test discipline steers away from for --lib runs — russh teardown noise).
    //! It drives `process_llm_stream` directly with a Mock provider against a
    //! real ephemeral kernel + block store, so the publish is exercised end to
    //! end through the actual stream loop, not a stub.
    use super::*;
    use crate::block_store::{BlockStore, DocumentKind};
    use crate::flows::{FlowBus, SharedBlockFlowBus};
    use crate::kernel_db::KernelDb;
    use crate::llm::{MockClient, Provider};
    use kaijutsu_types::SessionId;

    use crate::runtime::interrupt::ContextInterruptState;
    use crate::runtime::turn_state::ConversationCache;

    /// Build the args and run one `process_llm_stream` against a Mock provider.
    /// Returns the documents store and the principal so the caller can inspect
    /// the inserted blocks.
    async fn drive_one_turn(
        origin: TurnOrigin,
        kernel: Arc<Kernel>,
    ) -> (SharedBlockStore, ContextId, PrincipalId) {
        drive_turn_with(
            origin,
            kernel,
            Provider::Mock(MockClient::new("X:1\nK:C\nCDEF|\n")),
            |_| {},
        )
        .await
    }

    /// The general form: pick the provider, and get a chance to arm the
    /// interrupt state *before* the stream starts (how the cancel paths are
    /// driven — a cancel already pending when the loop first looks is the
    /// deterministic stand-in for one that lands mid-turn).
    async fn drive_turn_with(
        origin: TurnOrigin,
        kernel: Arc<Kernel>,
        provider: Provider,
        arm_interrupt: impl FnOnce(&Arc<ContextInterruptState>),
    ) -> (SharedBlockStore, ContextId, PrincipalId) {
        drive_turn_with_write_fault(origin, kernel, provider, arm_interrupt, 0).await
    }

    async fn drive_turn_with_write_fault(
        origin: TurnOrigin,
        kernel: Arc<Kernel>,
        provider: Provider,
        arm_interrupt: impl FnOnce(&Arc<ContextInterruptState>),
        rejected_write: usize,
    ) -> (SharedBlockStore, ContextId, PrincipalId) {
        let documents = kernel.blocks().clone();
        let ctx = ContextId::new();
        documents
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let player = PrincipalId::new();
        // The user/seed block the turn anchors after.
        let after = documents
            .insert_block_as(
                ctx,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "write a phrase",
                Status::Done,
                ContentType::Plain,
                Some(player),
            )
            .unwrap();

        documents.arm_accept_fault(rejected_write);

        let provider = Arc::new(provider);
        let kernel_db = kernel.kernel_db().clone();
        let conversation_cache = Arc::new(ConversationCache::new(8));
        let interrupt = ContextInterruptState::new();
        arm_interrupt(&interrupt);
        let tool_ctx = crate::ExecContext::new(
            player,
            ctx,
            std::path::PathBuf::from("/"),
            SessionId::new(),
            kernel.id(),
        );

        let turn_lease = kernel.clone().turns().begin_with_interrupt(ctx, interrupt.clone());
        process_llm_stream(
            provider,
            documents.clone(),
            ctx,
            "mock-model".to_string(),
            kernel.clone(),
            kernel_db,
            vec![],
            after,
            "system".to_string(),
            1024,
            StreamTimeouts::from_policy(kernel.timeouts()),
            None,
            conversation_cache,
            player,
            None,
            tool_ctx,
            interrupt,
            turn_lease,
            origin,
            None,
        )
        .await;

        (documents, ctx, player)
    }

    #[tokio::test]
    async fn provider_framing_rejects_missing_or_mismatched_content_boundaries() {
        let done = || StreamEvent::Done { stop_reason: Some("end_turn".into()), input_tokens: None, output_tokens: None, extra: None };
        let tool = || StreamEvent::ToolUse { id: "unrun".into(), name: "missing_tool".into(), input: serde_json::json!({}) };
        let cases = vec![
            ("text delta without start", vec![StreamEvent::TextDelta("unframed bytes".into()), done()]),
            ("thinking delta without start", vec![StreamEvent::ThinkingDelta("unframed bytes".into()), done()]),
            ("text end without start", vec![StreamEvent::TextEnd, done()]),
            ("thinking end without start", vec![StreamEvent::ThinkingEnd { signature: Some("orphan signature".into()) }, done()]),
            ("text in thinking", vec![StreamEvent::ThinkingStart, StreamEvent::TextDelta("wrong-kind bytes".into()), StreamEvent::ThinkingEnd { signature: None }, done()]),
            ("thinking in text", vec![StreamEvent::TextStart, StreamEvent::ThinkingDelta("wrong-kind bytes".into()), StreamEvent::TextEnd, done()]),
            ("text end during thinking", vec![StreamEvent::ThinkingStart, StreamEvent::TextEnd, done()]),
            ("thinking end during text", vec![StreamEvent::TextStart, StreamEvent::ThinkingEnd { signature: Some("wrong signature".into()) }, done()]),
            ("nested start", vec![StreamEvent::TextStart, StreamEvent::ThinkingStart, StreamEvent::ThinkingEnd { signature: None }, done()]),
            ("tool inside text", vec![StreamEvent::TextStart, tool(), StreamEvent::TextEnd, done()]),
            ("inline tool inside text", vec![StreamEvent::TextStart, StreamEvent::InlineToolUse { id: "unrun".into(), name: "missing_tool".into(), input: serde_json::json!({}) }, done()]),
            ("provider result", vec![StreamEvent::ToolResult { tool_use_id: "invented".into(), content: "invented result".into(), is_error: false }, done()]),
            ("done inside text", vec![StreamEvent::TextStart, StreamEvent::TextDelta("partial".into()), done()]),
            ("empty EOF", vec![]),
            ("closed content EOF", vec![StreamEvent::TextStart, StreamEvent::TextDelta("retained".into()), StreamEvent::TextEnd]),
            ("pending tool EOF", vec![tool()]),
        ];
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            for (case, events) in cases {
                let kernel = Arc::new(Kernel::new_ephemeral("provider-framing").await);
                let mut terminal = kernel.turn_flows().subscribe("turn.*");
                let provider = Provider::Mock(MockClient::new("").with_scripted_stream(vec![events]));
                let (documents, ctx, _) = drive_turn_with(TurnOrigin::Interactive, kernel, provider, |_| {}).await;
                let event = terminal.try_recv().expect("one terminal event").payload;
                let TurnFlow::Failed { error, .. } = event else { panic!("{case}: {event:?}") };
                assert!(error.starts_with("Invalid provider stream:"), "{case}: {error}");
                assert!(terminal.try_recv().is_none());
                let blocks = documents.block_snapshots(ctx).unwrap();
                assert!(!blocks.iter().any(|b| b.status == Status::Running));
                assert!(!blocks.iter().any(|b| b.content.contains("wrong-kind bytes") || b.content.contains("unframed bytes")));
                assert!(!blocks.iter().any(|b| b.kind == BlockKind::ToolResult), "{case}: malformed stream must not dispatch tools");
                assert!(!blocks.iter().any(|b| b.kind == BlockKind::Text && b.signature.is_some()));
            }
        }).await;
    }

    #[tokio::test(start_paused = true)]
    async fn provider_framing_done_ends_the_stream_without_waiting_for_eof() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            for trailing in [false, true] {
                let kernel = Arc::new(Kernel::new_ephemeral("terminal-framing").await);
                let mut terminal = kernel.turn_flows().subscribe("turn.*");
                let mut events = vec![StreamEvent::TextStart, StreamEvent::TextDelta("accepted output".into()), StreamEvent::TextEnd,
                    StreamEvent::Done { stop_reason: Some("end_turn".into()), input_tokens: None, output_tokens: None, extra: None }];
                if trailing { events.extend([StreamEvent::TextStart, StreamEvent::TextDelta("after terminal".into()), StreamEvent::TextEnd]); }
                let provider = Provider::Mock(MockClient::new("").with_scripted_stream(vec![events]).hangs_when_exhausted());
                let (documents, ctx, _) = tokio::time::timeout(std::time::Duration::from_secs(1),
                    drive_turn_with(TurnOrigin::Interactive, kernel, provider, |_| {})).await.expect("Done is terminal; no EOF wait");
                let event = terminal.try_recv().unwrap().payload;
                let TurnFlow::Completed { output_block_id: Some(output), .. } = event else { panic!("{event:?}") };
                assert_eq!(documents.get_block_snapshot(ctx, &output).unwrap().unwrap().content, "accepted output");
                assert!(!documents.block_snapshots(ctx).unwrap().iter().any(|b| b.content == "after terminal"));
                assert!(terminal.try_recv().is_none());
            }
        }).await;
    }

    #[tokio::test(start_paused = true)]
    async fn provider_framing_cancel_drain_has_one_deadline_and_preserves_accepted_prefix() {
        use std::time::Duration;
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            let kernel = Arc::new(Kernel::new_ephemeral("cancel-framing").await.with_timeouts(kaijutsu_types::TimeoutPolicy {
                llm_idle_timeout: Duration::from_millis(40), ..Default::default()
            }));
            let mut terminal = kernel.turn_flows().subscribe("turn.*");
            let mut events = vec![StreamEvent::TextStart, StreamEvent::TextDelta("accepted prefix".into())];
            events.extend((0..100).map(|_| StreamEvent::TextDelta("after cancel".into())));
            let provider = Provider::Mock(MockClient::new("").with_scripted_stream(vec![events])
                .with_event_delay(Duration::from_millis(10)).hangs_when_exhausted());
            let (documents, ctx, _) = tokio::time::timeout(Duration::from_millis(100),
                drive_turn_with(TurnOrigin::Interactive, kernel, provider, |interrupt| {
                    let interrupt = interrupt.clone();
                    tokio::task::spawn_local(async move {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        interrupt.hard();
                    });
                })).await.expect("trickling events must not extend cancellation");
            let event = terminal.try_recv().unwrap().payload;
            assert!(matches!(event, TurnFlow::Completed { reason: TurnStopReason::Cancelled { immediate: true }, .. }));
            let blocks = documents.block_snapshots(ctx).unwrap();
            let partial = blocks.iter().find(|block| block.role == Role::Model && block.kind == BlockKind::Text).unwrap();
            assert_eq!(partial.content, "accepted prefix");
            assert_eq!(partial.status, Status::Error);
            assert!(terminal.try_recv().is_none());
        }).await;
    }

    #[tokio::test(start_paused = true)]
    async fn provider_framing_requested_cancel_wins_over_a_ready_terminal() {
        use std::time::Duration;
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            for ending in ["error", "eof", "done"] {
                for _ in 0..16 {
                    let kernel = Arc::new(Kernel::new_ephemeral("cancel-terminal-race").await.with_timeouts(kaijutsu_types::TimeoutPolicy {
                        llm_idle_timeout: Duration::from_millis(40), ..Default::default()
                    }));
                    let mut terminal = kernel.turn_flows().subscribe("turn.*");
                    let mut events = vec![StreamEvent::TextStart, StreamEvent::TextDelta("accepted prefix".into())];
                    match ending {
                        "error" => events.push(StreamEvent::Error("transport stopped".into())),
                        "done" => events.push(StreamEvent::Done { stop_reason: None, input_tokens: None, output_tokens: None, extra: None }),
                        _ => {},
                    }
                    let provider = Provider::Mock(MockClient::new("").with_scripted_stream(vec![events]).with_event_delay(Duration::from_millis(10)));
                    let interrupt = Arc::new(parking_lot::Mutex::new(None));
                    let armed = interrupt.clone();
                    let mut turn = Box::pin(drive_turn_with(TurnOrigin::Interactive, kernel, provider,
                        move |state| *armed.lock() = Some(state.clone())));
                    // Poll through the accepted prefix, then make the provider's
                    // terminal and cancellation ready before polling either.
                    for _ in 0..3 {
                        assert!(futures::poll!(turn.as_mut()).is_pending());
                        tokio::time::advance(Duration::from_millis(10)).await;
                    }
                    interrupt.lock().as_ref().unwrap().hard();
                    let (documents, ctx, _) = turn.await;
                    let event = terminal.try_recv().unwrap().payload;
                    assert!(matches!(event, TurnFlow::Completed { reason: TurnStopReason::Cancelled { immediate: true }, .. }), "{ending}: {event:?}");
                    let blocks = documents.block_snapshots(ctx).unwrap();
                    let partial = blocks.iter().find(|block| block.role == Role::Model && block.kind == BlockKind::Text).unwrap();
                    assert_eq!(partial.content, "accepted prefix");
                    assert_eq!(partial.status, Status::Error);
                    assert!(terminal.try_recv().is_none());
                }
            }
        }).await;
    }

    #[tokio::test]
    async fn rejected_provider_content_writes_fail_the_turn() {
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            for thinking in [false, true] {
                // Insert, append, optional thinking signature/summary, final status.
                for rejected_write in 1..=if thinking { 5 } else { 3 } {
                    let kernel = Arc::new(Kernel::new_ephemeral("content-write-refusal").await);
                    let mut terminal = kernel.turn_flows().subscribe("turn.*");
                    let mut events = if thinking { vec![StreamEvent::ThinkingStart,
                        StreamEvent::ThinkingDelta("reasoning must survive hydration".into()),
                        StreamEvent::ThinkingEnd { signature: Some("signed-reasoning".into()) }] }
                    else { vec![StreamEvent::TextStart, StreamEvent::TextDelta("required output".into()), StreamEvent::TextEnd] };
                    events.push(StreamEvent::Done { stop_reason: Some("end_turn".into()), input_tokens: None, output_tokens: None, extra: None });
                    let provider = Provider::Mock(MockClient::new("").with_scripted_stream(vec![events]));
                    let (documents, ctx, _) = drive_turn_with_write_fault(
                        TurnOrigin::Interactive, kernel, provider, |_| {}, rejected_write,
                    ).await;
                    let event = terminal.try_recv().expect("one terminal event").payload;
                    if thinking && rejected_write == 4 {
                        assert!(matches!(event, TurnFlow::Completed { .. }), "display summary is optional: {event:?}");
                    } else {
                        let TurnFlow::Failed { error, .. } = event else {
                            panic!("thinking={thinking} write={rejected_write}: {event:?}");
                        };
                        assert!(error.contains("Could not persist model output: ") && error.contains("injected acceptance refusal"), "{error}");
                    }
                    assert!(terminal.try_recv().is_none());
                    let blocks = documents.block_snapshots(ctx).unwrap();
                    assert!(!blocks.iter().any(|block| block.status == Status::Running));
                    let output = blocks.iter().find(|block| block.role == Role::Model
                        && block.kind == if thinking { BlockKind::Thinking } else { BlockKind::Text });
                    if rejected_write == 1 {
                        assert!(output.is_none(), "refused insert must not appear");
                    } else {
                        let output = output.expect("accepted partial output must remain");
                        let expected = if rejected_write == 2 { "" }
                            else if thinking { "reasoning must survive hydration" } else { "required output" };
                        assert_eq!(output.content, expected);
                        assert_eq!(output.status, if thinking && rejected_write == 4 { Status::Done } else { Status::Error });
                        if thinking {
                            assert_eq!(output.signature.as_deref(), if rejected_write <= 3 { None } else { Some("signed-reasoning") });
                            assert_eq!(output.summary.is_some(), rejected_write == 5);
                        }
                    }
                }
            }
        }).await;
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_live_tool_pairing_fails_once_without_retry() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("pairing-refusal").await);
                let mut failed = kernel.turn_flows().subscribe("turn.failed");
                let mut completed = kernel.turn_flows().subscribe("turn.completed");
                // A duplicate call id makes the live loop's appended history
                // ambiguous. The second completion must never reach the mock.
                let call = || StreamEvent::ToolUse {
                    id: "duplicate_call".into(),
                    name: "nonexistent_tool".into(),
                    input: serde_json::json!({}),
                };
                let provider = Provider::Mock(MockClient::new("").with_scripted_stream(vec![
                    vec![call(), call(), StreamEvent::Done {
                        stop_reason: Some("tool_use".into()),
                        input_tokens: Some(10),
                        output_tokens: Some(5),
                        extra: None,
                    }],
                ]));
                let (documents, ctx, _) = drive_turn_with(
                    TurnOrigin::Interactive, kernel, provider, |_| {},
                ).await;

                match failed.try_recv().expect("invalid pairing must fail the turn").payload {
                    TurnFlow::Failed { context_id, error, .. } => {
                        assert_eq!(context_id, ctx);
                        assert!(error.contains("duplicate_call"), "{error}");
                    }
                    other => panic!("expected Failed, got {other:?}"),
                }
                assert!(failed.try_recv().is_none(), "exactly one failure event");
                assert!(completed.try_recv().is_none(), "failed turn cannot complete");
                let blocks = documents.block_snapshots(ctx).unwrap();
                let errors: Vec<_> = blocks.iter().filter(|b| {
                    b.kind == BlockKind::Error && b.content.contains("Conversation tool pairing is invalid")
                }).collect();
                assert_eq!(errors.len(), 1, "refusal must leave one visible error");
                assert!(errors[0].content.contains("Failed after 1 attempts"), "{}", errors[0].content);
                assert!(errors[0].content.contains("duplicate_call"), "{}", errors[0].content);
            })
            .await;
    }

    /// An announced turn publishes `Completed` only after the stream ends, and
    /// the event carries the id of the model's text block (the one the OODA Act
    /// will crystallize) — never the seed prompt, never published at spawn.
    #[tokio::test]
    async fn turn_completed_publishes_at_stream_end_with_output_block_id() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("publish-test").await);
                let mut sub = kernel.turn_flows().subscribe("turn.completed");

                // Nothing published before the turn runs.
                assert!(sub.try_recv().is_none(), "no Completed before the turn");

                let (documents, ctx, _player) =
                    drive_one_turn(TurnOrigin::Autonomous, kernel.clone()).await;

                // The stream has fully ended; exactly one Completed is queued.
                let msg = sub
                    .try_recv()
                    .expect("an announced turn publishes Completed at stream end");
                let output_id = match msg.payload {
                    TurnFlow::Completed {
                        context_id,
                        output_block_id,
                        reason,
                        origin,
                        ..
                    } => {
                        assert_eq!(context_id, ctx);
                        assert_eq!(reason, TurnStopReason::EndTurn, "the mock ends its turn");
                        assert_eq!(origin, TurnOrigin::Autonomous);
                        output_block_id.expect("the mock turn produced text → Some(id)")
                    }
                    other => panic!("expected Completed, got {other:?}"),
                };

                // The carried id is the model's text block — not the user seed.
                let snap = documents
                    .get_block_snapshot(ctx, &output_id)
                    .unwrap()
                    .expect("output block exists");
                assert_eq!(snap.role, Role::Model, "Completed points at the MODEL block");
                assert_eq!(snap.kind, BlockKind::Text);
                assert!(
                    snap.content.contains("CDEF"),
                    "the output block carries the model's ABC, not the seed prompt"
                );

                assert!(sub.try_recv().is_none(), "exactly one Completed per turn");
            })
            .await;
    }

    /// An INTERACTIVE turn announces too — this is the whole point of the
    /// change. A human-prompted turn ending is the event an app or an ACP
    /// frontend most needs; it used to be invisible on the bus (and therefore
    /// on the wire), leaving clients to infer completion from block-status
    /// polling. It carries `origin: Interactive` so the one consumer that must
    /// ignore it (the beat scheduler's OODA Act) still can.
    #[tokio::test]
    async fn interactive_turn_announces_with_interactive_origin() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("publish-test").await);
                let mut sub = kernel.turn_flows().subscribe("turn.completed");

                let (_documents, ctx, _player) =
                    drive_one_turn(TurnOrigin::Interactive, kernel.clone()).await;

                let msg = sub
                    .try_recv()
                    .expect("an interactive turn publishes Completed like any other");
                match msg.payload {
                    TurnFlow::Completed {
                        context_id,
                        origin,
                        reason,
                        output_block_id,
                        ..
                    } => {
                        assert_eq!(context_id, ctx);
                        assert_eq!(
                            origin,
                            TurnOrigin::Interactive,
                            "the origin is what keeps this out of the OODA Act"
                        );
                        assert_eq!(reason, TurnStopReason::EndTurn);
                        assert!(
                            output_block_id.is_some(),
                            "the model wrote text, so the id is carried"
                        );
                    }
                    other => panic!("expected Completed, got {other:?}"),
                }
                assert!(sub.try_recv().is_none(), "exactly one Completed per turn");
            })
            .await;
    }

    /// A SOFT interrupt is a cancellation, not a failure: `Completed` with
    /// `Cancelled { immediate: false }`. It used to be indistinguishable from a
    /// clean end of turn (it simply broke the loop and published a bare
    /// `Completed`), so no observer could tell a player had stopped the turn.
    ///
    /// `immediate: false` also records that the in-flight model call was allowed
    /// to finish — the output block is a whole phrase, which is why this ending
    /// still feeds the Act while a hard cancel does not.
    #[tokio::test]
    async fn soft_interrupt_completes_as_a_soft_cancel() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("soft-cancel").await);
                let mut completed = kernel.turn_flows().subscribe("turn.completed");
                let mut failed = kernel.turn_flows().subscribe("turn.failed");

                // Arm the soft interrupt before the loop's first look at it:
                // deterministic, and semantically the same check the RPC trips.
                let (_documents, ctx, _player) = drive_turn_with(
                    TurnOrigin::Autonomous,
                    kernel.clone(),
                    Provider::Mock(MockClient::new("X:1\nK:C\nCDEF|\n")),
                    |interrupt| interrupt.soft(),
                )
                .await;

                let msg = completed
                    .try_recv()
                    .expect("a cancelled turn is a Completed, not a Failed");
                match msg.payload {
                    TurnFlow::Completed {
                        context_id, reason, ..
                    } => {
                        assert_eq!(context_id, ctx);
                        assert_eq!(
                            reason,
                            TurnStopReason::Cancelled { immediate: false },
                            "a soft interrupt reports itself as a soft cancel"
                        );
                        assert!(
                            reason.output_is_complete(),
                            "a soft cancel let the model finish its phrase"
                        );
                    }
                    other => panic!("expected Completed, got {other:?}"),
                }
                assert!(
                    failed.try_recv().is_none(),
                    "a cancel must never arrive as a bare-error Failed"
                );
            })
            .await;
    }

    /// A HARD interrupt reports `Cancelled { immediate: true }` — the ending
    /// that used to arrive as `Failed { error: "turn interrupted before
    /// completion" }`, a string an observer had to pattern-match to learn it
    /// was a cancel and not a crash.
    ///
    /// The cancel token is armed before the stream starts, so the loop's
    /// `select!` sees a ready cancellation branch on every poll. `select!`
    /// chooses randomly among ready branches, so the scripted stream is long
    /// (64 deltas): the chance of never picking the cancel branch is 2^-64,
    /// which is far below the flake floor of the machine running it.
    #[tokio::test]
    async fn hard_interrupt_completes_as_an_immediate_cancel() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("hard-cancel").await);
                let mut completed = kernel.turn_flows().subscribe("turn.completed");
                let mut failed = kernel.turn_flows().subscribe("turn.failed");

                let mut events = vec![crate::llm::StreamEvent::TextStart];
                events.extend(
                    (0..64).map(|_| crate::llm::StreamEvent::TextDelta("C".into())),
                );
                events.push(crate::llm::StreamEvent::TextEnd);
                events.push(crate::llm::StreamEvent::Done {
                    stop_reason: Some("end_turn".into()),
                    input_tokens: Some(0),
                    output_tokens: Some(0),
                    extra: None,
                });

                let (_documents, ctx, _player) = drive_turn_with(
                    TurnOrigin::Autonomous,
                    kernel.clone(),
                    Provider::Mock(
                        MockClient::new("unused").with_scripted_stream(vec![events]),
                    ),
                    |interrupt| interrupt.hard(),
                )
                .await;

                let msg = completed
                    .try_recv()
                    .expect("a hard-cancelled turn publishes Completed with a cancel reason");
                match msg.payload {
                    TurnFlow::Completed {
                        context_id, reason, ..
                    } => {
                        assert_eq!(context_id, ctx);
                        assert_eq!(
                            reason,
                            TurnStopReason::Cancelled { immediate: true },
                            "a hard interrupt reports itself as an immediate cancel"
                        );
                        assert!(
                            !reason.output_is_complete(),
                            "the stream was severed mid-token — the output is a fragment, \
                             so the beat scheduler must not crystallize it"
                        );
                    }
                    other => panic!("expected Completed, got {other:?}"),
                }
                assert!(
                    failed.try_recv().is_none(),
                    "a cancel is not a failure"
                );
            })
            .await;
    }

    /// Regression test: a hard cancel that races a hung provider must still
    /// publish `Cancelled`, not `Failed`. After a hard cancel, the
    /// drain loop polls the stream once more to let the provider flush its
    /// pending Done event. If the provider is HUNG — the connection stays
    /// open but never delivers that flush — the idle-timeout branch used to
    /// construct `StreamEvent::Error`, which routed through the ordinary
    /// error arm and published `Failed`. That overwrites the `Cancelled`
    /// outcome the player actually asked for with an error string, violating
    /// the cancel-is-not-failure contract this same module tests above
    /// (`hard_interrupt_completes_as_an_immediate_cancel`).
    ///
    /// `MockClient::hangs_when_exhausted` models the hang: once the scripted
    /// deltas run out, `next_event()` never resolves. `start_paused = true`
    /// lets the idle timeout actually elapse (via virtual-clock
    /// auto-advance) without the test burning real wall-clock time.
    #[tokio::test(start_paused = true)]
    async fn post_cancel_idle_timeout_with_hung_provider_is_cancelled_not_failed() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("hard-cancel-hang").await);
                let mut completed = kernel.turn_flows().subscribe("turn.completed");
                let mut failed = kernel.turn_flows().subscribe("turn.failed");

                // No TextEnd/Done in the script — deliberately: the point is
                // that nothing more ever arrives after these deltas. Long
                // enough that `select!`'s random branch pick almost certainly
                // catches the already-armed hard cancel before the script
                // runs dry on its own (same reasoning as
                // `hard_interrupt_completes_as_an_immediate_cancel` above).
                let mut events = vec![crate::llm::StreamEvent::TextStart];
                events.extend(
                    (0..64).map(|_| crate::llm::StreamEvent::TextDelta("C".into())),
                );

                let (_documents, ctx, _player) = drive_turn_with(
                    TurnOrigin::Autonomous,
                    kernel.clone(),
                    Provider::Mock(
                        MockClient::new("unused")
                            .with_scripted_stream(vec![events])
                            .hangs_when_exhausted(),
                    ),
                    |interrupt| interrupt.hard(),
                )
                .await;

                let msg = completed.try_recv().expect(
                    "a hard-cancelled turn with a hung post-cancel drain must still \
                     publish Completed, not silently hang or fail",
                );
                match msg.payload {
                    TurnFlow::Completed {
                        context_id, reason, ..
                    } => {
                        assert_eq!(context_id, ctx);
                        assert_eq!(
                            reason,
                            TurnStopReason::Cancelled { immediate: true },
                            "the hung post-cancel drain must not downgrade the outcome \
                             away from the cancel that was actually requested"
                        );
                    }
                    other => panic!("expected Completed, got {other:?}"),
                }
                assert!(
                    failed.try_recv().is_none(),
                    "a hung post-cancel drain must never publish Failed — the \
                     cancel-is-not-failure contract this file's other test pins for \
                     the clean-confirmation case applies just as much here"
                );
            })
            .await;
    }

    /// One truncated inference in the middle of a turn, exactly as the
    /// benchmark recorded it: reasoning spent the whole output ceiling, the
    /// provider stopped with `length`, and nothing had run yet. The turn must
    /// carry on — a driven worker has nobody to say "continue" — and the model
    /// must be told why its own output stops mid-thought.
    #[tokio::test]
    async fn output_ceiling_continues_the_turn_with_a_notice() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("ceiling-continue").await);
                let mut completed = kernel.turn_flows().subscribe("turn.completed");

                let truncated = vec![
                    crate::llm::StreamEvent::TextStart,
                    crate::llm::StreamEvent::TextDelta("X:1\nK:C\nCD".into()),
                    crate::llm::StreamEvent::TextEnd,
                    crate::llm::StreamEvent::Done {
                        stop_reason: Some("length".into()),
                        input_tokens: Some(10),
                        output_tokens: Some(1024),
                        extra: None,
                    },
                ];
                let tool = vec![
                    crate::llm::StreamEvent::ToolUse {
                        id: "call_after_notice".into(),
                        name: "nonexistent_tool".into(),
                        input: serde_json::json!({}),
                    },
                    crate::llm::StreamEvent::Done {
                        stop_reason: Some("tool_use".into()),
                        input_tokens: Some(20),
                        output_tokens: Some(5),
                        extra: None,
                    },
                ];
                let finish = vec![
                    crate::llm::StreamEvent::TextStart,
                    crate::llm::StreamEvent::TextDelta("EFGA|".into()),
                    crate::llm::StreamEvent::TextEnd,
                    crate::llm::StreamEvent::Done {
                        stop_reason: Some("end_turn".into()),
                        input_tokens: Some(30),
                        output_tokens: Some(5),
                        extra: None,
                    },
                ];

                let (documents, ctx, _player) = drive_turn_with(
                    TurnOrigin::Interactive,
                    kernel.clone(),
                    Provider::Mock(
                        MockClient::new("unused")
                            .with_scripted_stream(vec![truncated, tool, finish]),
                    ),
                    |_| {},
                )
                .await;

                let msg = completed.try_recv().expect("the turn completes once");
                match msg.payload {
                    TurnFlow::Completed {
                        context_id, reason, ..
                    } => {
                        assert_eq!(context_id, ctx);
                        assert_eq!(
                            reason,
                            TurnStopReason::EndTurn,
                            "the turn ended where the model ended it, not at the ceiling"
                        );
                    }
                    other => panic!("expected Completed, got {other:?}"),
                }

                let blocks = documents.block_snapshots(ctx).unwrap();
                let notices: Vec<_> = blocks
                    .iter()
                    .filter(|b| b.kind == BlockKind::Notification)
                    .collect();
                assert_eq!(notices.len(), 1, "one ceiling stop, one notice");
                assert_eq!(notices[0].role, Role::System, "a kernel fact, not model text");
                assert!(
                    notices[0]
                        .content
                        .contains("stopped at the output limit of 1024 tokens"),
                    "{}",
                    notices[0].content
                );
                assert!(
                    notices[0].content.contains("Continue from where you stopped"),
                    "{}",
                    notices[0].content
                );
                assert!(
                    blocks.iter().any(|b| b.content.contains("EFGA|")),
                    "the continuation's output is in the context"
                );
            })
            .await;
    }

    /// The continuation budget is bounded. A model that spends every ceiling on
    /// reasoning would otherwise burn `max_tokens` per inference until the
    /// iteration cap, so the turn gives up after
    /// `MAX_OUTPUT_CEILING_CONTINUATIONS` notices and reports the provider's own
    /// reason — a turn cut off at the output ceiling must not read as one the
    /// model chose to end.
    #[tokio::test]
    async fn repeated_output_ceilings_end_the_turn_with_max_tokens() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("max-tokens").await);
                let mut completed = kernel.turn_flows().subscribe("turn.completed");

                let truncated = || {
                    vec![
                        crate::llm::StreamEvent::TextStart,
                        crate::llm::StreamEvent::TextDelta("X:1\nK:C\nCD".into()),
                        crate::llm::StreamEvent::TextEnd,
                        crate::llm::StreamEvent::Done {
                            stop_reason: Some("max_tokens".into()),
                            input_tokens: Some(10),
                            output_tokens: Some(1024),
                            extra: None,
                        },
                    ]
                };
                let script: Vec<_> = (0..MAX_OUTPUT_CEILING_CONTINUATIONS + 1)
                    .map(|_| truncated())
                    .collect();

                let (documents, ctx, _player) = drive_turn_with(
                    TurnOrigin::Interactive,
                    kernel.clone(),
                    Provider::Mock(MockClient::new("unused").with_scripted_stream(script)),
                    |_| {},
                )
                .await;

                let msg = completed.try_recv().expect("a truncated turn still completes");
                match msg.payload {
                    TurnFlow::Completed {
                        context_id, reason, ..
                    } => {
                        assert_eq!(context_id, ctx);
                        assert_eq!(reason, TurnStopReason::MaxTokens);
                    }
                    other => panic!("expected Completed, got {other:?}"),
                }

                let notices = documents
                    .block_snapshots(ctx)
                    .unwrap()
                    .iter()
                    .filter(|b| b.kind == BlockKind::Notification)
                    .count();
                assert_eq!(
                    notices, 3,
                    "three notices, then the fourth ceiling stop ends the turn"
                );
            })
            .await;
    }

    /// A pending soft interrupt ends the turn at a ceiling stop, and leaves no
    /// notice behind: a durable "continue from where you stopped" for an
    /// inference that never ran would instruct the *next* turn instead. The
    /// script holds one inference, so a continuation would also panic the mock.
    #[tokio::test(start_paused = true)]
    async fn an_interrupt_at_a_ceiling_stop_leaves_no_notice() {
        use std::time::Duration;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("ceiling-interrupt").await);
                let mut completed = kernel.turn_flows().subscribe("turn.completed");
                let events = vec![
                    crate::llm::StreamEvent::TextStart,
                    crate::llm::StreamEvent::TextDelta("half a plan".into()),
                    crate::llm::StreamEvent::TextEnd,
                    crate::llm::StreamEvent::Done {
                        stop_reason: Some("length".into()),
                        input_tokens: Some(10),
                        output_tokens: Some(1024),
                        extra: None,
                    },
                ];
                let provider = Provider::Mock(
                    MockClient::new("unused")
                        .with_scripted_stream(vec![events])
                        .with_event_delay(Duration::from_millis(10)),
                );
                let (documents, ctx, _player) = drive_turn_with(
                    TurnOrigin::Interactive,
                    kernel.clone(),
                    provider,
                    |interrupt| {
                        // Land the request while the first inference streams,
                        // after the loop's own soft-interrupt check has passed.
                        let interrupt = interrupt.clone();
                        tokio::task::spawn_local(async move {
                            tokio::time::sleep(Duration::from_millis(25)).await;
                            interrupt.soft();
                        });
                    },
                )
                .await;

                match completed.try_recv().expect("the turn completes").payload {
                    TurnFlow::Completed { reason, .. } => assert_eq!(
                        reason,
                        TurnStopReason::Cancelled { immediate: false },
                        "the interrupt names the ending it caused"
                    ),
                    other => panic!("expected Completed, got {other:?}"),
                }
                assert!(
                    !documents
                        .block_snapshots(ctx)
                        .unwrap()
                        .iter()
                        .any(|b| b.kind == BlockKind::Notification),
                    "no notice for an inference that never ran"
                );
            })
            .await;
    }

    /// What the provider is handed on the continuation, for each shape a
    /// truncated response can take. A lone Reasoning block is not an assistant
    /// turn and neither is an empty one; the hydrator drops both
    /// (`llm/hydrate.rs`, `flush_assistant`), so sending either live would put
    /// a message on the wire that no later hydration could reproduce — and
    /// reasoning-only is the shape the benchmark actually produced.
    #[tokio::test]
    async fn the_continuation_request_carries_no_reasoning_only_or_empty_assistant() {
        use crate::llm::{MessageContent, Role as LlmRole};
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let thinking = || {
                    vec![
                        crate::llm::StreamEvent::ThinkingStart,
                        crate::llm::StreamEvent::ThinkingDelta("long reasoning".into()),
                        crate::llm::StreamEvent::ThinkingEnd {
                            signature: Some("signed".into()),
                        },
                    ]
                };
                let text = || {
                    vec![
                        crate::llm::StreamEvent::TextStart,
                        crate::llm::StreamEvent::TextDelta("half a plan".into()),
                        crate::llm::StreamEvent::TextEnd,
                    ]
                };
                let cases: Vec<(&str, Vec<crate::llm::StreamEvent>, bool)> = vec![
                    ("text only", text(), true),
                    ("text and signed reasoning", [thinking(), text()].concat(), true),
                    ("signed reasoning only", thinking(), false),
                    ("nothing at all", Vec::new(), false),
                ];

                for (shape, prefix, expect_assistant) in cases {
                    let kernel = Arc::new(Kernel::new_ephemeral("continuation-shape").await);
                    let mut truncated = prefix;
                    truncated.push(crate::llm::StreamEvent::Done {
                        stop_reason: Some("length".into()),
                        input_tokens: Some(10),
                        output_tokens: Some(1024),
                        extra: None,
                    });
                    let finish = vec![
                        crate::llm::StreamEvent::TextStart,
                        crate::llm::StreamEvent::TextDelta("finished".into()),
                        crate::llm::StreamEvent::TextEnd,
                        crate::llm::StreamEvent::Done {
                            stop_reason: Some("end_turn".into()),
                            input_tokens: Some(20),
                            output_tokens: Some(5),
                            extra: None,
                        },
                    ];
                    let (mock, sent) = MockClient::new("unused")
                        .with_scripted_stream(vec![truncated, finish])
                        .recording_sent_messages();
                    let (documents, ctx, _player) = drive_turn_with(
                        TurnOrigin::Interactive,
                        kernel.clone(),
                        Provider::Mock(mock),
                        |_| {},
                    )
                    .await;

                    let sent = sent.lock();
                    assert_eq!(sent.len(), 2, "{shape}: the turn continued once");
                    let request = &sent[1];
                    for (index, message) in request.iter().enumerate() {
                        if message.role != LlmRole::Assistant {
                            continue;
                        }
                        match &message.content {
                            MessageContent::Text(text) => {
                                assert!(!text.trim().is_empty(), "{shape}: empty assistant text")
                            }
                            MessageContent::Blocks(blocks) => {
                                assert!(
                                    !blocks.is_empty(),
                                    "{shape}: assistant message {index} has no content"
                                );
                                assert!(
                                    blocks.iter().any(|block| !matches!(
                                        block,
                                        crate::llm::ContentBlock::Reasoning { .. }
                                    )),
                                    "{shape}: assistant message {index} is reasoning only"
                                );
                                assert!(
                                    !blocks.iter().any(|block| matches!(
                                        block,
                                        crate::llm::ContentBlock::Text { text } if text.trim().is_empty()
                                    )),
                                    "{shape}: assistant message {index} carries empty text"
                                );
                            }
                        }
                    }
                    assert_eq!(
                        request.iter().any(|m| m.role == LlmRole::Assistant),
                        expect_assistant,
                        "{shape}: assistant replay"
                    );
                    let last = request.last().expect("the notice is the last message");
                    assert_eq!(last.role, LlmRole::User);
                    assert!(
                        last.as_text()
                            .unwrap_or_default()
                            .contains("stopped at the output limit"),
                        "{shape}: {last:?}"
                    );

                    // Live and rehydrated must serialize the same way: the
                    // next turn rebuilds this conversation from the block log.
                    let blocks = documents.block_snapshots(ctx).unwrap();
                    let rehydrated = crate::llm::hydrate_from_blocks(&blocks);
                    let live = serde_json::to_value(request).unwrap();
                    let replayed =
                        serde_json::to_value(&rehydrated[..request.len()]).unwrap();
                    assert_eq!(live, replayed, "{shape}: live and rehydrated differ");
                }
            })
            .await;
    }

    /// The raw arguments the benchmark recorded: a `write` call whose JSON was
    /// cut off mid-string. The turn must not fail — the model only learns its
    /// call was cut off if it gets a tool result saying so.
    const TRUNCATED_ARGUMENTS: &str = "{\"path\": \"/app/solve.py\", \"content\": \"import re";

    fn truncated_write_call() -> crate::llm::StreamEvent {
        crate::llm::StreamEvent::ToolUseInvalid {
            id: "call_truncated".into(),
            name: "write".into(),
            arguments: TRUNCATED_ARGUMENTS.into(),
            error: "EOF while parsing a string at line 1 column 7174".into(),
        }
    }

    #[tokio::test]
    async fn truncated_tool_arguments_answer_the_model_and_keep_the_turn() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("truncated-arguments").await);
                let mut completed = kernel.turn_flows().subscribe("turn.completed");
                let mut failed = kernel.turn_flows().subscribe("turn.failed");

                let cut_off = vec![
                    truncated_write_call(),
                    crate::llm::StreamEvent::Done {
                        stop_reason: Some("tool_use".into()),
                        input_tokens: Some(10),
                        output_tokens: Some(1024),
                        extra: None,
                    },
                ];
                let retry = vec![
                    crate::llm::StreamEvent::TextStart,
                    crate::llm::StreamEvent::TextDelta("writing it in pieces".into()),
                    crate::llm::StreamEvent::TextEnd,
                    crate::llm::StreamEvent::Done {
                        stop_reason: Some("end_turn".into()),
                        input_tokens: Some(20),
                        output_tokens: Some(5),
                        extra: None,
                    },
                ];

                let (documents, ctx, _player) = drive_turn_with(
                    TurnOrigin::Interactive,
                    kernel.clone(),
                    Provider::Mock(
                        MockClient::new("unused").with_scripted_stream(vec![cut_off, retry]),
                    ),
                    |_| {},
                )
                .await;

                assert!(failed.try_recv().is_none(), "a cut-off call is not a failed turn");
                match completed.try_recv().expect("the turn completes").payload {
                    TurnFlow::Completed { reason, .. } => {
                        assert_eq!(reason, TurnStopReason::EndTurn)
                    }
                    other => panic!("expected Completed, got {other:?}"),
                }

                let blocks = documents.block_snapshots(ctx).unwrap();
                let call = blocks
                    .iter()
                    .find(|b| b.kind == BlockKind::ToolCall)
                    .expect("the call the model made is recorded");
                assert_eq!(call.tool_name.as_deref(), Some("write"));
                assert_eq!(
                    call.status,
                    Status::Error,
                    "the call block settles like any other answered call"
                );
                assert!(
                    call.tool_input.as_deref().unwrap_or_default().contains("import re"),
                    "the raw arguments are preserved: {:?}",
                    call.tool_input
                );
                let result = blocks
                    .iter()
                    .find(|b| b.kind == BlockKind::ToolResult)
                    .expect("the call is answered");
                assert!(result.is_error, "the model must see this as an error");
                assert!(
                    result.content.contains("not valid JSON")
                        && result.content.contains("47 bytes")
                        && result.content.contains("line 1 column 7174"),
                    "{}",
                    result.content
                );
                assert!(
                    result.content.contains("smaller pieces"),
                    "the result says what to do next: {}",
                    result.content
                );
                assert!(
                    blocks.iter().any(|b| b.content.contains("writing it in pieces")),
                    "the loop went on to another inference"
                );
            })
            .await;
    }

    /// A cut-off call and a `length` stop are one event, not two. The tool
    /// result already says the output limit ended the call, so the ceiling
    /// notice must not repeat it.
    #[tokio::test]
    async fn a_cut_off_call_with_a_length_stop_sends_one_message() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("one-message").await);
                let mut completed = kernel.turn_flows().subscribe("turn.completed");

                let cut_off = vec![
                    truncated_write_call(),
                    crate::llm::StreamEvent::Done {
                        stop_reason: Some("length".into()),
                        input_tokens: Some(10),
                        output_tokens: Some(1024),
                        extra: None,
                    },
                ];
                let retry = vec![
                    crate::llm::StreamEvent::TextStart,
                    crate::llm::StreamEvent::TextDelta("writing it in pieces".into()),
                    crate::llm::StreamEvent::TextEnd,
                    crate::llm::StreamEvent::Done {
                        stop_reason: Some("end_turn".into()),
                        input_tokens: Some(20),
                        output_tokens: Some(5),
                        extra: None,
                    },
                ];

                let (documents, ctx, _player) = drive_turn_with(
                    TurnOrigin::Interactive,
                    kernel.clone(),
                    Provider::Mock(
                        MockClient::new("unused").with_scripted_stream(vec![cut_off, retry]),
                    ),
                    |_| {},
                )
                .await;

                match completed.try_recv().expect("the turn completes").payload {
                    TurnFlow::Completed { reason, .. } => {
                        assert_eq!(reason, TurnStopReason::EndTurn)
                    }
                    other => panic!("expected Completed, got {other:?}"),
                }

                let blocks = documents.block_snapshots(ctx).unwrap();
                assert!(
                    !blocks.iter().any(|b| b.kind == BlockKind::Notification),
                    "the tool result is the one message about the cut-off"
                );
                assert_eq!(
                    blocks.iter().filter(|b| b.kind == BlockKind::ToolResult).count(),
                    1,
                    "one call, one answer"
                );
            })
            .await;
    }

    /// A hydration failure on an ANNOUNCED turn must still publish exactly one
    /// terminal event (`Failed`). The early `return` on `Err(())` otherwise leaves
    /// the OODA Act handoff with NO signal — silently dropping the turn off the
    /// design's "exactly one terminal event per announced turn" contract (§7),
    /// while the two stream-error paths and the clean tail all announce. An
    /// un-announced (interactive) turn stays silent even when hydration fails.
    async fn drive_failed_hydration(origin: TurnOrigin, kernel: Arc<Kernel>) -> ContextId {
        let bus: SharedBlockFlowBus = Arc::new(FlowBus::new(256));
        let documents: SharedBlockStore =
            Arc::new(BlockStore::with_flows(PrincipalId::new(), bus));
        // NO create_document for `ctx` → `block_snapshots` errors DocumentNotFound →
        // hydration fails before the stream begins. The anchor is fabricated; the
        // missing document is what we are exercising.
        let ctx = ContextId::new();
        let player = PrincipalId::new();
        let after = kaijutsu_types::BlockId::new(ctx, player, 0);

        let provider = Arc::new(Provider::Mock(MockClient::new("X:1\nK:C\nCDEF|\n")));
        let kernel_db = Arc::new(parking_lot::Mutex::new(KernelDb::temporary().unwrap()));
        let conversation_cache = Arc::new(ConversationCache::new(8));
        let interrupt = ContextInterruptState::new();
        let tool_ctx = crate::ExecContext::new(
            player,
            ctx,
            std::path::PathBuf::from("/"),
            SessionId::new(),
            kernel.id(),
        );

        let turn_lease = kernel.clone().turns().begin_with_interrupt(ctx, interrupt.clone());
        process_llm_stream(
            provider,
            documents,
            ctx,
            "mock-model".to_string(),
            kernel.clone(),
            kernel_db,
            vec![],
            after,
            "system".to_string(),
            1024,
            StreamTimeouts::from_policy(kernel.timeouts()),
            None,
            conversation_cache,
            player,
            None,
            tool_ctx,
            interrupt,
            turn_lease,
            origin,
            None,
        )
        .await;
        ctx
    }

    #[tokio::test]
    async fn failed_hydration_publishes_failed_when_announced() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("hydrate-fail").await);
                let mut failed = kernel.turn_flows().subscribe("turn.failed");
                let mut completed = kernel.turn_flows().subscribe("turn.completed");

                let ctx = drive_failed_hydration(TurnOrigin::Autonomous, kernel.clone()).await;

                let msg = failed
                    .try_recv()
                    .expect("an announced turn whose hydration fails must publish Failed");
                match msg.payload {
                    TurnFlow::Failed { context_id, .. } => assert_eq!(context_id, ctx),
                    other => panic!("expected Failed, got {other:?}"),
                }
                assert!(failed.try_recv().is_none(), "exactly one terminal event");
                assert!(
                    completed.try_recv().is_none(),
                    "a failed turn never publishes Completed"
                );
            })
            .await;
    }

    /// An interactive turn whose hydration fails publishes `Failed` too, tagged
    /// `Interactive`. It used to stay silent — which meant a client that had
    /// just submitted a prompt got no signal at all that its turn had died, and
    /// would wait on block status that was never going to change.
    #[tokio::test]
    async fn failed_hydration_announces_when_interactive() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("hydrate-fail").await);
                let mut failed = kernel.turn_flows().subscribe("turn.failed");

                let ctx = drive_failed_hydration(TurnOrigin::Interactive, kernel.clone()).await;

                let msg = failed
                    .try_recv()
                    .expect("a broken interactive turn must tell its client");
                match msg.payload {
                    TurnFlow::Failed {
                        context_id, origin, ..
                    } => {
                        assert_eq!(context_id, ctx);
                        assert_eq!(origin, TurnOrigin::Interactive);
                    }
                    other => panic!("expected Failed, got {other:?}"),
                }
                assert!(failed.try_recv().is_none(), "exactly one terminal event");
            })
            .await;
    }
}

/// Broker timeouts and interrupts use a paused clock for long-running calls.
#[cfg(test)]
mod tool_dispatch_timeout_tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use crate::mcp::{
        CallContext, ContextToolBinding, InstanceId, InstancePolicy, KernelCallParams,
        KernelTool, KernelToolResult, McpResult, McpServerLike, ServerNotification,
    };
    use kaijutsu_types::SessionId;

    use super::*;

    // ─────────────────────────────────────────────────────────────────
    // map_tool_dispatch_result — pure mapping, no I/O

    /// A failing tool that wrote a structured body must hand the model that
    /// body verbatim. Prefixing "Error: " onto the shell envelope made it
    /// unparseable and reported the failure twice — once in the prefix, once
    /// in the envelope's own `status`/`exit_code`.
    #[test]
    fn a_failing_tool_body_reaches_the_model_verbatim() {
        let envelope = kaijutsu_types::shell_envelope::ShellEnvelope {
            exit_code: Some(1),
            stderr: "boom".into(),
            ..kaijutsu_types::shell_envelope::ShellEnvelope::new(
                kaijutsu_types::shell_envelope::ShellStatus::Error,
            )
        };
        let body = envelope.to_value().to_string();
        let failed = crate::ExecResult {
            stdout: body.clone(),
            stderr: String::new(),
            exit_code: 1,
            success: false,
            output: None,
        };

        let out = map_tool_dispatch_result("shell", Ok(failed));
        assert!(out.is_error, "a nonzero exit is still an error");
        assert_eq!(out.content, body, "the body must not be editorialized");
        let parsed: serde_json::Value =
            serde_json::from_str(&out.content).expect("the model must receive parseable JSON");
        assert_eq!(parsed["exit_code"], 1);
        assert_eq!(parsed["status"], "error");
    }

    /// The ANSI regression this split closed: escape codes must not survive
    /// into either reader. Projecting the ENVELOPE finds nothing — its JSON
    /// spells an escape as the six characters `\u001b`, not a raw 0x1b — so
    /// the projection has to run on the command's real output, which is what
    /// step 4a extracts. Composed here exactly as the agentic loop composes
    /// it.
    #[test]
    fn ansi_escapes_survive_into_neither_the_block_nor_the_model() {
        let mut env = kaijutsu_types::shell_envelope::ShellEnvelope::new(
            kaijutsu_types::shell_envelope::ShellStatus::Done,
        );
        env.stdout = "\u{1b}[32mgreen\u{1b}[0m text".into();
        env.exit_code = Some(0);
        let tool_body = env.to_value().to_string();

        // Projecting the envelope directly is the no-op that made this a bug.
        assert!(
            crate::ansi_ingest::project(tool_body.as_bytes()).is_none(),
            "the envelope's JSON has no raw ESC to find — this is why 4a exists"
        );

        // Step 4a → 4b, as the loop does it.
        let recovered = ShellEnvelope::from_tool_result(&tool_body).expect("a shell envelope");
        let block_source = recovered.readable_output();
        let ansi = crate::ansi_ingest::project(block_source.as_bytes());
        let block_content = match ansi {
            Some(ref p) => p.text.clone(),
            None => block_source,
        };
        let model_content = recovered.with_clean_output(&block_content).to_value().to_string();

        assert_eq!(block_content, "green text", "the block holds clean output");
        assert!(
            !block_content.contains('\u{1b}'),
            "no raw escapes in the durable block"
        );
        assert!(
            !model_content.contains("\\u001b") && !model_content.contains('\u{1b}'),
            "no escapes, raw or spelled, reach the model: {model_content}"
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&model_content).expect("the model still gets an envelope");
        assert_eq!(parsed["stdout"], "green text");
        assert_eq!(parsed["exit_code"], 0);
    }

    /// A tool that is NOT the shell keeps one text for both readers — the
    /// split must not change anything for the rest of the tool surface.
    #[test]
    fn a_non_shell_tool_body_reaches_the_block_verbatim() {
        let body = "some ordinary tool output";
        assert!(
            ShellEnvelope::from_tool_result(body).is_none(),
            "ordinary output is not an envelope, so block and model stay one text"
        );
    }

    /// The "Error: " prefix still exists for a tool that failed with nothing
    /// but stderr to show — removing it there would leave a bare message with
    /// no indication it was a failure.
    #[test]
    fn a_failing_tool_with_only_stderr_still_gets_the_prefix() {
        let out = map_tool_dispatch_result(
            "something",
            Ok(crate::ExecResult::failure(1, "it broke")),
        );
        assert!(out.is_error);
        assert_eq!(out.content, "Error: it broke");
    }

    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn map_success_passes_stdout_through() {
        let ToolDispatch { content, is_error, payload, .. } =
            map_tool_dispatch_result("echo", Ok(crate::ExecResult::success("hi")));
        assert_eq!(content, "hi");
        assert!(!is_error);
        assert!(payload.is_none());
    }

    #[test]
    fn map_tool_failure_sets_generic_error_no_code() {
        let ToolDispatch { content, is_error, payload, .. } = map_tool_dispatch_result(
            "grep",
            Ok(crate::ExecResult::failure(1, "no matches")),
        );
        assert!(is_error);
        assert_eq!(content, "Error: no matches");
        let payload = payload.expect("failure carries a payload");
        assert!(payload.code.is_none(), "non-timeout tool failure has no `code`");
    }

    #[test]
    fn map_generic_mcp_error_sets_no_code() {
        let err = McpError::ToolNotFound {
            instance: InstanceId::new("x"),
            tool: "y".to_string(),
        };
        let ToolDispatch { content, is_error, payload, .. } = map_tool_dispatch_result("y", Err(err));
        assert!(is_error);
        assert!(content.starts_with("Execution error:"));
        let payload = payload.expect("execution error carries a payload");
        assert!(
            payload.code.is_none(),
            "a non-timeout McpError must not be mistaken for tool.timeout"
        );
    }

    #[test]
    fn map_policy_timeout_sets_tool_timeout_code_with_real_timeout_ms() {
        // The core of the Problem 1 fix: a broker policy timeout must map
        // to the SAME `code: "tool.timeout"` shape the old hardcoded-120s
        // outer wrapper produced, but using the REAL timeout_ms from the
        // broker's error, not a hardcoded number.
        let err = McpError::Policy(PolicyError::Timeout {
            instance: InstanceId::new("builtin.shell"),
            timeout_ms: 4242,
        });
        let ToolDispatch { content, is_error, payload, .. } = map_tool_dispatch_result("shell", Err(err));
        assert!(is_error);
        assert!(
            content.contains("shell") && content.contains("timed out"),
            "got: {content:?}"
        );
        assert!(
            content.contains("4.2"),
            "message should reflect the real 4242ms timeout, not a hardcoded 120s: {content:?}"
        );
        let payload = payload.expect("timeout carries a payload");
        assert_eq!(
            payload.code.as_deref(),
            Some("tool.timeout"),
            "must keep the same code so existing consumers matching on it keep working"
        );
        assert!(
            payload
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("4.2")),
            "detail should carry the real timeout_ms-derived duration: {payload:?}"
        );
    }

    /// A gate refusal reaching the model's own tool path is a verdict, and
    /// the three kinds settle their blocks differently. Before this, every
    /// refusal fell into the generic arm: content prefixed "Execution
    /// error", no code, and `Status::Error` derived from `is_error` — so a
    /// pending ask read as a failed tool call to the model, to the context
    /// list, and to ACP.
    ///
    /// Falsified by deriving `status` from `is_error` again, or by letting
    /// a refusal fall through to the generic arm.
    #[test]
    fn a_pending_gate_stays_loud_but_does_not_settle_as_a_failure() {
        let err = McpError::refused_gate(
            kaijutsu_types::RefusalKind::Pending,
            "shell_write",
            Some(kaijutsu_types::AskRef {
                request_id: "01a05d19-0000-7000-8000-000000000000".to_string(),
                status: kaijutsu_types::AskStatus::Pending,
            }),
            "nothing was run",
        );
        let out = map_tool_dispatch_result("shell_write", Err(err));

        assert!(out.is_error, "a refusal is never a quiet success");
        assert_eq!(
            out.status,
            Status::Waiting,
            "an unanswered question is not a refusal; the blocks must not say it was",
        );
        assert!(
            !out.content.contains("Execution error"),
            "a verdict is not a crash: {}",
            out.content
        );
        assert!(
            out.content.contains("01a05d19-0000-7000-8000-000000000000"),
            "the model must be handed the ask id: {}",
            out.content
        );
    }

    /// A pending ask is not an error: the `ToolResult` block already carries
    /// the refusal's text via `content`, and nothing outside this file reads
    /// a `gate.pending` error child. The fill touches only the linked pair,
    /// never a sibling error child, so one authored here would stay in the
    /// log saying nothing ran after the pair carries the real output.
    ///
    /// `is_error` still reports true (a refusal is loud, always); only the
    /// block-store payload is withheld. Falsified by authoring a payload
    /// for a `Waiting` dispatch again.
    #[test]
    fn a_pending_gate_carries_no_error_payload_to_author() {
        let err = McpError::refused_gate(
            kaijutsu_types::RefusalKind::Pending,
            "shell_write",
            Some(kaijutsu_types::AskRef {
                request_id: "01a05d19-0000-7000-8000-000000000000".to_string(),
                status: kaijutsu_types::AskStatus::Pending,
            }),
            "nothing was run",
        );
        let out = map_tool_dispatch_result("shell_write", Err(err));

        assert_eq!(out.status, Status::Waiting);
        assert!(out.is_error, "a refusal is still loud on the D-28 channel");
        assert!(
            out.payload.is_none(),
            "a Waiting dispatch must not carry an error payload to author; got: {:?}",
            out.payload
        );
    }

    /// The neighbouring kinds settle `Error` — nothing is coming back for a
    /// denial or a broken control, and a block left `Waiting` would name a
    /// question no one is composing.
    #[test]
    fn a_denial_and_a_broken_gate_settle_as_errors() {
        for (kind, code) in [
            (kaijutsu_types::RefusalKind::Denied, "gate.denied"),
            (kaijutsu_types::RefusalKind::GateUnavailable, "gate.unavailable"),
        ] {
            let err = McpError::refused_gate(kind, "shell_write", None, "no");
            let out = map_tool_dispatch_result("shell_write", Err(err));
            assert!(out.is_error);
            assert_eq!(out.status, Status::Error, "{kind} has no answer coming");
            assert_eq!(out.payload.expect("payload").code.as_deref(), Some(code));
        }
    }

    #[test]
    fn map_other_policy_errors_are_not_mistaken_for_timeout() {
        // Only PolicyError::Timeout gets the tool.timeout code — a
        // concurrency-cap or result-too-large policy error must still fall
        // through to the generic path (no `code`), same as before.
        let err = McpError::Policy(PolicyError::ConcurrencyCap {
            instance: InstanceId::new("builtin.shell"),
            max: 4,
        });
        let ToolDispatch { is_error, payload, .. } = map_tool_dispatch_result("shell", Err(err));
        assert!(is_error);
        let payload = payload.expect("payload present");
        assert!(payload.code.is_none());
    }

    // ─────────────────────────────────────────────────────────────────
    // dispatch_and_map_tool_result — real kernel + broker, paused clock
    // ─────────────────────────────────────────────────────────────────

    /// A single-tool `McpServerLike` that sleeps for a fixed duration then
    /// returns success — stands in for a slow shell command / cargo build.
    struct SleepyServer {
        id: InstanceId,
        sleep_for: Duration,
        notif_tx: broadcast::Sender<ServerNotification>,
    }

    impl SleepyServer {
        fn new(id: &str, sleep_for: Duration) -> Self {
            let (notif_tx, _) = broadcast::channel(4);
            Self {
                id: InstanceId::new(id),
                sleep_for,
                notif_tx,
            }
        }
    }

    #[async_trait]
    impl McpServerLike for SleepyServer {
        fn instance_id(&self) -> &InstanceId {
            &self.id
        }

        async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
            Ok(vec![KernelTool {
                instance: self.id.clone(),
                name: "sleep".to_string(),
                description: None,
                input_schema: serde_json::json!({ "type": "object" }),
            }])
        }

        async fn call_tool(
            &self,
            _params: KernelCallParams,
            _ctx: &CallContext,
            _cancel: CancellationToken,
        ) -> McpResult<KernelToolResult> {
            tokio::time::sleep(self.sleep_for).await;
            Ok(KernelToolResult::text("done"))
        }

        fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
            self.notif_tx.subscribe()
        }
    }

    /// Build an ephemeral kernel with `napper`/`sleep` registered under the
    /// given `call_timeout`, bound into a fresh context. Returns
    /// `(kernel, ExecContext)` ready for `dispatch_and_map_tool_result`.
    async fn kernel_with_sleepy_tool(
        name: &str,
        sleep_for: Duration,
        call_timeout: Duration,
    ) -> (Arc<Kernel>, crate::ExecContext) {
        let kernel = Arc::new(Kernel::new_ephemeral(name).await);
        let ctx = ContextId::new();
        let server = Arc::new(SleepyServer::new("napper", sleep_for));
        kernel
            .broker()
            .register(
                server,
                InstancePolicy {
                    call_timeout,
                    max_result_bytes: 1024,
                    max_concurrency: 4,
                },
            )
            .await
            .unwrap();
        kernel
            .broker()
            .set_binding(
                ctx,
                ContextToolBinding::with_instances(vec![InstanceId::new("napper")]),
            )
            .await.unwrap();
        let tool_ctx = crate::ExecContext::new(
            PrincipalId::new(),
            ctx,
            PathBuf::from("/"),
            SessionId::new(),
            kernel.id(),
        );
        (kernel, tool_ctx)
    }

    #[tokio::test(start_paused = true)]
    async fn low_policy_timeout_produces_tool_timeout_result() {
        let (kernel, tool_ctx) =
            kernel_with_sleepy_tool("timeout-low", Duration::from_secs(5), Duration::from_millis(50))
                .await;

        let ToolDispatch { content, is_error, payload, .. } = dispatch_and_map_tool_result(
            &kernel,
            "sleep",
            "{}",
            &tool_ctx,
            CancellationToken::new(),
        )
        .await;

        assert!(is_error, "a call exceeding the low policy timeout must error");
        let payload = payload.expect("timeout carries a payload");
        assert_eq!(payload.code.as_deref(), Some("tool.timeout"));
        assert!(content.contains("timed out"), "got: {content:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn regression_call_exceeding_old_hardcoded_120s_succeeds_when_policy_allows_more() {
        // The regression guard for the removed `TOOL_TIMEOUT_SECS = 120`
        // const: with the per-instance policy timeout set ABOVE 120s, a
        // call that runs LONGER than 120s (but under the policy timeout)
        // must still succeed. Paused clock ⇒ this is instant, not a
        // 125-second test.
        let (kernel, tool_ctx) = kernel_with_sleepy_tool(
            "timeout-regress",
            Duration::from_secs(125),
            Duration::from_secs(130),
        )
        .await;

        let ToolDispatch { content, is_error, payload, .. } = dispatch_and_map_tool_result(
            &kernel,
            "sleep",
            "{}",
            &tool_ctx,
            CancellationToken::new(),
        )
        .await;

        assert!(
            !is_error,
            "a call under the configured 130s policy timeout must succeed even though it \
             exceeds the OLD hardcoded 120s wrapper — got error: {content:?} / {payload:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn update_policy_raising_timeout_above_120s_lets_the_call_through() {
        // Mirrors the real `kj policy set` path (`Broker::update_policy`,
        // the same mutation `kj policy set` performs): a timeout that
        // starts low is raised live, past the old hardcoded 120s ceiling,
        // and the very next call honors the new value.
        let (kernel, tool_ctx) = kernel_with_sleepy_tool(
            "timeout-update",
            Duration::from_secs(125),
            Duration::from_millis(50), // would fail outright if left as-is
        )
        .await;

        kernel
            .broker()
            .update_policy(&InstanceId::new("napper"), Some(Duration::from_secs(130)), None)
            .await
            .unwrap();

        let ToolDispatch { content, is_error, .. } = dispatch_and_map_tool_result(
            &kernel,
            "sleep",
            "{}",
            &tool_ctx,
            CancellationToken::new(),
        )
        .await;

        assert!(
            !is_error,
            "`kj policy set` raising call_timeout above 120s must actually take effect once \
             the redundant loop-level timeout is gone — got: {content:?}"
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // dispatch_inline_tool_result — the error child's anchor
    // ─────────────────────────────────────────────────────────────────

    /// A single-tool `McpServerLike` that always refuses — stands in for a
    /// denied gate without wiring up the hook machinery that normally
    /// produces one.
    struct RefusingServer {
        id: InstanceId,
        notif_tx: broadcast::Sender<ServerNotification>,
    }

    impl RefusingServer {
        fn new(id: &str) -> Self {
            let (notif_tx, _) = broadcast::channel(4);
            Self { id: InstanceId::new(id), notif_tx }
        }
    }

    #[async_trait]
    impl McpServerLike for RefusingServer {
        fn instance_id(&self) -> &InstanceId {
            &self.id
        }

        async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
            Ok(vec![KernelTool {
                instance: self.id.clone(),
                name: "denyme".to_string(),
                description: None,
                input_schema: serde_json::json!({ "type": "object" }),
            }])
        }

        async fn call_tool(
            &self,
            _params: KernelCallParams,
            _ctx: &CallContext,
            _cancel: CancellationToken,
        ) -> McpResult<KernelToolResult> {
            Err(McpError::refused_gate(
                kaijutsu_types::RefusalKind::Denied,
                "denyme",
                None,
                "not this time",
            ))
        }

        fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
            self.notif_tx.subscribe()
        }
    }

    /// Build an ephemeral kernel with `refuser`/`denyme` registered and bound
    /// into a fresh context that already holds one anchor block. Returns
    /// `(kernel, documents, context_id, tool_ctx, anchor)`.
    async fn kernel_with_refusing_tool(
        name: &str,
    ) -> (
        Arc<Kernel>,
        SharedBlockStore,
        ContextId,
        crate::ExecContext,
        kaijutsu_types::BlockId,
    ) {
        let kernel = Arc::new(Kernel::new_ephemeral(name).await);
        let ctx = ContextId::new();
        let documents = kernel.blocks().clone();
        documents
            .create_document(ctx, crate::DocumentKind::Conversation, None)
            .expect("create document");
        let anchor = documents
            .insert_block_as(
                ctx,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "run it",
                Status::Done,
                ContentType::Plain,
                Some(PrincipalId::new()),
            )
            .expect("insert anchor block");

        let server = Arc::new(RefusingServer::new("refuser"));
        kernel
            .broker()
            .register(
                server,
                InstancePolicy {
                    call_timeout: Duration::from_secs(5),
                    max_result_bytes: 1024,
                    max_concurrency: 4,
                },
            )
            .await
            .unwrap();
        kernel
            .broker()
            .set_binding(
                ctx,
                ContextToolBinding::with_instances(vec![InstanceId::new("refuser")]),
            )
            .await
            .unwrap();
        let tool_ctx = crate::ExecContext::new(
            PrincipalId::new(),
            ctx,
            PathBuf::from("/"),
            SessionId::new(),
            kernel.id(),
        );
        (kernel, documents, ctx, tool_ctx, anchor)
    }

    /// `insert_error_block_as` anchors the error child `after_id:
    /// Some(result_block_id)` — the child, not the result, is the tail of a
    /// denied tool's output. Before this, `dispatch_inline_tool_result` left
    /// `last_block_id` at the result, so the next block inserted after this
    /// call landed BETWEEN the result and its own error child. Document
    /// order must be call, result, error child, next.
    ///
    /// Falsified by anchoring `last_block_id` at `result_block_id` again.
    #[tokio::test]
    async fn a_denied_inline_tool_anchors_the_next_block_past_its_error_child() {
        let (kernel, documents, ctx, tool_ctx, anchor) =
            kernel_with_refusing_tool("inline-anchor").await;
        let mut last_block_id = anchor;

        dispatch_inline_tool_result(
            &documents,
            ctx,
            &mut last_block_id,
            &kernel,
            "denyme",
            serde_json::json!({}),
            &tool_ctx,
            CancellationToken::new(),
            "call-1",
            PrincipalId::new(),
            &kernel.turns().begin(ctx),
        )
        .await.unwrap();

        // Simulate the next iteration's block, anchored wherever this
        // dispatch left `last_block_id`.
        let next = documents
            .insert_block_as(
                ctx,
                None,
                Some(&last_block_id),
                Role::Model,
                BlockKind::Text,
                "continuing",
                Status::Done,
                ContentType::Plain,
                Some(PrincipalId::new()),
            )
            .expect("insert next block");

        let blocks = documents.block_snapshots(ctx).expect("read blocks");
        let call = blocks
            .iter()
            .find(|b| b.kind == BlockKind::ToolCall)
            .expect("tool call block must exist")
            .id;
        let result = blocks
            .iter()
            .find(|b| b.kind == BlockKind::ToolResult)
            .expect("tool result block must exist")
            .id;
        let error_child = blocks
            .iter()
            .find(|b| b.kind == BlockKind::Error)
            .expect("a denied refusal must author an error child")
            .id;

        let ordered: Vec<_> = blocks.iter().map(|b| b.id).collect();
        let pos = |id: &kaijutsu_types::BlockId| ordered.iter().position(|x| x == id).unwrap();
        assert!(
            pos(&call) < pos(&result)
                && pos(&result) < pos(&error_child)
                && pos(&error_child) < pos(&next),
            "expected document order call, result, error child, next; \
             got positions {:?}",
            [pos(&call), pos(&result), pos(&error_child), pos(&next)]
        );
    }
}

/// Pending refusals retain the ask identity needed for result settlement.
#[cfg(test)]
mod ask_link_tests {
    use super::*;

    fn pending_refusal(ask: &str) -> McpError {
        McpError::refused_gate(
            kaijutsu_types::RefusalKind::Pending,
            "shell_write",
            Some(kaijutsu_types::AskRef {
                request_id: ask.to_string(),
                status: kaijutsu_types::AskStatus::Pending,
            }),
            "nothing was run",
        )
    }

    /// The settle sites have nothing to hand the ledger if the mapping drops
    /// the ask id — the link would silently never write.
    #[test]
    fn a_pending_refusal_carries_its_ask_id_out_of_the_mapping() {
        let out = map_tool_dispatch_result("shell_write", Err(pending_refusal("01a05d19-ask")));
        assert_eq!(out.status, Status::Waiting, "a pending ask is not a failure");
        assert_eq!(
            out.ask_id.as_deref(),
            Some("01a05d19-ask"),
            "the ask id must reach the settle site that links the pair"
        );
    }

    /// A denial settles its blocks `Error`: there is nothing left holding the
    /// ask, so there is nothing to link.
    #[test]
    fn a_denial_carries_no_ask_to_link() {
        let denied = McpError::refused_gate(
            kaijutsu_types::RefusalKind::Denied,
            "shell_write",
            None,
            "not this time",
        );
        let out = map_tool_dispatch_result("shell_write", Err(denied));
        assert_eq!(out.status, Status::Error);
        assert_eq!(out.ask_id, None);
    }


}

#[cfg(test)]
mod usage_tests {
    //! Token-usage gauge coverage (`ContextUsageRow`, `kj context info
    //! --json`'s `usage` field). Covers:
    //! 1. a single completed call's usage lands on the context, with the
    //!    Claude cache-token normalization applied (`input_tokens` excludes
    //!    cache_read/cache_creation on the wire — the stored value must
    //!    include them back in, since they were still part of the prompt);
    //! 2. the OpenAI-compatible path's `prompt_tokens` is NOT double-counted
    //!    with its cache-hit split (unlike Claude, it's already inclusive);
    //! 3. a multi-iteration (tool-call) agentic turn persists only the FINAL
    //!    call's usage, never a sum across iterations — the multiply-counting
    //!    failure mode this feature exists to avoid (each call resends the
    //!    whole growing conversation, so summing double/triple/N-tuple counts
    //!    the same history);
    //! 4. a context well past the old 200-block auto-compaction threshold is
    //!    untouched by a turn — regression coverage for the deleted M1-A5
    //!    auto-compaction (`kj/compact.rs`, removed along with its only call
    //!    site in `spawn_llm_for_prompt`).
    use super::*;
    use crate::block_store::DocumentKind;
    use crate::kernel_db::KernelDb;
    use crate::llm::{
        ClaudeUsageExtra, MockClient, OpenAiCompatUsageExtra, Provider, UsageExtra,
    };
    use kaijutsu_types::SessionId;

    use crate::runtime::interrupt::ContextInterruptState;
    use crate::runtime::turn_state::ConversationCache;

    /// Build-and-drive helper: like `publish_tests::drive_one_turn` but takes
    /// an explicit `Provider` (so a test can script `StreamEvent`s via
    /// `MockClient::with_scripted_stream`) and hands back the `kernel_db`
    /// handle so a test can inspect the persisted `ContextUsageRow`.
    /// `pre_seed_blocks` inserts that many extra text blocks before the
    /// turn's own seed block — used by the large-history regression test.
    async fn drive_turn_with(
        provider: Provider,
        pre_seed_blocks: usize,
        kernel: Arc<Kernel>,
    ) -> (
        SharedBlockStore,
        ContextId,
        PrincipalId,
        Arc<parking_lot::Mutex<KernelDb>>,
    ) {
        drive_turn_with_breakpoints(provider, pre_seed_blocks, kernel, &[]).await
    }

    /// Like `drive_turn_with`, but seeds `breakpoints` as this context's
    /// `cache_breakpoints` before driving the turn — the TTL-selection tests
    /// need a real breakpoint row for `process_llm_stream` to read via
    /// `list_cache_breakpoints`, same as the rc-populated production path.
    async fn drive_turn_with_breakpoints(
        provider: Provider,
        pre_seed_blocks: usize,
        kernel: Arc<Kernel>,
        breakpoints: &[CacheTarget],
    ) -> (
        SharedBlockStore,
        ContextId,
        PrincipalId,
        Arc<parking_lot::Mutex<KernelDb>>,
    ) {
        let documents = kernel.blocks().clone();
        let ctx = ContextId::new();
        documents
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let player = PrincipalId::new();
        let actor = PrincipalId::new();
        let reviewer = PrincipalId::new();

        let mut last: Option<kaijutsu_types::BlockId> = None;
        for i in 0..pre_seed_blocks {
            let id = documents
                .insert_block_as(
                    ctx,
                    None,
                    last.as_ref(),
                    Role::User,
                    BlockKind::Text,
                    format!("history block {i}"),
                    Status::Done,
                    ContentType::Plain,
                    Some(player),
                )
                .unwrap();
            last = Some(id);
        }

        // The user/seed block the turn anchors after.
        let after = documents
            .insert_block_as(
                ctx,
                None,
                last.as_ref(),
                Role::User,
                BlockKind::Text,
                "write a phrase",
                Status::Done,
                ContentType::Plain,
                Some(player),
            )
            .unwrap();

        let provider = Arc::new(provider);
        let kernel_db = kernel.kernel_db().clone();
        // The journal created the document; usage also needs its context row.
        {
            let db = kernel_db.lock();
            db.insert_context(&crate::ContextRow {
                context_id: ctx,
                label: None,
                provider: None,
                model: None,
                system_prompt: None,
                consent_mode: ConsentMode::Collaborative,
                context_state: kaijutsu_types::ContextState::Live,
                context_type: "default".to_string(),
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: player,
                forked_from: None,
                fork_kind: None,
                archived_at: None,
                workspace_id: None,
                preset_id: None,
                concluded_at: None,
                last_activity_at: None,
                promoted_at: None,
                demoted_at: None,
                paused_at: None,
                cast_id: None,
                origin_host: None,
                played_by: None,
                reviewer_id: None,
                director_id: None,
            })
            .unwrap();
            for bp in breakpoints {
                db.add_cache_breakpoint(ctx, bp).unwrap();
            }
        }
        let conversation_cache = Arc::new(ConversationCache::new(8));
        let interrupt = ContextInterruptState::new();
        let tool_ctx = crate::ExecContext::new(
            player,
            ctx,
            std::path::PathBuf::from("/"),
            SessionId::new(),
            kernel.id(),
        )
        .with_actor(actor, Some(reviewer));

        let turn_lease = kernel.clone().turns().begin_with_interrupt(ctx, interrupt.clone());
        process_llm_stream(
            provider,
            documents.clone(),
            ctx,
            "mock-model".to_string(),
            kernel.clone(),
            kernel_db.clone(),
            vec![],
            after,
            "system".to_string(),
            1024,
            StreamTimeouts::from_policy(kernel.timeouts()),
            None,
            conversation_cache,
            player,
            Some(TurnSpanIdentity {
                performer: crate::CharacterIdentity {
                    principal_id: actor,
                    name: "Coder".to_string(),
                },
                reviewer: crate::CharacterIdentity {
                    principal_id: reviewer,
                    name: "Lead".to_string(),
                },
                director: Some(player),
                review_source: crate::approval_identity::ReviewSource::Walk,
            }),
            tool_ctx,
            interrupt,
            turn_lease,
            TurnOrigin::Autonomous,
            None,
        )
        .await;

        (documents, ctx, player, kernel_db)
    }

    #[tokio::test]
    async fn single_call_usage_lands_on_context_with_claude_cache_normalized() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("usage-single").await);
                let provider = Provider::Mock(MockClient::new("unused").with_scripted_stream(
                    vec![vec![
                        StreamEvent::TextStart,
                        StreamEvent::TextDelta("ok".into()),
                        StreamEvent::TextEnd,
                        StreamEvent::Done {
                            stop_reason: Some("end_turn".into()),
                            input_tokens: Some(42),
                            output_tokens: Some(7),
                            extra: Some(UsageExtra::Claude(ClaudeUsageExtra {
                                cache_read_input_tokens: 10,
                                cache_creation_input_tokens: 5,
                            })),
                        },
                    ]],
                ));

                let (_documents, ctx, _player, kernel_db) =
                    drive_turn_with(provider, 0, kernel.clone()).await;

                let usage = kernel_db
                    .lock()
                    .get_context_usage(ctx)
                    .unwrap()
                    .expect("a completed call must record usage");
                assert_eq!(
                    usage.input_tokens, 57,
                    "Claude's input_tokens excludes cache tokens — total prompt size is \
                     42 + cache_read(10) + cache_creation(5) = 57"
                );
                assert_eq!(usage.output_tokens, 7);
                assert_eq!(usage.cache_read_tokens, 10);
                assert_eq!(usage.cache_write_tokens, 5);
                assert_eq!(usage.provider, "mock");
                assert_eq!(usage.model, "mock-model");
                assert_eq!(
                    usage.cache_ttl_secs, 0,
                    "no cache breakpoints configured for this context — 0, not a guess"
                );
            })
            .await;
    }

    /// The TTL-selection path: a Claude-shaped `Done` with cache breakpoints
    /// of MIXED ttls configured on the context must record the LONGEST one,
    /// not the first, the last, or a sum.
    #[tokio::test]
    async fn claude_call_with_mixed_ttl_breakpoints_records_the_longest() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("usage-ttl-mixed").await);
                let provider = Provider::Mock(MockClient::new("unused").with_scripted_stream(
                    vec![vec![
                        StreamEvent::TextStart,
                        StreamEvent::TextDelta("ok".into()),
                        StreamEvent::TextEnd,
                        StreamEvent::Done {
                            stop_reason: Some("end_turn".into()),
                            input_tokens: Some(42),
                            output_tokens: Some(7),
                            extra: Some(UsageExtra::Claude(ClaudeUsageExtra {
                                cache_read_input_tokens: 10,
                                cache_creation_input_tokens: 5,
                            })),
                        },
                    ]],
                ));

                let (_documents, ctx, _player, kernel_db) = drive_turn_with_breakpoints(
                    provider,
                    0,
                    kernel.clone(),
                    &[
                        CacheTarget::Tools(crate::llm::CacheTtl::Ephemeral),
                        CacheTarget::System(crate::llm::CacheTtl::Extended),
                    ],
                )
                .await;

                let usage = kernel_db.lock().get_context_usage(ctx).unwrap().unwrap();
                assert_eq!(
                    usage.cache_ttl_secs, 3600,
                    "Extended (3600s) must win over Ephemeral (300s) when mixed"
                );
            })
            .await;
    }

    /// A provider path that never builds breakpoints (anything but Claude's
    /// `build()`) must record 0 even when the context has breakpoints
    /// configured — a breakpoint sitting unused in storage is not a TTL the
    /// request actually applied.
    #[tokio::test]
    async fn non_claude_call_with_breakpoints_configured_records_zero_ttl() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("usage-ttl-non-claude").await);
                let provider = Provider::Mock(MockClient::new("unused").with_scripted_stream(
                    vec![vec![
                        StreamEvent::TextStart,
                        StreamEvent::TextDelta("ok".into()),
                        StreamEvent::TextEnd,
                        StreamEvent::Done {
                            stop_reason: Some("end_turn".into()),
                            input_tokens: Some(100),
                            output_tokens: Some(20),
                            extra: Some(UsageExtra::OpenAiCompat(OpenAiCompatUsageExtra {
                                prompt_cache_hit_tokens: 80,
                                prompt_cache_miss_tokens: 20,
                                reasoning_tokens: 0,
                            })),
                        },
                    ]],
                ));

                let (_documents, ctx, _player, kernel_db) = drive_turn_with_breakpoints(
                    provider,
                    0,
                    kernel.clone(),
                    &[CacheTarget::Tools(crate::llm::CacheTtl::Extended)],
                )
                .await;

                let usage = kernel_db.lock().get_context_usage(ctx).unwrap().unwrap();
                assert_eq!(
                    usage.cache_ttl_secs, 0,
                    "DeepSeek/OpenAI-compat build() ignores cache_breakpoints — a \
                     configured breakpoint here is dead storage, not an applied TTL"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn openai_compat_prompt_tokens_already_include_cache_split() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("usage-openai").await);
                let provider = Provider::Mock(MockClient::new("unused").with_scripted_stream(
                    vec![vec![
                        StreamEvent::TextStart,
                        StreamEvent::TextDelta("ok".into()),
                        StreamEvent::TextEnd,
                        StreamEvent::Done {
                            stop_reason: Some("end_turn".into()),
                            input_tokens: Some(100),
                            output_tokens: Some(20),
                            extra: Some(UsageExtra::OpenAiCompat(OpenAiCompatUsageExtra {
                                prompt_cache_hit_tokens: 80,
                                prompt_cache_miss_tokens: 20,
                                reasoning_tokens: 3,
                            })),
                        },
                    ]],
                ));

                let (_documents, ctx, _player, kernel_db) =
                    drive_turn_with(provider, 0, kernel.clone()).await;

                let usage = kernel_db.lock().get_context_usage(ctx).unwrap().unwrap();
                assert_eq!(
                    usage.input_tokens, 100,
                    "prompt_tokens (100) already includes the hit(80)+miss(20) split — \
                     adding cache_read again would double-count to 180"
                );
                assert_eq!(usage.output_tokens, 20);
                assert_eq!(usage.cache_read_tokens, 80);
                assert_eq!(usage.reasoning_tokens, 3);
            })
            .await;
    }

    /// The critical accumulation-model test: a tool-call round-trip means TWO
    /// LLM calls inside one user turn. The persisted usage must reflect only
    /// the LAST call (150 in + 30 out), never the sum across iterations
    /// (250 + 50) — each call resends the whole growing conversation, so
    /// summing would multiply-count it.
    #[tokio::test]
    async fn multi_iteration_turn_keeps_only_final_call_usage() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("usage-multi").await);
                let provider = Provider::Mock(MockClient::new("unused").with_scripted_stream(
                    vec![
                        // Iteration 1: a tool call, forcing a second round-trip.
                        // The tool doesn't need to exist — dispatch against an
                        // unbound test context gracefully errors (ToolNotFound),
                        // which still produces a ToolResult and continues the
                        // loop, exactly like a real failed tool call would.
                        vec![
                            StreamEvent::ToolUse {
                                id: "call_1".into(),
                                name: "nonexistent_tool".into(),
                                input: serde_json::json!({}),
                            },
                            StreamEvent::Done {
                                stop_reason: Some("tool_use".into()),
                                input_tokens: Some(100),
                                output_tokens: Some(20),
                                extra: None,
                            },
                        ],
                        // Iteration 2: final text, no more tool calls — the loop
                        // stops here.
                        vec![
                            StreamEvent::TextStart,
                            StreamEvent::TextDelta("done".into()),
                            StreamEvent::TextEnd,
                            StreamEvent::Done {
                                stop_reason: Some("end_turn".into()),
                                input_tokens: Some(150),
                                output_tokens: Some(30),
                                extra: None,
                            },
                        ],
                    ],
                ));

                let (_documents, ctx, _player, kernel_db) =
                    drive_turn_with(provider, 0, kernel.clone()).await;

                let usage = kernel_db.lock().get_context_usage(ctx).unwrap().unwrap();
                assert_eq!(
                    (usage.input_tokens, usage.output_tokens),
                    (150, 30),
                    "must equal the LAST iteration's usage only — summing (100+150, 20+30) \
                     would multiply-count the resent history"
                );
            })
            .await;
    }

    /// A `Done` carrying NO token counts must NOT overwrite the context's
    /// good snapshot with zeros.
    ///
    /// "The provider never told us" and "the provider said zero" are
    /// different facts, and the persist is an unconditional upsert over the
    /// context's single row — so conflating them DESTROYS real data. The
    /// OpenAI-compatible path only learns usage from a final chunk that never
    /// arrives when a stream is cancelled first (`llm/openai/stream.rs` yields
    /// `None` for both counts), so before the guard a hard interrupt would
    /// silently reset the dock gauge from a true "234k/1M" to "0" and leave
    /// it lying until the next completed call.
    ///
    /// Scripts exactly that: a good first round-trip, then a second whose
    /// `Done` reports nothing.
    #[tokio::test(flavor = "current_thread")]
    async fn done_without_token_counts_does_not_clobber_a_good_snapshot() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("usage-nocounts").await);
                let provider = Provider::Mock(MockClient::new("unused").with_scripted_stream(
                    vec![
                        // Round-trip 1: a real, complete measurement.
                        vec![
                            StreamEvent::ToolUse {
                                id: "call_1".into(),
                                name: "nonexistent_tool".into(),
                                input: serde_json::json!({}),
                            },
                            StreamEvent::Done {
                                stop_reason: Some("tool_use".into()),
                                input_tokens: Some(234_000),
                                output_tokens: Some(1_200),
                                extra: None,
                            },
                        ],
                        // Round-trip 2: ends with no usage reported at all —
                        // the cancelled-OpenAI-stream shape.
                        vec![
                            StreamEvent::TextStart,
                            StreamEvent::TextDelta("interrupted".into()),
                            StreamEvent::TextEnd,
                            StreamEvent::Done {
                                stop_reason: None,
                                input_tokens: None,
                                output_tokens: None,
                                extra: None,
                            },
                        ],
                    ],
                ));

                let (_documents, ctx, _player, kernel_db) =
                    drive_turn_with(provider, 0, kernel.clone()).await;

                let usage = kernel_db.lock().get_context_usage(ctx).unwrap().unwrap();
                assert_eq!(
                    (usage.input_tokens, usage.output_tokens),
                    (234_000, 1_200),
                    "a usage-less Done must leave the previous snapshot intact — writing \
                     unwrap_or(0) here makes the gauge read 0 after any interrupted turn"
                );
            })
            .await;
    }

    /// Regression: M1-A5 auto-compaction (block-count threshold, LLM-summarize
    /// the older half, mark it superseded) was deleted along with its only
    /// call site (`spawn_llm_for_prompt`, one layer above `process_llm_stream`
    /// — too heavy a harness for a --lib test, per this file's own
    /// `publish_tests` note about the SSH e2e harness). The `compacted` block
    /// flag it used to set was itself removed later (2026-08-02) as inert
    /// residue — a block can no longer even represent that state. This proves
    /// the layer under it stays inert: a context with 250 pre-existing blocks
    /// (well past the old `DEFAULT_COMPACT_THRESHOLD = 200`) is untouched by
    /// a turn — no Drift summary appears — where the old
    /// `auto_compact_if_needed` call used to fire before every prompt.
    #[tokio::test]
    async fn large_history_is_never_auto_compacted() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("usage-no-compact").await);
                let provider = Provider::Mock(MockClient::new("X:1\nK:C\nCDEF|\n"));

                let (documents, ctx, _player, _kernel_db) =
                    drive_turn_with(provider, 250, kernel.clone()).await;

                let blocks = documents.block_snapshots(ctx).unwrap();
                assert!(
                    !blocks.iter().any(|b| b.kind == BlockKind::Drift),
                    "no Drift summary should appear — nothing collapses old history anymore"
                );
                // 250 history blocks + 1 turn seed + 1 model reply.
                assert_eq!(blocks.len(), 252);
            })
            .await;
    }

    /// The `llm.turn` span carries the turn's usage ONCE. Every `Done` used to
    /// call `Span::record`, and `tracing_subscriber`'s fmt layer APPENDS each
    /// record to a span's formatted fields instead of replacing it
    /// (`fmt/fmt_layer.rs`, `on_record` → `add_fields`), so by iteration 10
    /// every log line under the span carried ten copies of the usage block.
    /// Pins one record per usage field per turn, holding the final call's
    /// numbers — the same gauge `ContextUsageRow` keeps.
    ///
    /// The counting subscriber is the process-wide default, not a thread-local
    /// one: a callsite caches its `Interest` the first time any thread hits
    /// it, against that thread's default. Tests start together, so most of
    /// the kernel's callsites are first hit by a sibling test's thread; with
    /// a thread-local default they cache `never` and this thread's turn
    /// creates no spans at all. A global default is what those registrations
    /// consult. Spans are told apart by the thread that created them.
    #[tokio::test(flavor = "current_thread")]
    async fn turn_span_records_usage_once_with_the_final_call() {
        use std::sync::Mutex as StdMutex;
        use std::thread::ThreadId;
        use tracing::field::{Field, Visit};
        use tracing::span::{Attributes, Id, Record};
        use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
        use tracing_subscriber::registry::LookupSpan;

        #[derive(Default)]
        struct Seen {
            count: usize,
            last: Option<u64>,
            text: Option<String>,
        }
        #[derive(Default)]
        struct SpanSeen {
            thread: Option<ThreadId>,
            fields: HashMap<String, Seen>,
        }
        #[derive(Clone, Default)]
        struct RecordCounter(Arc<StdMutex<HashMap<u64, SpanSeen>>>);
        struct Count<'a>(&'a mut HashMap<String, Seen>);
        impl Visit for Count<'_> {
            fn record_u64(&mut self, field: &Field, value: u64) {
                let seen = self.0.entry(field.name().to_string()).or_default();
                seen.count += 1;
                seen.last = Some(value);
            }
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                let seen = self.0.entry(field.name().to_string()).or_default();
                seen.count += 1;
                seen.text = Some(format!("{value:?}"));
            }
            fn record_str(&mut self, field: &Field, value: &str) {
                let seen = self.0.entry(field.name().to_string()).or_default();
                seen.count += 1;
                seen.text = Some(value.to_string());
            }
        }
        impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for RecordCounter {
            fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
                if attrs.metadata().name() != "llm.turn" {
                    return;
                }
                let mut spans = self.0.lock().unwrap();
                let seen = spans.entry(id.into_u64()).or_default();
                seen.thread = Some(std::thread::current().id());
                attrs.record(&mut Count(&mut seen.fields));
            }
            fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
                let span = ctx.span(id).expect("a recorded span exists");
                if span.name() != "llm.turn" {
                    return;
                }
                let mut spans = self.0.lock().unwrap();
                values.record(&mut Count(&mut spans.entry(id.into_u64()).or_default().fields));
            }
        }

        let counter = RecordCounter::default();
        tracing::subscriber::set_global_default(
            tracing_subscriber::registry().with(counter.clone()),
        )
        .expect("this is the only global subscriber the kernel test binary installs");
        let here = std::thread::current().id();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("usage-span-once").await);
                // A callsite whose registration straddled the install above
                // may still hold the interest it computed against no
                // subscriber; recompute now that they have all landed.
                tracing::callsite::rebuild_interest_cache();
                let provider = Provider::Mock(MockClient::new("unused").with_scripted_stream(
                    vec![
                        vec![
                            StreamEvent::ToolUse {
                                id: "call_1".into(),
                                name: "nonexistent_tool".into(),
                                input: serde_json::json!({}),
                            },
                            StreamEvent::Done {
                                stop_reason: Some("tool_use".into()),
                                input_tokens: Some(100),
                                output_tokens: Some(20),
                                extra: None,
                            },
                        ],
                        vec![
                            StreamEvent::TextStart,
                            StreamEvent::TextDelta("done".into()),
                            StreamEvent::TextEnd,
                            StreamEvent::Done {
                                stop_reason: Some("end_turn".into()),
                                input_tokens: Some(150),
                                output_tokens: Some(30),
                                extra: None,
                            },
                        ],
                    ],
                ));
                drive_turn_with(provider, 0, kernel.clone()).await;
            })
            .await;

        let spans = counter.0.lock().unwrap();
        let mine: Vec<&SpanSeen> = spans.values().filter(|s| s.thread == Some(here)).collect();
        assert_eq!(
            mine.len(),
            1,
            "this thread drove one turn, so one llm.turn span was created here"
        );
        let seen = &mine[0].fields;
        for field in ["context.id", "principal.id", "actor.id", "reviewer.id", "director.id", "review.source"] {
            assert!(
                seen.get(field).and_then(|value| value.text.as_ref()).is_some(),
                "{field} was not recorded on llm.turn",
            );
        }
        assert_ne!(seen["principal.id"].text, seen["actor.id"].text);
        assert_ne!(seen["actor.id"].text, seen["reviewer.id"].text);
        assert_eq!(seen["actor.name"].text.as_deref(), Some("Coder"));
        assert_eq!(seen["reviewer.name"].text.as_deref(), Some("Lead"));
        assert_eq!(seen["review.source"].text.as_deref(), Some("walk"));
        for field in [
            "llm.usage.input_tokens",
            "llm.usage.output_tokens",
            "llm.usage.cache_read_tokens",
            "llm.usage.cache_write_tokens",
            "llm.usage.reasoning_tokens",
            "llm.response.stop_reason",
        ] {
            let s = seen
                .get(field)
                .unwrap_or_else(|| panic!("{field} was never recorded on llm.turn"));
            assert_eq!(
                s.count, 1,
                "{field} recorded {} times on one llm.turn span — the fmt layer appends \
                 every record, so a per-call record grows every log line",
                s.count
            );
        }
        assert_eq!(
            seen["llm.usage.input_tokens"].last,
            Some(150),
            "the span carries the final call's usage, not the first's"
        );
        assert_eq!(seen["llm.usage.output_tokens"].last, Some(30));
    }
}

#[cfg(test)]
mod error_child_anchor_tests {
    //! `insert_error_block_as` anchors its child `after_id: Some(rb_id)` —
    //! the child, not the `ToolResult` it hangs off, is the tail of a
    //! failed tool's output. The agentic loop's "Step 6b" used to return
    //! `rb_id` as the next iteration's anchor anyway, so the next model
    //! turn's blocks landed BETWEEN the result and its own error child.
    //! This drives a real two-iteration
    //! agentic turn through `process_llm_stream` end to end and pins
    //! document order across the whole loop, not just the anchor variable.
    use super::*;
    use crate::block_store::DocumentKind;
    use crate::llm::{MockClient, Provider};
    use kaijutsu_types::SessionId;

    use crate::runtime::interrupt::ContextInterruptState;
    use crate::runtime::turn_state::ConversationCache;

    /// Drive one `process_llm_stream` turn against a scripted Mock provider,
    /// same shape as `publish_tests::drive_turn_with` and
    /// `usage_tests::drive_turn_with`.
    async fn drive_turn_with(provider: Provider, kernel: Arc<Kernel>) -> (SharedBlockStore, ContextId) {
        let documents = kernel.blocks().clone();
        let ctx = ContextId::new();
        documents
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let player = PrincipalId::new();
        let after = documents
            .insert_block_as(
                ctx,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "run the tool",
                Status::Done,
                ContentType::Plain,
                Some(player),
            )
            .unwrap();

        let provider = Arc::new(provider);
        let kernel_db = kernel.kernel_db().clone();
        let conversation_cache = Arc::new(ConversationCache::new(8));
        let interrupt = ContextInterruptState::new();
        let tool_ctx = crate::ExecContext::new(
            player,
            ctx,
            std::path::PathBuf::from("/"),
            SessionId::new(),
            kernel.id(),
        );

        let turn_lease = kernel.clone().turns().begin_with_interrupt(ctx, interrupt.clone());
        process_llm_stream(
            provider,
            documents.clone(),
            ctx,
            "mock-model".to_string(),
            kernel.clone(),
            kernel_db,
            vec![],
            after,
            "system".to_string(),
            1024,
            StreamTimeouts::from_policy(kernel.timeouts()),
            None,
            conversation_cache,
            player,
            None,
            tool_ctx,
            interrupt,
            turn_lease,
            TurnOrigin::Autonomous,
            None,
        )
        .await;

        (documents, ctx)
    }

    /// A failed tool call (`nonexistent_tool` against an unbound context,
    /// same fixture `usage_tests::multi_iteration_turn_keeps_only_final_call_usage`
    /// uses) authors an Error child off its `ToolResult`. The second
    /// iteration's own text block must land AFTER that child, in document
    /// order: call, result, error child, text.
    ///
    /// Falsified by anchoring Step 6b's return at the result block instead
    /// of the error child it inserts.
    #[tokio::test]
    async fn a_failed_tool_calls_error_child_precedes_the_next_iterations_text() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("error-child-anchor").await);
                let provider = Provider::Mock(MockClient::new("unused").with_scripted_stream(vec![
                    // Iteration 1: a tool call that fails (unknown tool name),
                    // forcing a second round-trip.
                    vec![
                        StreamEvent::ToolUse {
                            id: "call_1".into(),
                            name: "nonexistent_tool".into(),
                            input: serde_json::json!({}),
                        },
                        StreamEvent::Done {
                            stop_reason: Some("tool_use".into()),
                            input_tokens: Some(10),
                            output_tokens: Some(5),
                            extra: None,
                        },
                    ],
                    // Iteration 2: final text, no more tool calls.
                    vec![
                        StreamEvent::TextStart,
                        StreamEvent::TextDelta("done".into()),
                        StreamEvent::TextEnd,
                        StreamEvent::Done {
                            stop_reason: Some("end_turn".into()),
                            input_tokens: Some(20),
                            output_tokens: Some(3),
                            extra: None,
                        },
                    ],
                ]));

                let (documents, ctx) = drive_turn_with(provider, kernel.clone()).await;

                let blocks = documents.block_snapshots(ctx).expect("read blocks");
                let call = blocks
                    .iter()
                    .find(|b| b.kind == BlockKind::ToolCall)
                    .expect("tool call block must exist")
                    .id;
                let result = blocks
                    .iter()
                    .find(|b| b.kind == BlockKind::ToolResult)
                    .expect("tool result block must exist")
                    .id;
                let error_child = blocks
                    .iter()
                    .find(|b| b.kind == BlockKind::Error)
                    .expect("the failed call must author an error child")
                    .id;
                let next_text = blocks
                    .iter()
                    .find(|b| b.kind == BlockKind::Text && b.content == "done")
                    .expect("iteration 2's text block must exist")
                    .id;

                let ordered: Vec<_> = blocks.iter().map(|b| b.id).collect();
                let pos = |id: &kaijutsu_types::BlockId| ordered.iter().position(|x| x == id).unwrap();
                assert!(
                    pos(&call) < pos(&result)
                        && pos(&result) < pos(&error_child)
                        && pos(&error_child) < pos(&next_text),
                    "expected document order call, result, error child, text; \
                     got positions {:?}",
                    [pos(&call), pos(&result), pos(&error_child), pos(&next_text)]
                );
            })
            .await;
    }
}

#[cfg(test)]
mod authorship_tests {
    //! Pins the PROVIDER-OUTPUT / KERNEL-OUTPUT block-provenance split drawn
    //! around `actor_principal` (see its declaration comment near the top of
    //! `process_llm_stream`). Two kinds of coverage:
    //!
    //! - a driven turn's real blocks carry the expected principal on each
    //!   category — regression coverage against a `None` author or a block
    //!   landing with an unrelated principal;
    use super::*;
    use crate::block_store::DocumentKind;
    use crate::llm::{MockClient, Provider};
    use kaijutsu_types::SessionId;

    use crate::runtime::interrupt::ContextInterruptState;
    use crate::runtime::turn_state::ConversationCache;

    /// Drive one turn against a scripted Mock provider that emits a Thinking
    /// block, a Text block, and a tool call against a tool that doesn't
    /// exist — the dispatch failure still produces a durable ToolResult plus
    /// a structured Error child, the same shape `usage_tests` relies on —
    /// then a second iteration's final text closes the turn. Returns the
    /// documents store so the caller can inspect every block's author.
    async fn drive_turn_with_tool_call(kernel: Arc<Kernel>) -> (SharedBlockStore, ContextId, PrincipalId, PrincipalId) {
        let documents = kernel.blocks().clone();
        let ctx = ContextId::new();
        documents
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let player = PrincipalId::new();
        let actor = PrincipalId::new();
        let after = documents
            .insert_block_as(
                ctx,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "write a phrase",
                Status::Done,
                ContentType::Plain,
                Some(player),
            )
            .unwrap();

        let provider = Arc::new(Provider::Mock(MockClient::new("unused").with_scripted_stream(
            vec![
                vec![
                    StreamEvent::ThinkingStart,
                    StreamEvent::ThinkingDelta("reasoning about it".into()),
                    StreamEvent::ThinkingEnd { signature: None },
                    StreamEvent::TextStart,
                    StreamEvent::TextDelta("checking a tool".into()),
                    StreamEvent::TextEnd,
                    StreamEvent::ToolUse {
                        id: "call_1".into(),
                        name: "nonexistent_tool".into(),
                        input: serde_json::json!({}),
                    },
                    StreamEvent::Done {
                        stop_reason: Some("tool_use".into()),
                        input_tokens: Some(10),
                        output_tokens: Some(5),
                        extra: None,
                    },
                ],
                vec![
                    StreamEvent::TextStart,
                    StreamEvent::TextDelta("done".into()),
                    StreamEvent::TextEnd,
                    StreamEvent::Done {
                        stop_reason: Some("end_turn".into()),
                        input_tokens: Some(20),
                        output_tokens: Some(8),
                        extra: None,
                    },
                ],
            ],
        )));

        let kernel_db = kernel.kernel_db().clone();
        let conversation_cache = Arc::new(ConversationCache::new(8));
        let interrupt = ContextInterruptState::new();
        let tool_ctx = crate::ExecContext::new(
            player,
            ctx,
            std::path::PathBuf::from("/"),
            SessionId::new(),
            kernel.id(),
        ).with_actor(actor, Some(player));

        let turn_lease = kernel.clone().turns().begin_with_interrupt(ctx, interrupt.clone());
        process_llm_stream(
            provider,
            documents.clone(),
            ctx,
            "mock-model".to_string(),
            kernel.clone(),
            kernel_db,
            vec![],
            after,
            "system".to_string(),
            1024,
            StreamTimeouts::from_policy(kernel.timeouts()),
            None,
            conversation_cache,
            player,
            None,
            tool_ctx,
            interrupt,
            turn_lease,
            TurnOrigin::Autonomous,
            None,
        )
        .await;

        (documents, ctx, actor, player)
    }

    /// Provider output belongs to the performer; the prompt belongs to its requester.
    #[tokio::test]
    async fn provider_output_blocks_carry_the_performer_and_prompt_keeps_requester() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("authorship-provider").await);
                let (documents, ctx, actor, player) = drive_turn_with_tool_call(kernel).await;
                let blocks = documents.block_snapshots(ctx).unwrap();

                let thinking = blocks
                    .iter()
                    .find(|b| b.kind == BlockKind::Thinking)
                    .expect("thinking block inserted");
                assert_eq!(thinking.id.principal_id, actor);

                let text_blocks: Vec<_> = blocks
                    .iter()
                    .filter(|b| b.kind == BlockKind::Text && b.role == Role::Model)
                    .collect();
                assert!(!text_blocks.is_empty(), "at least one model text block");
                for t in &text_blocks {
                    assert_eq!(t.id.principal_id, actor);
                }

                let tool_call = blocks
                    .iter()
                    .find(|b| b.kind == BlockKind::ToolCall)
                    .expect("tool call block inserted");
                assert_eq!(tool_call.id.principal_id, actor);
                let prompt = blocks.iter().find(|b| b.role == Role::User).unwrap();
                assert_eq!(prompt.id.principal_id, player);
                assert_ne!(actor, player);
            })
            .await;
    }

    /// KERNEL-OUTPUT blocks (ToolResult, the structured tool Error) carry the
    /// system principal too — stamped directly at each site, never through
    /// `actor_principal`.
    #[tokio::test]
    async fn kernel_output_blocks_carry_the_system_principal() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("authorship-kernel").await);
                let (documents, ctx, _actor, _player) = drive_turn_with_tool_call(kernel).await;
                let blocks = documents.block_snapshots(ctx).unwrap();

                let tool_result = blocks
                    .iter()
                    .find(|b| b.kind == BlockKind::ToolResult)
                    .expect("tool result block inserted");
                assert_eq!(tool_result.id.principal_id, PrincipalId::system());

                let error_block = blocks
                    .iter()
                    .find(|b| b.kind == BlockKind::Error)
                    .expect("the nonexistent-tool dispatch produces a structured error block");
                assert_eq!(error_block.id.principal_id, PrincipalId::system());
            })
            .await;
    }


}

#[cfg(test)]
mod gate_resume_cache_eviction_tests {
    //! Regression for `ConversationCache::evict`
    //! (`docs/conversation-session.md`, "Tool pairing at send"): the
    //! gate-resume driver (`crates/kaijutsu-server/src/rpc.rs`,
    //! `act_on_executable_answer`) fills a `Waiting` ToolCall/ToolResult pair
    //! in place, but `ConversationMailbox::catch_up` only folds blocks it has
    //! not yet seen — an in-place edit to an already-seen block is invisible
    //! to it. Without eviction, a cached mailbox keeps serving the
    //! pre-approval text forever. This drives two real turns through
    //! `process_llm_stream` against one shared `ConversationCache` and
    //! inspects what the second turn actually hydrated.
    use super::*;
    use crate::block_store::{BlockStore, DocumentKind};
    use crate::flows::{FlowBus, SharedBlockFlowBus};
    use crate::kernel_db::KernelDb;
    use crate::llm::{MockClient, Provider};
    use kaijutsu_types::{BlockId, SessionId};

    use crate::runtime::interrupt::ContextInterruptState;
    use crate::runtime::turn_state::ConversationCache;

    /// Drive one scripted turn against a caller-owned `documents` /
    /// `conversation_cache`, so a test can inspect what a *later* turn
    /// hydrates. Same shape as `usage_tests::drive_turn_with`, minus the
    /// kernel_db context/document seeding neither this test nor
    /// `error_child_anchor_tests::drive_turn_with` needs
    /// (`get_hydration_policy` returns `Ok(None)` for an unknown context,
    /// not an error).
    #[allow(clippy::too_many_arguments)]
    async fn drive_turn(
        provider: Provider,
        documents: &SharedBlockStore,
        ctx: ContextId,
        player: PrincipalId,
        kernel: &Arc<Kernel>,
        kernel_db: Arc<parking_lot::Mutex<KernelDb>>,
        conversation_cache: Arc<ConversationCache>,
        after: BlockId,
    ) {
        let provider = Arc::new(provider);
        let interrupt = ContextInterruptState::new();
        let tool_ctx = crate::ExecContext::new(
            player,
            ctx,
            std::path::PathBuf::from("/"),
            SessionId::new(),
            kernel.id(),
        );
        let turn_lease = kernel.clone().turns().begin_with_interrupt(ctx, interrupt.clone());
        process_llm_stream(
            provider,
            documents.clone(),
            ctx,
            "mock-model".to_string(),
            kernel.clone(),
            kernel_db,
            vec![],
            after,
            "system".to_string(),
            1024,
            StreamTimeouts::from_policy(kernel.timeouts()),
            None,
            conversation_cache,
            player,
            None,
            tool_ctx,
            interrupt,
            turn_lease,
            TurnOrigin::Autonomous,
            None,
        )
        .await;
    }

    fn text_reply(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextStart,
            StreamEvent::TextDelta(text.to_string()),
            StreamEvent::TextEnd,
            StreamEvent::Done {
                stop_reason: Some("end_turn".into()),
                input_tokens: Some(1),
                output_tokens: Some(1),
                extra: None,
            },
        ]
    }

    /// Documents + a context carrying a ToolCall/ToolResult pair already
    /// `Waiting` on "waiting on a human" — the shape a gated tool call
    /// leaves behind for the gate-resume driver to fill in.
    fn seed_waiting_pair() -> (SharedBlockStore, ContextId, PrincipalId, BlockId, BlockId) {
        let bus: SharedBlockFlowBus = Arc::new(FlowBus::new(256));
        let documents: SharedBlockStore =
            Arc::new(BlockStore::with_flows(PrincipalId::new(), bus));
        let ctx = ContextId::new();
        documents
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();
        let player = PrincipalId::new();

        let user_block = documents
            .insert_block_as(
                ctx,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "run the migration",
                Status::Done,
                ContentType::Plain,
                Some(player),
            )
            .unwrap();
        let command_block = documents
            .insert_tool_call_as(
                ctx,
                None,
                Some(&user_block),
                "shell",
                serde_json::json!({"code": "echo hi"}),
                Some(TypesToolKind::Shell),
                Some(player),
                None,
                None,
            )
            .unwrap();
        let output_block = documents
            .insert_tool_result_as(
                ctx,
                &command_block,
                Some(&command_block),
                "waiting on a human",
                Status::Done,
                None,
                Some(TypesToolKind::Shell),
                Some(PrincipalId::system()),
                None,
            )
            .unwrap();
        documents
            .set_status(ctx, &output_block, Status::Waiting)
            .unwrap();
        documents
            .set_status(ctx, &command_block, Status::Waiting)
            .unwrap();

        (documents, ctx, player, command_block, output_block)
    }

    /// The gate-resume driver's in-place fill: the exact calls
    /// `run_into_blocks` (success) and `settle_pair_error` (failure) make on
    /// the pair's blocks.
    fn fill_pair_in_place(
        documents: &SharedBlockStore,
        ctx: ContextId,
        command_block: &BlockId,
        output_block: &BlockId,
    ) {
        documents
            .edit_text_as(
                ctx,
                output_block,
                0,
                "real output",
                "waiting on a human".len(),
                Some(PrincipalId::system()),
            )
            .unwrap();
        documents
            .set_status(ctx, output_block, Status::Done)
            .unwrap();
        documents
            .set_status(ctx, command_block, Status::Done)
            .unwrap();
    }

    #[tokio::test]
    async fn eviction_after_an_in_place_fill_makes_the_next_turn_hydrate_the_real_output() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("gate-resume-evict").await);
                let (documents, ctx, player, command_block, output_block) = seed_waiting_pair();
                let kernel_db = Arc::new(parking_lot::Mutex::new(KernelDb::temporary().unwrap()));
                let conversation_cache = Arc::new(ConversationCache::new(8));

                // Turn 1: hydrates cold, folds the Waiting pair into the cache.
                drive_turn(
                    Provider::Mock(
                        MockClient::new("unused").with_scripted_stream(vec![text_reply("ack")]),
                    ),
                    &documents,
                    ctx,
                    player,
                    &kernel,
                    kernel_db.clone(),
                    conversation_cache.clone(),
                    documents.last_block_id(ctx).unwrap(),
                )
                .await;

                // The approval lands: the driver settles the pair in place —
                // and, with the fix, evicts the cache.
                fill_pair_in_place(&documents, ctx, &command_block, &output_block);
                conversation_cache.evict(ctx);

                let followup = documents
                    .insert_block_as(
                        ctx,
                        None,
                        Some(&documents.last_block_id(ctx).unwrap()),
                        Role::User,
                        BlockKind::Text,
                        "did it work?",
                        Status::Done,
                        ContentType::Plain,
                        Some(player),
                    )
                    .unwrap();

                // Turn 2: must hydrate cold and see the real output.
                drive_turn(
                    Provider::Mock(
                        MockClient::new("unused").with_scripted_stream(vec![text_reply("yes")]),
                    ),
                    &documents,
                    ctx,
                    player,
                    &kernel,
                    kernel_db.clone(),
                    conversation_cache.clone(),
                    followup,
                )
                .await;

                let snapshot = conversation_cache.get_or_create(ctx).lock().await.snapshot();
                let rendered = format!("{snapshot:?}");
                assert!(
                    rendered.contains("real output"),
                    "the second turn must hydrate the settled output: {rendered}"
                );
                assert!(
                    !rendered.contains("waiting on a human"),
                    "eviction must drop the stale cached text: {rendered}"
                );
            })
            .await;
    }
}

#[cfg(test)]
mod lifetime_tests {
    // Existing fixture input still uses the production startup owner.
    async fn start_fixture_turn(
        kernel: &Arc<Kernel>,
        context_admission: crate::runtime::admission::ContextAdmission,
        model: Option<&str>,
        after_block_id: &kaijutsu_types::BlockId,
        tool_ctx: crate::ExecContext,
        user_principal_id: PrincipalId,
        origin: TurnOrigin,
        continuation_epoch: Option<i64>,
    ) -> Result<(), String> {
        let context_id = context_admission.context();
        let lease = match continuation_epoch {
            Some(_) => kernel.turns().begin_if_idle(context_id)
                .ok_or("automatic continuation found another accepted turn")?,
            None => kernel.turns().begin(context_id),
        };
        crate::runtime::turn_request::queue_startup(kernel, crate::runtime::turn_request::StartupRequest {
            admission: context_admission, lease,
            request: crate::runtime::turn_request::TurnRequest {
                context_id, after_block_id: *after_block_id, content: String::new(),
                principal_id: user_principal_id, model: model.map(str::to_owned),
                continuation_epoch, score: None,
            },
            origin, session: tool_ctx.session_id, tool_ctx: Some(tool_ctx), submit: None,
        }, None)?.await.map_err(|_| "turn preparation stopped before replying".to_string())?
    }

    use super::*;
    use crate::llm::MockClient;
    use kaijutsu_types::{BlockId, ContextState, SessionId};
    use crate::DocumentKind;
    use std::time::Duration;

    async fn fixture(mock: Option<MockClient>) -> (Arc<Kernel>, ContextId, BlockId, crate::ExecContext) {
        let kernel = Arc::new(Kernel::new_ephemeral("turn-lifetime").await.with_timeouts(
            kaijutsu_types::TimeoutPolicy {
                llm_idle_timeout: Duration::from_millis(100),
                ..Default::default()
            },
        ));
        let context = ContextId::new();
        let actor = PrincipalId::new();
        let reviewer = PrincipalId::new();
        kernel.blocks().create_document(context, DocumentKind::Conversation, None).unwrap();
        {
            let db = kernel.kernel_db().lock();
            for (id, name) in [(actor, "performer"), (reviewer, "reviewer")] {
                db.insert_character(&crate::kernel_db::CharacterRow {
                    principal_id: id, name: name.into(), created_at: 0, retired_at: None,
                    handoff_ctx: None, root_ctx: None, root: false,
                }).unwrap();
            }
            db.insert_context(&crate::ContextRow {
                context_id: context, label: None, provider: None, model: None, system_prompt: None,
                consent_mode: ConsentMode::Collaborative, context_state: ContextState::Live,
                context_type: "default".into(), created_at: 0, created_by: reviewer,
                forked_from: None, fork_kind: None, archived_at: None, workspace_id: None,
                preset_id: None, concluded_at: None, last_activity_at: None, promoted_at: None,
                demoted_at: None, paused_at: None, cast_id: None, origin_host: None,
                played_by: Some(actor), reviewer_id: Some(reviewer), director_id: None,
            }).unwrap();
        }
        if let Some(mock) = mock {
            let mut registry = kernel.llm().write().await;
            registry.register("mock", Arc::new(Provider::Mock(mock)));
            assert!(registry.set_default("mock"));
            registry.set_default_model("mock-model");
        }
        let after = kernel.blocks().insert_block_as(context, None, None, Role::User,
            BlockKind::Text, "answer", Status::Done, ContentType::Plain, Some(reviewer)).unwrap();
        let call = crate::ExecContext::new(reviewer, context, "/", SessionId::new(), kernel.id());
        (kernel, context, after, call)
    }

    #[tokio::test]
    async fn dropped_prompt_preparation_wait_keeps_its_live_turn() {
        let (kernel, context, after, call) = fixture(Some(MockClient::new("owned preparation"))).await;
        let admission = kernel.admit_context(context).unwrap();
        let held = kernel.llm().write().await;
        let mut completed = kernel.turn_flows().subscribe("turn.completed");
        let mut startup = Box::pin(start_fixture_turn(&kernel, admission, None,
            &after, call.clone(), call.principal_id, TurnOrigin::Interactive, None));
        assert!(futures::poll!(&mut startup).is_pending());
        assert!(kernel.turn_in_flight(context), "accepted preparation owns its turn before provider selection");
        drop(startup);
        drop(held);
        tokio::time::timeout(Duration::from_secs(3), completed.recv()).await
            .expect("caller drop must not discard accepted preparation").unwrap();
        kernel.shutdown_runtime_worker().await.unwrap();
        assert!(kernel.blocks().block_snapshots(context).unwrap().iter().any(|block|
            block.role == Role::Model && block.content == "owned preparation" && block.status == Status::Done));
        assert!(!kernel.turn_in_flight(context));
    }

    #[tokio::test]
    async fn shutdown_settles_prompt_preparation_without_provider_selection() {
        let (kernel, context, after, call) = fixture(Some(MockClient::new("must not start"))).await;
        let admission = kernel.admit_context(context).unwrap();
        let held = kernel.llm().write().await;
        let mut completed = kernel.turn_flows().subscribe("turn.completed");
        let mut startup = Box::pin(start_fixture_turn(&kernel, admission, None,
            &after, call.clone(), call.principal_id, TurnOrigin::Interactive, None));
        assert!(futures::poll!(&mut startup).is_pending());
        tokio::time::timeout(Duration::from_secs(3), kernel.shutdown_runtime_worker()).await.unwrap().unwrap();
        let error = tokio::time::timeout(Duration::from_secs(1), startup).await
            .expect("shutdown owns pending preparation").unwrap_err();
        assert!(error.contains("shut down"), "{error}");
        let event = tokio::time::timeout(Duration::from_secs(1), completed.recv()).await.unwrap().unwrap();
        assert!(matches!(event.payload, TurnFlow::Completed {
            origin: TurnOrigin::Interactive, reason: TurnStopReason::Cancelled { immediate: true }, .. }));
        assert!(!kernel.turn_in_flight(context));
        drop(held);
    }

    #[tokio::test]
    async fn hard_interrupt_settles_prompt_preparation_without_provider_selection() {
        let (kernel, context, after, call) = fixture(Some(MockClient::new("must not start"))).await;
        let held = kernel.llm().write().await;
        let mut completed = kernel.turn_flows().subscribe("turn.completed");
        let mut startup = Box::pin(start_fixture_turn(&kernel, kernel.admit_context(context).unwrap(),
            None, &after, call.clone(), call.principal_id, TurnOrigin::Interactive, None));
        assert!(futures::poll!(&mut startup).is_pending());
        assert!(kernel.turns().interrupt(context, true));
        assert!(tokio::time::timeout(Duration::from_secs(1), startup).await.unwrap().is_err());
        let event = tokio::time::timeout(Duration::from_secs(1), completed.recv()).await.unwrap().unwrap();
        assert!(matches!(event.payload, TurnFlow::Completed {
            reason: TurnStopReason::Cancelled { immediate: true }, .. }));
        assert!(!kernel.turn_in_flight(context));
        kernel.shutdown_runtime_worker().await.unwrap();
        drop(held);
    }

    #[tokio::test]
    async fn accepted_model_preparation_can_finish_after_archive() {
        let (kernel, context, after, call) = fixture(Some(MockClient::new("accepted answer"))).await;
        let held = kernel.llm().write().await;
        let mut completed = kernel.turn_flows().subscribe("turn.completed");
        let startup = start_fixture_turn(&kernel, kernel.admit_context(context).unwrap(), None,
            &after, call.clone(), call.principal_id, TurnOrigin::Interactive, None);
        tokio::pin!(startup);
        assert!(futures::poll!(&mut startup).is_pending(), "provider preparation is paused after admission");
        kernel.kernel_db().lock().archive_context(context).unwrap();
        drop(held);
        startup.await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), completed.recv()).await.unwrap().unwrap();
        kernel.shutdown_runtime_worker().await.unwrap();
        assert!(kernel.blocks().block_snapshots(context).unwrap().iter().any(|block|
            block.role == Role::Model && block.content == "accepted answer" && block.status == Status::Done));
        assert!(kernel.kernel_db().lock().get_context(context).unwrap().unwrap().is_archived());
    }

    #[tokio::test]
    async fn context_interrupt_cancels_all_queued_turns() {
        // Any provider entry panics: both turns must cancel while the lock is held.
        let (kernel, context, after, call) = fixture(Some(
            MockClient::new("").with_scripted_stream(vec![]))).await;
        let session = kernel.turns().conversations().get_or_create(context);
        let held = session.lock().await;
        let mut completed = kernel.turn_flows().subscribe("turn.completed");
        for _ in 0..2 {
            start_fixture_turn(&kernel, kernel.admit_context(context).unwrap(), None, &after, call.clone(),
                call.principal_id, TurnOrigin::Interactive, None).await.unwrap();
        }
        assert_eq!(kernel.turns().active_count(context), 2);
        assert!(kernel.turns().interrupt(context, true));
        for _ in 0..2 {
            let event = tokio::time::timeout(Duration::from_secs(3), completed.recv()).await
                .expect("every queued turn must observe interruption").unwrap();
            assert!(matches!(event.payload, TurnFlow::Completed {
                reason: TurnStopReason::Cancelled { immediate: true }, ..
            }));
        }
        assert!(!kernel.turn_in_flight(context));
        assert!(completed.try_recv().is_none());
        drop(held);
        kernel.shutdown_runtime_worker().await.unwrap();
    }

    #[tokio::test]
    async fn score_turn_keeps_its_admitted_target_and_validates_its_seed() {
        use super::super::turn_request::{TurnAdmission, TurnRequest};
        use kaijutsu_hyoushigi::{Disposition, Fallback, FallbackReason, Readiness, TickClock};
        use kaijutsu_types::{Tick, TrackId};
        for changed_seed in [false, true] {
            let (kernel, context, after, call) = fixture(Some(MockClient::new("X:1\nK:C\nCDEF|\n"))).await;
            let track = TrackId::new("score-admission").unwrap();
            let timeline = kernel.arm_track_timeline(track.clone(), TickClock::default(), Tick::ZERO);
            let mut completed = kernel.turn_flows().subscribe("turn.completed");
            let TurnAdmission::Accepted { turn_id, work_id: Some(work) } = kernel.request_turn(TurnRequest {
                context_id: context, after_block_id: after.clone(), content: String::new(),
                principal_id: call.principal_id, model: None, continuation_epoch: None,
                score: Some(crate::hyoushigi::model::ScoreIntent { track, start: Tick::new(10), fallback: Fallback::Skip }),
            }).unwrap() else { panic!("score admission must return its work ID") };
            assert_eq!(timeline.lock().status(work).unwrap().started_at, Some(Tick::ZERO));
            let event = tokio::time::timeout(Duration::from_secs(5), completed.recv()).await.unwrap().unwrap();
            assert_eq!(event.payload.turn_id(), turn_id);
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    {
                        let mut tl = timeline.lock();
                        tl.advance_to(Tick::new(4));
                        if tl.status(work).unwrap().readiness == Readiness::Ready { break; }
                        assert_ne!(tl.status(work).unwrap().readiness, Readiness::Failed, "{:?}", tl.status(work));
                    }
                    tokio::task::yield_now().await;
                }
            }).await.unwrap();
            if changed_seed { kernel.blocks().set_excluded(context, &after, true).unwrap(); }
            let mut tl = timeline.lock();
            tl.advance_to(Tick::new(9));
            let status = tl.status(work).unwrap();
            assert_eq!(status.start, Tick::new(10));
            assert_eq!(status.attempt, 1);
            if changed_seed {
                assert!(matches!(status.disposition, Some(Disposition::Fallback { reason: FallbackReason::InvalidBasis, .. })));
                assert!(tl.committed().is_empty());
            } else {
                assert!(matches!(status.disposition, Some(Disposition::Committed { .. })));
                assert_eq!(tl.committed().len(), 1);
            }
            drop(tl);
            kernel.shutdown_runtime_worker().await.unwrap();
        }
    }

    /// A turn a beat is waiting on does not continue at the output ceiling. It
    /// ends with `MaxTokens`, as it did before continuations existed: extra
    /// inferences would put slow work on the beat path, and the resolver
    /// validates one block as a whole tune, so a continuation's block is a
    /// tail fragment either way (`docs/tracks.md`, `docs/hyoushigi.md`). The
    /// script holds one inference, so a continuation panics the mock.
    #[tokio::test]
    async fn a_timed_turn_ends_at_its_first_output_ceiling() {
        use super::super::turn_request::{TurnAdmission, TurnRequest};
        use kaijutsu_hyoushigi::{Fallback, TickClock};
        use kaijutsu_types::{Tick, TrackId};
        let truncated = vec![
            StreamEvent::TextStart,
            StreamEvent::TextDelta("X:1\nK:C\nCD".into()),
            StreamEvent::TextEnd,
            StreamEvent::Done {
                stop_reason: Some("length".into()),
                input_tokens: Some(10),
                output_tokens: Some(1024),
                extra: None,
            },
        ];
        let (kernel, context, after, call) = fixture(Some(
            MockClient::new("unused").with_scripted_stream(vec![truncated]),
        ))
        .await;
        let track = TrackId::new("timed-ceiling").unwrap();
        let _timeline = kernel.arm_track_timeline(track.clone(), TickClock::default(), Tick::ZERO);
        let mut completed = kernel.turn_flows().subscribe("turn.completed");
        let TurnAdmission::Accepted { .. } = kernel.request_turn(TurnRequest {
            context_id: context, after_block_id: after, content: String::new(),
            principal_id: call.principal_id, model: None, continuation_epoch: None,
            score: Some(crate::hyoushigi::model::ScoreIntent {
                track, start: Tick::new(10), fallback: Fallback::Skip,
            }),
        }).unwrap() else { panic!("score admission must be accepted") };

        let event = tokio::time::timeout(Duration::from_secs(5), completed.recv())
            .await.unwrap().unwrap();
        assert!(matches!(event.payload, TurnFlow::Completed {
            reason: TurnStopReason::MaxTokens, .. }), "{:?}", event.payload);
        assert!(
            !kernel.blocks().block_snapshots(context).unwrap().iter()
                .any(|block| block.kind == BlockKind::Notification),
            "a beat's turn is not continued, so it gets no notice"
        );
        kernel.shutdown_runtime_worker().await.unwrap();
    }

    #[tokio::test]
    async fn score_capacity_and_cancellation_belong_to_one_turn() {
        use super::super::turn_request::{TurnAdmission, TurnRequest};
        use kaijutsu_hyoushigi::{Fallback, TickClock, Timeline};
        use kaijutsu_types::{Tick, TrackId};
        let (kernel, context, after, call) = fixture(Some(MockClient::new("").with_scripted_stream(vec![]))).await;
        let session = kernel.turns().conversations().get_or_create(context);
        let held = session.lock().await;
        let track = TrackId::new("bounded-score").unwrap();
        let timeline = kernel.arm_track_timeline(track.clone(), TickClock::default(), Tick::ZERO);
        *timeline.lock() = Timeline::with_capacity(TickClock::default(), std::num::NonZeroUsize::new(1).unwrap());
        let mut completed = kernel.turn_flows().subscribe("turn.completed");
        let request = TurnRequest {
            context_id: context, after_block_id: after, content: String::new(),
            principal_id: call.principal_id, model: None, continuation_epoch: None,
            score: Some(crate::hyoushigi::model::ScoreIntent { track, start: Tick::new(10), fallback: Fallback::Skip }),
        };
        let TurnAdmission::Accepted { turn_id, work_id: Some(work) } = kernel.request_turn(request.clone()).unwrap()
            else { panic!("expected score admission") };
        let TurnAdmission::Accepted { turn_id: other, .. } = kernel.request_turn(TurnRequest { score: None, ..request.clone() }).unwrap()
            else { panic!("expected ordinary admission") };
        assert!(kernel.request_turn(request).unwrap_err().contains("capacity"));
        assert_eq!(kernel.turns().active_count(context), 2);
        assert!(timeline.lock().cancel(work));
        let event = tokio::time::timeout(Duration::from_secs(5), completed.recv()).await.unwrap().unwrap();
        assert_eq!(event.payload.turn_id(), turn_id);
        assert_eq!(kernel.turns().active_count(context), 1, "score cancellation must leave the unrelated queued turn alive");
        assert!(kernel.turns().interrupt(context, true));
        let event = tokio::time::timeout(Duration::from_secs(5), completed.recv()).await.unwrap().unwrap();
        assert_eq!(event.payload.turn_id(), other);
        assert_eq!(timeline.lock().future_len(), 0);
        assert!(!kernel.turn_in_flight(context));
        drop(held);
        kernel.shutdown_runtime_worker().await.unwrap();
    }

    #[tokio::test]
    async fn score_reservation_is_cancelled_when_runtime_admission_is_closed() {
        use super::super::turn_request::TurnRequest;
        use kaijutsu_hyoushigi::{Disposition, Fallback, TickClock};
        use kaijutsu_types::{Tick, TrackId};
        let (kernel, context, after, call) = fixture(None).await;
        let track = TrackId::new("closed-runtime").unwrap();
        let timeline = kernel.arm_track_timeline(track.clone(), TickClock::default(), Tick::ZERO);
        kernel.shutdown_runtime_worker().await.unwrap();
        assert!(kernel.request_turn(TurnRequest {
            context_id: context, after_block_id: after, content: String::new(),
            principal_id: call.principal_id, model: None, continuation_epoch: None,
            score: Some(crate::hyoushigi::model::ScoreIntent { track, start: Tick::new(10), fallback: Fallback::Skip }),
        }).is_err());
        assert_eq!(timeline.lock().future_len(), 0);
        assert_eq!(timeline.lock().statuses()[0].disposition, Some(Disposition::Cancelled));
        assert!(!kernel.turn_in_flight(context));
    }

    #[tokio::test]
    async fn score_malformed_output_is_quarantined_with_anchored_feedback() {
        use super::super::turn_request::{TurnAdmission, TurnRequest};
        use kaijutsu_hyoushigi::{Fallback, Readiness, TickClock};
        use kaijutsu_types::{Tick, TrackId};
        let (kernel, context, after, call) = fixture(Some(MockClient::new("this is not music"))).await;
        let track = TrackId::new("bad-score").unwrap();
        let timeline = kernel.arm_track_timeline(track.clone(), TickClock::default(), Tick::ZERO);
        let mut completed = kernel.turn_flows().subscribe("turn.completed");
        let TurnAdmission::Accepted { work_id: Some(work), .. } = kernel.request_turn(TurnRequest {
            context_id: context, after_block_id: after, content: String::new(),
            principal_id: call.principal_id, model: None, continuation_epoch: None,
            score: Some(crate::hyoushigi::model::ScoreIntent { track, start: Tick::new(10), fallback: Fallback::Skip }),
        }).unwrap() else { panic!("expected score admission") };
        let event = tokio::time::timeout(Duration::from_secs(5), completed.recv()).await.unwrap().unwrap();
        let TurnFlow::Completed { output_block_id: Some(output), .. } = event.payload else { panic!("model output") };
        let block = kernel.blocks().get_block_snapshot(context, &output).unwrap().unwrap();
        assert!(block.excluded, "malformed output must not teach the next turn its bad notation");
        let errors: Vec<_> = kernel.blocks().block_snapshots(context).unwrap().into_iter()
            .filter(|b| b.kind == BlockKind::Error).collect();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].parent_id, Some(output));
        assert_eq!(errors[0].id.principal_id, output.principal_id);
        timeline.lock().advance_to(Tick::new(4));
        assert_eq!(timeline.lock().status(work).unwrap().readiness, Readiness::Failed);
        timeline.lock().advance_to(Tick::new(10));
        assert!(timeline.lock().committed().is_empty());
        kernel.shutdown_runtime_worker().await.unwrap();
    }

    #[tokio::test]
    async fn score_deadline_cancels_a_queued_turn_without_rescheduling_it() {
        use super::super::turn_request::{TurnAdmission, TurnRequest};
        use kaijutsu_hyoushigi::{Disposition, Fallback, FallbackReason, TickClock};
        use kaijutsu_types::{Tick, TrackId};
        let (kernel, context, after, call) = fixture(Some(MockClient::new("").with_scripted_stream(vec![]))).await;
        let session = kernel.turns().conversations().get_or_create(context);
        let held = session.lock().await;
        let track = TrackId::new("missed-score").unwrap();
        let timeline = kernel.arm_track_timeline(track.clone(), TickClock::default(), Tick::ZERO);
        let mut completed = kernel.turn_flows().subscribe("turn.completed");
        let TurnAdmission::Accepted { work_id: Some(work), .. } = kernel.request_turn(TurnRequest {
            context_id: context, after_block_id: after, content: String::new(),
            principal_id: call.principal_id, model: None, continuation_epoch: None,
            score: Some(crate::hyoushigi::model::ScoreIntent { track, start: Tick::new(10), fallback: Fallback::Skip }),
        }).unwrap() else { panic!("expected score admission") };
        timeline.lock().advance_to(Tick::new(10));
        let event = tokio::time::timeout(Duration::from_secs(5), completed.recv()).await.unwrap().unwrap();
        assert!(matches!(event.payload, TurnFlow::Completed { reason: TurnStopReason::Cancelled { immediate: true }, .. }));
        assert!(!kernel.turn_in_flight(context));
        {
            let mut tl = timeline.lock();
            tl.advance_to(Tick::new(100));
            assert!(matches!(tl.status(work).unwrap().disposition,
                Some(Disposition::Fallback { reason: FallbackReason::DeadlineMissed, .. })));
            assert!(tl.committed().is_empty());
            assert_eq!(tl.future_len(), 0);
        }
        drop(held);
        kernel.shutdown_runtime_worker().await.unwrap();
    }

    #[tokio::test]
    async fn headless_turn_ids_survive_queuing_and_cancellation() {
        use super::super::turn_request::TurnRequest;
        let (kernel, context, after, call) = fixture(Some(
            MockClient::new("").with_scripted_stream(vec![]))).await;
        let session = kernel.turns().conversations().get_or_create(context);
        let held = session.lock().await;
        let mut events = kernel.turn_flows().subscribe("turn.*");
        let mut ids = std::collections::HashSet::new();
        for _ in 0..2 {
            kernel.request_turn(TurnRequest {
                score: None,
                context_id: context, after_block_id: after.clone(), content: String::new(),
                principal_id: call.principal_id, model: None, continuation_epoch: None,
            }).unwrap();
            let requested = serde_json::to_value(events.try_recv().unwrap().payload).unwrap();
            let id = requested["Requested"]["turn_id"].as_str()
                .expect("admission must identify its own turn").to_owned();
            assert!(ids.insert(id), "overlapping turns must have distinct IDs");
        }
        assert_eq!(kernel.turns().active_count(context), 2);
        kernel.turns().interrupt(context, true);
        for _ in 0..2 {
            let event = tokio::time::timeout(Duration::from_secs(3), events.recv()).await.unwrap().unwrap();
            assert!(matches!(event.payload, TurnFlow::Completed {
                reason: TurnStopReason::Cancelled { immediate: true }, ..
            }), "{:?}", event.payload);
            let completed = serde_json::to_value(event.payload).unwrap();
            assert!(ids.remove(completed["Completed"]["turn_id"].as_str().unwrap()),
                "terminal event must identify exactly one admitted turn");
        }
        assert!(ids.is_empty());
        assert!(!kernel.turn_in_flight(context));
        drop(held);
        kernel.shutdown_runtime_worker().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_before_headless_startup_settles_the_admitted_id() {
        use super::super::turn_request::{TurnAdmission, TurnRequest};
        let (kernel, context, after, call) = fixture(None).await;
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        kernel.spawn_runtime_task(move |_| async move {
            entered.send(()).unwrap();
            // Hold this test's dedicated runtime before it can poll admission.
            held.recv_timeout(Duration::from_secs(5)).unwrap();
        }).unwrap();
        ready.await.unwrap();
        let mut events = kernel.turn_flows().subscribe("turn.*");
        let TurnAdmission::Accepted { turn_id: id, .. } = kernel.request_turn(TurnRequest {
            score: None,
            context_id: context, after_block_id: after, content: String::new(),
            principal_id: call.principal_id, model: None, continuation_epoch: None,
        }).unwrap() else { panic!("explicit request must be admitted") };
        assert_eq!(events.try_recv().unwrap().payload.turn_id(), id);
        kernel.stop_runtime_worker();
        release.send(()).unwrap();
        kernel.shutdown_runtime_worker().await.unwrap();
        let event = events.try_recv().expect("accepted startup must settle during shutdown");
        assert_eq!(event.payload.turn_id(), id);
        assert!(matches!(event.payload, TurnFlow::Completed {
            reason: TurnStopReason::Cancelled { immediate: true }, .. }));
        assert!(!kernel.turn_in_flight(context));
        assert!(events.try_recv().is_none());
    }

    #[tokio::test]
    async fn headless_startup_failure_follows_requested_and_releases_ownership() {
        use super::super::turn_request::{TurnAdmission, TurnRequest};
        let (kernel, context, after, call) = fixture(None).await;
        let mut events = kernel.turn_flows().subscribe("turn.*");
        let admission = kernel.request_turn(TurnRequest {
            score: None,
            context_id: context, after_block_id: after, content: String::new(),
            principal_id: call.principal_id, model: None, continuation_epoch: None,
        }).unwrap();
        let TurnAdmission::Accepted { turn_id: admitted_id, .. } = admission else { panic!("turn was not admitted") };
        let first = events.recv().await.unwrap();
        assert!(matches!(first.payload, TurnFlow::Requested { .. }));
        assert_eq!(first.payload.turn_id(), admitted_id);
        let requested = serde_json::to_value(first.payload).unwrap();
        let id = requested["Requested"]["turn_id"].as_str().expect("startup owns an admitted turn ID");
        let last = tokio::time::timeout(Duration::from_secs(3), events.recv()).await
            .expect("startup refusal must reach observers").unwrap();
        assert!(matches!(last.payload, TurnFlow::Failed { .. }));
        assert_eq!(serde_json::to_value(last.payload).unwrap()["Failed"]["turn_id"], id);
        assert!(!kernel.turn_in_flight(context));
        kernel.shutdown_runtime_worker().await.unwrap();
        assert!(events.try_recv().is_none());
    }

    #[tokio::test]
    async fn explicit_preparation_cannot_open_an_epoch_for_a_replaced_performer() {
        let (kernel, context, after, call) = fixture(Some(MockClient::new("").with_scripted_stream(vec![]))).await;
        let held = kernel.llm().write().await;
        let startup = spawn_admitted_turn(&kernel, context, None, &after, call.clone(),
            call.principal_id, TurnOrigin::Interactive, None, kernel.turns().begin(context),
            kernel.admit_context(context).unwrap());
        tokio::pin!(startup);
        assert!(futures::poll!(&mut startup).is_pending(), "provider selection waits after resolving identity");
        kernel.kernel_db().lock().update_context_review(context, None, Some(call.principal_id)).unwrap();
        drop(held);
        let error = startup.await.unwrap_err();
        assert!(error.contains("performer changed during turn preparation"), "{error}");
        assert_eq!(kernel.kernel_db().lock().continuation_epoch(context).unwrap(), None);
        assert!(!kernel.turn_in_flight(context));
    }

    #[tokio::test]
    async fn accepted_inference_survives_signoff_and_a_new_explicit_epoch() {
        for signoff in [true, false] {
            let (kernel, context, after, call) = fixture(Some(MockClient::new("accepted answer"))).await;
            let session = kernel.turns().conversations().get_or_create(context);
            let held = session.lock().await;
            let mut events = kernel.turn_flows().subscribe("turn.*");
            start_fixture_turn(&kernel, kernel.admit_context(context).unwrap(), None, &after, call.clone(),
                call.principal_id, TurnOrigin::Interactive, None).await.unwrap();
            {
                let db = kernel.kernel_db().lock();
                let now = kaijutsu_types::now_millis() as i64;
                if signoff { db.sign_off_continuation(context, now).unwrap(); }
                else { db.begin_continuation(context, now).unwrap(); }
            }
            drop(held);
            let terminal = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let event = events.recv().await.unwrap();
                    if matches!(event.payload, TurnFlow::Completed { .. } | TurnFlow::Failed { .. }) { break event.payload; }
                }
            }).await.unwrap();
            assert!(matches!(terminal, TurnFlow::Completed { output_block_id: Some(_), .. }), "{signoff}: {terminal:?}");
            let unstamped: bool = kernel.kernel_db().lock().conn_for_ledger().query_row(
                "SELECT last_request_at IS NULL FROM context_continuations WHERE context_id=?1",
                [context.as_bytes().as_slice()], |row| row.get(0),
            ).unwrap();
            assert!(unstamped, "an old turn cannot refresh a closed or newer window");
            kernel.shutdown_runtime_worker().await.unwrap();
        }
    }

    #[tokio::test]
    async fn reassignment_rejects_queued_continuation_before_provider_entry() {
        use super::super::turn_request::TurnRequest;
        for after_claim in [false, true] {
            // An empty script panics on provider entry: no inference is allowed.
            let (kernel, context, after, call) = fixture(Some(
                MockClient::new("").with_scripted_stream(vec![]))).await;
            let replacement = PrincipalId::new();
            let epoch = {
                let db = kernel.kernel_db().lock();
                db.insert_character(&crate::kernel_db::CharacterRow {
                    principal_id: replacement, name: "replacement".into(), created_at: 0,
                    retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
                }).unwrap();
                let now = kaijutsu_types::now_millis() as i64;
                let epoch = db.begin_continuation(context, now).unwrap().epoch;
                db.record_continuation_request(context, epoch, now).unwrap();
                db.record_continuation_yield(context, epoch, now).unwrap();
                epoch
            };
            let mut failures = kernel.turn_flows().subscribe("turn.failed");
            let session = kernel.turns().conversations().get_or_create(context);
            let held_conversation = session.lock().await;
            let release = if after_claim {
                start_fixture_turn(&kernel, kernel.admit_context(context).unwrap(), None, &after, call.clone(),
                    call.principal_id, TurnOrigin::Autonomous, Some(epoch)).await.unwrap();
                None
            } else {
                let (entered, ready) = tokio::sync::oneshot::channel();
                let (release, held) = std::sync::mpsc::channel();
                kernel.spawn_runtime_task(move |_| async move {
                    entered.send(()).unwrap();
                    held.recv_timeout(Duration::from_secs(5)).unwrap();
                }).unwrap();
                ready.await.unwrap();
                kernel.request_turn(TurnRequest {
                    score: None, context_id: context, after_block_id: after,
                    content: String::new(), principal_id: call.principal_id,
                    model: None, continuation_epoch: Some(epoch),
                }).unwrap();
                Some(release)
            };
            kernel.kernel_db().lock().update_context_review(context, Some(replacement), Some(call.principal_id)).unwrap();
            drop(held_conversation);
            if let Some(release) = release { release.send(()).unwrap(); }
            let event = tokio::time::timeout(Duration::from_secs(3), failures.recv()).await
                .expect("stale continuation must settle").unwrap();
            let TurnFlow::Failed { error, .. } = event.payload else { panic!("expected failed continuation") };
            assert!(error.contains("continuation") && error.contains("closed"), "{after_claim}: {error}");
            assert!(!kernel.turn_in_flight(context));
            kernel.shutdown_runtime_worker().await.unwrap();
        }
    }

    #[tokio::test]
    async fn automatic_admission_leaves_an_existing_turn_in_charge() {
        use super::super::turn_request::{TurnAdmission, TurnRequest};
        let (kernel, context, after, call) = fixture(None).await;
        let existing = kernel.turns().begin(context);
        let mut events = kernel.turn_flows().subscribe("turn.*");
        assert_eq!(kernel.request_turn(TurnRequest {
            score: None,
            context_id: context, after_block_id: after, content: String::new(),
            principal_id: call.principal_id, model: None, continuation_epoch: Some(1),
        }).unwrap(), TurnAdmission::AlreadyActive);
        assert_eq!(kernel.turns().active_count(context), 1);
        assert!(events.try_recv().is_none());
        drop(existing);
        assert!(!kernel.turn_in_flight(context));
        kernel.shutdown_runtime_worker().await.unwrap();
    }

    #[tokio::test]
    async fn rejected_startup_leaves_no_interrupt() {
        let (kernel, context, after, call) = fixture(None).await;
        let error = start_fixture_turn(&kernel, kernel.admit_context(context).unwrap(), None, &after, call.clone(),
            call.principal_id, TurnOrigin::Interactive, None).await.unwrap_err();
        assert!(error.contains("No LLM backend"), "{error}");
        assert!(kernel.turns().active_count(context) == 0, "no task owns this interrupt");
        assert!(!kernel.turn_in_flight(context));
    }

    #[tokio::test]
    async fn accepted_turn_survives_submitting_localset() {
        let (kernel, context, after, call) = fixture(Some(MockClient::new("durable answer"))).await;
        let session = kernel.turns().conversations().get_or_create(context);
        let held = session.lock().await;
        let mut completed = kernel.turn_flows().subscribe("turn.completed");
        let local = tokio::task::LocalSet::new();
        local.run_until(start_fixture_turn(&kernel, kernel.admit_context(context).unwrap(), None, &after, call.clone(),
            call.principal_id, TurnOrigin::Interactive, None)).await.unwrap();
        assert!(kernel.turn_in_flight(context));
        drop(local);
        drop(held);
        let event = tokio::time::timeout(Duration::from_secs(3), completed.recv()).await
            .expect("accepted turn must outlive its submitter").unwrap();
        assert!(matches!(event.payload, TurnFlow::Completed { output_block_id: Some(_), .. }));
        kernel.shutdown_runtime_worker().await.unwrap();
        assert!(!kernel.turn_in_flight(context));
        assert!(kernel.turns().active_count(context) == 0);
        assert!(completed.try_recv().is_none());
    }

    #[tokio::test]
    async fn shutdown_joins_cancelled_turn_and_removes_interrupt() {
        let mock = MockClient::new("").with_scripted_stream(vec![vec![]]).hangs_when_exhausted();
        let (kernel, context, after, call) = fixture(Some(mock)).await;
        let mut completed = kernel.turn_flows().subscribe("turn.completed");
        let local = tokio::task::LocalSet::new();
        local.run_until(async {
            start_fixture_turn(&kernel, kernel.admit_context(context).unwrap(), None, &after, call.clone(),
                call.principal_id, TurnOrigin::Interactive, None).await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), kernel.shutdown_runtime_worker()).await
                .expect("shutdown must drain accepted turns").unwrap();
        }).await;
        assert!(!kernel.turn_in_flight(context), "shutdown left a live turn");
        assert!(kernel.turns().active_count(context) == 0);
        assert!(matches!(completed.try_recv().map(|event| event.payload), Some(TurnFlow::Completed {
            reason: TurnStopReason::Cancelled { immediate: true }, ..
        })));
        assert!(completed.try_recv().is_none());
    }

    #[tokio::test]
    async fn interrupted_stream_settles_only_the_blocks_owned_by_its_turn() {
        use super::super::turn_request::TurnRequest;
        for (ending, thinking) in ["panic", "error", "eof", "cancel"].into_iter().flat_map(|ending| [false, true].map(|thinking| (ending, thinking))) {
            let partial = if thinking { "unfinished thought" } else { "unfinished text" };
            let mut events = vec![
                StreamEvent::TextStart, StreamEvent::TextDelta("finished text".into()), StreamEvent::TextEnd,
                StreamEvent::ToolUse { id: "not-run".into(), name: "missing_tool".into(), input: serde_json::json!({}) },
            ];
            events.extend(if thinking { [StreamEvent::ThinkingStart, StreamEvent::ThinkingDelta(partial.into())] }
                else { [StreamEvent::TextStart, StreamEvent::TextDelta(partial.into())] });
            if ending == "error" { events.push(StreamEvent::Error("provider broke".into())); }
            let mock = MockClient::new("").with_scripted_stream(vec![events]);
            let mock = match ending {
                "panic" => mock.panics_when_exhausted(), "cancel" => mock.hangs_when_exhausted(), _ => mock,
            };
            let (kernel, context, after, call) = fixture(Some(mock)).await;
            // A context may contain another writer and an approval awaiting an answer.
            let other = kernel.blocks().insert_block_as(context, None, Some(&after), Role::Model,
                BlockKind::Text, "another writer", Status::Running, ContentType::Plain, Some(PrincipalId::new())).unwrap();
            let waiting = kernel.blocks().insert_block_as(context, None, Some(&other), Role::Model,
                BlockKind::ToolCall, "waiting for review", Status::Waiting, ContentType::Plain, Some(PrincipalId::new())).unwrap();
            let mut failed = kernel.turn_flows().subscribe("turn.failed");
            let mut completed = kernel.turn_flows().subscribe("turn.completed");
            kernel.request_turn(TurnRequest {
                context_id: context, after_block_id: after, content: String::new(), principal_id: call.principal_id,
                model: None, continuation_epoch: None, score: None,
            }).unwrap();
            if ending == "cancel" {
                tokio::time::timeout(Duration::from_secs(3), async {
                    loop {
                        if kernel.blocks().block_snapshots(context).unwrap().iter().any(|b| b.content == partial) { break; }
                        tokio::task::yield_now().await;
                    }
                }).await.unwrap();
                assert!(kernel.turns().interrupt(context, true));
                let event = tokio::time::timeout(Duration::from_secs(3), completed.recv()).await.unwrap().unwrap();
                assert!(matches!(event.payload, TurnFlow::Completed { reason: TurnStopReason::Cancelled { immediate: true }, .. }));
            } else {
                let event = tokio::time::timeout(Duration::from_secs(3), async {
                    tokio::select! { event = failed.recv() => event, event = completed.recv() => event }
                }).await.unwrap().unwrap();
                assert!(matches!(event.payload, TurnFlow::Failed { .. }), "{ending}: {:?}", event.payload);
            }
            let blocks = kernel.blocks().block_snapshots(context).unwrap();
            let unfinished: Vec<_> = blocks.iter().filter(|b| ["unfinished thought", "unfinished text"].contains(&b.content.as_str())
                || b.tool_use_id.as_deref() == Some("not-run")).collect();
            assert_eq!(unfinished.len(), 2, "{ending}: missing partial output");
            for block in unfinished {
                assert_eq!(block.status, Status::Error, "{ending}: orphan {}", block.id);
            }
            assert_eq!(blocks.iter().find(|b| b.content == "finished text").unwrap().status, Status::Done);
            assert_eq!(kernel.blocks().get_block_snapshot(context, &other).unwrap().unwrap().status, Status::Running);
            assert_eq!(kernel.blocks().get_block_snapshot(context, &waiting).unwrap().unwrap().status, Status::Waiting);
            let shutdown = kernel.shutdown_runtime_worker().await;
            assert_eq!(shutdown.is_err(), ending == "panic");
            assert!(!kernel.turn_in_flight(context));
        }
    }

    #[tokio::test]
    async fn tool_write_refusal_stops_execution_or_provider_continuation() {
        use crate::mcp::{CallContext, ContextToolBinding, InstanceId, InstancePolicy, KernelCallParams,
            KernelTool, KernelToolResult, McpResult, McpServerLike, ServerNotification};
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountingTool {
            id: InstanceId,
            calls: Arc<AtomicUsize>,
            documents: SharedBlockStore,
            reject_settlement: bool,
        }
        #[async_trait::async_trait]
        impl McpServerLike for CountingTool {
            fn instance_id(&self) -> &InstanceId { &self.id }
            async fn list_tools(&self, _: &CallContext) -> McpResult<Vec<KernelTool>> {
                Ok(vec![KernelTool { instance: self.id.clone(), name: "count".into(), description: None,
                    input_schema: serde_json::json!({"type": "object"}) }])
            }
            async fn call_tool(&self, _: KernelCallParams, _: &CallContext, _: tokio_util::sync::CancellationToken) -> McpResult<KernelToolResult> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if self.reject_settlement { self.documents.arm_accept_fault(1); }
                Ok(KernelToolResult::text("required tool output"))
            }
            fn notifications(&self) -> tokio::sync::broadcast::Receiver<ServerNotification> {
                tokio::sync::broadcast::channel(1).1
            }
        }
        for inline in [false, true] {
            for refused in ["result", "call", "settlement"] {
                let done = || StreamEvent::Done { stop_reason: Some("end_turn".into()), input_tokens: None, output_tokens: None, extra: None };
                let first = if inline { vec![StreamEvent::InlineToolUse { id: "call-count".into(), name: "count".into(), input: serde_json::json!({}) }] }
                    else { vec![StreamEvent::ToolUse { id: "call-count".into(), name: "count".into(), input: serde_json::json!({}) }, done()] };
                let mock = MockClient::new("").with_scripted_stream(vec![first,
                    vec![StreamEvent::TextStart, StreamEvent::TextDelta("continued past lost result".into()), StreamEvent::TextEnd, done()]]);
                let (kernel, context, after, call) = fixture(Some(mock)).await;
                let calls = Arc::new(AtomicUsize::new(0));
                let instance = InstanceId::new("count-test");
                kernel.broker().register(Arc::new(CountingTool { id: instance.clone(), calls: calls.clone(),
                    documents: kernel.blocks().clone(), reject_settlement: refused == "settlement" }), InstancePolicy::default()).await.unwrap();
                kernel.broker().set_binding(context, ContextToolBinding::with_instances(vec![instance])).await.unwrap();
                let session = kernel.turns().conversations().get_or_create(context);
                let held = session.lock().await;
                let mut terminal = kernel.turn_flows().subscribe("turn.*");
                start_fixture_turn(&kernel, kernel.admit_context(context).unwrap(), None, &after, call.clone(), call.principal_id, TurnOrigin::Interactive, None).await.unwrap();
                kernel.blocks().arm_accept_fault(match refused { "call" => 1, "result" => 2, _ => 0 });
                drop(held);
                let event = tokio::time::timeout(Duration::from_secs(3), terminal.recv()).await.unwrap().unwrap().payload;
                assert_eq!(calls.load(Ordering::SeqCst), usize::from(refused == "settlement"), "inline={inline} refused={refused}: must create durable pair before execution");
                let TurnFlow::Failed { error, .. } = event else { panic!("inline={inline} refused={refused}: {event:?}") };
                assert!(error.contains("Could not persist model output") && error.contains("injected acceptance refusal"), "{error}");
                let blocks = kernel.blocks().block_snapshots(context).unwrap();
                assert!(!blocks.iter().any(|block| block.status == Status::Running || block.content == "continued past lost result"));
                assert!(!blocks.iter().any(|block| block.kind == BlockKind::ToolResult && (block.status == Status::Done || !block.is_error)), "a refused result cannot look complete or hydrate as success");
                kernel.shutdown_runtime_worker().await.unwrap();
                assert!(terminal.try_recv().is_none());
            }
        }
    }

    #[tokio::test]
    async fn waiting_model_result_requires_its_ask_link_before_publication() {
        use crate::mcp::{CallContext, ContextToolBinding, InstanceId, InstancePolicy, KernelCallParams,
            KernelTool, KernelToolResult, McpResult, McpServerLike, ServerNotification};
        struct PendingTool { id: InstanceId }
        #[async_trait::async_trait]
        impl McpServerLike for PendingTool {
            fn instance_id(&self) -> &InstanceId { &self.id }
            async fn list_tools(&self, _: &CallContext) -> McpResult<Vec<KernelTool>> {
                Ok(vec![KernelTool { instance: self.id.clone(), name: "pending".into(), description: None,
                    input_schema: serde_json::json!({"type": "object"}) }])
            }
            async fn call_tool(&self, _: KernelCallParams, _: &CallContext, _: tokio_util::sync::CancellationToken) -> McpResult<KernelToolResult> {
                Err(crate::mcp::McpError::refused_gate(kaijutsu_types::RefusalKind::Pending,
                    "pending", Some(kaijutsu_types::AskRef { request_id: "missing-ask".into(), status: kaijutsu_types::AskStatus::Pending }), "waiting"))
            }
            fn notifications(&self) -> tokio::sync::broadcast::Receiver<ServerNotification> {
                tokio::sync::broadcast::channel(1).1
            }
        }
        let (kernel, context, mut anchor, call) = fixture(None).await;
        let instance = InstanceId::new("pending-test");
        kernel.broker().register(Arc::new(PendingTool { id: instance.clone() }), InstancePolicy::default()).await.unwrap();
        kernel.broker().set_binding(context, ContextToolBinding::with_instances(vec![instance])).await.unwrap();
        let lease = kernel.turns().begin(context);
        let result = dispatch_inline_tool_result(kernel.blocks(), context, &mut anchor, &kernel,
            "pending", serde_json::json!({}), &call, tokio_util::sync::CancellationToken::new(),
            "pending-call", call.actor_id, &lease).await;
        assert!(result.is_err(), "failed ask linkage cannot acknowledge a waiting result");
        let db = kernel.blocks().db().unwrap().clone();
        let workspace = db.lock().get_or_create_default_workspace(PrincipalId::system()).unwrap();
        let restored = crate::block_store::BlockStore::with_db(db, workspace, PrincipalId::system());
        restored.load_from_db().unwrap();
        assert!(!restored.block_snapshots(context).unwrap().iter().any(|block| block.status == Status::Waiting));
    }

    #[tokio::test]
    async fn ordinary_and_inline_tools_preserve_the_same_shell_projection() {
        use crate::mcp::{CallContext, ContextToolBinding, InstanceId, InstancePolicy, KernelCallParams,
            KernelTool, KernelToolResult, McpResult, McpServerLike, ServerNotification};
        struct EnvelopeTool { id: InstanceId }
        #[async_trait::async_trait]
        impl McpServerLike for EnvelopeTool {
            fn instance_id(&self) -> &InstanceId { &self.id }
            async fn list_tools(&self, _: &CallContext) -> McpResult<Vec<KernelTool>> {
                Ok(vec![KernelTool { instance: self.id.clone(), name: "envelope".into(), description: None,
                    input_schema: serde_json::json!({"type": "object"}) }])
            }
            async fn call_tool(&self, _: KernelCallParams, call: &CallContext, _: tokio_util::sync::CancellationToken) -> McpResult<KernelToolResult> {
                assert!(call.publishes_pair, "model dispatch must declare its publication owner to the broker");
                let mut envelope = ShellEnvelope::new(kaijutsu_types::shell_envelope::ShellStatus::Done);
                envelope.stdout = "\x1b[31mred\x1b[0m".into();
                envelope.exit_code = Some(0);
                envelope.data = Some(serde_json::json!({"phase": 3}));
                Ok(KernelToolResult::text(envelope.to_value().to_string()))
            }
            fn notifications(&self) -> tokio::sync::broadcast::Receiver<ServerNotification> {
                tokio::sync::broadcast::channel(1).1
            }
        }
        for inline in [false, true] {
            let (kernel, context, after, call) = fixture(None).await;
            let instance = InstanceId::new("envelope-test");
            kernel.broker().register(Arc::new(EnvelopeTool { id: instance.clone() }), InstancePolicy::default()).await.unwrap();
            kernel.broker().set_binding(context, ContextToolBinding::with_instances(vec![instance])).await.unwrap();
            let lease = kernel.turns().begin(context);
            let mut anchor = after;
            let result = if inline {
                dispatch_inline_tool_result(kernel.blocks(), context, &mut anchor, &kernel, "envelope", serde_json::json!({}),
                    &call, lease.interrupt().cancel.clone(), "envelope-call", call.actor_id, &lease).await.unwrap()
            } else {
                let id = kernel.blocks().insert_tool_call_as(context, None, Some(&after), "envelope", serde_json::json!({}),
                    Some(TypesToolKind::Builtin), Some(call.actor_id), Some("envelope-call".into()), None).unwrap();
                lease.track_block(id);
                let (result, tail) = dispatch_recorded_tool_result(kernel.blocks(), context, &kernel, "envelope", &serde_json::json!({}),
                    &call, lease.interrupt().cancel.clone(), "envelope-call", id, &lease).await.unwrap();
                anchor = tail;
                result
            };
            assert!(!result.is_error);
            let model = ShellEnvelope::from_tool_result(&result.content).unwrap();
            assert_eq!(model.stdout, "red");
            assert_eq!(model.data, Some(serde_json::json!({"phase": 3})));
            let block = kernel.blocks().get_block_snapshot(context, &anchor).unwrap().unwrap();
            assert_eq!(block.content, "red");
            assert_eq!(block.status, Status::Done);
            assert!(!block.style_spans.is_empty());
            assert!(block.provenance.is_some());
            assert!(!block.is_error);
            assert_eq!(lease.settle_blocks(kernel.blocks()).unwrap(), 0);
        }
    }

    #[tokio::test]
    async fn tool_write_failure_cancels_and_joins_its_sibling_result() {
        use crate::mcp::{CallContext, ContextToolBinding, InstanceId, InstancePolicy, KernelCallParams,
            KernelTool, KernelToolResult, McpResult, McpServerLike, ServerNotification};
        struct PairedTools {
            id: InstanceId,
            entered: tokio::sync::Notify,
            documents: SharedBlockStore,
        }
        #[async_trait::async_trait]
        impl McpServerLike for PairedTools {
            fn instance_id(&self) -> &InstanceId { &self.id }
            async fn list_tools(&self, _: &CallContext) -> McpResult<Vec<KernelTool>> {
                Ok(["fault", "pending"].iter().map(|name| KernelTool { instance: self.id.clone(),
                    name: (*name).into(), description: None, input_schema: serde_json::json!({"type": "object"}) }).collect())
            }
            async fn call_tool(&self, params: KernelCallParams, _: &CallContext, _: tokio_util::sync::CancellationToken) -> McpResult<KernelToolResult> {
                if params.tool == "pending" {
                    self.entered.notify_one();
                    std::future::pending().await
                } else {
                    self.entered.notified().await;
                    self.documents.arm_accept_fault(1);
                    Ok(KernelToolResult::text("result whose persistence fails"))
                }
            }
            fn notifications(&self) -> tokio::sync::broadcast::Receiver<ServerNotification> {
                tokio::sync::broadcast::channel(1).1
            }
        }
        let events = vec![
            StreamEvent::ToolUse { id: "fault-call".into(), name: "fault".into(), input: serde_json::json!({}) },
            StreamEvent::ToolUse { id: "pending-call".into(), name: "pending".into(), input: serde_json::json!({}) },
            StreamEvent::Done { stop_reason: Some("tool_use".into()), input_tokens: None, output_tokens: None, extra: None },
        ];
        let (kernel, context, after, call) = fixture(Some(MockClient::new("").with_scripted_stream(vec![events]))).await;
        let instance = InstanceId::new("paired-tools");
        kernel.broker().register(Arc::new(PairedTools { id: instance.clone(), entered: Default::default(),
            documents: kernel.blocks().clone() }), InstancePolicy::default()).await.unwrap();
        kernel.broker().set_binding(context, ContextToolBinding::with_instances(vec![instance])).await.unwrap();
        let mut failures = kernel.turn_flows().subscribe("turn.failed");
        start_fixture_turn(&kernel, kernel.admit_context(context).unwrap(), None, &after, call.clone(), call.principal_id, TurnOrigin::Interactive, None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), failures.recv()).await.expect("fault must interrupt the pending sibling").unwrap();
        let blocks = kernel.blocks().block_snapshots(context).unwrap();
        let sibling = blocks.iter().find(|block| block.kind == BlockKind::ToolResult
            && block.tool_use_id.as_deref() == Some("pending-call")).unwrap();
        assert_eq!(sibling.status, Status::Error);
        assert!(sibling.is_error);
        assert!(sibling.content.to_lowercase().contains("cancel"), "join sibling settlement rather than only sweeping its empty block: {}", sibling.content);
        assert!(!blocks.iter().any(|block| block.status == Status::Running));
        kernel.shutdown_runtime_worker().await.unwrap();
    }

    #[tokio::test]
    async fn tool_panic_settles_its_pair_and_cancels_the_call() {
        use crate::mcp::{CallContext, ContextToolBinding, InstanceId, InstancePolicy, KernelCallParams,
            KernelTool, KernelToolResult, McpResult, McpServerLike, ServerNotification};
        use tokio_util::sync::CancellationToken;
        struct PanickingTool {
            id: InstanceId,
            cancel: Arc<parking_lot::Mutex<Option<CancellationToken>>>,
        }
        #[async_trait::async_trait]
        impl McpServerLike for PanickingTool {
            fn instance_id(&self) -> &InstanceId { &self.id }
            async fn list_tools(&self, _: &CallContext) -> McpResult<Vec<KernelTool>> {
                Ok(vec![KernelTool { instance: self.id.clone(), name: "explode".into(), description: None,
                    input_schema: serde_json::json!({"type": "object"}) }])
            }
            async fn call_tool(&self, _: KernelCallParams, _: &CallContext, cancel: CancellationToken) -> McpResult<KernelToolResult> {
                *self.cancel.lock() = Some(cancel);
                panic!("controlled tool panic");
            }
            fn notifications(&self) -> tokio::sync::broadcast::Receiver<ServerNotification> {
                tokio::sync::broadcast::channel(1).1
            }
        }
        for inline in [false, true] {
            let events = if inline { vec![StreamEvent::InlineToolUse { id: "call-panic".into(), name: "explode".into(), input: serde_json::json!({}) }] }
            else { vec![StreamEvent::ToolUse { id: "call-panic".into(), name: "explode".into(), input: serde_json::json!({}) },
                StreamEvent::Done { stop_reason: Some("tool_use".into()), input_tokens: None, output_tokens: None, extra: None }] };
            let (kernel, context, after, call) = fixture(Some(MockClient::new("").with_scripted_stream(vec![events]))).await;
            let cancel = Arc::new(parking_lot::Mutex::new(None));
            let instance = InstanceId::new("panic-test");
            kernel.broker().register(Arc::new(PanickingTool { id: instance.clone(), cancel: cancel.clone() }), InstancePolicy::default()).await.unwrap();
            kernel.broker().set_binding(context, ContextToolBinding::with_instances(vec![instance])).await.unwrap();
            let mut failures = kernel.turn_flows().subscribe("turn.failed");
            start_fixture_turn(&kernel, kernel.admit_context(context).unwrap(), None, &after, call.clone(),
                call.principal_id, TurnOrigin::Interactive, None).await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), failures.recv()).await.unwrap().unwrap();
            let blocks = kernel.blocks().block_snapshots(context).unwrap();
            let pair: Vec<_> = blocks.iter().filter(|b| b.tool_use_id.as_deref() == Some("call-panic")).collect();
            assert_eq!(pair.len(), 2, "call and result are both owned: inline={inline}");
            assert!(pair.iter().all(|b| b.status == Status::Error));
            assert!(cancel.lock().as_ref().unwrap().is_cancelled(), "panic must stop its outstanding call");
            assert!(kernel.shutdown_runtime_worker().await.is_err());
        }
    }

    #[tokio::test]
    async fn provider_panic_publishes_failure_and_fails_worker() {
        use super::super::turn_request::{TurnAdmission, TurnRequest};
        for headless in [false, true] {
            let mock = MockClient::new("").with_scripted_stream(vec![]);
            let (kernel, context, after, call) = fixture(Some(mock)).await;
            let mut failed = kernel.turn_flows().subscribe("turn.failed");
            let mut completed = kernel.turn_flows().subscribe("turn.completed");
            let mut requested = kernel.turn_flows().subscribe("turn.requested");
            let turn_id = if headless {
                let TurnAdmission::Accepted { turn_id: id, .. } = kernel.request_turn(TurnRequest {
                    score: None,
                    context_id: context, after_block_id: after, content: String::new(),
                    principal_id: call.principal_id, model: None, continuation_epoch: None,
                }).unwrap() else { panic!("explicit request must be admitted") };
                assert_eq!(requested.try_recv().unwrap().payload.turn_id(), id);
                id
            } else {
                let lease = kernel.turns().begin(context);
                let id = lease.id();
                spawn_admitted_turn(&kernel, context, None, &after, call.clone(),
                    call.principal_id, TurnOrigin::Interactive, None, lease, kernel.admit_context(context).unwrap()).await.unwrap();
                id
            };
            let event = tokio::time::timeout(Duration::from_secs(3), failed.recv()).await
                .expect("a panicked turn must announce failure").unwrap();
            assert_eq!(event.payload.turn_id(), turn_id);
            assert!(matches!(event.payload, TurnFlow::Failed { ref error, .. } if error.contains("Model turn panicked")));
            assert!(kernel.shutdown_runtime_worker().await.is_err(), "panic must reach shutdown owner");
            assert!(!kernel.turn_in_flight(context));
            assert_eq!(kernel.turns().active_count(context), 0);
            assert!(failed.try_recv().is_none(), "startup must not report the stream panic a second time");
            assert!(completed.try_recv().is_none());
        }
    }
    #[tokio::test]
    async fn stopped_worker_refuses_turn_without_leaving_state() {
        let (kernel, context, after, call) = fixture(Some(MockClient::new("unused"))).await;
        kernel.shutdown_runtime_worker().await.unwrap();
        let error = start_fixture_turn(&kernel, kernel.admit_context(context).unwrap(), None, &after, call.clone(),
            call.principal_id, TurnOrigin::Interactive, None).await.unwrap_err();
        assert!(error.contains("shut down"), "{error}");
        assert!(!kernel.turn_in_flight(context));
        assert!(kernel.turns().active_count(context) == 0);
    }

    #[tokio::test]
    async fn shutdown_cancels_a_turn_waiting_for_the_conversation_lock() {
        // Any inference would panic: cancellation must win before provider entry.
        let mock = MockClient::new("").with_scripted_stream(vec![]);
        let (kernel, context, after, call) = fixture(Some(mock)).await;
        let session = kernel.turns().conversations().get_or_create(context);
        let held = session.lock().await;
        let mut completed = kernel.turn_flows().subscribe("turn.completed");
        start_fixture_turn(&kernel, kernel.admit_context(context).unwrap(), None, &after, call.clone(),
            call.principal_id, TurnOrigin::Interactive, None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), kernel.shutdown_runtime_worker()).await
            .expect("queued turn must cancel without acquiring the held lock").unwrap();
        drop(held);
        assert!(matches!(completed.try_recv().map(|event| event.payload), Some(TurnFlow::Completed {
            reason: TurnStopReason::Cancelled { immediate: true }, ..
        })));
        assert!(!kernel.turn_in_flight(context));
        assert!(kernel.turns().active_count(context) == 0);
    }

    #[tokio::test]
    async fn shutdown_drains_an_open_provider_stream() {
        let mock = MockClient::new("").with_scripted_stream(vec![vec![
            StreamEvent::TextStart, StreamEvent::TextDelta("stream entered".into()),
        ]]).hangs_when_exhausted();
        let (kernel, context, after, call) = fixture(Some(mock)).await;
        let mut completed = kernel.turn_flows().subscribe("turn.completed");
        start_fixture_turn(&kernel, kernel.admit_context(context).unwrap(), None, &after, call.clone(),
            call.principal_id, TurnOrigin::Interactive, None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if kernel.blocks().block_snapshots(context).unwrap().iter()
                    .any(|block| block.role == Role::Model && block.content == "stream entered") { break; }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }).await.expect("provider must enter before shutdown");
        tokio::time::timeout(Duration::from_secs(3), kernel.shutdown_runtime_worker()).await
            .expect("shutdown must drain the cancelled provider").unwrap();
        assert!(matches!(completed.try_recv().map(|event| event.payload), Some(TurnFlow::Completed {
            reason: TurnStopReason::Cancelled { immediate: true }, ..
        })));
        assert!(!kernel.turn_in_flight(context));
        assert!(kernel.turns().active_count(context) == 0);
        assert!(completed.try_recv().is_none());
    }

    #[tokio::test]
    async fn shutdown_cancels_provider_connection_before_it_opens() {
        let mut mock = MockClient::new("must not finish connecting");
        mock.stream_start_delay = Duration::from_secs(5);
        let (kernel, context, after, call) = fixture(Some(mock)).await;
        let mut completed = kernel.turn_flows().subscribe("turn.completed");
        start_fixture_turn(&kernel, kernel.admit_context(context).unwrap(), None, &after, call.clone(),
            call.principal_id, TurnOrigin::Interactive, None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let started: bool = kernel.kernel_db().lock().conn_for_ledger().query_row(
                    "SELECT last_request_at IS NOT NULL FROM context_continuations WHERE context_id = ?1",
                    rusqlite::params![context.as_bytes().as_slice()], |row| row.get(0),
                ).unwrap();
                if started { break; }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }).await.expect("provider connection must start before shutdown");
        tokio::time::timeout(Duration::from_millis(500), kernel.shutdown_runtime_worker()).await
            .expect("shutdown must cancel provider connection, not wait for it to open").unwrap();
        assert!(matches!(completed.try_recv().map(|event| event.payload), Some(TurnFlow::Completed {
            reason: TurnStopReason::Cancelled { immediate: true }, ..
        })));
        assert!(!kernel.turn_in_flight(context));
        assert!(kernel.turns().active_count(context) == 0);
    }

}
