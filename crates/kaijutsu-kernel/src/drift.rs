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
//! DriftRouter.flush() → insert_drift_block() on target document
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use kaijutsu_crdt::{
    BlockKind, BlockSnapshot, ContextId, DriftKind, PrefixError, Role, resolve_context_prefix,
};
use kaijutsu_types::{ContextState, PrincipalId};

use crate::llm::config::ToolFilter;

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
    /// Working directory in VFS (e.g., "/mnt/kaijutsu").
    pub pwd: Option<String>,
    /// Provider name if configured (e.g., "anthropic", "gemini").
    pub provider: Option<String>,
    /// Model name if configured (e.g., "claude-opus-4-6", "gemini-2.0-flash").
    pub model: Option<String>,
    /// Fork source context ID (for fork lineage).
    pub forked_from: Option<ContextId>,
    /// Who created this context.
    pub created_by: PrincipalId,
    /// Creation timestamp (Unix millis).
    pub created_at: u64,
    /// Long-running OTel trace ID for this context.
    ///
    /// Generated at registration time. All RPC operations touching this
    /// context become child spans under this trace ID, enabling
    /// "show me everything that happened in context X" queries.
    pub trace_id: [u8; 16],
    /// Per-context tool filter (None = inherit kernel default).
    ///
    /// When set, merged with the kernel's tool config at resolution time.
    /// Context filters can restrict (not relax) the kernel's tool set.
    pub tool_filter: Option<ToolFilter>,
    /// Lifecycle state — controls what operations are permitted.
    pub state: ContextState,
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
    /// Number of times this drift has been requeued after a delivery failure.
    /// Items exceeding [`MAX_DRIFT_RETRIES`] are dropped on requeue.
    pub retry_count: u32,
}

/// Maximum number of requeue attempts before a staged drift is discarded.
const MAX_DRIFT_RETRIES: u32 = 5;

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
    /// Dead letter queue: drifts that exceeded retry limit or whose target was
    /// unregistered before delivery. Drained by the flush engine into the
    /// "lost+found" context so content is never silently discarded.
    dead_letter: Vec<StagedDrift>,
    /// Counter for staged drift IDs.
    next_staged_id: u64,
    /// Reverse lookup: label → ContextId (for prefix matching).
    label_to_id: HashMap<String, ContextId>,
    /// ContextId for the lazy "lost+found" context, created on first dead letter.
    lost_found_id: Option<ContextId>,
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
            dead_letter: Vec::new(),
            next_staged_id: 1,
            label_to_id: HashMap::new(),
            lost_found_id: None,
        }
    }

    /// Register a context with a pre-assigned ContextId.
    ///
    /// The caller (server RPC) creates the ContextId and passes it in.
    /// Provider and model default to None; use `configure_llm()` to set them.
    /// Returns an error if the label is already in use by a different context.
    #[tracing::instrument(skip(self), name = "drift.register")]
    pub fn register(
        &mut self,
        id: ContextId,
        label: Option<&str>,
        forked_from: Option<ContextId>,
        created_by: PrincipalId,
    ) -> Result<(), DriftError> {
        if let Some(l) = label {
            self.check_label_available(l, id)?;
            self.label_to_id.insert(l.to_string(), id);
        }

        let handle = ContextHandle {
            id,
            label: label.map(|s| s.to_string()),
            pwd: None,
            provider: None,
            model: None,
            forked_from,
            created_by,
            created_at: kaijutsu_types::now_millis(),
            trace_id: uuid::Uuid::new_v4().into_bytes(),
            tool_filter: None,
            state: ContextState::Live,
        };

        self.contexts.insert(id, handle);
        Ok(())
    }

    /// Register a forked context, inheriting provider/model from the parent.
    ///
    /// Model is immutable on a context after creation — fork to change it.
    /// Returns an error if the parent context is not registered.
    #[tracing::instrument(skip(self), name = "drift.register_fork")]
    pub fn register_fork(
        &mut self,
        id: ContextId,
        label: Option<&str>,
        forked_from: ContextId,
        created_by: PrincipalId,
    ) -> Result<(), DriftError> {
        let parent = self
            .contexts
            .get(&forked_from)
            .ok_or_else(|| DriftError::UnknownContext(forked_from.short()))?;

        // Inherit parent's provider/model/tool_filter (COW semantics — snapshot at fork time)
        let parent_provider = parent.provider.clone();
        let parent_model = parent.model.clone();
        let parent_tool_filter = parent.tool_filter.clone();

        if let Some(l) = label {
            self.check_label_available(l, id)?;
            self.label_to_id.insert(l.to_string(), id);
        }

        let handle = ContextHandle {
            id,
            label: label.map(|s| s.to_string()),
            pwd: None,
            provider: parent_provider,
            model: parent_model,
            forked_from: Some(forked_from),
            created_by,
            created_at: kaijutsu_types::now_millis(),
            trace_id: uuid::Uuid::new_v4().into_bytes(),
            tool_filter: parent_tool_filter,
            state: ContextState::Live,
        };

        self.contexts.insert(id, handle);
        Ok(())
    }

    /// Unregister a context (e.g., when a context is destroyed).
    #[tracing::instrument(skip(self), name = "drift.unregister")]
    pub fn unregister(&mut self, id: ContextId) {
        if let Some(handle) = self.contexts.remove(&id)
            && let Some(label) = &handle.label
        {
            self.label_to_id.remove(label);
        }
        // Move staged drifts involving this context to the dead letter queue
        // so content is never silently discarded. Source-deleted drifts are
        // still deliverable (we have the content string); target-deleted drifts
        // can never reach their destination. Both go to dead letter so the
        // flush engine can write them to "lost+found" for human recovery.
        let (dead, keep): (Vec<_>, Vec<_>) = self
            .staging
            .drain(..)
            .partition(|s| s.source_ctx == id || s.target_ctx == id);
        self.staging = keep;
        if !dead.is_empty() {
            tracing::info!(
                context = %id.short(),
                count = dead.len(),
                "moving staged drifts to dead letter for unregistered context"
            );
            self.dead_letter.extend(dead);
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
    ///
    /// Returns an error if the new label is already in use by a different context.
    #[tracing::instrument(skip(self), name = "drift.rename")]
    pub fn rename(&mut self, id: ContextId, new_label: Option<&str>) -> Result<(), DriftError> {
        // Check availability before mutating anything
        if let Some(l) = new_label {
            self.check_label_available(l, id)?;
        }

        let handle = self
            .contexts
            .get_mut(&id)
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

    /// Check that a label is not already in use by a different context.
    fn check_label_available(&self, label: &str, id: ContextId) -> Result<(), DriftError> {
        if let Some(&existing_id) = self.label_to_id.get(label) {
            if existing_id != id {
                return Err(DriftError::LabelInUse {
                    label: label.to_string(),
                    existing: existing_id.short(),
                });
            }
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

    /// Update tool filter for a context.
    ///
    /// Set to `Some(filter)` to restrict tools, or `None` to inherit kernel default.
    pub fn configure_tools(
        &mut self,
        id: ContextId,
        filter: Option<ToolFilter>,
    ) -> Result<(), DriftError> {
        let handle = self
            .contexts
            .get_mut(&id)
            .ok_or_else(|| DriftError::UnknownContext(id.short()))?;
        handle.tool_filter = filter;
        Ok(())
    }

    /// Set the working directory for a context.
    pub fn set_pwd(&mut self, id: ContextId, pwd: Option<String>) -> Result<(), DriftError> {
        let handle = self
            .contexts
            .get_mut(&id)
            .ok_or_else(|| DriftError::UnknownContext(id.short()))?;
        handle.pwd = pwd;
        Ok(())
    }

    /// Get the lifecycle state of a context.
    pub fn context_state(&self, id: ContextId) -> Option<ContextState> {
        self.contexts.get(&id).map(|h| h.state)
    }

    /// Set the lifecycle state of a context (e.g., Staging → Live).
    pub fn set_state(
        &mut self,
        id: ContextId,
        state: ContextState,
    ) -> Result<(), DriftError> {
        let handle = self
            .contexts
            .get_mut(&id)
            .ok_or_else(|| DriftError::UnknownContext(id.short()))?;
        handle.state = state;
        Ok(())
    }

    /// List all registered contexts.
    pub fn list_contexts(&self) -> Vec<&ContextHandle> {
        let mut contexts: Vec<_> = self.contexts.values().collect();
        contexts.sort_by_key(|c| c.created_at);
        contexts
    }

    /// Look up the trace_id for a context.
    pub fn trace_id_for_context(&self, id: ContextId) -> Option<[u8; 16]> {
        self.contexts.get(&id).map(|h| h.trace_id)
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
            created_at: kaijutsu_types::now_millis(),
            retry_count: 0,
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
                let (matched, remaining): (Vec<_>, Vec<_>) = std::mem::take(&mut self.staging)
                    .into_iter()
                    .partition(|s| s.source_ctx == ctx || s.target_ctx == ctx);
                self.staging = remaining;
                matched
            }
        }
    }

    /// Re-queue staged drifts that failed to deliver.
    ///
    /// Increments each item's `retry_count`. Items that have already been
    /// requeued [`MAX_DRIFT_RETRIES`] times move to the dead letter queue
    /// rather than accumulating indefinitely under persistent failure.
    /// The flush engine writes dead letter items to "lost+found" so content
    /// is never silently discarded.
    pub fn requeue(&mut self, items: Vec<StagedDrift>) {
        for mut item in items {
            item.retry_count += 1;
            if item.retry_count > MAX_DRIFT_RETRIES {
                tracing::warn!(
                    drift_id = item.id,
                    source = %item.source_ctx.short(),
                    target = %item.target_ctx.short(),
                    retries = item.retry_count,
                    "staged drift exceeded {} retries, moving to dead letter queue",
                    MAX_DRIFT_RETRIES,
                );
                self.dead_letter.push(item);
            } else {
                self.staging.push(item);
            }
        }
    }

    /// Drain all items from the dead letter queue.
    ///
    /// Called by the flush engine to write dead letter content to the
    /// "lost+found" context in the block store.
    pub fn drain_dead_letter(&mut self) -> Vec<StagedDrift> {
        std::mem::take(&mut self.dead_letter)
    }

    /// Get or create the ContextId for the "lost+found" context.
    ///
    /// Lazily creates and registers the context on first call. The caller is
    /// responsible for creating the corresponding block store document.
    pub fn ensure_lost_found(&mut self) -> (ContextId, bool) {
        if let Some(id) = self.lost_found_id {
            return (id, false);
        }
        let id = ContextId::new();
        self.lost_found_id = Some(id);
        // lost+found label is system-reserved; conflict is a programming error
        self.register(id, Some("lost+found"), None, PrincipalId::system())
            .expect("lost+found label should never conflict");
        tracing::info!(context = %id.short(), "created lost+found context");
        (id, true)
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
    #[error("label '{label}' already in use by context {existing}")]
    LabelInUse { label: String, existing: String },
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
// TODO: Use query_blocks with kind/compacted filter once drift goes through RPC.
// Current in-process DashMap reads are fast; optimize when drift becomes remote-capable.
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
            Role::Asset => "Asset",
        };

        let kind_suffix = match block.kind {
            BlockKind::Thinking => " (thinking)",
            BlockKind::ToolCall => " (tool call)",
            BlockKind::ToolResult => " (tool result)",
            BlockKind::Drift => " (drift)",
            BlockKind::File => " (file)",
            BlockKind::Error => " (error)",
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
        transcript.push_str(&format!("\n---\n\nFocus your summary on: {}\n", prompt));
    }

    transcript
}

// Drift engines removed — all drift operations go through `kj` commands via KjDispatcher.

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
        router.register(id, Some("main-session"), None, PrincipalId::system()).unwrap();

        let handle = router.get(id).unwrap();
        assert_eq!(handle.label.as_deref(), Some("main-session"));
        assert!(handle.forked_from.is_none());
    }

    #[test]
    fn test_register_with_parent() {
        let mut router = DriftRouter::new();
        let parent_id = ContextId::new();
        let child_id = ContextId::new();
        router.register(parent_id, Some("main"), None, PrincipalId::system()).unwrap();
        router.register(
            child_id,
            Some("fork-debug"),
            Some(parent_id),
            PrincipalId::system(),
        ).unwrap();

        let child = router.get(child_id).unwrap();
        assert_eq!(child.forked_from, Some(parent_id));
    }

    #[test]
    fn test_resolve_by_label() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, Some("test-ctx"), None, PrincipalId::system()).unwrap();
        assert_eq!(router.resolve_context("test-ctx").unwrap(), id);
    }

    #[test]
    fn test_resolve_by_label_prefix() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, Some("test-ctx"), None, PrincipalId::system()).unwrap();
        let other_id = ContextId::new();
        router.register(other_id, Some("debug"), None, PrincipalId::system()).unwrap();
        assert_eq!(router.resolve_context("test").unwrap(), id);
    }

    #[test]
    fn test_resolve_by_hex_prefix() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, None, None, PrincipalId::system()).unwrap();
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
        router.register(id, Some("main"), None, PrincipalId::system()).unwrap();
        assert!(router.resolve_context("nonexistent").is_err());
    }

    #[test]
    fn test_configure_llm() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, Some("test"), None, PrincipalId::system()).unwrap();

        router
            .configure_llm(id, "gemini", "gemini-2.0-flash")
            .unwrap();

        let handle = router.get(id).unwrap();
        assert_eq!(handle.provider.as_deref(), Some("gemini"));
        assert_eq!(handle.model.as_deref(), Some("gemini-2.0-flash"));
    }

    #[test]
    fn test_register_fork_inherits_model() {
        let mut router = DriftRouter::new();
        let parent_id = ContextId::new();
        router.register(parent_id, Some("parent"), None, PrincipalId::system()).unwrap();
        router
            .configure_llm(parent_id, "anthropic", "claude-sonnet-4-20250514")
            .unwrap();

        let child_id = ContextId::new();
        router
            .register_fork(child_id, Some("child"), parent_id, PrincipalId::system())
            .unwrap();

        let child = router.get(child_id).unwrap();
        assert_eq!(child.provider.as_deref(), Some("anthropic"));
        assert_eq!(child.model.as_deref(), Some("claude-sonnet-4-20250514"));
        assert_eq!(child.forked_from, Some(parent_id));
    }

    #[test]
    fn test_register_fork_no_parent_model() {
        let mut router = DriftRouter::new();
        let parent_id = ContextId::new();
        router.register(parent_id, Some("bare"), None, PrincipalId::system()).unwrap();
        // Parent has no model set

        let child_id = ContextId::new();
        router
            .register_fork(child_id, None, parent_id, PrincipalId::system())
            .unwrap();

        let child = router.get(child_id).unwrap();
        assert_eq!(child.provider, None);
        assert_eq!(child.model, None);
        assert_eq!(child.forked_from, Some(parent_id));
    }

    #[test]
    fn test_register_fork_missing_parent_errors() {
        let mut router = DriftRouter::new();
        let missing_parent = ContextId::new();
        let child_id = ContextId::new();
        let result = router.register_fork(
            child_id,
            Some("orphan"),
            missing_parent,
            PrincipalId::system(),
        );
        assert!(
            result.is_err(),
            "should error when parent is not registered"
        );
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
        router.register(src, Some("source"), None, PrincipalId::system()).unwrap();
        router.register(tgt, Some("target"), None, PrincipalId::system()).unwrap();

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
        router.register(src, Some("source"), None, PrincipalId::system()).unwrap();

        let result = router.stage(src, ContextId::new(), "nope".into(), None, DriftKind::Push);
        assert!(result.is_err());
    }

    #[test]
    fn test_cancel() {
        let mut router = DriftRouter::new();
        let src = ContextId::new();
        let tgt = ContextId::new();
        router.register(src, Some("src"), None, PrincipalId::system()).unwrap();
        router.register(tgt, Some("tgt"), None, PrincipalId::system()).unwrap();

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
        router.register(src, Some("src"), None, PrincipalId::system()).unwrap();
        router.register(tgt, Some("tgt"), None, PrincipalId::system()).unwrap();

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
    fn test_unregister() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, Some("test"), None, PrincipalId::system()).unwrap();

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
        router.register(a, Some("alpha"), None, PrincipalId::system()).unwrap();
        router.register(b, Some("beta"), None, PrincipalId::system()).unwrap();
        router.register(c, Some("gamma"), None, PrincipalId::system()).unwrap();

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
        router.register(a, Some("alpha"), None, PrincipalId::system()).unwrap();
        router.register(b, Some("beta"), None, PrincipalId::system()).unwrap();
        router.register(c, Some("gamma"), None, PrincipalId::system()).unwrap();

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
        router.register(id, Some("old-name"), None, PrincipalId::system()).unwrap();

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
        use kaijutsu_types::PrincipalId;
        let ctx = ContextId::new();
        let agent = PrincipalId::new();

        let blocks = vec![
            BlockSnapshot::text(
                BlockId::new(ctx, agent, 0),
                None,
                Role::User,
                "How do I fix the auth bug?",
            ),
            BlockSnapshot::text(
                BlockId::new(ctx, agent, 1),
                None,
                Role::Model,
                "The auth bug is caused by a race condition in the session handler.",
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
        use kaijutsu_types::PrincipalId;
        let ctx = ContextId::new();
        let agent = PrincipalId::new();

        let blocks = vec![BlockSnapshot::text(
            BlockId::new(ctx, agent, 0),
            None,
            Role::User,
            "Let's discuss auth and caching.",
        )];

        let prompt = build_distillation_prompt(&blocks, Some("what was decided about caching?"));
        assert!(prompt.contains("Focus your summary on: what was decided about caching?"));
    }

    #[test]
    fn test_build_distillation_prompt_truncates_long_blocks() {
        use kaijutsu_crdt::BlockId;
        use kaijutsu_types::{PrincipalId, ToolKind};
        let ctx = ContextId::new();
        let agent = PrincipalId::new();

        let long_content = "x".repeat(3000);
        let blocks = vec![BlockSnapshot::tool_result(
            BlockId::new(ctx, agent, 0),
            BlockId::new(ctx, agent, 99),
            ToolKind::Builtin,
            &long_content,
            false,
            None,
            None,
        )];

        let prompt = build_distillation_prompt(&blocks, None);
        assert!(prompt.contains("[truncated, 3000 bytes total]"));
        assert!(!prompt.contains(&long_content));
    }

    #[test]
    fn test_build_distillation_prompt_skips_empty() {
        use kaijutsu_crdt::BlockId;
        use kaijutsu_types::PrincipalId;
        let ctx = ContextId::new();
        let agent = PrincipalId::new();

        let blocks = vec![
            BlockSnapshot::text(BlockId::new(ctx, agent, 0), None, Role::User, ""),
            BlockSnapshot::text(
                BlockId::new(ctx, agent, 1),
                None,
                Role::Model,
                "Only this should appear.",
            ),
        ];

        let prompt = build_distillation_prompt(&blocks, None);
        assert!(!prompt.contains("**User**:"));
        assert!(prompt.contains("**Assistant**: Only this should appear."));
    }

    #[tokio::test]
    async fn test_shared_drift_on_fork() {
        // The SharedDriftRouter should be shareable across kernel fork/thread
        let router = shared_drift_router();

        // Register from "parent" side
        let parent_id = ContextId::new();
        {
            let mut r = router.write().await;
            r.register(parent_id, Some("main"), None, PrincipalId::system()).unwrap();
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
            r.register(
                child_id,
                Some("debug-fork"),
                Some(parent_id),
                PrincipalId::system(),
            ).unwrap();
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
        router.register(src, Some("source"), None, PrincipalId::system()).unwrap();
        router.register(tgt, Some("target"), None, PrincipalId::system()).unwrap();

        let staged_id = router
            .stage(src, tgt, "test content".into(), None, DriftKind::Push)
            .unwrap();

        assert_eq!(router.queue().len(), 1);
        assert_eq!(router.queue()[0].id, staged_id);

        // Unregister target (simulating context shutdown).
        // New behavior: staged drifts targeting the removed context move to the
        // dead letter queue rather than remaining in staging indefinitely.
        router.unregister(tgt);

        // Staging queue is now empty — item moved to dead letter.
        assert!(
            router.queue().is_empty(),
            "staging queue should be empty after unregister"
        );

        // Dead letter should have exactly the item that was waiting for tgt.
        let dead = router.drain_dead_letter();
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].id, staged_id);
        assert_eq!(dead[0].content, "test content");
        assert_eq!(dead[0].target_ctx, tgt);
    }

    #[test]
    fn test_requeue_method() {
        let mut router = DriftRouter::new();
        let src = ContextId::new();
        let tgt = ContextId::new();
        router.register(src, Some("source"), None, PrincipalId::system()).unwrap();
        router.register(tgt, Some("target"), None, PrincipalId::system()).unwrap();

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
        // retry_count should have been incremented
        assert!(router.queue().iter().all(|s| s.retry_count == 1));
    }

    #[test]
    fn test_requeue_retry_limit_moves_to_dead_letter() {
        let mut router = DriftRouter::new();
        let src = ContextId::new();
        let tgt = ContextId::new();
        router.register(src, Some("source"), None, PrincipalId::system()).unwrap();
        router.register(tgt, Some("target"), None, PrincipalId::system()).unwrap();

        let _id = router
            .stage(src, tgt, "persistent failure".into(), None, DriftKind::Push)
            .unwrap();

        // Drain and requeue MAX_DRIFT_RETRIES + 1 times — last requeue should
        // push to dead letter.
        for i in 0..=MAX_DRIFT_RETRIES {
            let drained = router.drain(None);
            assert_eq!(
                drained.len(),
                1,
                "expected item in queue on iteration {}",
                i
            );
            router.requeue(drained);
            if i < MAX_DRIFT_RETRIES {
                assert_eq!(
                    router.queue().len(),
                    1,
                    "item should still be in queue after {} requeues",
                    i + 1
                );
                assert!(
                    router.drain_dead_letter().is_empty(),
                    "dead letter should be empty at retry {}",
                    i
                );
            }
        }

        // After MAX_DRIFT_RETRIES+1 requeues, staging is empty and dead letter has the item.
        assert!(
            router.queue().is_empty(),
            "staging should be empty after retry limit"
        );
        let dead = router.drain_dead_letter();
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].content, "persistent failure");
        assert_eq!(dead[0].retry_count, MAX_DRIFT_RETRIES + 1);
    }

    #[test]
    fn test_ensure_lost_found_idempotent() {
        let mut router = DriftRouter::new();
        let (id1, is_new1) = router.ensure_lost_found();
        assert!(is_new1);
        let (id2, is_new2) = router.ensure_lost_found();
        assert!(!is_new2);
        assert_eq!(id1, id2);
        // Should be registered with label "lost+found"
        assert!(router.resolve_context("lost+found").is_ok());
    }

    #[test]
    fn test_trace_id_generated() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, Some("traced"), None, PrincipalId::system()).unwrap();

        let handle = router.get(id).unwrap();
        // trace_id should be non-zero (generated from UUIDv4)
        assert_ne!(handle.trace_id, [0u8; 16]);
    }

    #[test]
    fn test_trace_ids_unique() {
        let mut router = DriftRouter::new();
        let a = ContextId::new();
        let b = ContextId::new();
        router.register(a, Some("alpha"), None, PrincipalId::system()).unwrap();
        router.register(b, Some("beta"), None, PrincipalId::system()).unwrap();

        let ta = router.get(a).unwrap().trace_id;
        let tb = router.get(b).unwrap().trace_id;
        assert_ne!(ta, tb);
    }

    #[test]
    fn test_trace_id_for_context() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, Some("test"), None, PrincipalId::system()).unwrap();

        let trace_id = router.trace_id_for_context(id);
        assert!(trace_id.is_some());
        assert_eq!(trace_id.unwrap(), router.get(id).unwrap().trace_id);

        assert!(router.trace_id_for_context(ContextId::new()).is_none());
    }

    #[test]
    fn test_unregister_cleans_label_index() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, Some("ephemeral"), None, PrincipalId::system()).unwrap();

        assert!(router.get(id).is_some());
        router.unregister(id);
        assert!(router.get(id).is_none());
        assert!(router.resolve_context("ephemeral").is_err());
    }

    /// Duplicate labels are rejected at registration time.
    #[test]
    fn test_duplicate_label_rejected_at_registration() {
        let mut router = DriftRouter::new();
        let pid_a = PrincipalId::new();
        let pid_b = PrincipalId::new();
        let id_a = ContextId::new();
        let id_b = ContextId::new();

        // User A registers "notes"
        router.register(id_a, Some("notes"), None, pid_a).unwrap();
        assert_eq!(router.resolve_context("notes").unwrap(), id_a);

        // User B tries to register "notes" — rejected
        let result = router.register(id_b, Some("notes"), None, pid_b);
        assert!(
            result.is_err(),
            "Duplicate label should be rejected, but got Ok",
        );

        // User A's context is still resolvable
        assert_eq!(router.resolve_context("notes").unwrap(), id_a);

        // User B's context was not registered (label conflict aborts the whole op)
        assert!(router.get(id_b).is_none());
    }

    /// Re-registering the same context with the same label is idempotent.
    #[test]
    fn test_reregister_same_label_is_idempotent() {
        let mut router = DriftRouter::new();
        let pid = PrincipalId::new();
        let id = ContextId::new();

        router.register(id, Some("notes"), None, pid).unwrap();
        // Re-registering with the same ID and label should succeed
        router.register(id, Some("notes"), None, pid).unwrap();
        assert_eq!(router.resolve_context("notes").unwrap(), id);
    }

    /// Renaming to an existing label is rejected.
    #[test]
    fn test_rename_to_existing_label_rejected() {
        let mut router = DriftRouter::new();
        let a = ContextId::new();
        let b = ContextId::new();

        router.register(a, Some("alpha"), None, PrincipalId::new()).unwrap();
        router.register(b, Some("beta"), None, PrincipalId::new()).unwrap();

        let result = router.rename(b, Some("alpha"));
        assert!(result.is_err(), "Rename to existing label should be rejected");

        // Original labels still work
        assert_eq!(router.resolve_context("alpha").unwrap(), a);
        assert_eq!(router.resolve_context("beta").unwrap(), b);
    }
}
