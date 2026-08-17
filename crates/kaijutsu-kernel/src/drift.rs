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

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

use kaijutsu_types::{
    BlockId, BlockKind, BlockSnapshot, ContentType, ContextId, DriftKind, PrefixError, Role,
    Status, resolve_context_prefix,
};
use kaijutsu_types::{ContextState, PrincipalId};

use crate::block_store::SharedBlockStore;
use crate::kernel_db::{ContextRow, KernelDb, WellKnownRole};

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
// DriftOrigin — slice 2, docs/drifting-dead-letters.md
// ============================================================================
//
// This widens `StagedDrift`'s old `source_ctx: ContextId` field so a second
// kind of sender fits: a peer that has no kaijutsu context of its own (e.g.
// an inbound Claude Code message, before slice 4 gives it somewhere to
// attach). It is deliberately NOT a trait, a registry, or a plugin point —
// the doc is explicit that one caller does not reveal an abstraction's
// shape, and the second real source (slice 4) is what should drive any
// further generalization. `target_ctx` stays a plain `ContextId`: only the
// origin widens, a drift always lands *in* a real context.

/// A sender that is not a kaijutsu context — the shape slice 2's exit
/// criterion names: "a peer with a kind, a display name, and a reply
/// address." Fields are owned strings rather than a reference to any
/// specific transport type (e.g. `claude-code-peer`'s socket descriptor), so
/// `drift.rs` stays free of a dependency on any one peer transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerOrigin {
    /// What kind of peer this is (e.g. `"claude-code"`). A free-form string,
    /// not an enum — the router does not need to know the closed set of
    /// peer kinds that exist.
    pub kind: String,
    /// Human-facing name for provenance and logging.
    pub display_name: String,
    /// Where a reply should go, in whatever address form the peer's own
    /// transport understands (e.g. a socket path). Opaque to the router.
    pub reply_address: String,
}

/// Where a staged drift came from: a registered kaijutsu context (the only
/// kind that existed before slice 2), or a [`PeerOrigin`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriftOrigin {
    /// A registered context — `DriftRouter::stage`/`unregister` can look it
    /// up in `contexts`.
    Context(ContextId),
    /// An external sender with no context to look up.
    Peer(PeerOrigin),
}

impl DriftOrigin {
    /// The `ContextId` behind this origin, if it is one. `None` for a peer
    /// — callers that need a real `ContextId` (edge writes, block
    /// authorship provenance) branch on this rather than inventing one.
    pub fn as_context(&self) -> Option<ContextId> {
        match self {
            DriftOrigin::Context(id) => Some(*id),
            DriftOrigin::Peer(_) => None,
        }
    }

    /// Short display form for logs. Mirrors `ContextId::short()` for the
    /// `Context` case; a peer has no hex id to shorten, so this uses its
    /// kind and display name instead.
    pub fn short(&self) -> String {
        match self {
            DriftOrigin::Context(id) => id.short(),
            DriftOrigin::Peer(p) => format!("peer:{}:{}", p.kind, p.display_name),
        }
    }
}

/// Every existing caller stages a bare `ContextId` — this keeps every one of
/// them compiling unchanged (`DriftRouter::stage` takes `impl Into<DriftOrigin>`)
/// rather than forcing a `DriftOrigin::Context(..)` wrapper at each call site.
impl From<ContextId> for DriftOrigin {
    fn from(id: ContextId) -> Self {
        DriftOrigin::Context(id)
    }
}

// ============================================================================
// StagedDrift — queued drift operation
// ============================================================================

/// A drift operation staged in the queue, pending flush.
///
/// `Serialize`/`Deserialize` back the durable mirror slice 3 writes into the
/// drift-queue context (see [`DriftRouter::attach_persistence`]) — this is
/// the wire format for that block's content, not a network-facing type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagedDrift {
    /// Unique ID for this staged operation.
    pub id: u64,
    /// Where this drift came from — a context, or (slice 2) a peer with no
    /// context of its own. See [`DriftOrigin`].
    pub origin: DriftOrigin,
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
// Durable backing for the queues (slice 3, docs/drifting-dead-letters.md)
// ============================================================================
//
// `staging` and `dead_letter` above are the in-memory queue, same as before
// slice 3 — that does not change, and every caller that never attaches
// persistence sees identical behavior to pre-slice-3 `DriftRouter`. What
// slice 3 adds is a durable *mirror*: every item that enters one of those
// Vecs is also written as a block into the drift-queue well-known context,
// and every item that leaves is marked consumed there. `rehydrate_from_block_log`
// is the read side — the `ConversationMailbox` shape this doc calls for: the
// block log is the queue, the Vec is a materialized cursor over it, rebuilt on
// cold start rather than trusted to survive a restart on its own.

/// Which durable sub-queue a persisted drift-queue block represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum QueueSlot {
    Staging,
    DeadLetter,
}

/// The wire format written into a drift-queue block's content.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedDrift {
    slot: QueueSlot,
    drift: StagedDrift,
}

/// A block store + the drift-queue context id to write into. Attached once,
/// after construction — `DriftRouter::new()` has no block store in scope (the
/// kernel does not own a `BlockStore`; it is assembled by the frontend and
/// handed in later). Absent, the router behaves exactly as it did before
/// slice 3: in-memory only.
struct DriftPersistence {
    blocks: SharedBlockStore,
    queue_ctx: ContextId,
}

impl std::fmt::Debug for DriftPersistence {
    // `BlockStore` has no `Debug` impl (it holds live document state), so this
    // is hand-written rather than derived — needed for `DriftRouter`'s own
    // `#[derive(Debug)]` to keep compiling with an `Option<DriftPersistence>`
    // field.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriftPersistence")
            .field("queue_ctx", &self.queue_ctx.short())
            .finish_non_exhaustive()
    }
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
    /// ContextId for the durable drift-queue context (slice 3), claimed the
    /// same way `lost_found_id` is — see [`Self::claim_drift_queue`] /
    /// [`Self::adopt_drift_queue`].
    drift_queue_id: Option<ContextId>,
    /// Durable backing for `staging`/`dead_letter`, if attached (slice 3). See
    /// [`Self::attach_persistence`].
    persistence: Option<DriftPersistence>,
    /// `StagedDrift::id` → the `BlockId` currently holding its durable mirror
    /// in the drift-queue context. Only populated while `persistence` is
    /// attached; empty and unused otherwise.
    persisted_blocks: HashMap<u64, BlockId>,
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
            drift_queue_id: None,
            persistence: None,
            persisted_blocks: HashMap::new(),
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
            .partition(|s| s.origin.as_context() == Some(id) || s.target_ctx == id);
        self.staging = keep;
        if !dead.is_empty() {
            tracing::info!(
                context = %id.short(),
                count = dead.len(),
                "moving staged drifts to dead letter for unregistered context"
            );
            // A swept item may have been mid-flight for STAGING delivery
            // (`drain`'s in_flight) at the moment its context vanished.
            // Dead-letter draining (`drain_dead_letter`) has its own,
            // separate in-flight domain (Task B, docs/drifting-dead-
            // letters.md) — carrying the staging flag over would make the
            // item look already-checked-out there and `drain_dead_letter`
            // would silently skip it forever, since nothing will ever call
            // `ack_dead_letter`/`restore_dead_letters` to clear a flag that
            // belongs to the wrong domain. Reset both flags on the move.
            let dead: Vec<StagedDrift> = dead
                .into_iter()
                .map(|mut item| {
                    item.in_flight = false;
                    item.cancelled = false;
                    item
                })
                .collect();
            for item in &dead {
                // Consume the staging record, write a fresh dead-letter one —
                // same append-only-history reasoning as the `requeue` path.
                self.mark_consumed(item.id);
                self.persist_item(QueueSlot::DeadLetter, item);
            }
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

    /// The drift-queue context id, if one has been claimed (slice 3).
    pub fn drift_queue_id(&self) -> Option<ContextId> {
        self.drift_queue_id
    }

    /// Claim an already-registered context as the drift-queue sink — the
    /// slice-3 counterpart to [`Self::adopt_lost_found`], same cold-start
    /// shape: `drift_queue_id` resets to `None` across a restart, and the
    /// caller re-claims it from `WellKnownRole::DriftQueue` rather than
    /// re-deriving it from a label.
    pub fn adopt_drift_queue(&mut self, id: ContextId) {
        match self.drift_queue_id {
            Some(existing) if existing == id => {}
            Some(existing) => tracing::warn!(
                existing = %existing.short(),
                candidate = %id.short(),
                "adopt_drift_queue ignored: a different drift-queue is already claimed"
            ),
            None => self.drift_queue_id = Some(id),
        }
    }

    /// Claim `id` — a context whose `KernelDb` row the caller has **already
    /// persisted** — as the drift-queue sink, registering it under `label`.
    /// See [`Self::claim_lost_found`]'s doc for why row-then-claim matters;
    /// the same reasoning applies here. Unlike lost+found, this role has no
    /// rotation story yet, so `label` is not the caller's concern to
    /// disambiguate — a fixed `"drift-queue"` is fine.
    pub fn claim_drift_queue(
        &mut self,
        id: ContextId,
        label: &str,
    ) -> Result<ContextId, DriftError> {
        if let Some(existing) = self.drift_queue_id {
            if existing != id {
                tracing::warn!(
                    existing = %existing.short(),
                    candidate = %id.short(),
                    "claim_drift_queue lost a race: the candidate's persisted row is now unused"
                );
            }
            return Ok(existing);
        }
        self.register(id, Some(label), None, PrincipalId::system())?;
        self.drift_queue_id = Some(id);
        tracing::info!(context = %id.short(), label, "created drift-queue context");
        Ok(id)
    }

    /// Attach durable backing (slice 3, docs/drifting-dead-letters.md): every
    /// future `stage`/dead-letter transition also writes or marks a block in
    /// `queue_ctx`, and prior content can be recovered via
    /// [`Self::rehydrate_from_block_log`]. `DriftRouter::new()` cannot do
    /// this at construction — the kernel does not own a `BlockStore` (it is
    /// assembled by the frontend and handed in later, see `kernel.rs`'s
    /// `register_builtin_mcp_servers`) — so this is attached after the fact
    /// by whoever resolves both. Every existing caller that never attaches
    /// this is completely unaffected: `persistence` stays `None` and every
    /// method below degrades to its pre-slice-3, in-memory-only behavior.
    pub fn attach_persistence(&mut self, blocks: SharedBlockStore, queue_ctx: ContextId) {
        self.persistence = Some(DriftPersistence { blocks, queue_ctx });
    }

    /// Whether a durable backing is attached. Diagnostic/test convenience —
    /// no caller needs to branch on this to use the router correctly.
    pub fn has_persistence(&self) -> bool {
        self.persistence.is_some()
    }

    /// Write `item` into the drift-queue context as a `slot` record, and
    /// track the resulting block id so a later transition can mark it
    /// consumed. A no-op if no persistence is attached. If persistence IS
    /// attached but the write fails, this logs loudly rather than silently
    /// continuing (CLAUDE.md: "silent fallbacks are often a mistake") — the
    /// in-memory `staging`/`dead_letter` Vecs stay authoritative for this
    /// process regardless, so a write failure here degrades durability
    /// (this item will not survive a restart), not this process's
    /// correctness.
    fn persist_item(&mut self, slot: QueueSlot, item: &StagedDrift) {
        let Some(p) = &self.persistence else {
            return;
        };
        let payload = PersistedDrift {
            slot,
            drift: item.clone(),
        };
        let content = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    drift_id = item.id,
                    "failed to serialize drift-queue record, item will NOT survive a restart: {e}"
                );
                return;
            }
        };
        let after = p.blocks.last_block_id(p.queue_ctx);
        match p.blocks.insert_block(
            p.queue_ctx,
            None,
            after.as_ref(),
            Role::System,
            BlockKind::Trace,
            content,
            Status::Pending,
            ContentType::Plain,
        ) {
            Ok(block_id) => {
                self.persisted_blocks.insert(item.id, block_id);
            }
            Err(e) => {
                tracing::error!(
                    drift_id = item.id,
                    "failed to persist drift-queue block, item will NOT survive a restart: {e}"
                );
            }
        }
    }

    /// Mark `staged_id`'s durable record consumed — delivered, cancelled, or
    /// superseded by a fresh record for the same item (a retry or a
    /// dead-letter/replay transition writes a new record rather than
    /// mutating the old one in place, so the old one is consumed here). A
    /// no-op if no persistence is attached, or if `staged_id` has no durable
    /// record (never staged with persistence attached, or already consumed).
    fn mark_consumed(&mut self, staged_id: u64) {
        let Some(p) = &self.persistence else {
            return;
        };
        if let Some(block_id) = self.persisted_blocks.remove(&staged_id)
            && let Err(e) = p.blocks.set_status(p.queue_ctx, &block_id, Status::Done)
        {
            tracing::error!(
                drift_id = staged_id,
                "failed to mark drift-queue block consumed: {e}"
            );
        }
    }

    /// Rebuild `staging`/`dead_letter` from the durable drift-queue context —
    /// the read side of [`Self::attach_persistence`], and the reason this
    /// design pays off: the block log is the queue, `staging`/`dead_letter`
    /// are a materialized cursor over it (the `ConversationMailbox` shape,
    /// `docs/drifting-dead-letters.md`'s "three things that are one thing").
    /// Call once, right after attaching, before this router serves any
    /// traffic — it REPLACES `staging`/`dead_letter`/`persisted_blocks`
    /// wholesale rather than merging, which is correct for a fresh router
    /// (the cold-start case this exists for) and wrong for any other.
    ///
    /// Only `Status::Pending` drift-queue blocks are live, unconsumed
    /// records — `Status::Done` ones are already-delivered/cancelled history,
    /// left in place for audit but not requeued. `next_staged_id` is
    /// advanced past every recovered id so a `stage` call right after
    /// rehydrate cannot mint a colliding id. Nothing is truly in-flight the
    /// moment a process starts, so recovered items always come back with
    /// `in_flight`/`cancelled` cleared even if the persisted record predates
    /// that (a crash mid-flush cannot leave a phantom in-flight item behind).
    pub fn rehydrate_from_block_log(&mut self) -> Result<(), DriftError> {
        let Some(p) = &self.persistence else {
            return Ok(());
        };
        let snapshots = p.blocks.block_snapshots(p.queue_ctx).map_err(|e| {
            DriftError::DocumentError(format!(
                "drift-queue rehydrate: failed to read block log: {e}"
            ))
        })?;

        self.staging.clear();
        self.dead_letter.clear();
        self.persisted_blocks.clear();

        for block in &snapshots {
            if block.status != Status::Pending {
                continue;
            }
            let payload: PersistedDrift = match serde_json::from_str(&block.content) {
                Ok(parsed) => parsed,
                Err(e) => {
                    tracing::error!(
                        block_id = ?block.id,
                        "drift-queue block is Pending but not a valid record, skipping (stays \
                         in the log for inspection, will not be requeued): {e}"
                    );
                    continue;
                }
            };
            self.next_staged_id = self.next_staged_id.max(payload.drift.id + 1);
            self.persisted_blocks.insert(payload.drift.id, block.id);
            let mut item = payload.drift;
            item.in_flight = false;
            item.cancelled = false;
            match payload.slot {
                QueueSlot::Staging => self.staging.push(item),
                QueueSlot::DeadLetter => self.dead_letter.push(item),
            }
        }

        tracing::info!(
            staged = self.staging.len(),
            dead_letters = self.dead_letter.len(),
            "rehydrated drift queue from block log"
        );
        Ok(())
    }

    /// Stage a drift operation for later flush.
    ///
    /// `origin` accepts anything convertible to [`DriftOrigin`] — every
    /// call site today passes a bare `ContextId`, which converts via the
    /// blanket `From` impl, so slice 2's widening does not force any of
    /// them to change. Context validation applies only when `origin` is a
    /// `DriftOrigin::Context`: a peer has no entry in `contexts` to check
    /// by construction, and that absence is the point, not an oversight.
    ///
    /// Returns the staged drift ID.
    pub fn stage(
        &mut self,
        origin: impl Into<DriftOrigin>,
        target_ctx: ContextId,
        content: String,
        source_model: Option<String>,
        drift_kind: DriftKind,
        staged_by: PrincipalId,
    ) -> Result<u64, DriftError> {
        let origin = origin.into();
        if let DriftOrigin::Context(source_ctx) = &origin
            && !self.contexts.contains_key(source_ctx)
        {
            return Err(DriftError::UnknownContext(source_ctx.short()));
        }
        if !self.contexts.contains_key(&target_ctx) {
            return Err(DriftError::UnknownContext(target_ctx.short()));
        }

        let id = self.next_staged_id;
        self.next_staged_id += 1;

        let item = StagedDrift {
            id,
            origin,
            target_ctx,
            content,
            source_model,
            drift_kind,
            staged_by,
            created_at: kaijutsu_types::now_millis(),
            retry_count: 0,
            in_flight: false,
            cancelled: false,
        };
        // Durable mirror before the in-memory push reports success — see
        // `persist_item`'s doc for what "success" means when persistence
        // isn't attached or its write fails.
        self.persist_item(QueueSlot::Staging, &item);
        self.staging.push(item);

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
            self.mark_consumed(staged_id);
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
                Some(ctx) => item.origin.as_context() == Some(ctx) || item.target_ctx == ctx,
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
        self.mark_consumed(staged_id);
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
                self.mark_consumed(item.id);
                continue;
            }

            item.in_flight = false;
            item.cancelled = false;
            item.retry_count += 1;
            if item.retry_count > MAX_DRIFT_RETRIES {
                tracing::warn!(
                    drift_id = item.id,
                    source = %item.origin.short(),
                    target = %item.target_ctx.short(),
                    retries = item.retry_count,
                    "staged drift exceeded {} retries, moving to dead letter queue",
                    MAX_DRIFT_RETRIES,
                );
                // Consume the staging record and write a fresh dead-letter
                // one rather than mutate in place — blocks are append-only
                // history here, not a mutable cell.
                self.mark_consumed(item.id);
                self.persist_item(QueueSlot::DeadLetter, &item);
                self.dead_letter.push(item);
            } else {
                // Re-persist with the incremented retry_count so a restart
                // recovers an accurate count, not just "it was staged."
                self.mark_consumed(item.id);
                self.persist_item(QueueSlot::Staging, &item);
                self.staging.push(item);
            }
        }
    }

    /// Check out every not-already-checked-out item in the dead letter
    /// queue for delivery, without marking its durable record consumed.
    ///
    /// This is a two-phase drain (docs/issues.md, "Drift drain acks before
    /// the lost+found write") mirroring [`Self::drain`]'s shape for
    /// staging: matched items are flagged
    /// [`in_flight`](StagedDrift::in_flight) and a clone is returned, but
    /// they stay physically in `dead_letter` — so a concurrent
    /// `drain_dead_letter` cannot double-check-out the same item, and
    /// [`dead_letters`](Self::dead_letters) still shows them (checked out,
    /// not gone). The caller reports the outcome per item afterward:
    /// success via [`ack_dead_letter`](Self::ack_dead_letter) (only then is
    /// the durable record marked consumed), failure via
    /// [`restore_dead_letters`](Self::restore_dead_letters).
    ///
    /// The old, single-phase version of this method marked every drained
    /// item's durable record consumed immediately, before the caller's
    /// write into lost+found — a crash in the synchronous window between
    /// the two could lose the item for good, even though `dead_letter`'s
    /// whole reason to exist is "content is never silently discarded."
    /// With this shape, a crash at any point either leaves the durable
    /// record `Pending` (recovered as a dead letter again on restart — at
    /// worst re-delivered, never lost) or `Done` because
    /// `ack_dead_letter` ran, which only happens after the write actually
    /// succeeded. Never both-lost.
    pub fn drain_dead_letter(&mut self) -> Vec<StagedDrift> {
        let mut drained = Vec::new();
        for item in self.dead_letter.iter_mut() {
            if item.in_flight {
                continue;
            }
            item.in_flight = true;
            drained.push(item.clone());
        }
        drained
    }

    /// Report a [`drain_dead_letter`](Self::drain_dead_letter)ed item as
    /// successfully written into lost+found: removes it from `dead_letter`
    /// and marks its durable record consumed. Returns `false` if `staged_id`
    /// is not currently in the dead-letter queue (already acked, replayed
    /// out from under a concurrent caller, or never dead-lettered).
    ///
    /// This is the second phase of the two-phase drain — the caller must
    /// not call this until the lost+found write it is acking has actually
    /// succeeded. Calling it before that (or not at all) is exactly what
    /// keeps the item safe: see [`drain_dead_letter`](Self::drain_dead_letter)'s
    /// doc.
    pub fn ack_dead_letter(&mut self, staged_id: u64) -> bool {
        let Some(pos) = self.dead_letter.iter().position(|d| d.id == staged_id) else {
            return false;
        };
        self.dead_letter.remove(pos);
        self.mark_consumed(staged_id);
        true
    }

    /// Read-only snapshot of the dead-letter queue (M2-B4).
    ///
    /// Non-consuming — pairs with `replay_dead_letter` for clients that
    /// want to inspect failed drifts and selectively requeue. Includes
    /// items currently checked out by a [`drain_dead_letter`](Self::drain_dead_letter)
    /// awaiting [`ack_dead_letter`](Self::ack_dead_letter)/[`restore_dead_letters`](Self::restore_dead_letters)
    /// — same reasoning as [`queue`](Self::queue) for staging: a client
    /// inspecting mid-flush should see the whole picture, not have the item
    /// vanish until the flush resolves it.
    pub fn dead_letters(&self) -> &[StagedDrift] {
        &self.dead_letter
    }

    /// How many items are staged for delivery. Observability only — the
    /// cold-start rehydrate reports what it recovered, and a caller that
    /// wants the items themselves should take them rather than counting
    /// them first.
    pub fn staged_len(&self) -> usize {
        self.staging.len()
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
        // Consume the dead-letter record, write a fresh staging one.
        self.mark_consumed(item.id);
        self.persist_item(QueueSlot::Staging, &item);
        self.staging.push(item.clone());
        Some(item)
    }

    /// Report [`drain_dead_letter`](Self::drain_dead_letter)ed items whose
    /// write into lost+found failed: clears their checked-out flag so they
    /// are drainable again on the next flush.
    ///
    /// With the two-phase drain (see `drain_dead_letter`'s doc), these items
    /// were never marked consumed in the first place — they are still
    /// sitting in `dead_letter`, durable record `Pending`, exactly where
    /// `drain_dead_letter` left them. So the normal case here is cheap: find
    /// each item by id and clear `in_flight`, no re-persist needed. The
    /// fallback branch below (item not found — e.g. some other path already
    /// removed it) is defensive rather than expected: if it ever fires, this
    /// re-adds and re-persists the item so a write failure never silently
    /// drops content, but it also means something upstream violated the
    /// "restore only what you drained" contract and is worth investigating.
    pub fn restore_dead_letters(&mut self, items: Vec<StagedDrift>) {
        for item in items {
            if let Some(existing) = self.dead_letter.iter_mut().find(|d| d.id == item.id) {
                existing.in_flight = false;
                existing.cancelled = false;
            } else {
                tracing::warn!(
                    drift_id = item.id,
                    "restore_dead_letters: item was not checked out via drain_dead_letter \
                     (already restored, or drained by a concurrent caller?); re-adding and \
                     re-persisting it defensively rather than dropping it"
                );
                self.persist_item(QueueSlot::DeadLetter, &item);
                self.dead_letter.push(item);
            }
        }
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

/// Resolve (creating if necessary) the durable drift-queue context that
/// backs [`DriftRouter::attach_persistence`] — the slice-3 counterpart to
/// `KjDispatcher::ensure_lost_found_context` (`kj/drift.rs`), which this
/// mirrors: router cache → well-known registry → mint. It lives here rather
/// than there because `WellKnownRole::DriftQueue` and the persistence it
/// backs are this router's concern, and because `kj/drift.rs` is a
/// different lane's file this slice does not touch.
///
/// Unlike lost+found there is no rotation story for this role yet, so there
/// is no reserved-label disambiguation to do on mint — `"drift-queue"` is
/// fine.
///
/// This resolves and attaches the context; it does NOT rehydrate. Call
/// [`DriftRouter::rehydrate_from_block_log`] afterward if the caller wants
/// prior durable content folded back into `staging`/`dead_letter` — kept
/// separate because rehydrate has cold-start-only timing requirements
/// (replaces state wholesale) that resolving the context does not.
pub fn ensure_drift_queue_context(
    router: &SharedDriftRouter,
    blocks: &SharedBlockStore,
    kernel_db: &Mutex<KernelDb>,
) -> Result<ContextId, DriftError> {
    // 1. Router cache.
    if let Some(id) = router.read().drift_queue_id() {
        return Ok(id);
    }

    // 2. Well-known registry — a persisted assignment from this kernel's
    //    history. A hit means the row already exists; adopt it rather than
    //    minting a duplicate.
    {
        let registry_hit = kernel_db.lock().well_known_context(WellKnownRole::DriftQueue);
        match registry_hit {
            Ok(Some(id)) => {
                router.write().adopt_drift_queue(id);
                return Ok(id);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    "well-known registry lookup for drift-queue failed, minting instead: {e}"
                );
            }
        }
    }

    // 3. Mint: document row, then context + well-known row, then the router
    //    claim — row before register, same invariant `claim_lost_found`
    //    documents.
    let id = ContextId::new();
    let system = PrincipalId::system();

    blocks
        .create_document(id, crate::DocumentKind::Conversation, None)
        .map_err(|e| {
            DriftError::DocumentError(format!("failed to create drift-queue document: {e}"))
        })?;

    let persist: Result<(), String> = {
        let db = kernel_db.lock();
        let now = kaijutsu_types::now_millis() as i64;
        db.get_or_create_default_workspace(system)
            .map_err(|e| format!("failed to resolve workspace for drift-queue: {e}"))
            .and_then(|ws| {
                let row = ContextRow {
                    context_id: id,
                    label: Some("drift-queue".to_string()),
                    provider: None,
                    model: None,
                    system_prompt: None,
                    consent_mode: kaijutsu_types::ConsentMode::Collaborative,
                    context_state: ContextState::Live,
                    context_type: "default".to_string(),
                    created_at: now,
                    created_by: system,
                    forked_from: None,
                    fork_kind: None,
                    archived_at: None,
                    workspace_id: Some(ws),
                    preset_id: None,
                    concluded_at: None,
                    last_activity_at: None,
                    promoted_at: None,
                    demoted_at: None,
                    paused_at: None,
                    cast_id: None,
                    origin_host: None,
                };
                db.insert_context_with_document(&row, ws)
                    .map_err(|e| format!("failed to persist drift-queue context row: {e}"))
            })
            .and_then(|_| {
                db.set_well_known_context(WellKnownRole::DriftQueue, id)
                    .map_err(|e| {
                        format!("failed to register drift-queue in well-known registry: {e}")
                    })
            })
    };

    if let Err(e) = persist {
        // Roll the document back so a retry starts clean instead of leaving
        // an orphan `documents` row behind per attempt.
        if let Err(cleanup) = blocks.delete_document(id) {
            tracing::error!(
                context = %id.short(),
                "drift-queue rollback failed, orphan document row left behind: {cleanup}"
            );
        }
        return Err(DriftError::DocumentError(e));
    }

    router.write().claim_drift_queue(id, "drift-queue")
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
        assert_eq!(drained[0].origin.as_context(), Some(a));
        // Both remain visible in queue() — a→b as in-flight (drain no
        // longer removes it, only flags it), c→b untouched and still
        // waiting. OLD behavior encoded here was `queue().len() == 1`
        // (only c→b) because drain used to remove a→b outright.
        assert_eq!(router.queue().len(), 2);
        let waiting: Vec<_> = router.queue().iter().filter(|s| !s.in_flight).collect();
        assert_eq!(waiting.len(), 1, "only c→b should still be waiting");
        assert_eq!(waiting[0].origin.as_context(), Some(c));
        let in_flight: Vec<_> = router.queue().iter().filter(|s| s.in_flight).collect();
        assert_eq!(in_flight.len(), 1, "a→b should be in-flight, not gone");
        assert_eq!(in_flight[0].origin.as_context(), Some(a));
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
        // queue — the drain must not be able to lose it. Exercises the real
        // shape of the two-phase drain (Task B): items restored here must
        // have come from `drain_dead_letter` first, same as the flush engine
        // does in `kj/drift.rs`.
        let mut router = DriftRouter::new();
        let src = ContextId::new();
        let dst = ContextId::new();
        router.register(src, None, None, PrincipalId::system()).unwrap();
        router.register(dst, None, None, PrincipalId::system()).unwrap();
        router
            .stage(src, dst, "undeliverable".to_string(), None, DriftKind::Push, PrincipalId::new())
            .unwrap();
        for _ in 0..=(MAX_DRIFT_RETRIES as usize) {
            let drained = router.drain(None);
            router.requeue(drained);
        }
        assert_eq!(router.dead_letters().len(), 1);

        // The flush engine checks it out for a lost+found write...
        let checked_out = router.drain_dead_letter();
        assert_eq!(checked_out.len(), 1);
        assert_eq!(
            router.dead_letters().len(),
            1,
            "checked-out item stays visible while in flight, like queue() for staging"
        );

        // ...and the write fails.
        router.restore_dead_letters(checked_out);
        assert_eq!(router.dead_letters().len(), 1);
        assert_eq!(router.dead_letters()[0].content, "undeliverable");
        assert!(
            !router.dead_letters()[0].in_flight,
            "restore must clear the in-flight flag so it can be drained again"
        );

        // And it drains + acks like any other dead letter on the next flush.
        let redrained = router.drain_dead_letter();
        assert_eq!(redrained.len(), 1);
        assert!(router.ack_dead_letter(redrained[0].id));
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
    // DriftOrigin — slice 2, docs/drifting-dead-letters.md. `origin` accepts
    // `impl Into<DriftOrigin>`, so every test above that stages with a bare
    // `ContextId` already proves "drift still routes exactly as before" (the
    // first half of slice 2's exit criterion) without changing a line. These
    // cover the second half: a peer origin.
    // ========================================================================

    #[test]
    fn test_stage_peer_origin_without_a_context() {
        // The exit criterion, verbatim: "an inbound peer message can be
        // staged without inventing a source context for it."
        let mut router = DriftRouter::new();
        let tgt = ContextId::new();
        router.register(tgt, Some("tgt"), None, PrincipalId::system()).unwrap();

        let origin = DriftOrigin::Peer(PeerOrigin {
            kind: "claude-code".to_string(),
            display_name: "cc-kaijutsu-a1b2".to_string(),
            reply_address: "/tmp/cc-kaijutsu-a1b2.sock".to_string(),
        });

        let id = router
            .stage(
                origin.clone(),
                tgt,
                "inbound from peer".into(),
                None,
                DriftKind::Push,
                PrincipalId::new(),
            )
            .expect("staging a peer origin must not require a source context");

        assert_eq!(router.queue().len(), 1);
        assert_eq!(router.queue()[0].id, id);
        assert_eq!(router.queue()[0].origin, origin);
        assert_eq!(
            router.queue()[0].origin.as_context(),
            None,
            "a peer origin has no context — nothing was invented to fill it"
        );
    }

    #[test]
    fn test_stage_peer_origin_still_validates_the_target() {
        // The origin widens; the target does not. A peer message aimed at a
        // context that does not exist must still fail the same way a
        // context-origin push would.
        let mut router = DriftRouter::new();
        let origin = DriftOrigin::Peer(PeerOrigin {
            kind: "claude-code".to_string(),
            display_name: "cc".to_string(),
            reply_address: "sock".to_string(),
        });
        let result = router.stage(
            origin,
            ContextId::new(),
            "nope".into(),
            None,
            DriftKind::Push,
            PrincipalId::new(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_unregister_unrelated_context_leaves_peer_origin_item_alone() {
        let mut router = DriftRouter::new();
        let tgt = ContextId::new();
        let unrelated = ContextId::new();
        router.register(tgt, Some("tgt"), None, PrincipalId::system()).unwrap();
        router.register(unrelated, Some("unrelated"), None, PrincipalId::system()).unwrap();
        let origin = DriftOrigin::Peer(PeerOrigin {
            kind: "claude-code".to_string(),
            display_name: "cc".to_string(),
            reply_address: "sock".to_string(),
        });
        router
            .stage(origin, tgt, "content".into(), None, DriftKind::Push, PrincipalId::new())
            .unwrap();

        router.unregister(unrelated);
        assert_eq!(router.queue().len(), 1, "a peer origin has no context to match unregister");
    }

    #[test]
    fn test_unregister_target_dead_letters_a_peer_origin_item() {
        // `target_ctx` is still a plain `ContextId` — tearing it down must
        // dead-letter a peer-origin item exactly like a context-origin one.
        let mut router = DriftRouter::new();
        let tgt = ContextId::new();
        router.register(tgt, Some("tgt"), None, PrincipalId::system()).unwrap();
        let origin = DriftOrigin::Peer(PeerOrigin {
            kind: "claude-code".to_string(),
            display_name: "cc".to_string(),
            reply_address: "sock".to_string(),
        });
        router
            .stage(origin.clone(), tgt, "content".into(), None, DriftKind::Push, PrincipalId::new())
            .unwrap();

        router.unregister(tgt);
        assert!(router.queue().is_empty());
        // Check via the non-consuming inspect view first — `drain_dead_letter`
        // itself sets `in_flight`, so the flag has to be checked BEFORE
        // draining to prove `unregister` didn't already leave it set.
        assert!(
            !router.dead_letters()[0].in_flight,
            "moving domains must not carry the staging in_flight flag over"
        );
        let dead = router.drain_dead_letter();
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].origin, origin);
    }

    #[test]
    fn test_unregister_dead_letter_is_not_already_in_flight() {
        // `unregister` can sweep a STAGING item that is mid-flight (checked
        // out by a concurrent flush, per `drain`'s in_flight flag) at the
        // exact moment its target context is torn down. The item lands in
        // `dead_letter` — but "checked out for staging delivery" and
        // "checked out for a dead-letter lost+found write" (Task B) are
        // different in-flight domains. Carrying the flag over would make
        // `drain_dead_letter` skip the item forever, since nothing will
        // ever call `ack_dead_letter`/`restore_dead_letters` to clear a
        // flag from the wrong domain.
        let mut router = DriftRouter::new();
        let src = ContextId::new();
        let tgt = ContextId::new();
        router.register(src, Some("src"), None, PrincipalId::system()).unwrap();
        router.register(tgt, Some("tgt"), None, PrincipalId::system()).unwrap();
        router
            .stage(src, tgt, "mid-flight victim".into(), None, DriftKind::Push, PrincipalId::new())
            .unwrap();

        // Check it out for staging delivery (simulating an in-progress flush).
        let drained = router.drain(None);
        assert_eq!(drained.len(), 1);
        assert!(router.queue()[0].in_flight);

        // Its target is torn down mid-flight.
        router.unregister(tgt);

        let dl = router.dead_letters();
        assert_eq!(dl.len(), 1);
        assert!(
            !dl[0].in_flight,
            "dead-letter in_flight must not inherit the staging domain's flag"
        );

        // And it is actually drainable, not silently stuck forever.
        assert_eq!(router.drain_dead_letter().len(), 1);
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

    // ========================================================================
    // Slice 3 — durable drift queue (docs/drifting-dead-letters.md)
    //
    // The whole point: `DriftRouter.dead_letter`/`staging` used to be
    // in-memory-only `Vec`s (docs/issues.md, "The dead-letter queue does not
    // survive a restart") — content the kernel accepted and promised to
    // deliver, gone the moment the process exits. These tests drive a real,
    // file-backed `KernelDb` + `BlockStore` — not an in-memory stand-in — and
    // simulate a restart the same way `block_store.rs`'s own
    // `test_durability_across_kill` does: write, `mem::forget` everything
    // (no clean close — a real SIGKILL doesn't get one either), then open a
    // FRESH `KernelDb`/`BlockStore`/`DriftRouter` against the SAME on-disk
    // file and assert the content comes back. A test that instead kept the
    // same `DriftRouter` alive across the "restart" would prove nothing — the
    // in-memory Vec would still be right there regardless of durability.
    // ========================================================================
    mod persistence {
        use super::*;
        use crate::block_store::{DbHandle, shared_block_store_with_db};

        /// Open a fresh file-backed `KernelDb` + `BlockStore` pair rooted at
        /// `path`. Returns the `DbHandle` too so a caller can reopen the same
        /// file later to simulate a restart.
        fn open_store(path: &std::path::Path) -> (DbHandle, SharedBlockStore) {
            let db: DbHandle = Arc::new(Mutex::new(KernelDb::open(path).expect("open db")));
            let creator = PrincipalId::system();
            let ws = db
                .lock()
                .get_or_create_default_workspace(creator)
                .expect("workspace");
            let blocks = shared_block_store_with_db(db.clone(), ws, creator);
            (db, blocks)
        }

        /// Mint a drift-queue document directly (bypassing
        /// `ensure_drift_queue_context`'s router-cache/registry dance — that
        /// resolution order has its own tests below) and attach it to a
        /// fresh router, alongside two ordinary registered contexts to stage
        /// between.
        fn attached_router(
            blocks: &SharedBlockStore,
        ) -> (DriftRouter, ContextId, ContextId, ContextId) {
            let queue_ctx = ContextId::new();
            blocks
                .create_document(queue_ctx, crate::DocumentKind::Conversation, None)
                .expect("create drift-queue document");
            let mut router = DriftRouter::new();
            router.attach_persistence(blocks.clone(), queue_ctx);
            let src = ContextId::new();
            let tgt = ContextId::new();
            router
                .register(src, Some("src"), None, PrincipalId::system())
                .unwrap();
            router
                .register(tgt, Some("tgt"), None, PrincipalId::system())
                .unwrap();
            (router, queue_ctx, src, tgt)
        }

        #[test]
        fn stage_writes_a_durable_pending_block() {
            let dir = tempfile::tempdir().unwrap();
            let (_db, blocks) = open_store(&dir.path().join("t.db"));
            let (mut router, queue_ctx, src, tgt) = attached_router(&blocks);

            router
                .stage(
                    src,
                    tgt,
                    "durable content".into(),
                    None,
                    DriftKind::Push,
                    PrincipalId::new(),
                )
                .unwrap();

            let snaps = blocks.block_snapshots(queue_ctx).unwrap();
            assert_eq!(
                snaps.len(),
                1,
                "stage must write exactly one drift-queue block"
            );
            assert_eq!(snaps[0].status, Status::Pending);
            assert!(snaps[0].content.contains("durable content"));
        }

        #[test]
        fn complete_marks_the_block_done_so_it_is_not_resurrected() {
            let dir = tempfile::tempdir().unwrap();
            let (_db, blocks) = open_store(&dir.path().join("t.db"));
            let (mut router, queue_ctx, src, tgt) = attached_router(&blocks);

            let id = router
                .stage(src, tgt, "delivered".into(), None, DriftKind::Push, PrincipalId::new())
                .unwrap();
            router.complete(id);

            let snaps = blocks.block_snapshots(queue_ctx).unwrap();
            assert_eq!(snaps.len(), 1);
            assert_eq!(
                snaps[0].status,
                Status::Done,
                "a completed item's durable record must be marked consumed"
            );

            router.rehydrate_from_block_log().unwrap();
            assert!(
                router.queue().is_empty(),
                "a Done record must not be resurrected by rehydrate"
            );
        }

        #[test]
        fn cancel_marks_the_block_done_so_it_is_not_resurrected() {
            let dir = tempfile::tempdir().unwrap();
            let (_db, blocks) = open_store(&dir.path().join("t.db"));
            let (mut router, queue_ctx, src, tgt) = attached_router(&blocks);

            let id = router
                .stage(src, tgt, "cancel me".into(), None, DriftKind::Push, PrincipalId::new())
                .unwrap();
            assert!(router.cancel(id));

            let snaps = blocks.block_snapshots(queue_ctx).unwrap();
            assert_eq!(snaps[0].status, Status::Done);

            router.rehydrate_from_block_log().unwrap();
            assert!(router.queue().is_empty());
        }

        #[test]
        fn restart_recovers_a_staged_item_never_flushed() {
            // THE test slice 3 exists for: a restart between `stage` and any
            // flush must not lose the content.
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("t.db");
            let queue_ctx = ContextId::new();
            let src = ContextId::new();
            let tgt = ContextId::new();
            let creator = PrincipalId::system();

            let staged_id = {
                let db: DbHandle = Arc::new(Mutex::new(KernelDb::open(&db_path).expect("open db")));
                let ws = db.lock().get_or_create_default_workspace(creator).unwrap();
                let blocks = shared_block_store_with_db(db.clone(), ws, creator);
                blocks
                    .create_document(queue_ctx, crate::DocumentKind::Conversation, None)
                    .unwrap();

                let mut router = DriftRouter::new();
                router.attach_persistence(blocks.clone(), queue_ctx);
                router.register(src, Some("src"), None, creator).unwrap();
                router.register(tgt, Some("tgt"), None, creator).unwrap();
                let id = router
                    .stage(
                        src,
                        tgt,
                        "survive the restart".into(),
                        None,
                        DriftKind::Push,
                        PrincipalId::new(),
                    )
                    .unwrap();

                // Simulate an unclean kill: leak the router, the block store,
                // and the DB connection. Nothing is cleanly closed or
                // checkpointed — whatever is committed must survive on disk
                // alone, exactly as `test_durability_across_kill` verifies
                // for `BlockStore` itself.
                std::mem::forget(router);
                std::mem::forget(blocks);
                std::mem::forget(db);
                id
            };

            // "Restart": a fresh connection to the SAME on-disk file, a fresh
            // BlockStore that loads from it, and a fresh DriftRouter with
            // nothing in memory.
            let db2: DbHandle = Arc::new(Mutex::new(KernelDb::open(&db_path).expect("reopen db")));
            let ws2 = db2.lock().get_or_create_default_workspace(creator).unwrap();
            let blocks2 = shared_block_store_with_db(db2, ws2, creator);
            blocks2.load_from_db().expect("load_from_db");

            let mut router2 = DriftRouter::new();
            router2.attach_persistence(blocks2, queue_ctx);
            router2.register(src, Some("src"), None, creator).unwrap();
            router2.register(tgt, Some("tgt"), None, creator).unwrap();
            router2.rehydrate_from_block_log().unwrap();

            assert_eq!(
                router2.queue().len(),
                1,
                "a staged item never flushed before the restart must survive it"
            );
            assert_eq!(router2.queue()[0].id, staged_id);
            assert_eq!(router2.queue()[0].content, "survive the restart");
        }

        #[test]
        fn restart_recovers_a_dead_lettered_item() {
            // The other half of the same bug: `docs/issues.md`'s exact
            // words were "the mechanism whose whole job is 'content is never
            // silently discarded' silently discards content." This drives an
            // item past MAX_DRIFT_RETRIES into `dead_letter`, "restarts," and
            // checks it is still there — not lost, not duplicated.
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("t.db");
            let queue_ctx = ContextId::new();
            let src = ContextId::new();
            let tgt = ContextId::new();
            let creator = PrincipalId::system();

            {
                let db: DbHandle = Arc::new(Mutex::new(KernelDb::open(&db_path).expect("open db")));
                let ws = db.lock().get_or_create_default_workspace(creator).unwrap();
                let blocks = shared_block_store_with_db(db.clone(), ws, creator);
                blocks
                    .create_document(queue_ctx, crate::DocumentKind::Conversation, None)
                    .unwrap();

                let mut router = DriftRouter::new();
                router.attach_persistence(blocks.clone(), queue_ctx);
                router.register(src, Some("src"), None, creator).unwrap();
                router.register(tgt, Some("tgt"), None, creator).unwrap();
                router
                    .stage(src, tgt, "persistent failure".into(), None, DriftKind::Push, PrincipalId::new())
                    .unwrap();

                for _ in 0..=(MAX_DRIFT_RETRIES as usize) {
                    let drained = router.drain(None);
                    router.requeue(drained);
                }
                assert_eq!(router.dead_letters().len(), 1, "must be dead-lettered before the restart");

                std::mem::forget(router);
                std::mem::forget(blocks);
                std::mem::forget(db);
            }

            let db2: DbHandle = Arc::new(Mutex::new(KernelDb::open(&db_path).expect("reopen db")));
            let ws2 = db2.lock().get_or_create_default_workspace(creator).unwrap();
            let blocks2 = shared_block_store_with_db(db2, ws2, creator);
            blocks2.load_from_db().expect("load_from_db");

            let mut router2 = DriftRouter::new();
            router2.attach_persistence(blocks2, queue_ctx);
            router2.register(src, Some("src"), None, creator).unwrap();
            router2.register(tgt, Some("tgt"), None, creator).unwrap();
            router2.rehydrate_from_block_log().unwrap();

            assert!(router2.queue().is_empty(), "must not resurrect as staging");
            let dl = router2.dead_letters();
            assert_eq!(dl.len(), 1, "the dead letter must survive the restart, not just once but exactly once");
            assert_eq!(dl[0].content, "persistent failure");
        }

        #[test]
        fn drain_dead_letter_alone_does_not_consume_the_durable_record() {
            // Task B (docs/issues.md, "Drift drain acks before the
            // lost+found write..."): the two-phase drain leaves the durable
            // record `Pending` until `ack_dead_letter` confirms the write
            // into lost+found actually succeeded. Drain alone — no ack —
            // must NOT consume it, or a crash in the window between the two
            // loses the item exactly like the old single-phase version did.
            let dir = tempfile::tempdir().unwrap();
            let (_db, blocks) = open_store(&dir.path().join("t.db"));
            let (mut router, queue_ctx, src, tgt) = attached_router(&blocks);

            router
                .stage(src, tgt, "dies then flushes".into(), None, DriftKind::Push, PrincipalId::new())
                .unwrap();
            for _ in 0..=(MAX_DRIFT_RETRIES as usize) {
                let drained = router.drain(None);
                router.requeue(drained);
            }
            let drained_dead = router.drain_dead_letter();
            assert_eq!(drained_dead.len(), 1);

            let snaps = blocks.block_snapshots(queue_ctx).unwrap();
            assert!(
                snaps.iter().any(|s| s.status == Status::Pending),
                "drain alone must NOT consume the durable record — only ack_dead_letter does"
            );

            // A restart right now (ack never happened) must recover it.
            router.rehydrate_from_block_log().unwrap();
            assert!(router.queue().is_empty());
            assert_eq!(
                router.dead_letters().len(),
                1,
                "un-acked dead letter must survive rehydrate, not vanish"
            );
        }

        #[test]
        fn ack_dead_letter_marks_the_block_done_so_it_is_not_resurrected() {
            // The other half of the two-phase contract: once the caller
            // confirms the lost+found write succeeded, ack really does
            // consume the durable record — the queue must not grow forever.
            let dir = tempfile::tempdir().unwrap();
            let (_db, blocks) = open_store(&dir.path().join("t.db"));
            let (mut router, queue_ctx, src, tgt) = attached_router(&blocks);

            router
                .stage(src, tgt, "delivered eventually".into(), None, DriftKind::Push, PrincipalId::new())
                .unwrap();
            for _ in 0..=(MAX_DRIFT_RETRIES as usize) {
                let drained = router.drain(None);
                router.requeue(drained);
            }
            let drained_dead = router.drain_dead_letter();
            assert_eq!(drained_dead.len(), 1);
            assert!(router.ack_dead_letter(drained_dead[0].id));

            let snaps = blocks.block_snapshots(queue_ctx).unwrap();
            assert!(
                snaps.iter().all(|s| s.status == Status::Done),
                "ack must mark the durable record consumed"
            );

            router.rehydrate_from_block_log().unwrap();
            assert!(router.queue().is_empty());
            assert!(
                router.dead_letters().is_empty(),
                "an acked item must not be resurrected"
            );
        }

        #[test]
        fn drain_dead_letter_then_crash_before_ack_recovers_on_restart() {
            // THE test the residual gap asks for (docs/issues.md, "Drift
            // drain acks before the lost+found write, so a narrow crash
            // window remains"): drain checks an item out for delivery, the
            // process dies before the caller can ever call
            // `ack_dead_letter` (or `restore_dead_letters` on failure) —
            // simulating a crash squarely inside the old unsafe window —
            // and a restart must still recover the item. Never both-lost:
            // at worst this is a duplicate re-delivery on the next flush,
            // never a silent loss.
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("t.db");
            let queue_ctx = ContextId::new();
            let src = ContextId::new();
            let tgt = ContextId::new();
            let creator = PrincipalId::system();

            {
                let db: DbHandle = Arc::new(Mutex::new(KernelDb::open(&db_path).expect("open db")));
                let ws = db.lock().get_or_create_default_workspace(creator).unwrap();
                let blocks = shared_block_store_with_db(db.clone(), ws, creator);
                blocks
                    .create_document(queue_ctx, crate::DocumentKind::Conversation, None)
                    .unwrap();

                let mut router = DriftRouter::new();
                router.attach_persistence(blocks.clone(), queue_ctx);
                router.register(src, Some("src"), None, creator).unwrap();
                router.register(tgt, Some("tgt"), None, creator).unwrap();
                router
                    .stage(src, tgt, "crash before ack".into(), None, DriftKind::Push, PrincipalId::new())
                    .unwrap();
                for _ in 0..=(MAX_DRIFT_RETRIES as usize) {
                    let drained = router.drain(None);
                    router.requeue(drained);
                }
                assert_eq!(router.dead_letters().len(), 1);

                // The flush engine checks the item out for a lost+found
                // write...
                let drained_dead = router.drain_dead_letter();
                assert_eq!(drained_dead.len(), 1);
                // ...and the process dies right here — no ack_dead_letter,
                // no restore_dead_letters, nothing. This is the crash the
                // old single-phase drain_dead_letter lost.

                std::mem::forget(router);
                std::mem::forget(blocks);
                std::mem::forget(db);
            }

            let db2: DbHandle = Arc::new(Mutex::new(KernelDb::open(&db_path).expect("reopen db")));
            let ws2 = db2.lock().get_or_create_default_workspace(creator).unwrap();
            let blocks2 = shared_block_store_with_db(db2, ws2, creator);
            blocks2.load_from_db().expect("load_from_db");

            let mut router2 = DriftRouter::new();
            router2.attach_persistence(blocks2, queue_ctx);
            router2.register(src, Some("src"), None, creator).unwrap();
            router2.register(tgt, Some("tgt"), None, creator).unwrap();
            router2.rehydrate_from_block_log().unwrap();

            let dl = router2.dead_letters();
            assert_eq!(
                dl.len(),
                1,
                "a crash between drain_dead_letter and ack must not lose the item"
            );
            assert_eq!(dl[0].content, "crash before ack");
            assert!(!dl[0].in_flight, "rehydrate must clear in_flight — nothing is truly in flight at process start");
        }

        #[test]
        fn restore_dead_letters_survives_a_restart() {
            // With the two-phase drain (Task B), `drain_dead_letter` never
            // marks the durable record consumed in the first place — so
            // restoring after a failed write does not need to re-persist
            // anything, the record has been sitting `Pending` the whole
            // time. This pins that: a restart right after a restore must
            // still find the item, whether restore did real work or none.
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("t.db");
            let queue_ctx = ContextId::new();
            let src = ContextId::new();
            let tgt = ContextId::new();
            let creator = PrincipalId::system();

            {
                let db: DbHandle = Arc::new(Mutex::new(KernelDb::open(&db_path).expect("open db")));
                let ws = db.lock().get_or_create_default_workspace(creator).unwrap();
                let blocks = shared_block_store_with_db(db.clone(), ws, creator);
                blocks
                    .create_document(queue_ctx, crate::DocumentKind::Conversation, None)
                    .unwrap();

                let mut router = DriftRouter::new();
                router.attach_persistence(blocks.clone(), queue_ctx);
                router.register(src, Some("src"), None, creator).unwrap();
                router.register(tgt, Some("tgt"), None, creator).unwrap();
                router
                    .stage(src, tgt, "write failed, restored".into(), None, DriftKind::Push, PrincipalId::new())
                    .unwrap();
                for _ in 0..=(MAX_DRIFT_RETRIES as usize) {
                    let drained = router.drain(None);
                    router.requeue(drained);
                }
                let drained_dead = router.drain_dead_letter();
                // Simulate the flush engine's write into lost+found failing.
                router.restore_dead_letters(drained_dead);
                assert_eq!(router.dead_letters().len(), 1);

                std::mem::forget(router);
                std::mem::forget(blocks);
                std::mem::forget(db);
            }

            let db2: DbHandle = Arc::new(Mutex::new(KernelDb::open(&db_path).expect("reopen db")));
            let ws2 = db2.lock().get_or_create_default_workspace(creator).unwrap();
            let blocks2 = shared_block_store_with_db(db2, ws2, creator);
            blocks2.load_from_db().expect("load_from_db");

            let mut router2 = DriftRouter::new();
            router2.attach_persistence(blocks2, queue_ctx);
            router2.register(src, Some("src"), None, creator).unwrap();
            router2.register(tgt, Some("tgt"), None, creator).unwrap();
            router2.rehydrate_from_block_log().unwrap();

            let dl = router2.dead_letters();
            assert_eq!(dl.len(), 1, "a restored-after-failed-write dead letter must survive a restart");
            assert_eq!(dl[0].content, "write failed, restored");
        }

        #[test]
        fn replay_dead_letter_survives_a_restart_as_staging() {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("t.db");
            let queue_ctx = ContextId::new();
            let src = ContextId::new();
            let tgt = ContextId::new();
            let creator = PrincipalId::system();

            let replayed_id = {
                let db: DbHandle = Arc::new(Mutex::new(KernelDb::open(&db_path).expect("open db")));
                let ws = db.lock().get_or_create_default_workspace(creator).unwrap();
                let blocks = shared_block_store_with_db(db.clone(), ws, creator);
                blocks
                    .create_document(queue_ctx, crate::DocumentKind::Conversation, None)
                    .unwrap();

                let mut router = DriftRouter::new();
                router.attach_persistence(blocks.clone(), queue_ctx);
                router.register(src, Some("src"), None, creator).unwrap();
                router.register(tgt, Some("tgt"), None, creator).unwrap();
                let id = router
                    .stage(src, tgt, "replay me".into(), None, DriftKind::Push, PrincipalId::new())
                    .unwrap();
                for _ in 0..=(MAX_DRIFT_RETRIES as usize) {
                    let drained = router.drain(None);
                    router.requeue(drained);
                }
                let replayed = router.replay_dead_letter(id).expect("replay");
                assert_eq!(replayed.retry_count, 0);

                std::mem::forget(router);
                std::mem::forget(blocks);
                std::mem::forget(db);
                id
            };

            let db2: DbHandle = Arc::new(Mutex::new(KernelDb::open(&db_path).expect("reopen db")));
            let ws2 = db2.lock().get_or_create_default_workspace(creator).unwrap();
            let blocks2 = shared_block_store_with_db(db2, ws2, creator);
            blocks2.load_from_db().expect("load_from_db");

            let mut router2 = DriftRouter::new();
            router2.attach_persistence(blocks2, queue_ctx);
            router2.register(src, Some("src"), None, creator).unwrap();
            router2.register(tgt, Some("tgt"), None, creator).unwrap();
            router2.rehydrate_from_block_log().unwrap();

            assert!(router2.dead_letters().is_empty());
            assert_eq!(router2.queue().len(), 1, "a replayed item must survive as staging, not dead-letter");
            assert_eq!(router2.queue()[0].id, replayed_id);
            assert_eq!(router2.queue()[0].retry_count, 0);
        }

        #[test]
        fn unregister_sweep_persists_the_dead_letter_before_a_restart() {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("t.db");
            let queue_ctx = ContextId::new();
            let src = ContextId::new();
            let tgt = ContextId::new();
            let creator = PrincipalId::system();

            {
                let db: DbHandle = Arc::new(Mutex::new(KernelDb::open(&db_path).expect("open db")));
                let ws = db.lock().get_or_create_default_workspace(creator).unwrap();
                let blocks = shared_block_store_with_db(db.clone(), ws, creator);
                blocks
                    .create_document(queue_ctx, crate::DocumentKind::Conversation, None)
                    .unwrap();

                let mut router = DriftRouter::new();
                router.attach_persistence(blocks.clone(), queue_ctx);
                router.register(src, Some("src"), None, creator).unwrap();
                router.register(tgt, Some("tgt"), None, creator).unwrap();
                router
                    .stage(src, tgt, "orphaned by unregister".into(), None, DriftKind::Push, PrincipalId::new())
                    .unwrap();
                router.unregister(tgt);
                assert_eq!(router.dead_letters().len(), 1);

                std::mem::forget(router);
                std::mem::forget(blocks);
                std::mem::forget(db);
            }

            let db2: DbHandle = Arc::new(Mutex::new(KernelDb::open(&db_path).expect("reopen db")));
            let ws2 = db2.lock().get_or_create_default_workspace(creator).unwrap();
            let blocks2 = shared_block_store_with_db(db2, ws2, creator);
            blocks2.load_from_db().expect("load_from_db");

            let mut router2 = DriftRouter::new();
            router2.attach_persistence(blocks2, queue_ctx);
            router2.register(src, Some("src"), None, creator).unwrap();
            router2.rehydrate_from_block_log().unwrap();

            let dl = router2.dead_letters();
            assert_eq!(dl.len(), 1, "unregister's dead-letter sweep must survive a restart");
            assert_eq!(dl[0].content, "orphaned by unregister");
        }

        #[test]
        fn without_attach_persistence_behavior_is_unchanged() {
            // Non-vacuity guard for the whole module: every test above
            // depends on `attach_persistence` actually doing something. This
            // proves the DEFAULT (nothing attached) path — every existing
            // caller of `DriftRouter` today — is untouched by any of it.
            let mut router = DriftRouter::new();
            assert!(!router.has_persistence());
            let src = ContextId::new();
            let tgt = ContextId::new();
            router.register(src, Some("src"), None, PrincipalId::system()).unwrap();
            router.register(tgt, Some("tgt"), None, PrincipalId::system()).unwrap();
            let id = router
                .stage(src, tgt, "no persistence attached".into(), None, DriftKind::Push, PrincipalId::new())
                .unwrap();
            router.complete(id);
            // rehydrate on an unattached router is a documented no-op, not
            // an error.
            router.rehydrate_from_block_log().unwrap();
            assert!(router.queue().is_empty());
        }

        #[test]
        fn ensure_drift_queue_context_mints_once_and_is_recoverable_after_restart() {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("t.db");

            let minted_id = {
                let db: DbHandle = Arc::new(Mutex::new(KernelDb::open(&db_path).expect("open db")));
                let ws = db
                    .lock()
                    .get_or_create_default_workspace(PrincipalId::system())
                    .unwrap();
                let blocks = shared_block_store_with_db(db.clone(), ws, PrincipalId::system());
                let router = shared_drift_router();

                let id1 = ensure_drift_queue_context(&router, &blocks, &db).unwrap();
                // Idempotent within the same process: second call hits the
                // router cache and returns the same id without minting again.
                let id2 = ensure_drift_queue_context(&router, &blocks, &db).unwrap();
                assert_eq!(id1, id2);
                assert!(router.read().get(id1).is_some(), "must be a real registered handle");

                std::mem::forget(router);
                std::mem::forget(blocks);
                std::mem::forget(db);
                id1
            };

            // "Restart": a fresh router has no cache, but the well-known
            // registry row survived — `ensure_drift_queue_context` must
            // adopt it rather than minting a second one.
            let db2: DbHandle = Arc::new(Mutex::new(KernelDb::open(&db_path).expect("reopen db")));
            let ws2 = db2
                .lock()
                .get_or_create_default_workspace(PrincipalId::system())
                .unwrap();
            let blocks2 = shared_block_store_with_db(db2.clone(), ws2, PrincipalId::system());
            blocks2.load_from_db().expect("load_from_db");
            let router2 = shared_drift_router();

            let recovered = ensure_drift_queue_context(&router2, &blocks2, &db2).unwrap();
            assert_eq!(
                recovered, minted_id,
                "a restart must recover the same drift-queue context, never mint a duplicate"
            );
        }
    }
}
