//! Kernel-native drift — cross-context communication and content transfer.
//!
//! The DriftRouter is the central coordinator for moving content between contexts
//! *within a kernel*. It maintains a registry of all contexts (keyed by ContextId)
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

use kaijutsu_crdt::ids::{resolve_context_prefix, PrefixError};
use kaijutsu_crdt::{BlockKind, BlockSnapshot, ContextId, DriftKind, Role};

use crate::block_store::SharedBlockStore;
use crate::tools::{ExecResult, ExecutionEngine};

/// Shared, thread-safe DriftRouter reference.
pub type SharedDriftRouter = Arc<RwLock<DriftRouter>>;

/// Create a new shared DriftRouter.
pub fn shared_drift_router() -> SharedDriftRouter {
    Arc::new(RwLock::new(DriftRouter::new()))
}

// ============================================================================
// ContextHandle — registered context within a kernel
// ============================================================================

/// A registered context within this kernel.
///
/// Keyed by `ContextId` (UUIDv7). The `label` is an optional mutable
/// human-friendly name — never used as a lookup key.
#[derive(Debug, Clone)]
pub struct ContextHandle {
    /// Globally unique context identifier (UUIDv7).
    pub id: ContextId,
    /// Optional human-friendly label (mutable, not an identifier).
    pub label: Option<String>,
    /// Primary document ID in the shared BlockStore.
    pub document_id: String,
    /// Working directory in VFS (e.g., "/mnt/kaijutsu").
    pub pwd: Option<String>,
    /// Provider name if configured (e.g., "anthropic", "gemini").
    pub provider: Option<String>,
    /// Model name if configured (e.g., "claude-opus-4-6", "gemini-2.0-flash").
    pub model: Option<String>,
    /// Parent context ID (for fork lineage).
    pub parent_id: Option<ContextId>,
    /// Creation timestamp (Unix epoch seconds).
    pub created_at: u64,
    /// Long-running OTel trace ID for this context.
    ///
    /// Generated at registration time. All RPC operations touching this
    /// context become child spans under this trace ID, enabling
    /// "show me everything that happened in context X" queries.
    pub trace_id: [u8; 16],
}

impl ContextHandle {
    /// Display string: label if set, else short hex.
    pub fn display_name(&self) -> String {
        self.id.display_or(self.label.as_deref())
    }
}

// ============================================================================
// StagedDrift — queued drift operation
// ============================================================================

/// A drift operation staged in the queue, pending flush.
#[derive(Debug, Clone)]
pub struct StagedDrift {
    /// Unique ID for this staged operation.
    pub id: u64,
    /// Source context ID.
    pub source_ctx: ContextId,
    /// Target context ID.
    pub target_ctx: ContextId,
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
/// the same `SharedBlockStore`, so drift only needs ContextIds and document
/// IDs — no cross-kernel lookup required.
///
/// This is the single source of truth for context registration. The server-level
/// drift router has been removed; `listContexts` reads directly from here.
#[derive(Debug)]
pub struct DriftRouter {
    /// All registered contexts, keyed by ContextId.
    contexts: HashMap<ContextId, ContextHandle>,
    /// Staging queue for pending drift operations.
    staging: Vec<StagedDrift>,
    /// Counter for staged drift IDs.
    next_staged_id: u64,
    /// Reverse lookup: label → ContextId (for prefix matching).
    label_to_id: HashMap<String, ContextId>,
    /// Reverse lookup: document_id → ContextId (for document-keyed RPCs).
    doc_to_context: HashMap<String, ContextId>,
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
            label_to_id: HashMap::new(),
            doc_to_context: HashMap::new(),
        }
    }

    /// Register a context with a pre-assigned ContextId.
    ///
    /// The caller (server RPC) creates the ContextId and passes it in.
    #[tracing::instrument(skip(self, document_id), name = "drift.register")]
    pub fn register(
        &mut self,
        id: ContextId,
        label: Option<&str>,
        document_id: &str,
        parent_id: Option<ContextId>,
    ) {
        if let Some(l) = label {
            self.label_to_id.insert(l.to_string(), id);
        }
        self.doc_to_context.insert(document_id.to_string(), id);

        let handle = ContextHandle {
            id,
            label: label.map(|s| s.to_string()),
            document_id: document_id.to_string(),
            pwd: None,
            provider: None,
            model: None,
            parent_id,
            created_at: now_epoch(),
            trace_id: uuid::Uuid::new_v4().into_bytes(),
        };

        self.contexts.insert(id, handle);
    }

    /// Unregister a context (e.g., when a context is destroyed).
    #[tracing::instrument(skip(self), name = "drift.unregister")]
    pub fn unregister(&mut self, id: ContextId) {
        if let Some(handle) = self.contexts.remove(&id) {
            if let Some(label) = &handle.label {
                self.label_to_id.remove(label);
            }
            self.doc_to_context.remove(&handle.document_id);
        }
    }

    /// Look up a context by ContextId.
    pub fn get(&self, id: ContextId) -> Option<&ContextHandle> {
        self.contexts.get(&id)
    }

    /// Look up a context mutably by ContextId.
    pub fn get_mut(&mut self, id: ContextId) -> Option<&mut ContextHandle> {
        self.contexts.get_mut(&id)
    }

    /// Rename a context's label.
    #[tracing::instrument(skip(self), name = "drift.rename")]
    pub fn rename(&mut self, id: ContextId, new_label: Option<&str>) -> Result<(), DriftError> {
        let handle = self.contexts.get_mut(&id)
            .ok_or_else(|| DriftError::UnknownContext(id.short()))?;

        // Remove old label from index
        if let Some(old_label) = &handle.label {
            self.label_to_id.remove(old_label);
        }

        // Set new label
        handle.label = new_label.map(|s| s.to_string());
        if let Some(l) = new_label {
            self.label_to_id.insert(l.to_string(), id);
        }

        Ok(())
    }

    /// Resolve a query string (label, label prefix, or hex prefix) to a ContextId.
    ///
    /// Resolution order:
    /// 1. Exact label match
    /// 2. Unique label prefix match
    /// 3. Unique hex prefix match
    pub fn resolve_context(&self, query: &str) -> Result<ContextId, DriftError> {
        let entries = self.contexts.values().map(|h| (h.id, h.label.as_deref()));
        resolve_context_prefix(entries, query).map_err(|e| match e {
            PrefixError::NoMatch(q) => DriftError::UnknownContext(q),
            PrefixError::Ambiguous { prefix, candidates } => {
                DriftError::AmbiguousContext { prefix, candidates }
            }
        })
    }

    /// Update provider/model for a context.
    pub fn configure_llm(
        &mut self,
        id: ContextId,
        provider: &str,
        model: &str,
    ) -> Result<(), DriftError> {
        let handle = self
            .contexts
            .get_mut(&id)
            .ok_or_else(|| DriftError::UnknownContext(id.short()))?;
        handle.provider = Some(provider.to_string());
        handle.model = Some(model.to_string());
        Ok(())
    }

    /// Set the working directory for a context.
    pub fn set_pwd(
        &mut self,
        id: ContextId,
        pwd: Option<String>,
    ) -> Result<(), DriftError> {
        let handle = self
            .contexts
            .get_mut(&id)
            .ok_or_else(|| DriftError::UnknownContext(id.short()))?;
        handle.pwd = pwd;
        Ok(())
    }

    /// List all registered contexts.
    pub fn list_contexts(&self) -> Vec<&ContextHandle> {
        let mut contexts: Vec<_> = self.contexts.values().collect();
        contexts.sort_by_key(|c| c.created_at);
        contexts
    }

    /// Look up the ContextId for a document ID.
    pub fn context_for_document(&self, document_id: &str) -> Option<ContextId> {
        self.doc_to_context.get(document_id).copied()
    }

    /// Look up the trace_id for a document ID (convenience for document-keyed RPCs).
    pub fn trace_id_for_document(&self, document_id: &str) -> Option<[u8; 16]> {
        let ctx_id = self.doc_to_context.get(document_id)?;
        self.contexts.get(ctx_id).map(|h| h.trace_id)
    }

    /// Stage a drift operation for later flush.
    ///
    /// Returns the staged drift ID.
    #[tracing::instrument(skip(self, content, source_model), fields(drift.source = %source_ctx, drift.target = %target_ctx))]
    pub fn stage(
        &mut self,
        source_ctx: ContextId,
        target_ctx: ContextId,
        content: String,
        source_model: Option<String>,
        drift_kind: DriftKind,
    ) -> Result<u64, DriftError> {
        // Validate both contexts exist
        if !self.contexts.contains_key(&source_ctx) {
            return Err(DriftError::UnknownContext(source_ctx.short()));
        }
        if !self.contexts.contains_key(&target_ctx) {
            return Err(DriftError::UnknownContext(target_ctx.short()));
        }

        let id = self.next_staged_id;
        self.next_staged_id += 1;

        self.staging.push(StagedDrift {
            id,
            source_ctx,
            target_ctx,
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
    /// Failed items should be returned via [`requeue`](Self::requeue).
    #[tracing::instrument(skip(self), name = "drift.drain")]
    pub fn drain(&mut self, for_context: Option<ContextId>) -> Vec<StagedDrift> {
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

    /// Re-queue staged drifts that failed to deliver.
    pub fn requeue(&mut self, items: Vec<StagedDrift>) {
        self.staging.extend(items);
    }

    /// Build a BlockSnapshot for a staged drift, ready for insertion.
    pub fn build_drift_block(drift: &StagedDrift, author: &str) -> BlockSnapshot {
        BlockSnapshot::drift(
            kaijutsu_crdt::BlockId::new("", "", 0), // ID assigned by document
            None,                                     // parent set during insertion
            drift.content.clone(),
            author,
            drift.source_ctx.short(),
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
    #[error("ambiguous context prefix '{prefix}': matches {candidates:?}")]
    AmbiguousContext {
        prefix: String,
        candidates: Vec<String>,
    },
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
// Individual drift engines — one per operation
// ============================================================================

/// Upgrade a weak kernel reference, or return an error string.
fn drift_kernel(
    kernel: &std::sync::Weak<crate::kernel::Kernel>,
) -> Result<Arc<crate::kernel::Kernel>, String> {
    kernel
        .upgrade()
        .ok_or_else(|| "kernel has been dropped".to_string())
}

// ── DriftLsEngine ─────────────────────────────────────────────────────────

/// List all contexts in the kernel's drift router.
pub struct DriftLsEngine {
    kernel: std::sync::Weak<crate::kernel::Kernel>,
    context_id: ContextId,
}

impl DriftLsEngine {
    pub fn new(kernel: &Arc<crate::kernel::Kernel>, context_id: ContextId) -> Self {
        Self {
            kernel: Arc::downgrade(kernel),
            context_id,
        }
    }
}

#[async_trait]
impl ExecutionEngine for DriftLsEngine {
    fn name(&self) -> &str { "drift_ls" }
    fn description(&self) -> &str { "List all contexts in this kernel's drift router" }

    fn schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {},
            "description": "No parameters needed"
        }))
    }

    #[tracing::instrument(skip(self, _params), name = "engine.drift_ls")]
    async fn execute(&self, _params: &str) -> anyhow::Result<ExecResult> {
        let kernel = match drift_kernel(&self.kernel) {
            Ok(k) => k,
            Err(e) => return Ok(ExecResult::failure(1, e)),
        };

        let router = kernel.drift().read().await;
        let contexts = router.list_contexts();

        let mut output = String::new();
        for ctx in &contexts {
            let marker = if ctx.id == self.context_id { "* " } else { "  " };
            let display = ctx.display_name();
            let provider_info = match (&ctx.provider, &ctx.model) {
                (Some(p), Some(m)) => format!(" ({}:{})", p, m),
                (Some(p), None) => format!(" ({})", p),
                _ => String::new(),
            };
            let parent_info = ctx
                .parent_id
                .as_ref()
                .map(|p| format!(" [parent: {}]", p.short()))
                .unwrap_or_default();
            output.push_str(&format!(
                "{}{} {} [doc: {}]{}{}\n",
                marker, ctx.id.short(), display, ctx.document_id, provider_info, parent_info,
            ));
        }

        if contexts.is_empty() {
            output.push_str("No contexts registered.\n");
        }

        Ok(ExecResult::success(output))
    }

    async fn is_available(&self) -> bool { true }
}

// ── DriftPushEngine ───────────────────────────────────────────────────────

/// Stage content for transfer to another context.
pub struct DriftPushEngine {
    kernel: std::sync::Weak<crate::kernel::Kernel>,
    documents: SharedBlockStore,
    context_id: ContextId,
}

#[derive(serde::Deserialize)]
struct DriftPushParams {
    target_ctx: String,
    content: Option<String>,
    #[serde(default)]
    summarize: bool,
}

impl DriftPushEngine {
    pub fn new(
        kernel: &Arc<crate::kernel::Kernel>,
        documents: SharedBlockStore,
        context_id: ContextId,
    ) -> Self {
        Self {
            kernel: Arc::downgrade(kernel),
            documents,
            context_id,
        }
    }
}

#[async_trait]
impl ExecutionEngine for DriftPushEngine {
    fn name(&self) -> &str { "drift_push" }
    fn description(&self) -> &str { "Stage content for transfer to another context (optionally LLM-summarized)" }

    fn schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "target_ctx": {
                    "type": "string",
                    "description": "Label or hex prefix of the target context"
                },
                "content": {
                    "type": "string",
                    "description": "Content to push (required unless summarize=true)"
                },
                "summarize": {
                    "type": "boolean",
                    "description": "LLM-summarize this context before pushing (default: false)",
                    "default": false
                }
            },
            "required": ["target_ctx"]
        }))
    }

    #[tracing::instrument(skip(self, params), name = "drift.push")]
    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let p: DriftPushParams = match serde_json::from_str(params) {
            Ok(v) => v,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid params: {}", e))),
        };

        let kernel = match drift_kernel(&self.kernel) {
            Ok(k) => k,
            Err(e) => return Ok(ExecResult::failure(1, e)),
        };

        // Resolve target by label or hex prefix
        let target_id = {
            let router = kernel.drift().read().await;
            match router.resolve_context(&p.target_ctx) {
                Ok(id) => id,
                Err(e) => return Ok(ExecResult::failure(1, e.to_string())),
            }
        };

        if p.summarize {
            let (source_doc_id, source_model) = {
                let router = kernel.drift().read().await;
                let source_handle = match router.get(self.context_id) {
                    Some(h) => h,
                    None => return Ok(ExecResult::failure(1, format!("caller context {} not found", self.context_id.short()))),
                };
                (source_handle.document_id.clone(), source_handle.model.clone())
            };

            let blocks = match self.documents.block_snapshots(&source_doc_id) {
                Ok(b) => b,
                Err(e) => return Ok(ExecResult::failure(1, format!("failed to read blocks: {}", e))),
            };

            let user_prompt = build_distillation_prompt(&blocks, None);

            let registry = kernel.llm().read().await;
            let provider = match registry.default_provider() {
                Some(p) => p,
                None => return Ok(ExecResult::failure(1, "LLM not configured — check llm.rhai")),
            };
            let model = source_model.as_deref().unwrap_or_else(|| {
                provider.available_models().first().copied().unwrap_or("claude-sonnet-4-5-20250929")
            });
            drop(registry);

            let summary = match provider
                .prompt_with_system(model, Some(DISTILLATION_SYSTEM_PROMPT), &user_prompt)
                .await
            {
                Ok(s) => s,
                Err(e) => return Ok(ExecResult::failure(1, format!("distillation failed: {}", e))),
            };

            let mut router = kernel.drift().write().await;
            let staged_id = match router.stage(self.context_id, target_id, summary, Some(model.to_string()), DriftKind::Distill) {
                Ok(id) => id,
                Err(e) => return Ok(ExecResult::failure(1, e.to_string())),
            };

            Ok(ExecResult::success(format!("Staged distilled drift → {} (id={})", target_id.short(), staged_id)))
        } else {
            let content = match p.content {
                Some(c) if !c.is_empty() => c,
                _ => return Ok(ExecResult::failure(1, "Content required for direct push. Use summarize: true for auto-summary.")),
            };

            let mut router = kernel.drift().write().await;
            let source_model = router.get(self.context_id).and_then(|h| h.model.clone());
            let staged_id = match router.stage(self.context_id, target_id, content, source_model, DriftKind::Push) {
                Ok(id) => id,
                Err(e) => return Ok(ExecResult::failure(1, e.to_string())),
            };

            Ok(ExecResult::success(format!("Staged drift → {} (id={})", target_id.short(), staged_id)))
        }
    }

    async fn is_available(&self) -> bool { true }
}

// ── DriftPullEngine ───────────────────────────────────────────────────────

/// Read and LLM-summarize another context's conversation.
pub struct DriftPullEngine {
    kernel: std::sync::Weak<crate::kernel::Kernel>,
    documents: SharedBlockStore,
    context_id: ContextId,
}

#[derive(serde::Deserialize)]
struct DriftPullParams {
    source_ctx: String,
    prompt: Option<String>,
}

impl DriftPullEngine {
    pub fn new(
        kernel: &Arc<crate::kernel::Kernel>,
        documents: SharedBlockStore,
        context_id: ContextId,
    ) -> Self {
        Self {
            kernel: Arc::downgrade(kernel),
            documents,
            context_id,
        }
    }
}

#[async_trait]
impl ExecutionEngine for DriftPullEngine {
    fn name(&self) -> &str { "drift_pull" }
    fn description(&self) -> &str { "Read and LLM-summarize another context's conversation into this one" }

    fn schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "source_ctx": { "type": "string", "description": "Label or hex prefix of the source context" },
                "prompt": { "type": "string", "description": "Optional focus prompt to guide the summary" }
            },
            "required": ["source_ctx"]
        }))
    }

    #[tracing::instrument(skip(self, params), name = "drift.pull")]
    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let p: DriftPullParams = match serde_json::from_str(params) {
            Ok(v) => v,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid params: {}", e))),
        };

        let kernel = match drift_kernel(&self.kernel) {
            Ok(k) => k,
            Err(e) => return Ok(ExecResult::failure(1, e)),
        };

        // Resolve source by label or hex prefix
        let (source_id, source_doc_id, source_model) = {
            let router = kernel.drift().read().await;
            let source_id = match router.resolve_context(&p.source_ctx) {
                Ok(id) => id,
                Err(e) => return Ok(ExecResult::failure(1, e.to_string())),
            };
            let h = router.get(source_id).unwrap();
            (source_id, h.document_id.clone(), h.model.clone())
        };

        let blocks = match self.documents.block_snapshots(&source_doc_id) {
            Ok(b) => b,
            Err(e) => return Ok(ExecResult::failure(1, format!("failed to read source blocks: {}", e))),
        };

        let user_prompt = build_distillation_prompt(&blocks, p.prompt.as_deref());

        let registry = kernel.llm().read().await;
        let provider = match registry.default_provider() {
            Some(p) => p,
            None => return Ok(ExecResult::failure(1, "LLM not configured — check llm.rhai")),
        };
        let model = source_model.as_deref().unwrap_or_else(|| {
            provider.available_models().first().copied().unwrap_or("claude-sonnet-4-5-20250929")
        });
        drop(registry);

        tracing::info!("Pulling from {} ({} blocks, model={}) → {}", source_id.short(), blocks.len(), model, self.context_id.short());

        let summary = match provider
            .prompt_with_system(model, Some(DISTILLATION_SYSTEM_PROMPT), &user_prompt)
            .await
        {
            Ok(s) => s,
            Err(e) => return Ok(ExecResult::failure(1, format!("distillation LLM call failed: {}", e))),
        };

        let caller_doc_id = {
            let router = kernel.drift().read().await;
            match router.get(self.context_id) {
                Some(h) => h.document_id.clone(),
                None => return Ok(ExecResult::failure(1, format!("caller context {} not found", self.context_id.short()))),
            }
        };

        let staged = StagedDrift {
            id: 0,
            source_ctx: source_id,
            target_ctx: self.context_id,
            content: summary,
            source_model: Some(model.to_string()),
            drift_kind: DriftKind::Pull,
            created_at: now_epoch(),
        };

        let author = format!("drift:{}", source_id.short());
        let snapshot = DriftRouter::build_drift_block(&staged, &author);
        let after = self.documents.last_block_id(&caller_doc_id);

        let block_id = match self.documents.insert_from_snapshot(&caller_doc_id, snapshot, after.as_ref()) {
            Ok(id) => id,
            Err(e) => return Ok(ExecResult::failure(1, format!("failed to inject drift block: {}", e))),
        };

        Ok(ExecResult::success(format!("Pulled from {} → {} (block={})", source_id.short(), self.context_id.short(), block_id.to_key())))
    }

    async fn is_available(&self) -> bool { true }
}

// ── DriftFlushEngine ──────────────────────────────────────────────────────

/// Deliver all staged drifts to their target documents.
pub struct DriftFlushEngine {
    kernel: std::sync::Weak<crate::kernel::Kernel>,
    documents: SharedBlockStore,
    context_id: ContextId,
}

impl DriftFlushEngine {
    pub fn new(
        kernel: &Arc<crate::kernel::Kernel>,
        documents: SharedBlockStore,
        context_id: ContextId,
    ) -> Self {
        Self {
            kernel: Arc::downgrade(kernel),
            documents,
            context_id,
        }
    }
}

#[async_trait]
impl ExecutionEngine for DriftFlushEngine {
    fn name(&self) -> &str { "drift_flush" }
    fn description(&self) -> &str { "Deliver all staged drifts to their target documents" }

    fn schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {},
            "description": "No parameters needed"
        }))
    }

    #[tracing::instrument(skip(self, _params), name = "drift.flush")]
    async fn execute(&self, _params: &str) -> anyhow::Result<ExecResult> {
        let kernel = match drift_kernel(&self.kernel) {
            Ok(k) => k,
            Err(e) => return Ok(ExecResult::failure(1, e)),
        };

        let staged = {
            let mut router = kernel.drift().write().await;
            router.drain(Some(self.context_id))
        };

        let count = staged.len();
        let mut injected = 0;
        let mut failed: Vec<StagedDrift> = Vec::new();

        for drift in staged {
            let target_doc_id = {
                let router = kernel.drift().read().await;
                match router.get(drift.target_ctx) {
                    Some(h) => h.document_id.clone(),
                    None => {
                        tracing::warn!("Drift flush: target context {} not found, re-queuing", drift.target_ctx.short());
                        failed.push(drift);
                        continue;
                    }
                }
            };

            let author = format!("drift:{}", drift.source_ctx.short());
            let snapshot = DriftRouter::build_drift_block(&drift, &author);
            let after = self.documents.last_block_id(&target_doc_id);

            match self.documents.insert_from_snapshot(&target_doc_id, snapshot, after.as_ref()) {
                Ok(block_id) => {
                    tracing::info!("Drift flushed: {} → {} (block={})", drift.source_ctx.short(), drift.target_ctx.short(), block_id.to_key());
                    injected += 1;
                }
                Err(e) => {
                    tracing::error!("Drift flush failed for {} → {}: {}, re-queuing", drift.source_ctx.short(), drift.target_ctx.short(), e);
                    failed.push(drift);
                }
            }
        }

        // Re-queue any failed items so they aren't lost
        if !failed.is_empty() {
            let requeued = failed.len();
            let mut router = kernel.drift().write().await;
            router.requeue(failed);
            tracing::warn!("Re-queued {} failed drift items", requeued);
        }

        Ok(ExecResult::success(format!("Flushed {} drifts ({} injected)", count, injected)))
    }

    async fn is_available(&self) -> bool { true }
}

// ── DriftMergeEngine ──────────────────────────────────────────────────────

/// Summarize a forked context back into its parent.
pub struct DriftMergeEngine {
    kernel: std::sync::Weak<crate::kernel::Kernel>,
    documents: SharedBlockStore,
    #[allow(dead_code)] // kept for structural consistency; may be used for auth checks
    context_id: ContextId,
}

#[derive(serde::Deserialize)]
struct DriftMergeParams {
    source_ctx: String,
}

impl DriftMergeEngine {
    pub fn new(
        kernel: &Arc<crate::kernel::Kernel>,
        documents: SharedBlockStore,
        context_id: ContextId,
    ) -> Self {
        Self {
            kernel: Arc::downgrade(kernel),
            documents,
            context_id,
        }
    }
}

#[async_trait]
impl ExecutionEngine for DriftMergeEngine {
    fn name(&self) -> &str { "drift_merge" }
    fn description(&self) -> &str { "Summarize a forked context and inject the summary into its parent" }

    fn schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "source_ctx": { "type": "string", "description": "Label or hex prefix of the forked context to merge back" }
            },
            "required": ["source_ctx"]
        }))
    }

    #[tracing::instrument(skip(self, params), name = "drift.merge")]
    async fn execute(&self, params: &str) -> anyhow::Result<ExecResult> {
        let p: DriftMergeParams = match serde_json::from_str(params) {
            Ok(v) => v,
            Err(e) => return Ok(ExecResult::failure(1, format!("Invalid params: {}", e))),
        };

        let kernel = match drift_kernel(&self.kernel) {
            Ok(k) => k,
            Err(e) => return Ok(ExecResult::failure(1, e)),
        };

        let (source_id, source_doc_id, source_model, parent_ctx_id) = {
            let router = kernel.drift().read().await;
            let source_id = match router.resolve_context(&p.source_ctx) {
                Ok(id) => id,
                Err(e) => return Ok(ExecResult::failure(1, e.to_string())),
            };
            let source_handle = router.get(source_id).unwrap();
            let parent = match source_handle.parent_id {
                Some(p) => p,
                None => return Ok(ExecResult::failure(1, format!("context {} has no parent — cannot merge", source_id.short()))),
            };
            (source_id, source_handle.document_id.clone(), source_handle.model.clone(), parent)
        };

        let parent_doc_id = {
            let router = kernel.drift().read().await;
            match router.get(parent_ctx_id) {
                Some(h) => h.document_id.clone(),
                None => return Ok(ExecResult::failure(1, format!("parent context {} not found", parent_ctx_id.short()))),
            }
        };

        let blocks = match self.documents.block_snapshots(&source_doc_id) {
            Ok(b) => b,
            Err(e) => return Ok(ExecResult::failure(1, format!("failed to read source blocks: {}", e))),
        };

        let user_prompt = build_distillation_prompt(&blocks, None);

        let registry = kernel.llm().read().await;
        let provider = match registry.default_provider() {
            Some(p) => p,
            None => return Ok(ExecResult::failure(1, "LLM not configured — check llm.rhai")),
        };
        let model = source_model.as_deref().unwrap_or_else(|| {
            provider.available_models().first().copied().unwrap_or("claude-sonnet-4-5-20250929")
        });
        drop(registry);

        tracing::info!("Merging {} ({} blocks, model={}) → parent {}", source_id.short(), blocks.len(), model, parent_ctx_id.short());

        let summary = match provider
            .prompt_with_system(model, Some(DISTILLATION_SYSTEM_PROMPT), &user_prompt)
            .await
        {
            Ok(s) => s,
            Err(e) => return Ok(ExecResult::failure(1, format!("distillation LLM call failed: {}", e))),
        };

        let staged = StagedDrift {
            id: 0,
            source_ctx: source_id,
            target_ctx: parent_ctx_id,
            content: summary,
            source_model: Some(model.to_string()),
            drift_kind: DriftKind::Merge,
            created_at: now_epoch(),
        };

        let author = format!("drift:{}", source_id.short());
        let snapshot = DriftRouter::build_drift_block(&staged, &author);
        let after = self.documents.last_block_id(&parent_doc_id);

        let block_id = match self.documents.insert_from_snapshot(&parent_doc_id, snapshot, after.as_ref()) {
            Ok(id) => id,
            Err(e) => return Ok(ExecResult::failure(1, format!("failed to inject merge block: {}", e))),
        };

        Ok(ExecResult::success(format!("Merged {} → parent {} (block={})", source_id.short(), parent_ctx_id.short(), block_id.to_key())))
    }

    async fn is_available(&self) -> bool { true }
}

// (DriftEngine removed — replaced by the 5 individual engines above)

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
        let id = ContextId::new();
        router.register(id, Some("main-session"), "doc-abc", None);

        let handle = router.get(id).unwrap();
        assert_eq!(handle.label.as_deref(), Some("main-session"));
        assert_eq!(handle.document_id, "doc-abc");
        assert!(handle.parent_id.is_none());
    }

    #[test]
    fn test_register_with_parent() {
        let mut router = DriftRouter::new();
        let parent_id = ContextId::new();
        let child_id = ContextId::new();
        router.register(parent_id, Some("main"), "doc-1", None);
        router.register(child_id, Some("fork-debug"), "doc-2", Some(parent_id));

        let child = router.get(child_id).unwrap();
        assert_eq!(child.parent_id, Some(parent_id));
    }

    #[test]
    fn test_resolve_by_label() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, Some("test-ctx"), "doc-42", None);
        assert_eq!(router.resolve_context("test-ctx").unwrap(), id);
    }

    #[test]
    fn test_resolve_by_label_prefix() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, Some("test-ctx"), "doc-42", None);
        let other_id = ContextId::new();
        router.register(other_id, Some("debug"), "doc-43", None);
        assert_eq!(router.resolve_context("test").unwrap(), id);
    }

    #[test]
    fn test_resolve_by_hex_prefix() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, None, "doc-42", None);
        let hex_prefix = &id.to_hex()[..8];
        // Should match by hex prefix
        let result = router.resolve_context(hex_prefix);
        match result {
            Ok(resolved) => assert_eq!(resolved, id),
            Err(DriftError::AmbiguousContext { .. }) => {} // possible but unlikely
            Err(e) => panic!("unexpected error: {}", e),
        }
    }

    #[test]
    fn test_resolve_unknown() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, Some("main"), "doc-1", None);
        assert!(router.resolve_context("nonexistent").is_err());
    }

    #[test]
    fn test_configure_llm() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, Some("test"), "doc-1", None);

        router
            .configure_llm(id, "gemini", "gemini-2.0-flash")
            .unwrap();

        let handle = router.get(id).unwrap();
        assert_eq!(handle.provider.as_deref(), Some("gemini"));
        assert_eq!(handle.model.as_deref(), Some("gemini-2.0-flash"));
    }

    #[test]
    fn test_configure_llm_unknown_context() {
        let mut router = DriftRouter::new();
        let result = router.configure_llm(ContextId::new(), "anthropic", "claude-opus-4-6");
        assert!(result.is_err());
    }

    #[test]
    fn test_stage_and_queue() {
        let mut router = DriftRouter::new();
        let src = ContextId::new();
        let tgt = ContextId::new();
        router.register(src, Some("source"), "doc-1", None);
        router.register(tgt, Some("target"), "doc-2", None);

        let id = router
            .stage(src, tgt, "hello from source".into(), None, DriftKind::Push)
            .unwrap();

        assert_eq!(router.queue().len(), 1);
        assert_eq!(router.queue()[0].id, id);
        assert_eq!(router.queue()[0].content, "hello from source");
    }

    #[test]
    fn test_stage_unknown_target() {
        let mut router = DriftRouter::new();
        let src = ContextId::new();
        router.register(src, Some("source"), "doc-1", None);

        let result = router.stage(src, ContextId::new(), "nope".into(), None, DriftKind::Push);
        assert!(result.is_err());
    }

    #[test]
    fn test_cancel() {
        let mut router = DriftRouter::new();
        let src = ContextId::new();
        let tgt = ContextId::new();
        router.register(src, Some("src"), "doc-1", None);
        router.register(tgt, Some("tgt"), "doc-2", None);

        let id1 = router
            .stage(src, tgt, "one".into(), None, DriftKind::Push)
            .unwrap();
        let _id2 = router
            .stage(src, tgt, "two".into(), None, DriftKind::Push)
            .unwrap();

        assert_eq!(router.queue().len(), 2);
        assert!(router.cancel(id1));
        assert_eq!(router.queue().len(), 1);
        assert_eq!(router.queue()[0].content, "two");
    }

    #[test]
    fn test_drain() {
        let mut router = DriftRouter::new();
        let src = ContextId::new();
        let tgt = ContextId::new();
        router.register(src, Some("src"), "doc-1", None);
        router.register(tgt, Some("tgt"), "doc-2", None);

        router
            .stage(src, tgt, "a".into(), None, DriftKind::Push)
            .unwrap();
        router
            .stage(src, tgt, "b".into(), None, DriftKind::Push)
            .unwrap();

        let drained = router.drain(None);
        assert_eq!(drained.len(), 2);
        assert!(router.queue().is_empty());
    }

    #[test]
    fn test_build_drift_block() {
        let mut router = DriftRouter::new();
        let src = ContextId::new();
        let tgt = ContextId::new();
        router.register(src, Some("src"), "doc-1", None);
        router.register(tgt, Some("tgt"), "doc-2", None);

        let id = router
            .stage(
                src,
                tgt,
                "important finding".into(),
                Some("claude-opus-4-6".into()),
                DriftKind::Distill,
            )
            .unwrap();

        let staged = &router.queue()[0];
        assert_eq!(staged.id, id);

        let block = DriftRouter::build_drift_block(staged, &format!("drift:{}", src.short()));
        assert_eq!(block.kind, BlockKind::Drift);
        assert_eq!(block.role, Role::System);
        assert_eq!(block.content, "important finding");
        assert_eq!(block.source_context.as_deref(), Some(src.short().as_str()));
        assert_eq!(block.source_model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(block.drift_kind, Some(DriftKind::Distill));
    }

    #[test]
    fn test_unregister() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, Some("test"), "doc-1", None);

        assert!(router.get(id).is_some());
        router.unregister(id);
        assert!(router.get(id).is_none());
        assert!(router.resolve_context("test").is_err());
    }

    #[test]
    fn test_list_contexts_sorted() {
        let mut router = DriftRouter::new();
        let a = ContextId::new();
        let b = ContextId::new();
        let c = ContextId::new();
        router.register(a, Some("alpha"), "doc-1", None);
        router.register(b, Some("beta"), "doc-2", None);
        router.register(c, Some("gamma"), "doc-3", None);

        let list = router.list_contexts();
        assert_eq!(list.len(), 3);
        for i in 1..list.len() {
            assert!(list[i].created_at >= list[i - 1].created_at);
        }
    }

    #[test]
    fn test_drain_scoped() {
        let mut router = DriftRouter::new();
        let a = ContextId::new();
        let b = ContextId::new();
        let c = ContextId::new();
        router.register(a, Some("alpha"), "doc-1", None);
        router.register(b, Some("beta"), "doc-2", None);
        router.register(c, Some("gamma"), "doc-3", None);

        // Stage: a→b and c→b
        router
            .stage(a, b, "from alpha".into(), None, DriftKind::Push)
            .unwrap();
        router
            .stage(c, b, "from gamma".into(), None, DriftKind::Push)
            .unwrap();

        // Scoped drain for alpha — should only get a→b
        let drained = router.drain(Some(a));
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].source_ctx, a);
        // c→b should remain
        assert_eq!(router.queue().len(), 1);
        assert_eq!(router.queue()[0].source_ctx, c);
    }

    #[test]
    fn test_rename() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, Some("old-name"), "doc-1", None);

        assert!(router.resolve_context("old-name").is_ok());
        router.rename(id, Some("new-name")).unwrap();
        assert!(router.resolve_context("old-name").is_err());
        assert_eq!(router.resolve_context("new-name").unwrap(), id);

        let handle = router.get(id).unwrap();
        assert_eq!(handle.label.as_deref(), Some("new-name"));
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
    async fn test_drift_ls_engine() {
        let kernel = Arc::new(crate::kernel::Kernel::new("test").await);
        let main_id = ContextId::new();
        let debug_id = ContextId::new();
        {
            let mut r = kernel.drift().write().await;
            r.register(main_id, Some("main"), "doc-main", None);
            r.register(debug_id, Some("debug"), "doc-debug", None);
        }

        let engine = DriftLsEngine::new(&kernel, main_id);
        let result = engine.execute("{}").await.unwrap();

        assert!(result.success);
        assert!(result.stdout.contains("main"));
        assert!(result.stdout.contains("debug"));
    }

    #[tokio::test]
    async fn test_drift_push_and_flush_engines() {
        let kernel = Arc::new(crate::kernel::Kernel::new("test").await);
        let src_id = ContextId::new();
        let tgt_id = ContextId::new();
        {
            let mut r = kernel.drift().write().await;
            r.register(src_id, Some("source"), "doc-src", None);
            r.register(tgt_id, Some("target"), "doc-tgt", None);
        }

        let documents = crate::block_store::shared_block_store("test");
        // Create target document so flush can inject
        documents
            .create_document("doc-tgt".to_string(), crate::db::DocumentKind::Conversation, None)
            .unwrap();

        // Push via DriftPushEngine (target_ctx accepts label prefix)
        let push_engine = DriftPushEngine::new(&kernel, documents.clone(), src_id);
        let push_result = push_engine
            .execute(r#"{"target_ctx": "target", "content": "hello from source"}"#)
            .await
            .unwrap();
        assert!(push_result.success, "push failed: {}", push_result.stderr);

        // Verify queue directly on router
        {
            let router = kernel.drift().read().await;
            let queue = router.queue();
            assert_eq!(queue.len(), 1);
            assert_eq!(queue[0].source_ctx, src_id);
            assert_eq!(queue[0].target_ctx, tgt_id);
        }

        // Flush via DriftFlushEngine
        let flush_engine = DriftFlushEngine::new(&kernel, documents.clone(), src_id);
        let flush_result = flush_engine.execute("{}").await.unwrap();
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
        let parent_id = ContextId::new();
        {
            let mut r = router.write().await;
            r.register(parent_id, Some("main"), "doc-main", None);
        }

        // Clone the Arc (simulating what fork/thread does)
        let child_router = Arc::clone(&router);

        // Child should see the parent's contexts
        let child_handle = {
            let r = child_router.read().await;
            r.get(parent_id).map(|h| h.label.clone())
        };
        assert_eq!(child_handle, Some(Some("main".to_string())));

        // Child registers a new context
        let child_id = ContextId::new();
        {
            let mut r = child_router.write().await;
            r.register(child_id, Some("debug-fork"), "doc-debug", Some(parent_id));
        }

        // Parent should see the child's context
        let parent_sees_child = {
            let r = router.read().await;
            r.get(child_id).is_some()
        };
        assert!(parent_sees_child);
    }

    #[test]
    fn test_drift_flush_requeue_on_missing_target() {
        let mut router = DriftRouter::new();
        let src = ContextId::new();
        let tgt = ContextId::new();
        router.register(src, Some("source"), "doc-src", None);
        router.register(tgt, Some("target"), "doc-tgt", None);

        let staged_id = router
            .stage(src, tgt, "test content".into(), None, DriftKind::Push)
            .unwrap();

        assert_eq!(router.queue().len(), 1);
        assert_eq!(router.queue()[0].id, staged_id);

        // Unregister target (simulating context shutdown)
        router.unregister(tgt);

        let staged = router.drain(None);
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].id, staged_id);

        router.requeue(staged);

        assert_eq!(router.queue().len(), 1);
        assert_eq!(router.queue()[0].id, staged_id);
        assert_eq!(router.queue()[0].content, "test content");
    }

    #[test]
    fn test_requeue_method() {
        let mut router = DriftRouter::new();
        let src = ContextId::new();
        let tgt = ContextId::new();
        router.register(src, Some("source"), "doc-src", None);
        router.register(tgt, Some("target"), "doc-tgt", None);

        let id1 = router
            .stage(src, tgt, "first".into(), None, DriftKind::Push)
            .unwrap();
        let id2 = router
            .stage(src, tgt, "second".into(), None, DriftKind::Push)
            .unwrap();

        let drained = router.drain(None);
        assert_eq!(drained.len(), 2);
        assert!(router.queue().is_empty());

        router.requeue(drained);

        assert_eq!(router.queue().len(), 2);
        let ids: Vec<_> = router.queue().iter().map(|s| s.id).collect();
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
    }

    #[test]
    fn test_trace_id_generated() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, Some("traced"), "doc-traced", None);

        let handle = router.get(id).unwrap();
        // trace_id should be non-zero (generated from UUIDv4)
        assert_ne!(handle.trace_id, [0u8; 16]);
    }

    #[test]
    fn test_trace_ids_unique() {
        let mut router = DriftRouter::new();
        let a = ContextId::new();
        let b = ContextId::new();
        router.register(a, Some("alpha"), "doc-a", None);
        router.register(b, Some("beta"), "doc-b", None);

        let ta = router.get(a).unwrap().trace_id;
        let tb = router.get(b).unwrap().trace_id;
        assert_ne!(ta, tb);
    }

    #[test]
    fn test_doc_to_context_reverse_lookup() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, Some("main"), "doc-main", None);

        assert_eq!(router.context_for_document("doc-main"), Some(id));
        assert_eq!(router.context_for_document("doc-nonexistent"), None);
    }

    #[test]
    fn test_trace_id_for_document() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, Some("test"), "doc-test", None);

        let trace_id = router.trace_id_for_document("doc-test");
        assert!(trace_id.is_some());
        assert_eq!(trace_id.unwrap(), router.get(id).unwrap().trace_id);

        assert!(router.trace_id_for_document("doc-missing").is_none());
    }

    #[test]
    fn test_unregister_cleans_doc_index() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, Some("ephemeral"), "doc-eph", None);
        assert!(router.context_for_document("doc-eph").is_some());

        router.unregister(id);
        assert!(router.context_for_document("doc-eph").is_none());
    }
}
