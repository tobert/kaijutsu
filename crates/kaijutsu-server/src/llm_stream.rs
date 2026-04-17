//! LLM streaming + agentic tool-call loop.
//!
//! This module owns the background task that talks to a `RigProvider`, parses
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
use kaijutsu_kernel::llm::stream::{LlmStream, StreamEvent, StreamRequest};
use kaijutsu_kernel::llm::{ContentBlock, ToolDefinition};
use kaijutsu_kernel::{Kernel, LlmMessage, RigProvider, SharedBlockStore};
use kaijutsu_types::ToolKind as TypesToolKind;
use kaijutsu_types::{ContextId, PrincipalId};

use crate::interrupt::ContextInterruptState;
use crate::rpc::{ConversationCache, SharedKernelState};

/// Build tool definitions visible to the LLM in this context.
///
/// Phase 5 M4: `ToolFilter` retired (D-54). Per-context tool curation is
/// expressed by the `ContextToolBinding`'s `allowed_instances` and by
/// `HookPhase::ListTools` hooks (D-56) — both applied inside
/// `Broker::list_visible_tools` via `list_tool_defs_via_broker`. This
/// function now pass-throughs the broker output unmodified.
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
    content: &str,
    model: Option<&str>,
    after_block_id: &kaijutsu_crdt::BlockId,
    tool_ctx: kaijutsu_kernel::ExecContext,
    user_agent_id: PrincipalId,
) -> Result<(), capnp::Error> {
    let documents = kernel.documents.clone();
    let kernel_arc = kernel.kernel.clone();
    let config_backend = kernel.config_backend.clone();
    let conversation_cache = kernel.conversation_cache.clone();
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

    // Read per-context model from DriftRouter (quick read, release lock)
    let (ctx_model, ctx_provider_name) = {
        let drift = kernel_arc.drift().read().await;
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
            Some(h) => (h.model.clone(), h.provider.clone()),
            None => (None, None),
        }
    };

    // Resolve provider + model from LLM registry
    // Priority: explicit param > per-context (DriftRouter) > kernel default
    let (provider, model_name, max_output_tokens) = {
        let registry = kernel_arc.llm().read().await;
        let max_tokens = registry.max_output_tokens();

        let effective_model = model.map(|m| m.to_string()).or(ctx_model);

        match effective_model {
            Some(name) => {
                // Prefer per-context provider, then resolve via registry
                let provider = ctx_provider_name
                    .as_deref()
                    .and_then(|pn| registry.get(pn))
                    .or_else(|| registry.default_provider())
                    .ok_or_else(|| {
                        log::error!("No LLM provider configured");
                        capnp::Error::failed(
                            "No LLM provider configured (check models.toml)".into(),
                        )
                    })?;
                (provider, name, max_tokens)
            }
            None => {
                // No model anywhere — kernel default
                let p = registry.default_provider().ok_or_else(|| {
                    log::error!("No LLM provider configured");
                    capnp::Error::failed("No LLM provider configured (check models.toml)".into())
                })?;
                let m = registry
                    .default_model()
                    .unwrap_or(kaijutsu_kernel::DEFAULT_MODEL)
                    .to_string();
                (p, m, max_tokens)
            }
        }
    };

    // Build tool definitions via the broker (binding + ListTools filter do
    // the curation — D-54 retired the legacy post-filter).
    let tools = build_tool_definitions(&kernel_arc, context_id, user_agent_id).await;

    log::info!(
        "Spawning LLM stream: context={}, model={}",
        context_id,
        model_name
    );

    let content = content.to_owned();
    let after_block_id = *after_block_id;

    tokio::task::spawn_local(process_llm_stream(
        provider,
        documents,
        context_id,
        content,
        model_name,
        kernel_arc,
        tools,
        after_block_id,
        system_prompt,
        max_output_tokens,
        conversation_cache,
        user_agent_id,
        tool_ctx,
        interrupt,
        interrupt_generation,
        context_interrupts,
    ));

    Ok(())
}

/// Map a tool's registry category to the appropriate `ToolKind`.
///
/// Categories in use: "kernel", "block", "drift", "file", "mcp".
/// Only "mcp" maps to `Mcp`; everything else is `Builtin`.
fn tool_kind_for_category(category: &str) -> TypesToolKind {
    match category {
        "mcp" => TypesToolKind::Mcp,
        _ => TypesToolKind::Builtin,
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
async fn process_llm_stream(
    provider: Arc<RigProvider>,
    documents: SharedBlockStore,
    context_id: ContextId,
    content: String,
    model_name: String,
    kernel: Arc<Kernel>,
    tools: Vec<ToolDefinition>,
    after_block_id: kaijutsu_crdt::BlockId,
    system_prompt: String,
    max_output_tokens: u64,
    conversation_cache: Arc<ConversationCache>,
    // TODO: use for per-user attribution on model-generated blocks
    _user_agent_id: PrincipalId,
    tool_ctx: kaijutsu_kernel::ExecContext,
    interrupt: Arc<ContextInterruptState>,
    interrupt_generation: u64,
    context_interrupts: Arc<TokioRwLock<HashMap<ContextId, Arc<ContextInterruptState>>>>,
) {
    // Get per-context lock — held for the entire stream, serializing
    // concurrent prompts to the same context (Fix D+E).
    let cache_lock = conversation_cache.get_or_create(context_id);
    let mut messages = cache_lock.lock().await;

    // Always re-hydrate from blocks — ensures shell commands, MCP tool calls,
    // and other agent blocks added between prompts are visible to the LLM.
    // block_snapshots() reads from in-memory DashMap, sub-millisecond for typical conversations.
    // The user block was already inserted before this function was called, so
    // hydrated messages include it — no explicit push needed.
    match documents.block_snapshots(context_id) {
        Ok(blocks) => {
            let hydrated = kaijutsu_kernel::hydrate_from_blocks(&blocks);
            log::info!(
                "Hydrated {} messages from {} blocks for context {}",
                hydrated.len(),
                blocks.len(),
                context_id
            );
            *messages = hydrated;
        }
        Err(e) => {
            // Hydration failed — fall back to appending the user message to
            // whatever the cache currently holds (may be stale or empty).
            // TODO: surface this as a user-visible error block instead of silently
            // falling back. An empty cache means the model sees no history, which
            // produces confusing responses after cache eviction or first prompt
            // post-restart. Consider inserting a System block explaining the gap.
            log::warn!(
                "Could not hydrate cache for {}: {}, falling back to cache + append",
                context_id,
                e
            );
            messages.push(LlmMessage::user(&content));
        }
    }

    log::info!(
        "Sending {} messages for context {}",
        messages.len(),
        context_id
    );

    // Track total iterations to prevent infinite loops
    let max_iterations = 20;
    let mut iteration = 0;
    // Max retries for transient LLM provider failures (network blips, rate limits)
    const MAX_LLM_RETRIES: u32 = 2;

    // Track last inserted block for ordering - each new block goes after the previous
    let mut last_block_id = after_block_id;

    // Agentic loop - continue until model is done or max iterations
    loop {
        iteration += 1;
        if iteration > max_iterations {
            log::warn!(
                "Agentic loop hit max iterations ({}), stopping",
                max_iterations
            );
            let _ = documents.insert_block_as(
                context_id,
                None,
                Some(&last_block_id),
                Role::Model,
                BlockKind::Text,
                "⚠️ Maximum tool iterations reached",
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

        // Create streaming request with tools
        let stream_request = StreamRequest::new(&model_name, messages.clone())
            .with_system(&system_prompt)
            .with_max_tokens(max_output_tokens)
            .with_tools(tools.clone());

        // Start streaming with exponential backoff retry for transient failures.
        // Retries cover network blips and rate limits before any content is emitted;
        // mid-stream errors are not retried to avoid duplicate CRDT blocks.
        let mut stream = {
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                match provider.stream(stream_request.clone()).await {
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

        log::debug!("Entering stream event loop");
        let mut stream_cancelled = false;
        'stream: loop {
            // After cancel: only poll the stream (not the cancel signal) so rig
            // can flush its pending block-close + Done events before we stop.
            let event = if stream_cancelled {
                match stream.next_event().await {
                    Some(ev) => ev,
                    None => break 'stream,
                }
            } else {
                tokio::select! {
                    _ = interrupt.cancel.cancelled() => {
                        log::info!("Hard interrupt: cancelling LLM stream for {}", context_id);
                        stream.cancel();  // signals rig's AbortHandle → HTTP stream drops
                        stream_cancelled = true;
                        continue 'stream;  // drain one Done event for confirmation
                    }
                    maybe_event = stream.next_event() => {
                        match maybe_event {
                            Some(ev) => ev,
                            None => break 'stream,
                        }
                    }
                }
            };
            log::debug!("Received stream event: {:?}", event);
            match event {
                StreamEvent::ThinkingStart => {
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

                StreamEvent::ThinkingEnd => {
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
                } => {
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
                    return;
                }
            }
        }

        // After a hard interrupt, break the agentic loop immediately.
        if stream_cancelled {
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
                    // per-instance InstancePolicy cap.
                    const TOOL_TIMEOUT_SECS: u64 = 120;
                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(TOOL_TIMEOUT_SECS),
                        kernel.dispatch_tool_via_broker(&tool_name, &params, &tool_ctx),
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

        // Add assistant message with tool uses to conversation
        messages.push(LlmMessage::with_tool_uses(
            if assistant_text.is_empty() {
                None
            } else {
                Some(assistant_text)
            },
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
}
