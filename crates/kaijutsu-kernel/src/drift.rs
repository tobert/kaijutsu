//! Kernel-native drift — cross-context communication and content transfer.
//!
//! The DriftRouter is the central coordinator for moving content between contexts
//! *within a kernel*. It maintains a registry of all contexts (keyed by short IDs)
//! and a staging queue for drift operations.
//!
//! # Architecture
//!
//! All contexts share the same `SharedBlockStore`. Drift reads blocks from a
//! source context's document, optionally summarizes via LLM, and injects as a
//! `BlockKind::Drift` block into the target context's document.
//!
//! # Flow
//!
//! ```text
//! drift push <ctx> "content"
//!       │
//!       ▼
//! DriftRouter.stage(StagedDrift { source, target, content, ... })
//!       │
//! drift flush
//!       │
//!       ▼
//! DriftRouter.flush() → insert_from_snapshot() on target document
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::RwLock;

use kaijutsu_crdt::{BlockKind, BlockSnapshot, DriftKind, Role};

use crate::block_store::SharedBlockStore;
use crate::tools::{EngineArgs, ExecResult, ExecutionEngine};

/// Short ID length — first 6 hex chars of a UUID.
const SHORT_ID_LEN: usize = 6;

/// Shared, thread-safe DriftRouter reference.
pub type SharedDriftRouter = Arc<RwLock<DriftRouter>>;

/// Create a new shared DriftRouter.
pub fn shared_drift_router() -> SharedDriftRouter {
    Arc::new(RwLock::new(DriftRouter::new()))
}

// ============================================================================
// ContextHandle — registered context within a kernel
// ============================================================================

/// A registered context, mapping a short ID to a context within this kernel.
///
/// Unlike the previous server-level `ContextHandle` (which tracked `kernel_id`),
/// this tracks `context_name` + `document_id` since all contexts share the same
/// kernel's `SharedBlockStore`.
#[derive(Debug, Clone)]
pub struct ContextHandle {
    /// Short hex ID (e.g., "a1b2c3") — derived from a UUID.
    pub short_id: String,
    /// Context name (e.g., "main", "debug-session", "refactor-auth").
    pub context_name: String,
    /// Primary document ID in the shared BlockStore.
    pub document_id: String,
    /// Working directory in VFS (e.g., "/mnt/kaijutsu").
    pub pwd: Option<String>,
    /// Provider name if configured (e.g., "anthropic", "gemini").
    pub provider: Option<String>,
    /// Model name if configured (e.g., "claude-opus-4-6", "gemini-2.0-flash").
    pub model: Option<String>,
    /// Short ID of parent context (for fork lineage).
    pub parent_short_id: Option<String>,
    /// Creation timestamp (Unix epoch seconds).
    pub created_at: u64,
}

// ============================================================================
// StagedDrift — queued drift operation
// ============================================================================

/// A drift operation staged in the queue, pending flush.
#[derive(Debug, Clone)]
pub struct StagedDrift {
    /// Unique ID for this staged operation.
    pub id: u64,
    /// Short ID of the source context.
    pub source_ctx: String,
    /// Short ID of the target context.
    pub target_ctx: String,
    /// Content to transfer.
    pub content: String,
    /// Model that produced this content (if known).
    pub source_model: Option<String>,
    /// How this drift arrived.
    pub drift_kind: DriftKind,
    /// Creation timestamp (Unix epoch seconds).
    pub created_at: u64,
}

// ============================================================================
// DriftRouter — central coordinator
// ============================================================================

/// Central drift coordinator for a kernel.
///
/// Manages drift between contexts within a single kernel. All contexts share
/// the same `SharedBlockStore`, so drift only needs context names and document
/// IDs — no cross-kernel lookup required.
#[derive(Debug)]
pub struct DriftRouter {
    /// All registered contexts, keyed by short_id.
    contexts: HashMap<String, ContextHandle>,
    /// Staging queue for pending drift operations.
    staging: Vec<StagedDrift>,
    /// Counter for staged drift IDs.
    next_staged_id: u64,
    /// Reverse lookup: context_name → short_id.
    context_to_short: HashMap<String, String>,
}

impl Default for DriftRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl DriftRouter {
    /// Create a new empty drift router.
    pub fn new() -> Self {
        Self {
            contexts: HashMap::new(),
            staging: Vec::new(),
            next_staged_id: 1,
            context_to_short: HashMap::new(),
        }
    }

    /// Register a context.
    ///
    /// Generates a short ID from a fresh UUID. If the first 6 hex chars
    /// collide with an existing context, extends until unique.
    pub fn register(
        &mut self,
        context_name: &str,
        document_id: &str,
        parent_short_id: Option<&str>,
    ) -> String {
        let uuid = uuid::Uuid::new_v4();
        let hex = uuid.as_simple().to_string();
        let mut short_id = hex[..SHORT_ID_LEN].to_string();

        // Handle collisions by extending
        let mut len = SHORT_ID_LEN;
        while self.contexts.contains_key(&short_id) && len < hex.len() {
            len += 1;
            short_id = hex[..len].to_string();
        }

        let handle = ContextHandle {
            short_id: short_id.clone(),
            context_name: context_name.to_string(),
            document_id: document_id.to_string(),
            pwd: None,
            provider: None,
            model: None,
            parent_short_id: parent_short_id.map(|s| s.to_string()),
            created_at: now_epoch(),
        };

        self.context_to_short
            .insert(context_name.to_string(), short_id.clone());
        self.contexts.insert(short_id.clone(), handle);
        short_id
    }

    /// Unregister a context (e.g., when a context is destroyed).
    pub fn unregister(&mut self, short_id: &str) {
        if let Some(handle) = self.contexts.remove(short_id) {
            self.context_to_short.remove(&handle.context_name);
        }
    }

    /// Look up a context by short ID.
    pub fn get(&self, short_id: &str) -> Option<&ContextHandle> {
        self.contexts.get(short_id)
    }

    /// Look up context short ID by context name.
    pub fn short_id_for_context(&self, context_name: &str) -> Option<&str> {
        self.context_to_short
            .get(context_name)
            .map(|s| s.as_str())
    }

    /// Update provider/model for a context.
    pub fn configure_llm(
        &mut self,
        short_id: &str,
        provider: &str,
        model: &str,
    ) -> Result<(), DriftError> {
        let handle = self
            .contexts
            .get_mut(short_id)
            .ok_or_else(|| DriftError::UnknownContext(short_id.to_string()))?;
        handle.provider = Some(provider.to_string());
        handle.model = Some(model.to_string());
        Ok(())
    }

    /// Set the working directory for a context.
    pub fn set_pwd(
        &mut self,
        context_name: &str,
        pwd: Option<String>,
    ) -> Result<(), DriftError> {
        let short_id = self
            .context_to_short
            .get(context_name)
            .ok_or_else(|| DriftError::UnknownContext(context_name.to_string()))?
            .clone();
        let handle = self
            .contexts
            .get_mut(&short_id)
            .ok_or_else(|| DriftError::UnknownContext(short_id))?;
        handle.pwd = pwd;
        Ok(())
    }

    /// List all registered contexts.
    pub fn list_contexts(&self) -> Vec<&ContextHandle> {
        let mut contexts: Vec<_> = self.contexts.values().collect();
        contexts.sort_by_key(|c| c.created_at);
        contexts
    }

    /// Stage a drift operation for later flush.
    ///
    /// Returns the staged drift ID.
    pub fn stage(
        &mut self,
        source_ctx: &str,
        target_ctx: &str,
        content: String,
        source_model: Option<String>,
        drift_kind: DriftKind,
    ) -> Result<u64, DriftError> {
        // Validate both contexts exist
        if !self.contexts.contains_key(source_ctx) {
            return Err(DriftError::UnknownContext(source_ctx.to_string()));
        }
        if !self.contexts.contains_key(target_ctx) {
            return Err(DriftError::UnknownContext(target_ctx.to_string()));
        }

        let id = self.next_staged_id;
        self.next_staged_id += 1;

        self.staging.push(StagedDrift {
            id,
            source_ctx: source_ctx.to_string(),
            target_ctx: target_ctx.to_string(),
            content,
            source_model,
            drift_kind,
            created_at: now_epoch(),
        });

        Ok(id)
    }

    /// Cancel a staged drift by ID.
    pub fn cancel(&mut self, staged_id: u64) -> bool {
        let len_before = self.staging.len();
        self.staging.retain(|s| s.id != staged_id);
        self.staging.len() < len_before
    }

    /// View the staging queue.
    pub fn queue(&self) -> &[StagedDrift] {
        &self.staging
    }

    /// Drain the staging queue, returning staged drifts for processing.
    ///
    /// If `for_context` is `Some`, only drains items where the source or target
    /// matches the given context. Otherwise drains everything.
    ///
    /// The caller is responsible for injecting blocks into target documents.
    pub fn drain(&mut self, for_context: Option<&str>) -> Vec<StagedDrift> {
        match for_context {
            None => std::mem::take(&mut self.staging),
            Some(ctx) => {
                let (matched, remaining): (Vec<_>, Vec<_>) =
                    std::mem::take(&mut self.staging)
                        .into_iter()
                        .partition(|s| s.source_ctx == ctx || s.target_ctx == ctx);
                self.staging = remaining;
                matched
            }
        }
    }

    /// Build a BlockSnapshot for a staged drift, ready for insertion.
    pub fn build_drift_block(drift: &StagedDrift, author: &str) -> BlockSnapshot {
        BlockSnapshot::drift(
            kaijutsu_crdt::BlockId::new("", "", 0), // ID assigned by document
            None,                                     // parent set during insertion
            drift.content.clone(),
            author,
            drift.source_ctx.clone(),
            drift.source_model.clone(),
            drift.drift_kind.clone(),
        )
    }
}

// ============================================================================
// DriftError
// ============================================================================

/// Errors from drift operations.
#[derive(Debug, thiserror::Error)]
pub enum DriftError {
    #[error("unknown context: {0}")]
    UnknownContext(String),
    #[error("document error: {0}")]
    DocumentError(String),
    #[error("LLM error: {0}")]
    LlmError(String),
}

// ============================================================================
// Distillation helpers
// ============================================================================

/// System prompt for distillation — used when summarizing context for transfer.
pub const DISTILLATION_SYSTEM_PROMPT: &str = "\
Summarize this conversation for transfer to another context. \
Be concise. Preserve: key findings, decisions made, code references, \
and open questions. Format as a briefing, not a transcript. \
Use bullet points for clarity. Keep it under 500 words.";

/// Build a distillation prompt from a document's blocks.
///
/// Formats the conversation history as a transcript suitable for LLM summarization.
pub fn build_distillation_prompt(
    blocks: &[BlockSnapshot],
    directed_prompt: Option<&str>,
) -> String {
    let mut transcript = String::new();
    transcript.push_str("# Conversation to summarize\n\n");

    for block in blocks {
        let role_label = match block.role {
            Role::User => "User",
            Role::Model => "Assistant",
            Role::System => "System",
            Role::Tool => "Tool",
        };

        let kind_suffix = match block.kind {
            BlockKind::Thinking => " (thinking)",
            BlockKind::ToolCall => " (tool call)",
            BlockKind::ToolResult => " (tool result)",
            BlockKind::ShellCommand => " (shell)",
            BlockKind::ShellOutput => " (output)",
            BlockKind::Drift => " (drift)",
            BlockKind::Text => "",
        };

        // Skip empty blocks
        if block.content.is_empty() {
            continue;
        }

        // Truncate very long blocks — find a valid UTF-8 boundary near 2000 bytes
        let content = if block.content.len() > 2000 {
            let mut end = 2000;
            while end > 0 && !block.content.is_char_boundary(end) {
                end -= 1;
            }
            format!(
                "{}... [truncated, {} bytes total]",
                &block.content[..end],
                block.content.len()
            )
        } else {
            block.content.clone()
        };

        transcript.push_str(&format!(
            "**{}{}**: {}\n\n",
            role_label, kind_suffix, content
        ));
    }

    if let Some(prompt) = directed_prompt {
        transcript.push_str(&format!(
            "\n---\n\nFocus your summary on: {}\n",
            prompt
        ));
    }

    transcript
}

// ============================================================================
// Commit message helpers
// ============================================================================

/// System prompt for LLM-generated commit messages.
pub const COMMIT_SYSTEM_PROMPT: &str = "\
Generate a concise git commit message from the diff and conversation context below. \
Use conventional commit format (type(scope): description). \
Focus on the 'why' over the 'what'. Keep the subject line under 72 chars. \
Add a body paragraph only if the change is non-obvious.";

/// Build a commit prompt from a diff and recent conversation blocks.
///
/// Formats the diff (truncated at 8000 bytes) and last ~10 conversation blocks
/// as context for LLM commit message generation.
pub fn build_commit_prompt(diff: &str, context_blocks: &[BlockSnapshot]) -> String {
    let mut prompt = String::new();

    // Diff section (truncated for token budget)
    prompt.push_str("## Git Diff\n\n```diff\n");
    if diff.len() > 8000 {
        let mut end = 8000;
        while end > 0 && !diff.is_char_boundary(end) {
            end -= 1;
        }
        prompt.push_str(&diff[..end]);
        prompt.push_str(&format!(
            "\n... [truncated, {} bytes total]\n",
            diff.len()
        ));
    } else {
        prompt.push_str(diff);
    }
    prompt.push_str("```\n\n");

    // Conversation context section (last ~10 blocks)
    prompt.push_str("## Conversation Context\n\n");
    let recent = if context_blocks.len() > 10 {
        &context_blocks[context_blocks.len() - 10..]
    } else {
        context_blocks
    };

    for block in recent {
        let role_label = match block.role {
            Role::User => "User",
            Role::Model => "Assistant",
            Role::System => "System",
            Role::Tool => "Tool",
        };

        if block.content.is_empty() {
            continue;
        }

        // Truncate long blocks at 2000 bytes
        let content = if block.content.len() > 2000 {
            let mut end = 2000;
            while end > 0 && !block.content.is_char_boundary(end) {
                end -= 1;
            }
            format!(
                "{}... [truncated, {} bytes total]",
                &block.content[..end],
                block.content.len()
            )
        } else {
            block.content.clone()
        };

        prompt.push_str(&format!("**{}**: {}\n\n", role_label, content));
    }

    prompt
}

// ============================================================================
// DriftEngine — ExecutionEngine for the `drift` kaish command
// ============================================================================

/// Execution engine for the `drift` shell command.
///
/// **Context binding limitation:** Currently instantiated once per kernel with a
/// fixed `context_name` (typically `"default"`). Multi-context scenarios require
/// either per-context engine instances or a mechanism to pass the active context
/// through `ExecutionEngine::execute()`. See also `GitEngine`.
///
/// Provides commands for cross-context drift within a kernel:
/// - `drift ls` — list contexts in this kernel
/// - `drift push <ctx> "content"` — stage content for target context
/// - `drift push <ctx> --summarize` — LLM-summarize before staging
/// - `drift queue` — show staged drifts
/// - `drift cancel <id>` — cancel staged drift
/// - `drift flush` — deliver staged drifts to target documents
/// - `drift pull <ctx> [prompt]` — read + summarize source context
/// - `drift merge <ctx>` — summarize forked context into parent
/// - `drift help` — usage info
pub struct DriftEngine {
    /// Weak reference to the kernel (avoids Kernel→ToolRegistry→DriftEngine→Kernel cycle).
    kernel: std::sync::Weak<crate::kernel::Kernel>,
    /// Shared BlockStore (all contexts' documents).
    documents: SharedBlockStore,
    /// Which context this engine operates as (the "caller").
    context_name: String,
}

impl DriftEngine {
    /// Create a new drift engine.
    ///
    /// Takes an `Arc<Kernel>` but stores it as `Weak` to avoid a reference cycle
    /// (Kernel owns ToolRegistry which owns DriftEngine).
    pub fn new(
        kernel: &Arc<crate::kernel::Kernel>,
        documents: SharedBlockStore,
        context_name: impl Into<String>,
    ) -> Self {
        Self {
            kernel: Arc::downgrade(kernel),
            documents,
            context_name: context_name.into(),
        }
    }

    /// Upgrade the weak kernel reference, or return an error.
    fn kernel(&self) -> Result<Arc<crate::kernel::Kernel>, String> {
        self.kernel.upgrade().ok_or_else(|| "kernel has been dropped".to_string())
    }

    /// Get the caller's short ID from the router.
    async fn caller_short_id(&self) -> Result<String, String> {
        let kernel = self.kernel()?;
        let router = kernel.drift().read().await;
        router
            .short_id_for_context(&self.context_name)
            .map(|s| s.to_string())
            .ok_or_else(|| format!("context '{}' not registered in drift router", self.context_name))
    }

    fn execute_inner(
        &self,
        args: Vec<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ExecResult, String>> + Send + '_>>
    {
        Box::pin(async move {
            if args.is_empty() {
                return self.show_help();
            }

            match args[0].as_str() {
                "ls" | "list" => self.cmd_ls().await,
                "push" => self.cmd_push(&args[1..]).await,
                "queue" | "q" => self.cmd_queue().await,
                "cancel" => self.cmd_cancel(&args[1..]).await,
                "flush" => self.cmd_flush().await,
                "pull" => self.cmd_pull(&args[1..]).await,
                "merge" => self.cmd_merge(&args[1..]).await,
                "help" | "-h" | "--help" => self.show_help(),
                other => Err(format!(
                    "Unknown subcommand: {}. Use 'drift help' for usage.",
                    other
                )),
            }
        })
    }

    async fn cmd_ls(&self) -> Result<ExecResult, String> {
        let kernel = self.kernel()?;
        let router = kernel.drift().read().await;
        let contexts = router.list_contexts();
        let caller_short = router
            .short_id_for_context(&self.context_name)
            .unwrap_or("");

        let mut output = String::new();
        for ctx in &contexts {
            let marker = if ctx.short_id == caller_short {
                "* "
            } else {
                "  "
            };
            let provider_info = match (&ctx.provider, &ctx.model) {
                (Some(p), Some(m)) => format!(" ({}:{})", p, m),
                (Some(p), None) => format!(" ({})", p),
                _ => String::new(),
            };
            let parent_info = ctx
                .parent_short_id
                .as_ref()
                .map(|p| format!(" [parent: {}]", p))
                .unwrap_or_default();
            output.push_str(&format!(
                "{}{} {} [doc: {}]{}{}\n",
                marker, ctx.short_id, ctx.context_name, ctx.document_id, provider_info, parent_info,
            ));
        }

        if contexts.is_empty() {
            output.push_str("No contexts registered.\n");
        }

        Ok(ExecResult::success(output))
    }

    async fn cmd_push(&self, args: &[String]) -> Result<ExecResult, String> {
        if args.is_empty() {
            return Err("Usage: drift push <target-ctx> \"content\" [--summarize]".to_string());
        }

        let kernel = self.kernel()?;
        let target_ctx = &args[0];
        let summarize = args.iter().any(|a| a == "--summarize" || a == "-s");

        let content = args[1..]
            .iter()
            .filter(|a| *a != "--summarize" && *a != "-s")
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");

        let caller_short = self.caller_short_id().await?;

        if summarize {
            // LLM-summarize path
            let router = kernel.drift().read().await;
            let source_handle = router
                .get(&caller_short)
                .ok_or_else(|| format!("caller context {} not found", caller_short))?;
            let source_doc_id = source_handle.document_id.clone();
            let source_model = source_handle.model.clone();
            drop(router);

            let blocks = self
                .documents
                .block_snapshots(&source_doc_id)
                .map_err(|e| format!("failed to read blocks: {}", e))?;

            let user_prompt = build_distillation_prompt(&blocks, None);

            let registry = kernel.llm().read().await;
            let provider = registry
                .default_provider()
                .ok_or_else(|| "LLM not configured — check llm.rhai".to_string())?;
            let model = source_model.as_deref().unwrap_or_else(|| {
                provider
                    .available_models()
                    .first()
                    .copied()
                    .unwrap_or("claude-sonnet-4-5-20250929")
            });
            drop(registry);

            let summary = provider
                .prompt_with_system(model, Some(DISTILLATION_SYSTEM_PROMPT), &user_prompt)
                .await
                .map_err(|e| format!("distillation failed: {}", e))?;

            let mut router = kernel.drift().write().await;
            let staged_id = router
                .stage(
                    &caller_short,
                    target_ctx,
                    summary,
                    Some(model.to_string()),
                    DriftKind::Distill,
                )
                .map_err(|e| e.to_string())?;

            Ok(ExecResult::success(format!(
                "Staged distilled drift → {} (id={})",
                target_ctx, staged_id
            )))
        } else {
            // Direct push
            if content.is_empty() {
                return Err(
                    "Content required for direct push. Use --summarize for auto-summary."
                        .to_string(),
                );
            }

            let mut router = kernel.drift().write().await;
            let source_model = router
                .get(&caller_short)
                .and_then(|h| h.model.clone());
            let staged_id = router
                .stage(
                    &caller_short,
                    target_ctx,
                    content,
                    source_model,
                    DriftKind::Push,
                )
                .map_err(|e| e.to_string())?;

            Ok(ExecResult::success(format!(
                "Staged drift → {} (id={})",
                target_ctx, staged_id
            )))
        }
    }

    async fn cmd_queue(&self) -> Result<ExecResult, String> {
        let kernel = self.kernel()?;
        let router = kernel.drift().read().await;
        let queue = router.queue();

        if queue.is_empty() {
            return Ok(ExecResult::success("No staged drifts.\n"));
        }

        let mut output = String::new();
        for drift in queue {
            output.push_str(&format!(
                "  #{}: {} → {} [{}] ({})\n",
                drift.id,
                drift.source_ctx,
                drift.target_ctx,
                drift.drift_kind,
                if drift.content.len() > 60 {
                    format!("{}...", &drift.content[..60])
                } else {
                    drift.content.clone()
                }
            ));
        }

        Ok(ExecResult::success(output))
    }

    async fn cmd_cancel(&self, args: &[String]) -> Result<ExecResult, String> {
        if args.is_empty() {
            return Err("Usage: drift cancel <staged-id>".to_string());
        }

        let staged_id: u64 = args[0]
            .parse()
            .map_err(|_| format!("Invalid staged ID: {}", args[0]))?;

        let kernel = self.kernel()?;
        let mut router = kernel.drift().write().await;
        if router.cancel(staged_id) {
            Ok(ExecResult::success(format!(
                "Cancelled staged drift #{}",
                staged_id
            )))
        } else {
            Err(format!("Staged drift #{} not found", staged_id))
        }
    }

    async fn cmd_flush(&self) -> Result<ExecResult, String> {
        let kernel = self.kernel()?;
        let caller_short = self.caller_short_id().await?;

        let staged = {
            let mut router = kernel.drift().write().await;
            router.drain(Some(&caller_short))
        };

        let count = staged.len();
        let mut injected = 0;

        for drift in &staged {
            // Look up target document from the router
            let target_doc_id = {
                let router = kernel.drift().read().await;
                match router.get(&drift.target_ctx) {
                    Some(h) => h.document_id.clone(),
                    None => {
                        tracing::warn!(
                            "Drift flush: target context {} not found, skipping",
                            drift.target_ctx
                        );
                        continue;
                    }
                }
            };

            let author = format!("drift:{}", drift.source_ctx);
            let snapshot = DriftRouter::build_drift_block(drift, &author);
            let after = self.documents.last_block_id(&target_doc_id);

            match self
                .documents
                .insert_from_snapshot(&target_doc_id, snapshot, after.as_ref())
            {
                Ok(block_id) => {
                    tracing::info!(
                        "Drift flushed: {} → {} (block={})",
                        drift.source_ctx,
                        drift.target_ctx,
                        block_id.to_key()
                    );
                    injected += 1;
                }
                Err(e) => {
                    tracing::error!(
                        "Drift flush failed for {} → {}: {}",
                        drift.source_ctx,
                        drift.target_ctx,
                        e
                    );
                }
            }
        }

        Ok(ExecResult::success(format!(
            "Flushed {} drifts ({} injected)",
            count, injected
        )))
    }

    async fn cmd_pull(&self, args: &[String]) -> Result<ExecResult, String> {
        if args.is_empty() {
            return Err("Usage: drift pull <source-ctx> [focus prompt]".to_string());
        }

        let kernel = self.kernel()?;
        let source_ctx = &args[0];
        let directed_prompt = if args.len() > 1 {
            Some(args[1..].join(" "))
        } else {
            None
        };

        let caller_short = self.caller_short_id().await?;

        // Read source context info
        let (source_doc_id, source_model) = {
            let router = kernel.drift().read().await;
            let source_handle = router
                .get(source_ctx)
                .ok_or_else(|| format!("unknown source context: {}", source_ctx))?;
            (
                source_handle.document_id.clone(),
                source_handle.model.clone(),
            )
        };

        // Read source blocks
        let blocks = self
            .documents
            .block_snapshots(&source_doc_id)
            .map_err(|e| format!("failed to read source blocks: {}", e))?;

        let user_prompt =
            build_distillation_prompt(&blocks, directed_prompt.as_deref());

        // Get LLM
        let registry = kernel.llm().read().await;
        let provider = registry
            .default_provider()
            .ok_or_else(|| "LLM not configured — check llm.rhai".to_string())?;
        let model = source_model.as_deref().unwrap_or_else(|| {
            provider
                .available_models()
                .first()
                .copied()
                .unwrap_or("claude-sonnet-4-5-20250929")
        });
        drop(registry);

        tracing::info!(
            "Pulling from {} ({} blocks, model={}) → {}",
            source_ctx,
            blocks.len(),
            model,
            caller_short
        );

        let summary = provider
            .prompt_with_system(model, Some(DISTILLATION_SYSTEM_PROMPT), &user_prompt)
            .await
            .map_err(|e| format!("distillation LLM call failed: {}", e))?;

        // Inject drift block directly into caller's document
        let caller_doc_id = {
            let router = kernel.drift().read().await;
            router
                .get(&caller_short)
                .ok_or_else(|| format!("caller context {} not found", caller_short))?
                .document_id
                .clone()
        };

        let staged = StagedDrift {
            id: 0,
            source_ctx: source_ctx.to_string(),
            target_ctx: caller_short.clone(),
            content: summary,
            source_model: Some(model.to_string()),
            drift_kind: DriftKind::Pull,
            created_at: now_epoch(),
        };

        let author = format!("drift:{}", source_ctx);
        let snapshot = DriftRouter::build_drift_block(&staged, &author);
        let after = self.documents.last_block_id(&caller_doc_id);

        let block_id = self
            .documents
            .insert_from_snapshot(&caller_doc_id, snapshot, after.as_ref())
            .map_err(|e| format!("failed to inject drift block: {}", e))?;

        Ok(ExecResult::success(format!(
            "Pulled from {} → {} (block={})",
            source_ctx,
            caller_short,
            block_id.to_key()
        )))
    }

    async fn cmd_merge(&self, args: &[String]) -> Result<ExecResult, String> {
        if args.is_empty() {
            return Err("Usage: drift merge <source-ctx>".to_string());
        }

        let kernel = self.kernel()?;
        let source_ctx = &args[0];

        // Read source context + parent info
        let (source_doc_id, source_model, parent_ctx_id) = {
            let router = kernel.drift().read().await;
            let source_handle = router
                .get(source_ctx)
                .ok_or_else(|| format!("unknown source context: {}", source_ctx))?;
            let parent = source_handle
                .parent_short_id
                .clone()
                .ok_or_else(|| {
                    format!(
                        "context {} has no parent — cannot merge",
                        source_ctx
                    )
                })?;
            (
                source_handle.document_id.clone(),
                source_handle.model.clone(),
                parent,
            )
        };

        // Get parent's document ID
        let parent_doc_id = {
            let router = kernel.drift().read().await;
            router
                .get(&parent_ctx_id)
                .ok_or_else(|| format!("parent context {} not found", parent_ctx_id))?
                .document_id
                .clone()
        };

        // Read source blocks
        let blocks = self
            .documents
            .block_snapshots(&source_doc_id)
            .map_err(|e| format!("failed to read source blocks: {}", e))?;

        let user_prompt = build_distillation_prompt(&blocks, None);

        // Get LLM
        let registry = kernel.llm().read().await;
        let provider = registry
            .default_provider()
            .ok_or_else(|| "LLM not configured — check llm.rhai".to_string())?;
        let model = source_model.as_deref().unwrap_or_else(|| {
            provider
                .available_models()
                .first()
                .copied()
                .unwrap_or("claude-sonnet-4-5-20250929")
        });
        drop(registry);

        tracing::info!(
            "Merging {} ({} blocks, model={}) → parent {}",
            source_ctx,
            blocks.len(),
            model,
            parent_ctx_id
        );

        let summary = provider
            .prompt_with_system(model, Some(DISTILLATION_SYSTEM_PROMPT), &user_prompt)
            .await
            .map_err(|e| format!("distillation LLM call failed: {}", e))?;

        // Build drift block and inject into parent document
        let staged = StagedDrift {
            id: 0,
            source_ctx: source_ctx.to_string(),
            target_ctx: parent_ctx_id.clone(),
            content: summary,
            source_model: Some(model.to_string()),
            drift_kind: DriftKind::Merge,
            created_at: now_epoch(),
        };

        let author = format!("drift:{}", source_ctx);
        let snapshot = DriftRouter::build_drift_block(&staged, &author);
        let after = self.documents.last_block_id(&parent_doc_id);

        let block_id = self
            .documents
            .insert_from_snapshot(&parent_doc_id, snapshot, after.as_ref())
            .map_err(|e| format!("failed to inject merge block: {}", e))?;

        Ok(ExecResult::success(format!(
            "Merged {} → parent {} (block={})",
            source_ctx,
            parent_ctx_id,
            block_id.to_key()
        )))
    }

    fn show_help(&self) -> Result<ExecResult, String> {
        Ok(ExecResult::success(
            r#"drift - Cross-context communication within a kernel

USAGE:
    drift <command> [args]

COMMANDS:
    ls                          List contexts in this kernel
    push <ctx> "content"        Stage content for target context
    push <ctx> --summarize      LLM-summarize before staging
    queue                       Show staged drifts
    cancel <id>                 Cancel staged drift
    flush                       Deliver staged drifts to target documents
    pull <ctx> [focus prompt]   Read + summarize source context
    merge <ctx>                 Summarize forked context into parent
    help                        Show this help

EXAMPLES:
    drift ls
    drift push d4e5f6 "Here's what I found about the auth bug"
    drift push d4e5f6 --summarize
    drift queue
    drift flush
    drift pull a1b2c3 "what was decided about caching?"
    drift merge a1b2c3
"#,
        ))
    }
}

#[async_trait]
impl ExecutionEngine for DriftEngine {
    fn name(&self) -> &str {
        "drift"
    }

    fn description(&self) -> &str {
        "Cross-context drift: push, pull, merge between contexts within a kernel"
    }

    fn schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "_positional": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Subcommand and arguments: ls|push|queue|cancel|flush|pull|merge|help [args]"
                }
            },
            "required": []
        }))
    }

    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let parsed: serde_json::Value = match serde_json::from_str(params) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ExecResult::failure(
                    1,
                    format!("Invalid parameters: {}", e),
                ));
            }
        };

        let args = EngineArgs::from_json(&parsed).to_argv();

        match self.execute_inner(args).await {
            Ok(result) => Ok(result),
            Err(e) => Ok(ExecResult::failure(1, e)),
        }
    }

    async fn is_available(&self) -> bool {
        true
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Current Unix epoch in seconds.
fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_and_lookup() {
        let mut router = DriftRouter::new();
        let short_id = router.register("main-session", "doc-abc", None);

        assert_eq!(short_id.len(), SHORT_ID_LEN);
        let handle = router.get(&short_id).unwrap();
        assert_eq!(handle.context_name, "main-session");
        assert_eq!(handle.document_id, "doc-abc");
        assert!(handle.parent_short_id.is_none());
    }

    #[test]
    fn test_register_with_parent() {
        let mut router = DriftRouter::new();
        let parent_id = router.register("main", "doc-1", None);
        let child_id = router.register("fork-debug", "doc-2", Some(&parent_id));

        let child = router.get(&child_id).unwrap();
        assert_eq!(child.parent_short_id.as_deref(), Some(parent_id.as_str()));
    }

    #[test]
    fn test_short_id_uniqueness() {
        let mut router = DriftRouter::new();
        let mut ids = Vec::new();
        for i in 0..100 {
            let id = router.register(&format!("ctx-{}", i), &format!("doc-{}", i), None);
            assert!(!ids.contains(&id), "duplicate short_id: {}", id);
            ids.push(id);
        }
    }

    #[test]
    fn test_context_to_short_id() {
        let mut router = DriftRouter::new();
        let short = router.register("test-ctx", "doc-42", None);
        assert_eq!(router.short_id_for_context("test-ctx"), Some(short.as_str()));
        assert_eq!(router.short_id_for_context("nonexistent"), None);
    }

    #[test]
    fn test_configure_llm() {
        let mut router = DriftRouter::new();
        let short = router.register("test", "doc-1", None);

        router
            .configure_llm(&short, "gemini", "gemini-2.0-flash")
            .unwrap();

        let handle = router.get(&short).unwrap();
        assert_eq!(handle.provider.as_deref(), Some("gemini"));
        assert_eq!(handle.model.as_deref(), Some("gemini-2.0-flash"));
    }

    #[test]
    fn test_configure_llm_unknown_context() {
        let mut router = DriftRouter::new();
        let result = router.configure_llm("nonexistent", "anthropic", "claude-opus-4-6");
        assert!(result.is_err());
    }

    #[test]
    fn test_stage_and_queue() {
        let mut router = DriftRouter::new();
        let src = router.register("source", "doc-1", None);
        let tgt = router.register("target", "doc-2", None);

        let id = router
            .stage(&src, &tgt, "hello from source".into(), None, DriftKind::Push)
            .unwrap();

        assert_eq!(router.queue().len(), 1);
        assert_eq!(router.queue()[0].id, id);
        assert_eq!(router.queue()[0].content, "hello from source");
    }

    #[test]
    fn test_stage_unknown_target() {
        let mut router = DriftRouter::new();
        let src = router.register("source", "doc-1", None);

        let result = router.stage(&src, "bad", "nope".into(), None, DriftKind::Push);
        assert!(result.is_err());
    }

    #[test]
    fn test_cancel() {
        let mut router = DriftRouter::new();
        let src = router.register("src", "doc-1", None);
        let tgt = router.register("tgt", "doc-2", None);

        let id1 = router
            .stage(&src, &tgt, "one".into(), None, DriftKind::Push)
            .unwrap();
        let _id2 = router
            .stage(&src, &tgt, "two".into(), None, DriftKind::Push)
            .unwrap();

        assert_eq!(router.queue().len(), 2);
        assert!(router.cancel(id1));
        assert_eq!(router.queue().len(), 1);
        assert_eq!(router.queue()[0].content, "two");
    }

    #[test]
    fn test_drain() {
        let mut router = DriftRouter::new();
        let src = router.register("src", "doc-1", None);
        let tgt = router.register("tgt", "doc-2", None);

        router
            .stage(&src, &tgt, "a".into(), None, DriftKind::Push)
            .unwrap();
        router
            .stage(&src, &tgt, "b".into(), None, DriftKind::Push)
            .unwrap();

        let drained = router.drain(None);
        assert_eq!(drained.len(), 2);
        assert!(router.queue().is_empty());
    }

    #[test]
    fn test_build_drift_block() {
        let mut router = DriftRouter::new();
        let src = router.register("src", "doc-1", None);
        let tgt = router.register("tgt", "doc-2", None);

        let id = router
            .stage(
                &src,
                &tgt,
                "important finding".into(),
                Some("claude-opus-4-6".into()),
                DriftKind::Distill,
            )
            .unwrap();

        let staged = &router.queue()[0];
        assert_eq!(staged.id, id);

        let block = DriftRouter::build_drift_block(staged, &format!("drift:{}", src));
        assert_eq!(block.kind, BlockKind::Drift);
        assert_eq!(block.role, Role::System);
        assert_eq!(block.content, "important finding");
        assert_eq!(block.source_context.as_deref(), Some(src.as_str()));
        assert_eq!(block.source_model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(block.drift_kind, Some(DriftKind::Distill));
    }

    #[test]
    fn test_unregister() {
        let mut router = DriftRouter::new();
        let short = router.register("test", "doc-1", None);

        assert!(router.get(&short).is_some());
        router.unregister(&short);
        assert!(router.get(&short).is_none());
        assert!(router.short_id_for_context("test").is_none());
    }

    #[test]
    fn test_list_contexts_sorted() {
        let mut router = DriftRouter::new();
        let _a = router.register("alpha", "doc-1", None);
        let _b = router.register("beta", "doc-2", None);
        let _c = router.register("gamma", "doc-3", None);

        let list = router.list_contexts();
        assert_eq!(list.len(), 3);
        for i in 1..list.len() {
            assert!(list[i].created_at >= list[i - 1].created_at);
        }
    }

    #[test]
    fn test_drain_scoped() {
        let mut router = DriftRouter::new();
        let a = router.register("alpha", "doc-1", None);
        let b = router.register("beta", "doc-2", None);
        let c = router.register("gamma", "doc-3", None);

        // Stage: a→b and c→b
        router
            .stage(&a, &b, "from alpha".into(), None, DriftKind::Push)
            .unwrap();
        router
            .stage(&c, &b, "from gamma".into(), None, DriftKind::Push)
            .unwrap();

        // Scoped drain for alpha — should only get a→b
        let drained = router.drain(Some(&a));
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].source_ctx, a);
        // c→b should remain
        assert_eq!(router.queue().len(), 1);
        assert_eq!(router.queue()[0].source_ctx, c);
    }

    #[test]
    fn test_build_distillation_prompt_basic() {
        use kaijutsu_crdt::BlockId;

        let blocks = vec![
            BlockSnapshot::text(
                BlockId::new("doc", "agent", 0),
                None,
                Role::User,
                "How do I fix the auth bug?",
                "user",
            ),
            BlockSnapshot::text(
                BlockId::new("doc", "agent", 1),
                None,
                Role::Model,
                "The auth bug is caused by a race condition in the session handler.",
                "model",
            ),
        ];

        let prompt = build_distillation_prompt(&blocks, None);
        assert!(prompt.contains("# Conversation to summarize"));
        assert!(prompt.contains("**User**: How do I fix the auth bug?"));
        assert!(prompt.contains("**Assistant**: The auth bug is caused by"));
        assert!(!prompt.contains("Focus your summary on"));
    }

    #[test]
    fn test_build_distillation_prompt_with_directed_focus() {
        use kaijutsu_crdt::BlockId;

        let blocks = vec![BlockSnapshot::text(
            BlockId::new("doc", "agent", 0),
            None,
            Role::User,
            "Let's discuss auth and caching.",
            "user",
        )];

        let prompt =
            build_distillation_prompt(&blocks, Some("what was decided about caching?"));
        assert!(prompt.contains("Focus your summary on: what was decided about caching?"));
    }

    #[test]
    fn test_build_distillation_prompt_truncates_long_blocks() {
        use kaijutsu_crdt::BlockId;

        let long_content = "x".repeat(3000);
        let blocks = vec![BlockSnapshot::tool_result(
            BlockId::new("doc", "agent", 0),
            BlockId::new("doc", "agent", 99),
            &long_content,
            false,
            None,
            "tool",
        )];

        let prompt = build_distillation_prompt(&blocks, None);
        assert!(prompt.contains("[truncated, 3000 bytes total]"));
        assert!(!prompt.contains(&long_content));
    }

    #[test]
    fn test_build_distillation_prompt_skips_empty() {
        use kaijutsu_crdt::BlockId;

        let blocks = vec![
            BlockSnapshot::text(
                BlockId::new("doc", "agent", 0),
                None,
                Role::User,
                "",
                "user",
            ),
            BlockSnapshot::text(
                BlockId::new("doc", "agent", 1),
                None,
                Role::Model,
                "Only this should appear.",
                "model",
            ),
        ];

        let prompt = build_distillation_prompt(&blocks, None);
        assert!(!prompt.contains("**User**:"));
        assert!(prompt.contains("**Assistant**: Only this should appear."));
    }

    #[tokio::test]
    async fn test_drift_engine_ls() {
        let kernel = Arc::new(crate::kernel::Kernel::new("test").await);
        {
            let mut r = kernel.drift().write().await;
            r.register("main", "doc-main", None);
            r.register("debug", "doc-debug", None);
        }

        let documents = crate::block_store::shared_block_store("test");
        let engine = DriftEngine::new(&kernel, documents, "main");
        let result = engine
            .execute(r#"{"_positional": ["ls"]}"#)
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.stdout.contains("main"));
        assert!(result.stdout.contains("debug"));
    }

    #[tokio::test]
    async fn test_drift_engine_push_queue_flush() {
        let kernel = Arc::new(crate::kernel::Kernel::new("test").await);
        let src_short;
        let tgt_short;
        {
            let mut r = kernel.drift().write().await;
            src_short = r.register("source", "doc-src", None);
            tgt_short = r.register("target", "doc-tgt", None);
        }

        let documents = crate::block_store::shared_block_store("test");
        // Create target document so flush can inject
        documents
            .create_document("doc-tgt".to_string(), crate::db::DocumentKind::Conversation, None)
            .unwrap();

        let engine = DriftEngine::new(&kernel, documents.clone(), "source");

        // Push content
        let push_result = engine
            .execute(&format!(
                r#"{{"_positional": ["push", "{}", "hello from source"]}}"#,
                tgt_short
            ))
            .await
            .unwrap();
        assert!(push_result.success, "push failed: {}", push_result.stderr);

        // Check queue
        let queue_result = engine
            .execute(r#"{"_positional": ["queue"]}"#)
            .await
            .unwrap();
        assert!(queue_result.success);
        assert!(queue_result.stdout.contains(&src_short));
        assert!(queue_result.stdout.contains(&tgt_short));

        // Flush
        let flush_result = engine
            .execute(r#"{"_positional": ["flush"]}"#)
            .await
            .unwrap();
        assert!(flush_result.success, "flush failed: {}", flush_result.stderr);
        assert!(flush_result.stdout.contains("Flushed 1 drifts"));

        // Verify block was injected into target document
        let blocks = documents.block_snapshots("doc-tgt").unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Drift);
        assert_eq!(blocks[0].content, "hello from source");
    }

    #[tokio::test]
    async fn test_shared_drift_on_fork() {
        // The SharedDriftRouter should be shareable across kernel fork/thread
        let router = shared_drift_router();

        // Register from "parent" side
        let short_id = {
            let mut r = router.write().await;
            r.register("main", "doc-main", None)
        };

        // Clone the Arc (simulating what fork/thread does)
        let child_router = Arc::clone(&router);

        // Child should see the parent's contexts
        let child_handle = {
            let r = child_router.read().await;
            r.get(&short_id).map(|h| h.context_name.clone())
        };
        assert_eq!(child_handle, Some("main".to_string()));

        // Child registers a new context
        let child_short = {
            let mut r = child_router.write().await;
            r.register("debug-fork", "doc-debug", Some(&short_id))
        };

        // Parent should see the child's context
        let parent_sees_child = {
            let r = router.read().await;
            r.get(&child_short).is_some()
        };
        assert!(parent_sees_child);
    }
}
