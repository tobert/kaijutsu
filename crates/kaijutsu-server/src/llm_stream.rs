//! LLM streaming + agentic tool-call loop.
//!
//! This module owns the background task that talks to a `Provider`, parses
//! `StreamEvent`s into CRDT blocks, dispatches tool calls, and re-prompts until
//! the model stops. Extracted from `rpc.rs` so the stream semantics sit in one
//! file rather than interleaved with the RPC dispatch surface.
//!
//! Entry point: [`spawn_llm_for_prompt`], called by the `prompt` and
//! `submit_input` RPC handlers after they have inserted the user's message
//! block. It resolves the provider/model, builds the effective tool filter,
//! and spawns [`process_llm_stream`] as a local task.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock as TokioRwLock;

use kaijutsu_crdt::{BlockKind, ContentType, Role, Status};
use kaijutsu_kernel::flows::TurnFlow;
use kaijutsu_kernel::kernel_db::KernelDb;
use kaijutsu_kernel::llm::stream::{BuildOpts, CacheTarget, StreamEvent};
use kaijutsu_kernel::llm::{ContentBlock, ToolDefinition};
use kaijutsu_kernel::{Kernel, LlmMessage, Provider, SharedBlockStore};
use kaijutsu_types::ToolKind as TypesToolKind;
use kaijutsu_types::{ConsentMode, ContextId, PrincipalId};

use crate::interrupt::ContextInterruptState;
use crate::rpc::{ConversationCache, SharedKernelState};

/// Build tool definitions visible to the LLM in this context.
///
/// Phase 5 M4: `ToolFilter` retired (D-54). Per-context tool curation is
/// expressed by the `ContextToolBinding`'s `allowed_instances` and by
/// `HookPhase::ListTools` hooks (D-56) — both applied inside
/// `Broker::list_visible_tools` via `list_tool_defs_via_broker`. This
/// function now pass-throughs the broker output unmodified.
/// Surface a configuration / pre-stream failure as a visible error block so the
/// user gets feedback in the conversation, not just a capnp error returned to
/// the (typically silent) RPC client. Anchored after the user's message block.
fn insert_pre_stream_error_block(
    documents: &SharedBlockStore,
    context_id: ContextId,
    after_block_id: &kaijutsu_crdt::BlockId,
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
        log::warn!("Failed to insert pre-stream error block: {}", e);
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
    after_block_id: &kaijutsu_crdt::BlockId,
    mailbox: &mut kaijutsu_kernel::ConversationMailbox,
    // The hydration window policy `(marker, window)`, or `None` to hydrate the
    // whole history (the default; every non-composer context). When set, the
    // turn hydrates only `[0, marker] ∪ last-window` — the cost guard for
    // endless composer logs (design: docs/chameleon.md).
    policy: Option<(kaijutsu_crdt::BlockId, u32)>,
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
    after_block_id: &kaijutsu_crdt::BlockId,
    read: kaijutsu_kernel::BlockStoreResult<Vec<kaijutsu_crdt::BlockSnapshot>>,
    mailbox: &mut kaijutsu_kernel::ConversationMailbox,
    policy: Option<(kaijutsu_crdt::BlockId, u32)>,
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
                    log::info!(
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
                    log::info!(
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
            log::error!(
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
) -> Vec<ToolDefinition> {
    kernel
        .list_tool_defs_via_broker(context_id, principal_id)
        .await
        .into_iter()
        .map(|(name, schema, description)| ToolDefinition {
            name,
            description: description.unwrap_or_default(),
            input_schema: schema,
        })
        .collect()
}

/// Resolve LLM provider and spawn streaming for a user prompt.
///
/// Shared by `prompt` and `submit_input` handlers. Creates the assistant response
/// flow (thinking -> text -> tool calls -> results) as background blocks via
/// `process_llm_stream`.
pub(crate) async fn spawn_llm_for_prompt(
    kernel: &SharedKernelState,
    context_id: ContextId,
    model: Option<&str>,
    after_block_id: &kaijutsu_crdt::BlockId,
    tool_ctx: kaijutsu_kernel::ExecContext,
    user_principal_id: PrincipalId,
    // `true` only on the autonomous turn-driver path (the composer's OODA loop):
    // the stream publishes `TurnFlow::Completed`/`Failed` at its end. Interactive
    // human-prompt callers pass `false` — a human turn must never feed the
    // composer's OODA Act, so it announces nothing (design §7). This gate is what
    // keeps the publish-at-stream-end from silently extending Completed to every
    // interactive prompt.
    announce_completion: bool,
) -> Result<(), capnp::Error> {
    let documents = kernel.documents.clone();
    let kernel_arc = kernel.kernel.clone();
    let kernel_db = kernel.kernel_db.clone();
    let config_backend = kernel.config_backend.clone();
    let conversation_cache = kernel.conversation_cache.clone();
    let kj_dispatcher = kernel.kj_dispatcher.clone();
    // Create a fresh interrupt state for this prompt (replaces any previous entry).
    // The generation counter prevents the race where stream A's cleanup removes
    // stream B's interrupt state.
    let (interrupt, interrupt_generation) = kernel.create_interrupt(context_id).await;
    let context_interrupts = kernel.context_interrupts.clone();

    // Load system prompt from config
    let system_prompt = {
        if let Err(e) = config_backend.ensure_config("system.md").await {
            log::warn!("Failed to ensure system.md config: {}", e);
        }
        config_backend
            .get_content("system.md")
            .unwrap_or_else(|_| kaijutsu_kernel::DEFAULT_SYSTEM_PROMPT.to_string())
    };

    // Read per-context model from DriftRouter (quick read, release lock).
    // Capture label/state alongside for the situational system-prompt addendum.
    let (ctx_model, ctx_provider_name, ctx_label, ctx_state) = {
        let drift = kernel_arc.drift().read();
        // Guard: block LLM invocation while context is in Staging state
        if let Some(h) = drift.get(context_id) {
            if h.state == kaijutsu_types::ContextState::Staging {
                // Insert an ephemeral system block explaining why the prompt was rejected
                let _ = documents.insert_block_as(
                    context_id,
                    None,
                    Some(after_block_id),
                    kaijutsu_crdt::Role::System,
                    kaijutsu_crdt::BlockKind::Text,
                    "Context is in staging mode. Use `kj stage commit` to go live.",
                    kaijutsu_crdt::Status::Done,
                    kaijutsu_crdt::ContentType::Plain,
                    Some(PrincipalId::system()),
                ).and_then(|bid| documents.set_ephemeral(context_id, &bid, true));
                return Err(capnp::Error::failed(
                    "context is in staging mode — commit to enable LLM prompts".into(),
                ));
            }
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

    // Resolve provider + model from LLM registry
    // Priority: explicit param > per-context (DriftRouter) > kernel default
    let provider_resolution: Result<(Arc<Provider>, String, u64), &'static str> = {
        let registry = kernel_arc.llm().read().await;
        let max_tokens = registry.max_output_tokens();

        let effective_model = model.map(|m| m.to_string()).or(ctx_model);

        match effective_model {
            Some(name) => ctx_provider_name
                .as_deref()
                .and_then(|pn| registry.get(pn))
                .or_else(|| registry.default_provider())
                .map(|p| (p, name, max_tokens))
                .ok_or("No LLM provider configured (check models.toml)"),
            None => match registry.default_provider() {
                Some(p) => {
                    let m = registry
                        .default_model()
                        .unwrap_or(kaijutsu_kernel::DEFAULT_MODEL)
                        .to_string();
                    Ok((p, m, max_tokens))
                }
                None => Err("No LLM provider configured (check models.toml)"),
            },
        }
    };
    let (provider, model_name, max_output_tokens) = match provider_resolution {
        Ok(v) => v,
        Err(detail) => {
            log::error!("No LLM provider configured");
            insert_pre_stream_error_block(&documents, context_id, after_block_id, detail);
            return Err(capnp::Error::failed(detail.into()));
        }
    };

    // Compaction pressure (M1-A5): if the context's live block count is over
    // threshold, summarize the older half into a Drift block and mark the
    // originals compacted so the hydrator skips them. Logs but does not
    // fail the prompt on summarization errors — better to ship a too-long
    // history than to refuse the user's request.
    match kj_dispatcher.auto_compact_if_needed(context_id).await {
        Ok(true) => log::info!("Auto-compacted context {context_id} before prompt"),
        Ok(false) => {}
        Err(e) => log::warn!("Auto-compaction failed for {context_id}: {e}"),
    }

    // Build tool definitions via the broker (binding + ListTools filter do
    // the curation — D-54 retired the legacy post-filter).
    let tools = build_tool_definitions(&kernel_arc, context_id, user_principal_id).await;

    // Assemble situational system-prompt addendum (A4): static base + rc
    // sections (the `.md` lifecycle scripts) + per-call facts so the model
    // has context name, lifecycle state, and current tool inventory without
    // losing the static stance set in assets/defaults/system.md.
    //
    // rc sections come from `(Role::System, BlockKind::Text)` blocks in the
    // conversation — typically dropped in by rc-on-create/-on-fork. They
    // land between the static base and the `<situation>` addendum (matching
    // the doc layout: base → rc → situation).
    let situational = kaijutsu_kernel::SituationalContext {
        context_id: Some(context_id),
        context_label: ctx_label,
        context_state: ctx_state,
        provider: ctx_provider_name.clone(),
        model: Some(model_name.clone()),
        tool_names: tools.iter().map(|t| t.name.clone()).collect(),
    };
    let rc_sections = documents
        .block_snapshots(context_id)
        .map(|b| kaijutsu_kernel::extract_system_prompt_sections(&b))
        .unwrap_or_default();
    let system_prompt =
        kaijutsu_kernel::build_system_prompt(&system_prompt, &situational, &rc_sections);

    log::info!(
        "Spawning LLM stream: context={}, model={}",
        context_id,
        model_name
    );

    let after_block_id = *after_block_id;

    tokio::task::spawn_local(process_llm_stream(
        provider,
        documents,
        context_id,
        model_name,
        kernel_arc,
        kernel_db,
        tools,
        after_block_id,
        system_prompt,
        max_output_tokens,
        conversation_cache,
        user_principal_id,
        tool_ctx,
        interrupt,
        interrupt_generation,
        context_interrupts,
        announce_completion,
    ));

    Ok(())
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
    use kaijutsu_kernel::{BlockStoreError, DocumentKind, shared_block_store};

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

        let mut mailbox = kaijutsu_kernel::ConversationMailbox::new();

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

        let mut mailbox = kaijutsu_kernel::ConversationMailbox::new();
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

        let mut mailbox = kaijutsu_kernel::ConversationMailbox::new();
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
/// recorded from the terminal `Done` event so token/cache/reasoning
/// accounting lands on the trace, not just the metrics meter.
#[tracing::instrument(
    name = "llm.turn",
    skip_all,
    fields(
        llm.provider = provider.name(),
        llm.model = %model_name,
        llm.usage.input_tokens = tracing::field::Empty,
        llm.usage.output_tokens = tracing::field::Empty,
        llm.usage.cache_read_tokens = tracing::field::Empty,
        llm.usage.cache_write_tokens = tracing::field::Empty,
        llm.usage.reasoning_tokens = tracing::field::Empty,
        llm.response.stop_reason = tracing::field::Empty,
    )
)]
async fn process_llm_stream(
    provider: Arc<Provider>,
    documents: SharedBlockStore,
    context_id: ContextId,
    model_name: String,
    kernel: Arc<Kernel>,
    kernel_db: Arc<parking_lot::Mutex<KernelDb>>,
    tools: Vec<ToolDefinition>,
    after_block_id: kaijutsu_crdt::BlockId,
    system_prompt: String,
    max_output_tokens: u64,
    conversation_cache: Arc<ConversationCache>,
    // The turn's principal — authors the TurnFlow outcome event (and reserved
    // for future per-user attribution on model-generated blocks).
    user_principal_id: PrincipalId,
    tool_ctx: kaijutsu_kernel::ExecContext,
    interrupt: Arc<ContextInterruptState>,
    interrupt_generation: u64,
    context_interrupts: Arc<TokioRwLock<HashMap<ContextId, Arc<ContextInterruptState>>>>,
    // Only autonomous (turn-driver) turns announce their completion on the
    // TurnFlow bus; interactive human prompts pass `false` so the composer's
    // OODA Act never crystallizes a human-prompted turn (design §7). The publish
    // moved here from the spawn site (rpc.rs:391) so it fires at actual stream
    // end with the real output block id, not at spawn racing the model.
    announce_completion: bool,
) {
    // Get per-context mailbox lock — held for the entire stream,
    // serializing concurrent prompts to the same context (Fix D+E).
    // The mailbox holds the live conversation session (see
    // docs/conversation-session.md); we own this lock for the
    // duration of catch_up + snapshot.
    let cache_lock = conversation_cache.get_or_create(context_id);
    let mut mailbox = cache_lock.lock().await;

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
            log::error!(
                "Hydration policy read failed for context {context_id}: {e}; failing the turn"
            );
            if announce_completion {
                kernel.turn_flows().publish(TurnFlow::Failed {
                    context_id,
                    principal_id: user_principal_id,
                    error: format!("hydration policy unreadable: {e}"),
                });
            }
            return;
        }
    };
    let mut messages = match hydrate_messages(
        &documents,
        context_id,
        &after_block_id,
        &mut mailbox,
        hydration_policy,
    ) {
        Ok(messages) => messages,
        // Hydration failed and surfaced a visible Error block; fail the turn
        // loudly rather than streaming against an empty/partial session. This is a
        // terminal path like any other: an announced turn MUST publish exactly one
        // terminal event (§7), so emit Failed before returning — otherwise the OODA
        // Act handoff gets no signal and the turn silently falls off the bus while
        // every other terminal path announces.
        Err(()) => {
            if announce_completion {
                kernel.turn_flows().publish(TurnFlow::Failed {
                    context_id,
                    principal_id: user_principal_id,
                    error: "hydration failed: could not read conversation history".to_string(),
                });
            }
            return;
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
        let cas: std::sync::Arc<dyn kaijutsu_kernel::ContentStore> = kernel.cas().clone();
        kaijutsu_kernel::resolve_image_blocks_from_cas(
            &mut messages,
            cas,
            Some(conversation_cache.image_cache()),
        )
        .await;
    }

    log::info!(
        "Sending {} messages for context {}",
        messages.len(),
        context_id
    );

    // Track total iterations to prevent infinite loops. The cap is consent-
    // aware (M1-A6): in Collaborative mode the loop yields after one
    // tool round-trip + synthesis so the human stays in the loop; in
    // Autonomous mode the model can chain up to AUTONOMOUS_MAX_ITERATIONS.
    // Read consent once per stream so the cap and any halt message stay
    // coherent if the operator toggles consent mid-flight.
    let consent = kernel.consent_mode().await;
    let max_iterations = iteration_cap_for_consent(consent);
    let mut iteration: u32 = 0;
    // Max retries for transient LLM provider failures (network blips, rate limits)
    const MAX_LLM_RETRIES: u32 = 2;

    // Track last inserted block for ordering - each new block goes after the previous
    let mut last_block_id = after_block_id;
    // The last `Role::Model` / `BlockKind::Text` block this stream produced — the
    // turn's *output*, which the announced `Completed` carries so the OODA Act
    // crystallizes that exact block (design §7). `None` until the model emits
    // text (a tool-only turn, an interrupt before any text, or a hard error all
    // leave it `None`); a `None` Completed schedules nothing downstream.
    let mut output_block_id: Option<kaijutsu_crdt::BlockId> = None;
    // Terminal failure reason for an announced turn. `Some` on any path that
    // ends the turn without a clean Act (a hard cancel/interrupt); the two
    // hard-error early returns publish `Failed` inline and never reach the tail.
    // The scheduler must hear exactly one terminal event per announced turn so
    // it never waits forever — so the tail publishes Completed-or-Failed, gated
    // on `announce_completion` (design §7).
    let mut turn_error: Option<String> = None;

    // Agentic loop - continue until model is done or max iterations
    loop {
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
            log::warn!(
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
            break;
        }

        // Soft interrupt: stop before the next LLM call.
        if interrupt
            .stop_after_turn
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            log::info!(
                "Soft interrupt requested for {}, stopping agentic loop",
                context_id
            );
            break;
        }

        log::info!(
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
                    log::warn!(
                        "Failed to read cache breakpoints for {context_id}: {e} — proceeding without caching"
                    );
                    Vec::new()
                }
            }
        };

        let build_opts = BuildOpts::new(&model_name)
            .with_system(&system_prompt)
            .with_max_tokens(max_output_tokens)
            .with_tools(tools.clone())
            .with_cache_breakpoints(cache_breakpoints);

        // Start streaming with exponential backoff retry for transient failures.
        // Retries cover network blips and rate limits before any content is emitted;
        // mid-stream errors are not retried to avoid duplicate CRDT blocks.
        let mut stream = {
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                match provider.stream(build_opts.clone(), messages.clone()).await {
                    Ok(s) => {
                        if attempt > 1 {
                            log::info!("LLM stream started on attempt {}", attempt);
                        } else {
                            log::info!("LLM stream started successfully");
                        }
                        break s;
                    }
                    Err(e) if attempt <= MAX_LLM_RETRIES => {
                        let delay_secs = attempt as u64;
                        log::warn!(
                            "LLM stream failed (attempt {}/{}): {}, retrying in {}s",
                            attempt,
                            MAX_LLM_RETRIES + 1,
                            e,
                            delay_secs
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
                    }
                    Err(e) => {
                        log::error!(
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
                        // Terminal error for an announced turn — publish Failed so
                        // the scheduler isn't left waiting on this turn forever.
                        if announce_completion {
                            kernel.turn_flows().publish(TurnFlow::Failed {
                                context_id,
                                principal_id: user_principal_id,
                                error: format!("LLM stream failed to start: {e}"),
                            });
                        }
                        return;
                    }
                }
            }
        };

        // Process stream events
        let mut current_block_id: Option<kaijutsu_crdt::BlockId> = None;
        // Collect tool calls for this iteration
        let mut tool_calls: Vec<(String, String, serde_json::Value, TypesToolKind)> = vec![]; // (id, name, input, tool_kind)
        // Track tool_use_id → BlockId mapping for CRDT
        let mut tool_call_blocks: std::collections::HashMap<
            String,
            Option<kaijutsu_crdt::BlockId>,
        > = std::collections::HashMap::new();
        // Collect text output for conversation history
        let mut assistant_text = String::new();
        // Collect thinking output for in-call continuity (A3), one
        // `(text, signature)` entry **per** thinking block (ThinkingStart opens
        // a new entry; deltas append to it; ThinkingEnd stamps its signature).
        // Kept separate — never merged — because Anthropic verifies each
        // `signature_delta` against its own block's text; a later turn echoes
        // them back unmodified and in order. Reset per agentic-loop iteration so
        // it holds *this* turn's reasoning. This is the same per-block shape the
        // hydrator reconstructs from CRDT history, so live and rehydrated turns
        // serialize identically.
        let mut assistant_reasoning: Vec<(String, Option<String>)> = Vec::new();

        log::debug!("Entering stream event loop");
        let mut stream_cancelled = false;
        // Two-layer timeout: total wall-clock cap on the entire completion,
        // and a per-chunk idle guard for providers that open the connection
        // but stop sending tokens.
        let idle_timeout = kernel.timeouts().llm_idle_timeout;
        let request_timeout = kernel.timeouts().llm_request_timeout;
        let total_deadline =
            tokio::time::sleep(request_timeout);
        tokio::pin!(total_deadline);
        'stream: loop {
            // After cancel: only poll the stream (not the cancel signal) so rig
            // can flush its pending block-close + Done events before we stop.
            // Idle guard still applies — a hung post-cancel drain shouldn't
            // pin the loop forever.
            let event = if stream_cancelled {
                match tokio::time::timeout(idle_timeout, stream.next_event()).await {
                    Ok(Some(ev)) => ev,
                    Ok(None) => break 'stream,
                    Err(_) => {
                        log::warn!(
                            "LLM stream idle for {:?} during post-cancel drain ({})",
                            idle_timeout, context_id
                        );
                        StreamEvent::Error(format!(
                            "LLM stream idle for {:?} (post-cancel)",
                            idle_timeout
                        ))
                    }
                }
            } else {
                tokio::select! {
                    _ = interrupt.cancel.cancelled() => {
                        log::info!("Hard interrupt: cancelling LLM stream for {}", context_id);
                        stream.cancel();  // signals rig's AbortHandle → HTTP stream drops
                        stream_cancelled = true;
                        continue 'stream;  // drain one Done event for confirmation
                    }
                    _ = &mut total_deadline => {
                        log::warn!(
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
                            Ok(None) => break 'stream,
                            Err(_) => {
                                log::warn!(
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
            log::debug!("Received stream event: {:?}", event);
            match event {
                StreamEvent::ThinkingStart => {
                    // Open a fresh reasoning entry for this block (its deltas
                    // append here; ThinkingEnd stamps its signature).
                    assistant_reasoning.push((String::new(), None));
                    match documents.insert_block_as(
                        context_id,
                        None,
                        Some(&last_block_id),
                        Role::Model,
                        BlockKind::Thinking,
                        "",
                        Status::Running,
                        ContentType::Plain,
                        Some(PrincipalId::system()),
                    ) {
                        Ok(block_id) => {
                            last_block_id = block_id;
                            current_block_id = Some(block_id);
                        }
                        Err(e) => log::error!("Failed to insert thinking block: {}", e),
                    }
                }

                StreamEvent::ThinkingDelta(text) => {
                    // In-call thinking continuity (A3): accumulate alongside
                    // the CRDT block so the next agentic-loop iteration can
                    // include reasoning in the assistant message. Append to the
                    // current block's entry (defensive: open one if a delta
                    // arrives before ThinkingStart).
                    match assistant_reasoning.last_mut() {
                        Some((t, _)) => t.push_str(&text),
                        None => assistant_reasoning.push((text.clone(), None)),
                    }
                    if let Some(ref block_id) = current_block_id
                        && let Err(e) = documents.append_text_as(
                            context_id,
                            block_id,
                            &text,
                            Some(PrincipalId::system()),
                        )
                    {
                        log::error!("Failed to append thinking text: {}", e);
                    }
                }

                StreamEvent::ThinkingEnd { signature } => {
                    // Stamp this block's verifier (Anthropic's `signature_delta`,
                    // surfaced via ThinkingEnd) onto its own reasoning entry — a
                    // later turn echoes each thinking block back unmodified with
                    // its matching signature. `None` when the provider doesn't
                    // emit one.
                    if let Some(sig) = signature
                        && !sig.is_empty()
                    {
                        if let Some((_, slot)) = assistant_reasoning.last_mut() {
                            *slot = Some(sig.clone());
                        }
                        // Persist the token on the Thinking block so a later
                        // fork / cold-start / attach can rehydrate the reasoning
                        // (the hydrator only rehydrates *signed* Thinking blocks
                        // — see `llm::hydrate`). Best-effort: a failed write
                        // loses cross-turn continuity but never the turn.
                        if let Some(ref block_id) = current_block_id
                            && let Err(e) =
                                documents.set_signature(context_id, block_id, Some(sig.clone()))
                        {
                            log::warn!("Failed to persist thinking signature: {}", e);
                        }
                    }
                    if let Some(ref block_id) = current_block_id {
                        let _ = documents.set_status(context_id, block_id, Status::Done);
                    }
                    current_block_id = None;
                }

                StreamEvent::TextStart => {
                    match documents.insert_block_as(
                        context_id,
                        None,
                        Some(&last_block_id),
                        Role::Model,
                        BlockKind::Text,
                        "",
                        Status::Running,
                        ContentType::Plain,
                        Some(PrincipalId::system()),
                    ) {
                        Ok(block_id) => {
                            last_block_id = block_id;
                            current_block_id = Some(block_id);
                            // This is the turn's model-text output. A later text
                            // block in the same turn supersedes it — Completed
                            // carries the LAST one, the model's final say.
                            output_block_id = Some(block_id);
                        }
                        Err(e) => log::error!("Failed to insert text block: {}", e),
                    }
                }

                StreamEvent::TextDelta(text) => {
                    // Collect text for conversation history
                    assistant_text.push_str(&text);

                    if let Some(ref block_id) = current_block_id
                        && let Err(e) = documents.append_text_as(
                            context_id,
                            block_id,
                            &text,
                            Some(PrincipalId::system()),
                        )
                    {
                        log::error!("Failed to append text: {}", e);
                    }
                }

                StreamEvent::TextEnd => {
                    if let Some(ref block_id) = current_block_id {
                        let _ = documents.set_status(context_id, block_id, Status::Done);
                    }
                    current_block_id = None;
                }

                StreamEvent::ToolUse { id, name, input } => {
                    // Tool kind no longer tracked by a registry category after
                    // Phase 1 M5 — default to Builtin. Phase 2+ can enrich via
                    // broker instance metadata when we have a reason.
                    let tool_kind = TypesToolKind::Builtin;

                    // Store for later execution
                    tool_calls.push((id.clone(), name.clone(), input.clone(), tool_kind));

                    // Insert block and track it — on failure, store None so
                    // the execution future can surface the error to the model
                    // instead of silently losing the tool result.
                    match documents.insert_tool_call_as(
                        context_id,
                        None,
                        Some(&last_block_id),
                        &name,
                        input.clone(),
                        Some(tool_kind),
                        Some(PrincipalId::system()),
                        Some(id.clone()),
                        None,
                    ) {
                        Ok(block_id) => {
                            last_block_id = block_id;
                            tool_call_blocks.insert(id.clone(), Some(block_id));
                        }
                        Err(e) => {
                            log::error!("Failed to insert tool call block for {}: {}", name, e);
                            tool_call_blocks.insert(id.clone(), None);
                        }
                    }
                }

                StreamEvent::ToolResult { .. } => {
                    // This shouldn't happen during streaming - tool results are generated by us
                    log::warn!("Unexpected ToolResult event during streaming");
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
                    // cache read/creation. Gemini's cached-content tokens
                    // land on cache_read too. Unknown / absent → zeros.
                    use kaijutsu_kernel::llm::UsageExtra;
                    let (cache_read, cache_write, reasoning) = match &extra {
                        Some(UsageExtra::OpenAiCompat(d)) => {
                            (d.prompt_cache_hit_tokens, 0, d.reasoning_tokens)
                        }
                        Some(UsageExtra::Claude(c)) => (
                            c.cache_read_input_tokens,
                            c.cache_creation_input_tokens,
                            0,
                        ),
                        Some(UsageExtra::Gemini(g)) => (g.cached_content_tokens, 0, 0),
                        None => (0, 0, 0),
                    };

                    // Tack the usage onto the turn span (llm.* namespace) so
                    // the numbers reach the trace, not just the metrics meter.
                    let span = tracing::Span::current();
                    span.record("llm.usage.input_tokens", input_tokens.unwrap_or(0));
                    span.record("llm.usage.output_tokens", output_tokens.unwrap_or(0));
                    span.record("llm.usage.cache_read_tokens", cache_read);
                    span.record("llm.usage.cache_write_tokens", cache_write);
                    span.record("llm.usage.reasoning_tokens", reasoning);
                    if let Some(ref sr) = stop_reason {
                        span.record("llm.response.stop_reason", sr.as_str());
                    }

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
                    if stream_cancelled {
                        // Hard interrupt confirmation: rig flushed its buffer cleanly.
                        // stop_reason is None on cancel (vs "end_turn"/"tool_use" normally).
                        log::info!(
                            "LLM stream cancelled: tokens_in={:?}, tokens_out={:?}",
                            input_tokens,
                            output_tokens
                        );
                        let _ = documents.insert_block_as(
                            context_id,
                            None,
                            Some(&last_block_id),
                            Role::Model,
                            BlockKind::Text,
                            "⛔ Interrupted",
                            Status::Done,
                            ContentType::Plain,
                            Some(PrincipalId::system()),
                        );
                        // Exit the agentic loop; cleanup runs below.
                        break;
                    }
                    log::info!(
                        "LLM stream completed: stop_reason={:?}, tokens_in={:?}, tokens_out={:?}",
                        stop_reason,
                        input_tokens,
                        output_tokens
                    );
                }

                StreamEvent::Error(err) => {
                    log::error!("LLM stream error: {}", err);
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
                    // Terminal mid-stream error — Failed (not Completed) so an
                    // announced turn never leaves the scheduler waiting.
                    if announce_completion {
                        kernel.turn_flows().publish(TurnFlow::Failed {
                            context_id,
                            principal_id: user_principal_id,
                            error: format!("LLM stream error: {err}"),
                        });
                    }
                    return;
                }
            }
        }

        // After a hard interrupt, break the agentic loop immediately. A cancelled
        // turn is terminal-without-Act → Failed for an announced turn (design §7):
        // the scheduler hears it and moves on rather than waiting on a turn that
        // will never produce an Act.
        if stream_cancelled {
            turn_error = Some("turn interrupted before completion".to_string());
            break;
        }

        // Check if we need to execute tools.
        // rig doesn't expose stop_reason through FinalCompletionResponse — its own
        // agent uses the presence of tool calls as the continuation signal (see
        // rig-core streaming.rs did_call_tool pattern). This is reliable because
        // the API only emits ToolCall content blocks when stop_reason is "tool_use".
        if tool_calls.is_empty() {
            // Add final assistant message to history before saving
            if !assistant_text.is_empty() {
                messages.push(LlmMessage::assistant(&assistant_text));
            }
            log::info!("Agentic loop complete - no tool calls this iteration");
            break;
        }

        // Execute tools concurrently — CRDT handles concurrent block inserts
        log::info!("Executing {} tool calls concurrently", tool_calls.len());

        // Build assistant tool uses (for conversation history)
        let assistant_tool_uses: Vec<ContentBlock> = tool_calls
            .iter()
            .map(|(id, name, input, _)| ContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            })
            .collect();

        // Execute all tools concurrently with streaming results.
        // Pattern mirrors shell_execute: create empty Running block → yield →
        // execute → write content → set final status.
        let futures: Vec<_> = tool_calls
            .into_iter()
            .map(|(tool_use_id, tool_name, input, tool_kind)| {
                let kernel = kernel.clone();
                let documents = documents.clone();
                let tool_ctx = tool_ctx.clone();
                let interrupt = interrupt.clone();
                // Option<Option<BlockId>>: None = not in map (shouldn't happen),
                // Some(None) = insertion failed, Some(Some(id)) = normal
                let tool_call_entry = tool_call_blocks.get(&tool_use_id).cloned();
                async move {
                    let params = input.to_string();
                    log::info!("Executing tool: {} with params: {}", tool_name, params);

                    let tool_call_block_id = match tool_call_entry {
                        Some(Some(id)) => Some(id),
                        Some(None) => {
                            // ToolCall block insertion failed — the model should
                            // know its tool infrastructure is broken rather than
                            // getting a phantom result with no call.
                            log::warn!(
                                "Tool {} (id={}) has no ToolCall block — \
                                 returning error to model",
                                tool_name,
                                tool_use_id,
                            );
                            return (
                                ContentBlock::ToolResult {
                                    tool_use_id,
                                    content: format!(
                                        "Internal error: failed to create ToolCall block for {}. \
                                         The tool was not executed. Try again.",
                                        tool_name,
                                    ),
                                    is_error: true,
                                },
                                None,
                            );
                        }
                        None => None,
                    };

                    // Step 1-2: Create empty ToolResult block and set Running
                    let mut result_block_id = None;
                    if let Some(ref tcb_id) = tool_call_block_id {
                        match documents.insert_tool_result_as(
                            context_id,
                            tcb_id,
                            Some(tcb_id),
                            "",
                            false,
                            None,
                            Some(tool_kind),
                            Some(PrincipalId::system()),
                            Some(tool_use_id.clone()),
                        ) {
                            Ok(id) => {
                                let _ = documents.set_status(context_id, &id, Status::Running);
                                result_block_id = Some(id);
                            }
                            Err(e) => log::warn!(
                                // TODO: surface this in the UI — the model continues with a result
                                // the user never sees, which is confusing to debug. One option:
                                // insert a System/Text block with an error notice so the gap is
                                // visible in the conversation view.
                                "Failed to insert tool result block for {} — \
                                 model will still receive result but user won't see it: {}",
                                tool_name,
                                e,
                            ),
                        }
                    }

                    // Step 3: Let BlockInserted flush to clients before text ops
                    tokio::task::yield_now().await;

                    // Step 4: Execute tool via the Phase 1 broker. Outer
                    // timeout is belt-and-suspenders alongside the broker's
                    // per-instance InstancePolicy cap. interrupt.cancel
                    // (M2-B5) flows through to the broker so a hard
                    // interrupt aborts in-flight work — without this the
                    // user waits the full 120s timeout.
                    const TOOL_TIMEOUT_SECS: u64 = 120;
                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(TOOL_TIMEOUT_SECS),
                        kernel.dispatch_tool_via_broker_with_cancel(
                            &tool_name,
                            &params,
                            &tool_ctx,
                            interrupt.cancel.clone(),
                        ),
                    )
                    .await;

                    let (result_content, is_error, error_payload) = match result {
                        Err(_elapsed) => {
                            log::error!(
                                "Tool {} timed out after {}s",
                                tool_name,
                                TOOL_TIMEOUT_SECS
                            );
                            let payload = kaijutsu_types::ErrorPayload {
                                category: kaijutsu_types::ErrorCategory::Tool,
                                severity: kaijutsu_types::ErrorSeverity::Error,
                                code: Some("tool.timeout".into()),
                                detail: Some(format!(
                                    "Tool '{}' timed out after {}s",
                                    tool_name, TOOL_TIMEOUT_SECS
                                )),
                                span: None,
                                source_kind: Some(kaijutsu_types::BlockKind::ToolResult),
                            };
                            (
                                format!(
                                    "Error: tool '{}' timed out after {}s",
                                    tool_name, TOOL_TIMEOUT_SECS
                                ),
                                true,
                                Some(payload),
                            )
                        }
                        Ok(Ok(r)) if r.success => {
                            log::debug!("Tool {} succeeded: {}", tool_name, r.stdout);
                            (r.stdout, false, None)
                        }
                        Ok(Ok(r)) => {
                            log::warn!("Tool {} failed: {}", tool_name, r.stderr);
                            let payload = kaijutsu_types::ErrorPayload {
                                category: kaijutsu_types::ErrorCategory::Tool,
                                severity: kaijutsu_types::ErrorSeverity::Error,
                                code: None,
                                detail: Some(r.stderr.clone()),
                                span: None,
                                source_kind: Some(kaijutsu_types::BlockKind::ToolResult),
                            };
                            (format!("Error: {}", r.stderr), true, Some(payload))
                        }
                        Ok(Err(e)) => {
                            log::error!("Tool {} execution error: {}", tool_name, e);
                            let payload = kaijutsu_types::ErrorPayload {
                                category: kaijutsu_types::ErrorCategory::Tool,
                                severity: kaijutsu_types::ErrorSeverity::Error,
                                code: None,
                                detail: Some(e.to_string()),
                                span: None,
                                source_kind: Some(kaijutsu_types::BlockKind::ToolResult),
                            };
                            (format!("Execution error: {}", e), true, Some(payload))
                        }
                    };

                    // Step 5: Write result content via CRDT text ops
                    if let Some(ref rb_id) = result_block_id {
                        if !result_content.is_empty()
                            && let Err(e) = documents.edit_text_as(
                                context_id,
                                rb_id,
                                0,
                                &result_content,
                                0,
                                Some(PrincipalId::system()),
                            )
                        {
                            log::error!("Failed to write tool result text: {}", e);
                        }

                        // Step 6: Set final status on result and call blocks
                        let final_status = if is_error {
                            Status::Error
                        } else {
                            Status::Done
                        };
                        let _ = documents.set_status(context_id, rb_id, final_status);
                    }
                    if let Some(ref tcb_id) = tool_call_block_id {
                        let final_status = if is_error {
                            Status::Error
                        } else {
                            Status::Done
                        };
                        let _ = documents.set_status(context_id, tcb_id, final_status);
                    }

                    // Step 6b: Emit structured Error child block if tool failed
                    if let (Some(rb_id), Some(payload)) =
                        (&result_block_id, &error_payload)
                    {
                        if let Err(e) = documents.insert_error_block_as(
                            context_id,
                            rb_id,
                            payload,
                            payload.summary_line(),
                            Some(PrincipalId::system()),
                        ) {
                            log::warn!(
                                "Failed to insert error block for tool {}: {}",
                                tool_name,
                                e
                            );
                        }
                    }

                    // Step 7: Return for conversation history
                    (
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content: result_content,
                            is_error,
                        },
                        result_block_id,
                    )
                }
            })
            .collect();

        let results_with_ids = futures::future::join_all(futures).await;

        // Unzip and update last_block_id so the next iteration's blocks
        // appear after tool results, not after tool calls.
        let mut tool_results = Vec::new();
        for (content_block, block_id_opt) in results_with_ids {
            tool_results.push(content_block);
            if let Some(id) = block_id_opt {
                last_block_id = id;
            }
        }

        // Add assistant message with tool uses to conversation. Preserve
        // accumulated thinking (A3), one Reasoning block per thinking block, so
        // multi-step tool turns keep the model's chain-of-thought intact within
        // this `process_llm_stream` invocation. Each signature comes from the
        // provider via `StreamEvent::ThinkingEnd.signature` — load-bearing for
        // Anthropic when extended thinking is enabled and tool_use is in the
        // same turn (the builder skips any empty-text entries).
        let reasoning = std::mem::take(&mut assistant_reasoning);
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
    log::info!(
        "Conversation cache updated: {} messages for cell {}",
        messages.len(),
        context_id
    );

    // Each mutation is now journaled via journal_op — no explicit save needed.

    // Clean up interrupt state — only remove if our generation still matches.
    // A newer stream may have replaced our entry; removing it would be a bug.
    {
        let mut map = context_interrupts.write().await;
        if let Some(state) = map.get(&context_id)
            && state.generation == interrupt_generation
        {
            map.remove(&context_id);
        }
    }

    log::info!("LLM stream processing complete for cell {}", context_id);

    // Announce the turn outcome at ACTUAL stream end (design §7) — the publish
    // moved here from the spawn site so it carries the real `output_block_id`
    // (the model's last text block) rather than firing at spawn and racing the
    // model. Only announced (autonomous turn-driver) turns publish; interactive
    // human prompts stay silent so the composer's OODA Act never crystallizes a
    // human-prompted turn. Exactly one terminal event per announced turn — the
    // two hard-error early returns already published Failed and never reach here.
    if announce_completion {
        match turn_error {
            None => kernel.turn_flows().publish(TurnFlow::Completed {
                context_id,
                principal_id: user_principal_id,
                output_block_id,
            }),
            Some(error) => kernel.turn_flows().publish(TurnFlow::Failed {
                context_id,
                principal_id: user_principal_id,
                error,
            }),
        };
    }
}

#[cfg(test)]
mod publish_tests {
    //! T15 (design-chameleon-batch1-f2-notation §7/§16) — the turn-completion
    //! publish moves from stream *spawn* (the rpc.rs:391 race) to stream *end*,
    //! carrying the model's output block id, and only for *announced* turns.
    //!
    //! This is the smallest honest test of the publish site (the design allows
    //! it over the heavy mock-provider SSH e2e harness, which the project's
    //! test discipline steers away from for --lib runs — russh teardown noise).
    //! It drives `process_llm_stream` directly with a Mock provider against a
    //! real ephemeral kernel + block store, so the moved publish and the
    //! `announce_completion` gate are exercised end-to-end through the actual
    //! stream loop, not a stub.
    use super::*;
    use kaijutsu_kernel::block_store::{BlockStore, DocumentKind};
    use kaijutsu_kernel::flows::{FlowBus, SharedBlockFlowBus};
    use kaijutsu_kernel::kernel_db::KernelDb;
    use kaijutsu_kernel::llm::{MockClient, Provider};
    use kaijutsu_types::SessionId;

    use crate::interrupt::ContextInterruptState;
    use crate::rpc::ConversationCache;

    /// Build the args and run one `process_llm_stream` against a Mock provider.
    /// Returns the documents store and the principal so the caller can inspect
    /// the inserted blocks.
    async fn drive_one_turn(
        announce_completion: bool,
        kernel: Arc<Kernel>,
    ) -> (SharedBlockStore, ContextId, PrincipalId) {
        let bus: SharedBlockFlowBus = Arc::new(FlowBus::new(256));
        let documents: SharedBlockStore =
            Arc::new(BlockStore::with_flows(PrincipalId::new(), bus));
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

        let provider = Arc::new(Provider::Mock(MockClient::new(
            "X:1\nK:C\nCDEF|\n",
        )));
        let kernel_db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let conversation_cache = Arc::new(ConversationCache::new(8));
        let interrupt = ContextInterruptState::new(1);
        let context_interrupts = Arc::new(TokioRwLock::new(HashMap::new()));
        let tool_ctx = kaijutsu_kernel::ExecContext::new(
            player,
            ctx,
            std::path::PathBuf::from("/"),
            SessionId::new(),
            kernel.id(),
        );

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
            conversation_cache,
            player,
            tool_ctx,
            interrupt,
            1,
            context_interrupts,
            announce_completion,
        )
        .await;

        (documents, ctx, player)
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

                let (documents, ctx, _player) = drive_one_turn(true, kernel.clone()).await;

                // The stream has fully ended; exactly one Completed is queued.
                let msg = sub
                    .try_recv()
                    .expect("an announced turn publishes Completed at stream end");
                let output_id = match msg.payload {
                    TurnFlow::Completed {
                        context_id,
                        output_block_id,
                        ..
                    } => {
                        assert_eq!(context_id, ctx);
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

    /// An interactive (un-announced) turn publishes NOTHING — the human-prompt
    /// paths must never feed the composer's OODA Act (design §7).
    #[tokio::test]
    async fn interactive_turn_publishes_nothing() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("publish-test").await);
                let mut sub = kernel.turn_flows().subscribe("turn.completed");

                let (_documents, _ctx, _player) = drive_one_turn(false, kernel.clone()).await;

                assert!(
                    sub.try_recv().is_none(),
                    "an un-announced (interactive) turn must not publish Completed"
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
    async fn drive_failed_hydration(announce_completion: bool, kernel: Arc<Kernel>) -> ContextId {
        let bus: SharedBlockFlowBus = Arc::new(FlowBus::new(256));
        let documents: SharedBlockStore =
            Arc::new(BlockStore::with_flows(PrincipalId::new(), bus));
        // NO create_document for `ctx` → `block_snapshots` errors DocumentNotFound →
        // hydration fails before the stream begins. The anchor is fabricated; the
        // missing document is what we are exercising.
        let ctx = ContextId::new();
        let player = PrincipalId::new();
        let after = kaijutsu_crdt::BlockId::new(ctx, player, 0);

        let provider = Arc::new(Provider::Mock(MockClient::new("X:1\nK:C\nCDEF|\n")));
        let kernel_db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let conversation_cache = Arc::new(ConversationCache::new(8));
        let interrupt = ContextInterruptState::new(1);
        let context_interrupts = Arc::new(TokioRwLock::new(HashMap::new()));
        let tool_ctx = kaijutsu_kernel::ExecContext::new(
            player,
            ctx,
            std::path::PathBuf::from("/"),
            SessionId::new(),
            kernel.id(),
        );

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
            conversation_cache,
            player,
            tool_ctx,
            interrupt,
            1,
            context_interrupts,
            announce_completion,
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

                let ctx = drive_failed_hydration(true, kernel.clone()).await;

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

    #[tokio::test]
    async fn failed_hydration_stays_silent_when_interactive() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let kernel = Arc::new(Kernel::new_ephemeral("hydrate-fail").await);
                let mut failed = kernel.turn_flows().subscribe("turn.failed");

                let _ctx = drive_failed_hydration(false, kernel.clone()).await;

                assert!(
                    failed.try_recv().is_none(),
                    "an interactive turn announces nothing, even on hydration failure"
                );
            })
            .await;
    }
}
