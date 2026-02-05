//! Multi-context drifting — cross-context communication and content transfer.
//!
//! The DriftRouter is the central coordinator for moving content between kernel
//! contexts. It maintains a registry of all contexts (keyed by short IDs derived
//! from kernel UUIDs) and a staging queue for drift operations.
//!
//! # Flow
//!
//! ```text
//! drift push d4e5f6 "content"
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
use std::time::{SystemTime, UNIX_EPOCH};

use kaijutsu_crdt::{BlockKind, BlockSnapshot, DriftKind, Role};

/// Short ID length — first 6 hex chars of kernel UUID.
const SHORT_ID_LEN: usize = 6;

/// A registered context, mapping a human-friendly short ID to a kernel.
#[derive(Debug, Clone)]
pub struct ContextHandle {
    /// Short hex ID (e.g., "a1b2c3") — derived from kernel UUID.
    pub short_id: String,
    /// Internal kernel ID (e.g., "kernel-3") — key in ServerState.kernels.
    pub kernel_id: String,
    /// User-given name (e.g., "debug-session", "refactor-auth").
    pub name: String,
    /// Provider name if configured (e.g., "anthropic", "gemini").
    pub provider: Option<String>,
    /// Model name if configured (e.g., "claude-opus-4-6", "gemini-2.0-flash").
    pub model: Option<String>,
    /// Short ID of parent context (for fork lineage).
    pub parent_short_id: Option<String>,
    /// Creation timestamp (Unix epoch seconds).
    pub created_at: u64,
}

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

/// Central drift coordinator.
///
/// Lives in `ServerState` (not per-kernel) so it can see all contexts and
/// inject blocks into any kernel's document.
#[derive(Debug)]
pub struct DriftRouter {
    /// All registered contexts, keyed by short_id.
    contexts: HashMap<String, ContextHandle>,
    /// Staging queue for pending drift operations.
    staging: Vec<StagedDrift>,
    /// Counter for staged drift IDs.
    next_staged_id: u64,
    /// Reverse lookup: kernel_id → short_id.
    kernel_to_short: HashMap<String, String>,
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
            kernel_to_short: HashMap::new(),
        }
    }

    /// Register a context when a kernel is created.
    ///
    /// Generates a short ID from the kernel's UUID. If the first 6 hex chars
    /// collide with an existing context, extends until unique.
    pub fn register(
        &mut self,
        kernel_id: &str,
        name: &str,
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
            kernel_id: kernel_id.to_string(),
            name: name.to_string(),
            provider: None,
            model: None,
            parent_short_id: parent_short_id.map(|s| s.to_string()),
            created_at: now_epoch(),
        };

        self.kernel_to_short
            .insert(kernel_id.to_string(), short_id.clone());
        self.contexts.insert(short_id.clone(), handle);
        short_id
    }

    /// Unregister a context (e.g., when a kernel is destroyed).
    pub fn unregister(&mut self, short_id: &str) {
        if let Some(handle) = self.contexts.remove(short_id) {
            self.kernel_to_short.remove(&handle.kernel_id);
        }
    }

    /// Look up a context by short ID.
    pub fn get(&self, short_id: &str) -> Option<&ContextHandle> {
        self.contexts.get(short_id)
    }

    /// Look up context short ID by kernel ID.
    pub fn short_id_for_kernel(&self, kernel_id: &str) -> Option<&str> {
        self.kernel_to_short.get(kernel_id).map(|s| s.as_str())
    }

    /// Update provider/model for a context (used by configureLlm).
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
    /// This separation keeps DriftRouter free of CRDT/BlockStore dependencies.
    pub fn drain(&mut self, for_context: Option<&str>) -> Vec<StagedDrift> {
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

/// Errors from drift operations.
#[derive(Debug, thiserror::Error)]
pub enum DriftError {
    #[error("unknown context: {0}")]
    UnknownContext(String),
    #[error("kernel not found for context: {0}")]
    KernelNotFound(String),
    #[error("document error: {0}")]
    DocumentError(String),
    #[error("LLM error: {0}")]
    LlmError(String),
}

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

        // Skip empty blocks and overly verbose tool I/O
        if block.content.is_empty() {
            continue;
        }

        // Truncate very long blocks (tool results, shell output)
        // Find a valid UTF-8 boundary near 2000 bytes to avoid panicking
        let content = if block.content.len() > 2000 {
            let mut end = 2000;
            while end > 0 && !block.content.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}... [truncated, {} bytes total]", &block.content[..end], block.content.len())
        } else {
            block.content.clone()
        };

        transcript.push_str(&format!("**{}{}**: {}\n\n", role_label, kind_suffix, content));
    }

    if let Some(prompt) = directed_prompt {
        transcript.push_str(&format!(
            "\n---\n\nFocus your summary on: {}\n",
            prompt
        ));
    }

    transcript
}

/// Current Unix epoch in seconds.
fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_and_lookup() {
        let mut router = DriftRouter::new();
        let short_id = router.register("kernel-1", "main-session", None);

        assert_eq!(short_id.len(), SHORT_ID_LEN);
        let handle = router.get(&short_id).unwrap();
        assert_eq!(handle.kernel_id, "kernel-1");
        assert_eq!(handle.name, "main-session");
        assert!(handle.parent_short_id.is_none());
    }

    #[test]
    fn test_register_with_parent() {
        let mut router = DriftRouter::new();
        let parent_id = router.register("kernel-1", "main", None);
        let child_id = router.register("kernel-2", "fork-debug", Some(&parent_id));

        let child = router.get(&child_id).unwrap();
        assert_eq!(child.parent_short_id.as_deref(), Some(parent_id.as_str()));
    }

    #[test]
    fn test_short_id_uniqueness() {
        let mut router = DriftRouter::new();
        let mut ids = Vec::new();
        // Register many contexts — all should get unique IDs
        for i in 0..100 {
            let id = router.register(&format!("kernel-{}", i), &format!("ctx-{}", i), None);
            assert!(!ids.contains(&id), "duplicate short_id: {}", id);
            ids.push(id);
        }
    }

    #[test]
    fn test_kernel_to_short_id() {
        let mut router = DriftRouter::new();
        let short = router.register("kernel-42", "test", None);
        assert_eq!(router.short_id_for_kernel("kernel-42"), Some(short.as_str()));
        assert_eq!(router.short_id_for_kernel("kernel-99"), None);
    }

    #[test]
    fn test_configure_llm() {
        let mut router = DriftRouter::new();
        let short = router.register("kernel-1", "test", None);

        router.configure_llm(&short, "gemini", "gemini-2.0-flash").unwrap();

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
        let src = router.register("kernel-1", "source", None);
        let tgt = router.register("kernel-2", "target", None);

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
        let src = router.register("kernel-1", "source", None);

        let result = router.stage(&src, "bad", "nope".into(), None, DriftKind::Push);
        assert!(result.is_err());
    }

    #[test]
    fn test_cancel() {
        let mut router = DriftRouter::new();
        let src = router.register("kernel-1", "src", None);
        let tgt = router.register("kernel-2", "tgt", None);

        let id1 = router.stage(&src, &tgt, "one".into(), None, DriftKind::Push).unwrap();
        let _id2 = router.stage(&src, &tgt, "two".into(), None, DriftKind::Push).unwrap();

        assert_eq!(router.queue().len(), 2);
        assert!(router.cancel(id1));
        assert_eq!(router.queue().len(), 1);
        assert_eq!(router.queue()[0].content, "two");
    }

    #[test]
    fn test_drain() {
        let mut router = DriftRouter::new();
        let src = router.register("kernel-1", "src", None);
        let tgt = router.register("kernel-2", "tgt", None);

        router.stage(&src, &tgt, "a".into(), None, DriftKind::Push).unwrap();
        router.stage(&src, &tgt, "b".into(), None, DriftKind::Push).unwrap();

        let drained = router.drain(None);
        assert_eq!(drained.len(), 2);
        assert!(router.queue().is_empty());
    }

    #[test]
    fn test_build_drift_block() {
        let mut router = DriftRouter::new();
        let src = router.register("kernel-1", "src", None);
        let tgt = router.register("kernel-2", "tgt", None);

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
        let short = router.register("kernel-1", "test", None);

        assert!(router.get(&short).is_some());
        router.unregister(&short);
        assert!(router.get(&short).is_none());
        assert!(router.short_id_for_kernel("kernel-1").is_none());
    }

    #[test]
    fn test_list_contexts_sorted() {
        let mut router = DriftRouter::new();
        let _a = router.register("kernel-1", "alpha", None);
        let _b = router.register("kernel-2", "beta", None);
        let _c = router.register("kernel-3", "gamma", None);

        let list = router.list_contexts();
        assert_eq!(list.len(), 3);
        // Sorted by created_at (all same second, so insertion order ≈ stable)
        for i in 1..list.len() {
            assert!(list[i].created_at >= list[i - 1].created_at);
        }
    }

    #[test]
    fn test_drain_scoped() {
        let mut router = DriftRouter::new();
        let a = router.register("kernel-1", "alpha", None);
        let b = router.register("kernel-2", "beta", None);
        let c = router.register("kernel-3", "gamma", None);

        // Stage: a→b and c→b
        router.stage(&a, &b, "from alpha".into(), None, DriftKind::Push).unwrap();
        router.stage(&c, &b, "from gamma".into(), None, DriftKind::Push).unwrap();

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

        let blocks = vec![
            BlockSnapshot::text(
                BlockId::new("doc", "agent", 0),
                None,
                Role::User,
                "Let's discuss auth and caching.",
                "user",
            ),
        ];

        let prompt = build_distillation_prompt(&blocks, Some("what was decided about caching?"));
        assert!(prompt.contains("Focus your summary on: what was decided about caching?"));
    }

    #[test]
    fn test_build_distillation_prompt_truncates_long_blocks() {
        use kaijutsu_crdt::BlockId;

        let long_content = "x".repeat(3000);
        let blocks = vec![
            BlockSnapshot::tool_result(
                BlockId::new("doc", "agent", 0),
                BlockId::new("doc", "agent", 99), // fake tool_call_id
                &long_content,
                false,
                None,
                "tool",
            ),
        ];

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
}
