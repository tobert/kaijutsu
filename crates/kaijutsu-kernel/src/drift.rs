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
//! drift push <ctx> "content"          drift push --stage <ctx> "content"
//!       │                                   │
//!       ▼                                   ▼
//! insert_drift_block() on            DriftRouter.stage(StagedDrift { ... })
//! target document, now                     │
//!                                    drift flush
//!                                          │
//!                                          ▼
//!                                    DriftRouter.flush() → insert_drift_block()
//! ```
//!
//! `push` delivers immediately; `--stage` opts into the batching queue that
//! `queue`/`cancel`/`flush` operate on. A failed immediate delivery falls back
//! to the staging queue (loudly, as an error) so content is never lost.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use kaijutsu_types::{
    BlockKind, BlockSnapshot, ContextId, DriftKind, PrefixError, Role, resolve_context_prefix,
};
use kaijutsu_types::{ContextState, PrincipalId};

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
    /// The principal that STAGED this drift — the author the delivered block
    /// gets, not whoever later runs `flush`. Staging splits sender from
    /// deliverer, and contexts are shared, so the two are routinely different
    /// principals; attributing a flushed batch to the flusher would silently
    /// relabel every message in it. Captured here because the sender's
    /// identity is otherwise gone by the time the item is drained.
    pub staged_by: PrincipalId,
    /// Creation timestamp (Unix epoch seconds).
    pub created_at: u64,
    /// Number of times this drift has been requeued after a delivery failure.
    /// Items exceeding [`MAX_DRIFT_RETRIES`] are dropped on requeue.
    pub retry_count: u32,
    /// True while a flush has [`drain`](DriftRouter::drain)ed this item for
    /// delivery but has not yet reported the outcome via
    /// [`complete`](DriftRouter::complete) (success) or
    /// [`requeue`](DriftRouter::requeue) (failure). The item stays physically
    /// in `DriftRouter::staging` while checked out — only the flag flips —
    /// so it remains visible to `queue()` and reachable by `cancel()` during
    /// the async delivery window. Before this flag existed, `drain` removed
    /// the item into the caller's local `Vec`, making it invisible to a
    /// concurrent `cancel` until the flush finished; a cancelled-but-in-flight
    /// item then survived `requeue` and came back on the next flush.
    pub in_flight: bool,
    /// Set by [`cancel`](DriftRouter::cancel) when the item was already
    /// in flight. `requeue` consults this to drop the item instead of
    /// restoring it — a cancel that lands mid-delivery must not be undone
    /// by the delivery's own failure-handling.
    pub cancelled: bool,
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

        // Inherit parent's provider/model (COW semantics — snapshot at fork time)
        let parent_provider = parent.provider.clone();
        let parent_model = parent.model.clone();

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
        if let Some(&existing_id) = self.label_to_id.get(label)
            && existing_id != id
        {
            return Err(DriftError::LabelInUse {
                label: label.to_string(),
                existing: existing_id.short(),
            });
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

    /// The lost+found context id, if one has been created.
    pub fn lost_found_id(&self) -> Option<ContextId> {
        self.lost_found_id
    }

    /// Claim an already-registered context as the lost+found sink.
    ///
    /// Cold-start hydration rebuilds the router from `KernelDb` rows and
    /// registers every persisted context through the normal
    /// [`Self::register`] path — but the router's `lost_found_id` resets to
    /// `None` across restarts. It is no longer recovered by matching a
    /// reserved label: the caller reads `WellKnownRole::LostFound` out of the
    /// registry (`KernelDb::well_known_context`) and passes the id it finds
    /// there. Without re-claiming it, the next dead letter would mint a
    /// *duplicate* lost+found and orphan the persisted one.
    pub fn adopt_lost_found(&mut self, id: ContextId) {
        match self.lost_found_id {
            Some(existing) if existing == id => {}
            Some(existing) => tracing::warn!(
                existing = %existing.short(),
                candidate = %id.short(),
                "adopt_lost_found ignored: a different lost+found is already claimed"
            ),
            None => self.lost_found_id = Some(id),
        }
    }

    /// Vacate the lost+found role, returning the outgoing context's id (or
    /// `None` if no lost+found was claimed).
    ///
    /// This is the router half of a rotation: the outgoing context is left
    /// registered exactly as it was — it keeps its blocks, its label, and its
    /// handle, and goes on existing as an ordinary context. Only the
    /// router's notion of "the current lost+found" changes. The router holds
    /// no `KernelDb` handle, so clearing the persisted `well_known_contexts`
    /// row (or pointing it at a successor) is the caller's job — this alone
    /// does not survive a restart.
    pub fn rotate_lost_found(&mut self) -> Option<ContextId> {
        self.lost_found_id.take()
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
        staged_by: PrincipalId,
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
            staged_by,
            created_at: kaijutsu_types::now_millis(),
            retry_count: 0,
            in_flight: false,
            cancelled: false,
        });

        Ok(id)
    }

    /// Cancel a staged drift by ID.
    ///
    /// Works whether the item is still waiting (removed outright, the
    /// original behavior) or currently checked out by a flush's
    /// [`drain`](Self::drain) (marked [`cancelled`](StagedDrift::cancelled)
    /// instead — the item may already be mid-delivery, so `cancel` cannot
    /// yank it out from under the in-flight I/O; it records the intent and
    /// [`requeue`](Self::requeue) honors it if delivery fails).
    pub fn cancel(&mut self, staged_id: u64) -> bool {
        let Some(pos) = self.staging.iter().position(|s| s.id == staged_id) else {
            return false;
        };
        if self.staging[pos].in_flight {
            self.staging[pos].cancelled = true;
        } else {
            self.staging.remove(pos);
        }
        true
    }

    /// View the staging queue — includes items currently checked out by a
    /// flush ([`StagedDrift::in_flight`]), not just ones still waiting. A
    /// player running `kj drift queue` mid-flush sees the whole picture
    /// instead of the item silently vanishing until the flush resolves it.
    pub fn queue(&self) -> &[StagedDrift] {
        &self.staging
    }

    /// Drain the staging queue, returning staged drifts for processing.
    ///
    /// If `for_context` is `Some`, only drains items where the source or target
    /// matches the given context. Otherwise drains everything.
    ///
    /// Unlike a true drain, matched items are **not removed** from
    /// `staging` — they are marked [`in_flight`](StagedDrift::in_flight) and
    /// a clone is returned to the caller. This keeps them visible to
    /// `queue()` and reachable by `cancel()` for the duration of the async
    /// delivery the caller is about to perform, and keeps an item that is
    /// already checked out from being drained a second time by a concurrent
    /// flush. The caller is responsible for injecting blocks into target
    /// documents, then reporting the outcome back per item: success via
    /// [`complete`](Self::complete), failure via [`requeue`](Self::requeue).
    #[tracing::instrument(skip(self), name = "drift.drain")]
    pub fn drain(&mut self, for_context: Option<ContextId>) -> Vec<StagedDrift> {
        let mut drained = Vec::new();
        for item in self.staging.iter_mut() {
            if item.in_flight {
                continue;
            }
            let matches = match for_context {
                None => true,
                Some(ctx) => item.source_ctx == ctx || item.target_ctx == ctx,
            };
            if matches {
                item.in_flight = true;
                drained.push(item.clone());
            }
        }
        drained
    }

    /// Report a [`drain`](Self::drain)ed item as successfully delivered.
    ///
    /// Clears its in-flight bookkeeping in `staging` — the counterpart to
    /// `requeue` on the success path. Without this, a delivered item would
    /// linger in `staging` forever, permanently marked in-flight.
    pub fn complete(&mut self, staged_id: u64) {
        self.staging.retain(|s| s.id != staged_id);
    }

    /// Re-queue staged drifts that failed to deliver.
    ///
    /// `items` are the caller's [`drain`](Self::drain)ed copies. For each,
    /// the router-side record in `staging` is authoritative for whether
    /// `cancel` marked it cancelled while it was in flight — the caller's
    /// copy predates that mutation. A cancelled item is dropped here rather
    /// than restored or dead-lettered: the cancel already happened, and
    /// pretending the retry machinery still owns it would resurrect
    /// something a player explicitly cancelled.
    ///
    /// Surviving items have `retry_count` incremented. Items that have
    /// already been requeued [`MAX_DRIFT_RETRIES`] times move to the dead
    /// letter queue rather than accumulating indefinitely under persistent
    /// failure. The flush engine writes dead letter items to "lost+found" so
    /// content is never silently discarded.
    pub fn requeue(&mut self, items: Vec<StagedDrift>) {
        for mut item in items {
            let Some(pos) = self.staging.iter().position(|s| s.id == item.id) else {
                // Already gone from staging — e.g. `unregister` swept it to
                // the dead letter queue directly while it was in flight.
                // Nothing to restore; the item isn't lost, just already
                // accounted for elsewhere.
                continue;
            };
            let cancelled = self.staging[pos].cancelled;
            self.staging.remove(pos);
            if cancelled {
                tracing::info!(
                    drift_id = item.id,
                    "dropping cancelled in-flight drift instead of requeuing"
                );
                continue;
            }

            item.in_flight = false;
            item.cancelled = false;
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

    /// Read-only snapshot of the dead-letter queue (M2-B4).
    ///
    /// Non-consuming — pairs with `replay_dead_letter` for clients that
    /// want to inspect failed drifts and selectively requeue.
    pub fn dead_letters(&self) -> &[StagedDrift] {
        &self.dead_letter
    }

    /// Move a dead-letter item back into the staging queue by id (M2-B4).
    ///
    /// Resets `retry_count` to 0 so the requeued item gets a full set of
    /// retry attempts. Returns the requeued drift on success, or `None` if
    /// no dead-letter entry has the given id.
    pub fn replay_dead_letter(&mut self, id: u64) -> Option<StagedDrift> {
        let pos = self.dead_letter.iter().position(|d| d.id == id)?;
        let mut item = self.dead_letter.remove(pos);
        item.retry_count = 0;
        self.staging.push(item.clone());
        Some(item)
    }

    /// Put drained dead letters back on the dead-letter queue.
    ///
    /// [`drain_dead_letter`](Self::drain_dead_letter) takes the whole queue,
    /// so an item whose write into lost+found fails afterwards is gone —
    /// dropped by the one path whose entire job is *not* losing failed drifts.
    /// The flush engine hands those items back here instead, and they get
    /// another attempt on the next flush.
    pub fn restore_dead_letters(&mut self, items: Vec<StagedDrift>) {
        self.dead_letter.extend(items);
    }

    /// Claim `id` — a context whose `KernelDb` row the caller has **already
    /// persisted** — as the lost+found sink, registering it under `label` as
    /// a handle.
    ///
    /// The router deliberately cannot mint a lost+found of its own: it holds no
    /// DB handle, so minting here would register a drift handle with no
    /// `contexts` row behind it, breaking the "a registered handle implies a
    /// KernelDb row" invariant at the one call site nobody watches (and losing
    /// the dead letters again on the next restart). Row first, then claim.
    ///
    /// `label` is the caller's concern, not this router's: identity now comes
    /// from the well-known registry, not from a reserved string, so the
    /// caller is free to pick `"lost+found"` or a disambiguated
    /// `"lost+found-<id>"` when the plain label is already taken (e.g. by a
    /// rotated-out incumbent that is still registered under it).
    ///
    /// Returns the claimed id, or the id already claimed if someone else won
    /// the race — the loser's persisted row is then unused, so it is logged
    /// loudly enough to be traceable.
    pub fn claim_lost_found(&mut self, id: ContextId, label: &str) -> Result<ContextId, DriftError> {
        if let Some(existing) = self.lost_found_id {
            if existing != id {
                tracing::warn!(
                    existing = %existing.short(),
                    candidate = %id.short(),
                    "claim_lost_found lost a race: the candidate's persisted row is now unused"
                );
            }
            return Ok(existing);
        }
        // Register before claiming: a failed register (label taken by some
        // other context) must not leave a claim pointing at a handle that
        // does not exist.
        self.register(id, Some(label), None, PrincipalId::system())?;
        self.lost_found_id = Some(id);
        tracing::info!(context = %id.short(), label, "created lost+found context");
        Ok(id)
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
pub const DISTILLATION_SYSTEM_PROMPT: &str =
    include_str!("../../../assets/defaults/prompts/distillation.md");

/// Build a distillation prompt from a document's blocks.
///
/// Formats the conversation history as a transcript suitable for LLM summarization.
// TODO: Use query_blocks with a kind filter once drift goes through RPC.
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
            BlockKind::Notification => " (notification)",
            BlockKind::Resource => " (resource)",
            BlockKind::Trace => " (trace)",
            BlockKind::Task => " (task)",
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
    fn test_resolve_by_short_id() {
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router.register(id, None, None, PrincipalId::system()).unwrap();
        let short = id.short();
        // Should match by the entropy-tail short id
        let result = router.resolve_context(&short);
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
            .stage(src, tgt, "hello from source".into(), None, DriftKind::Push, PrincipalId::new())
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

        let result = router.stage(
            src,
            ContextId::new(),
            "nope".into(),
            None,
            DriftKind::Push,
            PrincipalId::new(),
        );
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
            .stage(src, tgt, "one".into(), None, DriftKind::Push, PrincipalId::new())
            .unwrap();
        let _id2 = router
            .stage(src, tgt, "two".into(), None, DriftKind::Push, PrincipalId::new())
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
            .stage(src, tgt, "a".into(), None, DriftKind::Push, PrincipalId::new())
            .unwrap();
        router
            .stage(src, tgt, "b".into(), None, DriftKind::Push, PrincipalId::new())
            .unwrap();

        let drained = router.drain(None);
        assert_eq!(drained.len(), 2);
        // OLD behavior encoded here was `queue().is_empty()` — drain used to
        // physically remove items into the caller's Vec. It now marks them
        // in-flight and leaves them visible in `staging` (see the in-flight
        // cancel tests below for why: a concurrent `cancel`/`queue` during
        // the delivery window must still see them).
        assert_eq!(router.queue().len(), 2, "in-flight items stay visible");
        assert!(router.queue().iter().all(|s| s.in_flight));
    }

    #[test]
    fn test_dead_letters_inspect_and_replay() {
        // M2-B4: dead_letters() reads non-consuming; replay_dead_letter
        // re-queues a single item by id (resetting retry count).
        let mut router = DriftRouter::new();
        let src = ContextId::new();
        let tgt = ContextId::new();
        router
            .register(src, Some("src"), None, PrincipalId::system())
            .unwrap();
        router
            .register(tgt, Some("tgt"), None, PrincipalId::system())
            .unwrap();
        router
            .stage(src, tgt, "fail".into(), None, DriftKind::Push, PrincipalId::new())
            .unwrap();
        // Cycle drain→requeue MAX_DRIFT_RETRIES+1 times so the per-item
        // retry_count actually crosses the threshold.
        for _ in 0..=(MAX_DRIFT_RETRIES as usize) {
            let drained = router.drain(None);
            router.requeue(drained);
        }

        let dl = router.dead_letters();
        assert_eq!(dl.len(), 1, "one item in DLQ, got {}", dl.len());
        let id = dl[0].id;
        // Inspect is non-consuming.
        assert_eq!(router.dead_letters().len(), 1);

        // Replay extracts it.
        let replayed = router.replay_dead_letter(id).expect("replay returns drift");
        assert_eq!(replayed.id, id);
        assert_eq!(replayed.retry_count, 0, "retry_count reset on replay");
        assert!(router.dead_letters().is_empty());
        assert_eq!(router.queue().len(), 1, "replayed back into staging");

        // Replay of a missing id is a no-op.
        assert!(router.replay_dead_letter(99_999).is_none());
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
            .stage(a, b, "from alpha".into(), None, DriftKind::Push, PrincipalId::new())
            .unwrap();
        router
            .stage(c, b, "from gamma".into(), None, DriftKind::Push, PrincipalId::new())
            .unwrap();

        // Scoped drain for alpha — should only get a→b
        let drained = router.drain(Some(a));
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].source_ctx, a);
        // Both remain visible in queue() — a→b as in-flight (drain no
        // longer removes it, only flags it), c→b untouched and still
        // waiting. OLD behavior encoded here was `queue().len() == 1`
        // (only c→b) because drain used to remove a→b outright.
        assert_eq!(router.queue().len(), 2);
        let waiting: Vec<_> = router.queue().iter().filter(|s| !s.in_flight).collect();
        assert_eq!(waiting.len(), 1, "only c→b should still be waiting");
        assert_eq!(waiting[0].source_ctx, c);
        let in_flight: Vec<_> = router.queue().iter().filter(|s| s.in_flight).collect();
        assert_eq!(in_flight.len(), 1, "a→b should be in-flight, not gone");
        assert_eq!(in_flight[0].source_ctx, a);
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
        use kaijutsu_types::BlockId;
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
        use kaijutsu_types::BlockId;
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
        use kaijutsu_types::BlockId;
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
        use kaijutsu_types::BlockId;
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

    #[test]
    fn test_shared_drift_on_fork() {
        // The SharedDriftRouter should be shareable across kernel fork/thread
        let router = shared_drift_router();

        // Register from "parent" side
        let parent_id = ContextId::new();
        router
            .write()
            .register(parent_id, Some("main"), None, PrincipalId::system())
            .unwrap();

        // Clone the Arc (simulating what fork/thread does)
        let child_router = Arc::clone(&router);

        // Child should see the parent's contexts
        let child_handle = child_router.read().get(parent_id).map(|h| h.label.clone());
        assert_eq!(child_handle, Some(Some("main".to_string())));

        // Child registers a new context
        let child_id = ContextId::new();
        child_router
            .write()
            .register(
                child_id,
                Some("debug-fork"),
                Some(parent_id),
                PrincipalId::system(),
            )
            .unwrap();

        // Parent should see the child's context
        assert!(router.read().get(child_id).is_some());
    }

    #[test]
    fn test_drift_flush_requeue_on_missing_target() {
        let mut router = DriftRouter::new();
        let src = ContextId::new();
        let tgt = ContextId::new();
        router.register(src, Some("source"), None, PrincipalId::system()).unwrap();
        router.register(tgt, Some("target"), None, PrincipalId::system()).unwrap();

        let staged_id = router
            .stage(src, tgt, "test content".into(), None, DriftKind::Push, PrincipalId::new())
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
            .stage(src, tgt, "first".into(), None, DriftKind::Push, PrincipalId::new())
            .unwrap();
        let id2 = router
            .stage(src, tgt, "second".into(), None, DriftKind::Push, PrincipalId::new())
            .unwrap();

        let drained = router.drain(None);
        assert_eq!(drained.len(), 2);
        // OLD behavior encoded here was `queue().is_empty()` — see
        // test_drain's comment; drain now flags in place instead of
        // removing, so both items are still visible (in-flight) here.
        assert_eq!(router.queue().len(), 2);
        assert!(router.queue().iter().all(|s| s.in_flight));

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
            .stage(src, tgt, "persistent failure".into(), None, DriftKind::Push, PrincipalId::new())
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
    fn test_claim_lost_found_idempotent() {
        let mut router = DriftRouter::new();
        let id1 = router.claim_lost_found(ContextId::new(), "lost+found").unwrap();
        // A second claim with a *different* candidate keeps the first — the
        // router never ends up with two lost+found handles.
        let id2 = router.claim_lost_found(ContextId::new(), "lost+found").unwrap();
        assert_eq!(id1, id2);
        assert_eq!(router.lost_found_id(), Some(id1));
        // Should be registered under the label it was claimed with
        assert!(router.resolve_context("lost+found").is_ok());
    }

    #[test]
    fn test_claim_lost_found_registers_a_handle() {
        // The invariant this API exists to protect runs the other way too: a
        // claimed lost+found must be a real, resolvable handle, not just an id
        // sitting in a field.
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        assert!(router.get(id).is_none());
        assert_eq!(router.claim_lost_found(id, "lost+found").unwrap(), id);
        assert!(router.get(id).is_some(), "claim must register the handle");
    }

    #[test]
    fn test_claim_lost_found_label_conflict_leaves_no_claim() {
        // Some other context already holds the label the caller is trying to
        // claim under: the claim must fail and leave `lost_found_id` unset,
        // so the caller can't proceed believing it has a sink.
        let mut router = DriftRouter::new();
        router
            .register(ContextId::new(), Some("lost+found"), None, PrincipalId::system())
            .unwrap();

        let candidate = ContextId::new();
        assert!(router.claim_lost_found(candidate, "lost+found").is_err());
        assert_eq!(router.lost_found_id(), None);
        assert!(router.get(candidate).is_none());
    }

    #[test]
    fn test_rotate_lost_found_keeps_the_incumbent_as_an_ordinary_context() {
        // The whole point of the well-known registry (docs/drifting-dead-
        // letters.md, slice 1): rotation is an UPDATE, not a rename or a
        // delete. The outgoing lost+found keeps its identity and stays
        // resolvable after it is rotated out.
        let mut router = DriftRouter::new();
        let a = router.claim_lost_found(ContextId::new(), "lost+found").unwrap();

        let outgoing = router.rotate_lost_found();
        assert_eq!(outgoing, Some(a));
        assert_eq!(router.lost_found_id(), None);

        // A is still registered and resolvable as an ordinary context.
        assert!(router.get(a).is_some(), "outgoing lost+found must stay registered");
        assert_eq!(router.resolve_context("lost+found").unwrap(), a);

        // B claims the role next, under a different label (the caller's job
        // is to disambiguate — A still holds "lost+found").
        let b_id = ContextId::new();
        let b = router.claim_lost_found(b_id, "lost+found-2").unwrap();
        assert_eq!(b, b_id);
        assert_eq!(router.lost_found_id(), Some(b));

        // A is still there, untouched, after B takes over.
        assert!(router.get(a).is_some());
        assert_eq!(router.resolve_context("lost+found").unwrap(), a);
        assert_eq!(router.resolve_context("lost+found-2").unwrap(), b);
    }

    #[test]
    fn test_restore_dead_letters_requeues_unwritten() {
        // A dead letter whose write into lost+found fails goes back on the
        // queue — the drain must not be able to lose it.
        let mut router = DriftRouter::new();
        let src = ContextId::new();
        let dst = ContextId::new();
        router.register(src, None, None, PrincipalId::system()).unwrap();
        router.register(dst, None, None, PrincipalId::system()).unwrap();
        router
            .stage(src, dst, "undeliverable".to_string(), None, DriftKind::Push, PrincipalId::new())
            .unwrap();
        let staged = router.drain(None);

        router.restore_dead_letters(staged);
        assert_eq!(router.dead_letters().len(), 1);
        assert_eq!(router.dead_letters()[0].content, "undeliverable");
        // And it drains like any other dead letter on the next flush.
        assert_eq!(router.drain_dead_letter().len(), 1);
        assert!(router.dead_letters().is_empty());
    }

    #[test]
    fn test_adopt_lost_found_claims_recovered_context() {
        // Simulates cold-start: the persisted lost+found is registered through
        // the normal path, then adopted so a later claim reuses it.
        let mut router = DriftRouter::new();
        let id = ContextId::new();
        router
            .register(id, Some("lost+found"), None, PrincipalId::system())
            .unwrap();

        assert_eq!(router.lost_found_id(), None, "not claimed until adopted");
        router.adopt_lost_found(id);
        assert_eq!(router.lost_found_id(), Some(id));

        // A later claim must reuse the adopted id, not mint a duplicate.
        let got = router.claim_lost_found(ContextId::new(), "lost+found").unwrap();
        assert_eq!(got, id);
    }

    #[test]
    fn test_adopt_lost_found_keeps_first_claim() {
        let mut router = DriftRouter::new();
        let first = router.claim_lost_found(ContextId::new(), "lost+found").unwrap();
        // A stray second adopt with a different id is ignored.
        router.adopt_lost_found(ContextId::new());
        assert_eq!(router.lost_found_id(), Some(first));
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

    // ========================================================================
    // In-flight cancel — the flush-blind-spot bug (docs/issues.md "Drift —
    // June 2026 audit": drift_flush is non-atomic over the router lock).
    //
    // `drain` used to *remove* items from `staging` into the caller's local
    // Vec, so a concurrent `cancel` during the async delivery window (block
    // insert + rc lifecycle in `kj/drift.rs::drift_flush`) found nothing and
    // reported false/"not staged" — the drift then survived to `requeue` and
    // came back on the next flush despite having been cancelled. These tests
    // drive the router directly, without any real concurrency, to prove the
    // sequence deterministically: drain (simulating a flush checking an item
    // out) → cancel (simulating a racing player) → requeue (simulating the
    // delivery failing) must drop the cancelled item, not restore it.
    // ========================================================================

    #[test]
    fn test_cancel_in_flight_drops_on_requeue() {
        let mut router = DriftRouter::new();
        let src = ContextId::new();
        let tgt = ContextId::new();
        router.register(src, Some("src"), None, PrincipalId::system()).unwrap();
        router.register(tgt, Some("tgt"), None, PrincipalId::system()).unwrap();

        let id1 = router
            .stage(src, tgt, "one".into(), None, DriftKind::Push, PrincipalId::new())
            .unwrap();
        let id2 = router
            .stage(src, tgt, "two".into(), None, DriftKind::Push, PrincipalId::new())
            .unwrap();

        // Simulate a flush checking both items out for delivery.
        let drained = router.drain(None);
        assert_eq!(drained.len(), 2);

        // A concurrent `kj drift cancel` for id1 must succeed even though the
        // item is mid-flight — this is exactly what the old `staging`-only
        // search missed.
        assert!(
            router.cancel(id1),
            "cancel must succeed for an item that is currently in flight"
        );
        // Cancelling something already checked out must not remove it from
        // sight immediately (delivery may already be underway) — it should
        // still show up as in-flight until the flush resolves it.
        assert_eq!(router.queue().len(), 2, "cancel must not vanish the item early");

        // Simulate the flush's delivery failing for both items (e.g. the
        // target document was gone) and requeuing as `drift_flush` does.
        router.requeue(drained);

        // The cancelled item must NOT come back; the other one must.
        let ids: Vec<_> = router.queue().iter().map(|s| s.id).collect();
        assert!(!ids.contains(&id1), "cancelled item resurrected: {ids:?}");
        assert!(ids.contains(&id2), "non-cancelled item lost: {ids:?}");
        assert_eq!(router.queue().len(), 1);
    }

    #[test]
    fn test_cancel_returns_true_and_queue_shows_in_flight_item() {
        // A player running `kj drift queue` mid-flush must still see the
        // item that's checked out — it hasn't vanished, it's in transit.
        let mut router = DriftRouter::new();
        let src = ContextId::new();
        let tgt = ContextId::new();
        router.register(src, Some("src"), None, PrincipalId::system()).unwrap();
        router.register(tgt, Some("tgt"), None, PrincipalId::system()).unwrap();
        let id = router
            .stage(src, tgt, "mid-flight".into(), None, DriftKind::Push, PrincipalId::new())
            .unwrap();

        let drained = router.drain(None);
        assert_eq!(drained.len(), 1);

        let queued = router.queue();
        assert_eq!(queued.len(), 1, "in-flight item vanished from queue()");
        assert_eq!(queued[0].id, id);
        assert!(
            queued[0].in_flight,
            "queue() should mark the item as in-flight"
        );
        assert!(!queued[0].cancelled);

        // Drain a second time (e.g. a second flush call) must not re-grab an
        // item that's already checked out — that would double-deliver it.
        assert!(
            router.drain(None).is_empty(),
            "an in-flight item must not be drained twice"
        );

        router.requeue(drained);
    }

    #[test]
    fn test_complete_removes_delivered_in_flight_item() {
        // A successful delivery must clear the in-flight bookkeeping — it
        // must not linger in queue() forever after the flush that delivered
        // it.
        let mut router = DriftRouter::new();
        let src = ContextId::new();
        let tgt = ContextId::new();
        router.register(src, Some("src"), None, PrincipalId::system()).unwrap();
        router.register(tgt, Some("tgt"), None, PrincipalId::system()).unwrap();
        let id = router
            .stage(src, tgt, "delivered".into(), None, DriftKind::Push, PrincipalId::new())
            .unwrap();

        let drained = router.drain(None);
        assert_eq!(drained.len(), 1);
        assert_eq!(router.queue().len(), 1, "still visible while in flight");

        router.complete(id);

        assert!(
            router.queue().is_empty(),
            "a delivered item must not linger in the queue"
        );
    }

    #[test]
    fn test_cancel_unknown_id_returns_false() {
        let mut router = DriftRouter::new();
        assert!(!router.cancel(999_999));
    }
}
