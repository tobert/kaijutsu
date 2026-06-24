//! Multi-context document store.
//!
//! Holds a [`SyncedDocument`] (and its compose-input CRDT) per joined context,
//! enabling instant context switching, background sync for inactive contexts,
//! and LRU eviction. This is the client-owned home for *all* conversation
//! document state and (incrementally) the sync logic that maintains it — the app
//! is a renderer over it, not the owner of CRDT/sync mechanics.
//!
//! Bevy-free on purpose: the app wraps this in a `Resource` newtype, but the
//! store itself is plain Rust so its logic is unit-testable without a world.

use std::collections::HashMap;
use std::time::Instant;

use kaijutsu_crdt::PrincipalId;
use kaijutsu_types::ContextId;

use crate::{ServerEvent, SyncEffect, SyncError, SyncState, SyncedDocument, SyncedInput};

/// A cached document for a single context: its CRDT doc, compose input, and the
/// bookkeeping the store needs to keep it fresh and pick eviction victims.
#[allow(dead_code)]
pub struct DocumentEntry {
    /// Synced document — wraps CrdtBlockStore + SyncManager.
    pub synced: SyncedDocument,
    /// CRDT-backed input document (compose scratchpad).
    /// `None` until the input state is fetched from the server.
    pub input: Option<SyncedInput>,
    /// Context name (e.g. the kernel_id or user-supplied name).
    pub context_name: String,
    /// Generation counter at last sync (for staleness detection).
    pub synced_at_generation: u64,
    /// When this document was last accessed (for LRU eviction).
    pub last_accessed: Instant,
    /// Set after submit/escape×3 to suppress late-arriving TextOps and
    /// SyncedInput restoration. Cleared when `InputCleared` arrives from
    /// the server and triggers a clean re-fetch.
    pub input_pending_clear: bool,
}

impl DocumentEntry {
    /// A freshly-synced entry around `synced`, marked current at `generation`.
    /// Input is unset until fetched.
    pub fn new(synced: SyncedDocument, context_name: String, generation: u64) -> Self {
        Self {
            synced,
            input: None,
            context_name,
            synced_at_generation: generation,
            last_accessed: Instant::now(),
            input_pending_clear: false,
        }
    }
}

/// What applying a streamed [`ServerEvent`] did to the store — the app reacts
/// (e.g. scroll-follow) without re-deriving sync mechanics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventApplied {
    /// A cached doc advanced (new/changed block).
    Updated,
    /// Not a document event, or for a context we don't cache — nothing changed.
    Ignored,
}

/// Multi-context document store — the authoritative source for all document
/// state. The active entry is what the renderer draws.
#[allow(dead_code)]
pub struct DocumentStore {
    /// Map from context_id → cached document state.
    documents: HashMap<ContextId, DocumentEntry>,
    /// Currently active (rendered) context_id.
    active_id: Option<ContextId>,
    /// Most-recently-used context IDs (front = most recent).
    mru: Vec<ContextId>,
    /// Maximum number of cached documents before LRU eviction.
    max_cached: usize,
    /// Monotonic sync generation. Bumped on reconnect, broadcast lag, or a
    /// `NeedsResync` effect; an entry whose `synced_at_generation` is behind is
    /// stale and wants a full re-fetch. The single source of truth (the app no
    /// longer keeps its own `SyncGeneration`).
    generation: u64,
}

impl Default for DocumentStore {
    fn default() -> Self {
        Self {
            documents: HashMap::new(),
            active_id: None,
            mru: Vec::new(),
            max_cached: 8,
            generation: 0,
        }
    }
}

#[allow(dead_code)]
impl DocumentStore {
    /// Get the active context ID.
    pub fn active_id(&self) -> Option<ContextId> {
        self.active_id
    }

    /// Get a reference to a cached document by context_id.
    pub fn get(&self, context_id: ContextId) -> Option<&DocumentEntry> {
        self.documents.get(&context_id)
    }

    /// Get a mutable reference to a cached document by context_id.
    pub fn get_mut(&mut self, context_id: ContextId) -> Option<&mut DocumentEntry> {
        self.documents.get_mut(&context_id)
    }

    /// Check if a document is cached.
    pub fn contains(&self, context_id: ContextId) -> bool {
        self.documents.contains_key(&context_id)
    }

    /// Insert a new cached document. Evicts LRU entry if at capacity.
    pub fn insert(&mut self, context_id: ContextId, cached: DocumentEntry) {
        if self.documents.len() >= self.max_cached {
            self.evict_lru();
        }
        self.documents.insert(context_id, cached);
        self.touch_mru(context_id);
    }

    /// Set the active document. Returns the previous active_id if changed.
    pub fn set_active(&mut self, context_id: ContextId) -> Option<ContextId> {
        let previous = self.active_id.take();
        self.active_id = Some(context_id);
        self.touch_mru(context_id);

        if let Some(doc) = self.documents.get_mut(&context_id) {
            doc.last_accessed = Instant::now();
        }

        previous
    }

    /// Get MRU-ordered context IDs (most recent first).
    pub fn mru_ids(&self) -> &[ContextId] {
        &self.mru
    }

    /// Number of cached documents.
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Whether the store holds no documents.
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Iterate over all cached documents.
    pub fn iter(&self) -> impl Iterator<Item = (ContextId, &DocumentEntry)> {
        self.documents.iter().map(|(&k, v)| (k, v))
    }

    // ── sync logic (the app holds none of this) ──────────────────────────

    /// Current sync generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Bump the generation so every cached doc reads stale until re-synced.
    /// Called on reconnect (`ServerEvent::Reconnected`) and broadcast lag.
    /// Returns the new value.
    pub fn bump_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// Apply a streamed server event to its context's document, keeping the
    /// entry's staleness marker in step. A `NeedsResync` effect (compaction)
    /// resets that doc and bumps the generation so a full re-fetch follows.
    /// Events with no document target — or for a context we don't cache — are
    /// [`EventApplied::Ignored`].
    pub fn apply_server_event(&mut self, event: &ServerEvent) -> EventApplied {
        let Some(ctx_id) = event_target_context(event) else {
            return EventApplied::Ignored;
        };
        let generation = self.generation;
        let Some(entry) = self.documents.get_mut(&ctx_id) else {
            return EventApplied::Ignored;
        };
        match entry.synced.apply_event(event) {
            SyncEffect::Updated { .. } | SyncEffect::FullSync { .. } => {
                entry.synced_at_generation = generation;
                EventApplied::Updated
            }
            SyncEffect::NeedsResync => {
                entry.synced_at_generation = 0;
                self.generation = self.generation.wrapping_add(1);
                EventApplied::Updated
            }
            SyncEffect::Ignored => EventApplied::Ignored,
        }
    }

    /// Apply a fetched [`SyncState`] (initial join sync, or a reconnect re-sync)
    /// to a context's document, creating the entry — named via `name` — if it's
    /// not cached yet. Marks the doc synced at the current generation. Returns
    /// `true` if a new entry was inserted.
    pub fn apply_sync(
        &mut self,
        context_id: ContextId,
        state: &SyncState,
        principal_id: PrincipalId,
        name: impl FnOnce() -> String,
    ) -> Result<bool, SyncError> {
        let generation = self.generation;
        if let Some(entry) = self.documents.get_mut(&context_id) {
            entry.synced.apply_sync_state(state)?;
            entry.synced_at_generation = generation;
            Ok(false)
        } else {
            let mut synced = SyncedDocument::new(context_id, principal_id);
            synced.apply_sync_state(state)?;
            self.insert(context_id, DocumentEntry::new(synced, name(), generation));
            Ok(true)
        }
    }

    /// The active context if its document is behind the current generation —
    /// i.e. it wants a full re-fetch (`get_context_sync`). `None` when there's
    /// no active context or it's already fresh.
    pub fn stale_active(&self) -> Option<ContextId> {
        let active = self.active_id?;
        let entry = self.documents.get(&active)?;
        (entry.synced_at_generation < self.generation).then_some(active)
    }

    /// Mark a context's document fresh at the current generation (after a
    /// re-fetch has been applied).
    pub fn mark_synced(&mut self, context_id: ContextId) {
        let generation = self.generation;
        if let Some(entry) = self.documents.get_mut(&context_id) {
            entry.synced_at_generation = generation;
        }
    }

    /// Move a context_id to the front of the MRU list.
    fn touch_mru(&mut self, context_id: ContextId) {
        self.mru.retain(|&id| id != context_id);
        self.mru.insert(0, context_id);
    }

    /// Evict the least-recently-used document (never the active one).
    fn evict_lru(&mut self) {
        let evict_id = self
            .mru
            .iter()
            .rev()
            .find(|&&id| self.active_id != Some(id))
            .copied();

        if let Some(id) = evict_id {
            self.documents.remove(&id);
            self.mru.retain(|&mid| mid != id);
            log::info!("DocumentStore: evicted LRU document {}", id);
        }
    }
}

/// The context a streamed block event applies to, or `None` if it's not a
/// version-gated document event. Deliberately omits `BlockMetadataChanged`
/// (exit_code/stderr/…): metadata is frontier-independent and applied to the
/// store directly, outside the version-gated render path — mirroring the app's
/// long-standing routing.
fn event_target_context(event: &ServerEvent) -> Option<ContextId> {
    match event {
        ServerEvent::BlockInserted { context_id, .. }
        | ServerEvent::BlockTextOps { context_id, .. }
        | ServerEvent::BlockStatusChanged { context_id, .. }
        | ServerEvent::BlockOutputChanged { context_id, .. }
        | ServerEvent::BlockDeleted { context_id, .. }
        | ServerEvent::BlockCollapsedChanged { context_id, .. }
        | ServerEvent::BlockExcludedChanged { context_id, .. }
        | ServerEvent::BlockMoved { context_id, .. }
        | ServerEvent::SyncReset { context_id, .. } => Some(*context_id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_crdt::block_store::BlockStore as CrdtBlockStore;
    use kaijutsu_types::{BlockKind, ContentType, Role, Status};

    fn ctx() -> ContextId {
        ContextId::new()
    }

    /// A server-side CRDT store with `n` text blocks.
    fn server_store(context_id: ContextId, n: usize) -> CrdtBlockStore {
        let mut store = CrdtBlockStore::new(context_id, PrincipalId::new());
        for i in 0..n {
            store
                .insert_block(
                    None,
                    None,
                    Role::User,
                    BlockKind::Text,
                    &format!("b{i}"),
                    Status::Done,
                    ContentType::Plain,
                )
                .expect("insert");
        }
        store
    }

    /// A full-snapshot `SyncState` carrying `n` blocks for `context_id`.
    fn sync_state(context_id: ContextId, n: usize) -> SyncState {
        let store = server_store(context_id, n);
        SyncState {
            context_id,
            version: n as u64,
            ops: kaijutsu_types::codec::encode(&store.snapshot()).expect("encode snapshot"),
        }
    }

    #[test]
    fn apply_sync_creates_then_updates() {
        let mut store = DocumentStore::default();
        let c = ctx();
        let created = store
            .apply_sync(c, &sync_state(c, 1), PrincipalId::new(), || "ctx".into())
            .unwrap();
        assert!(created, "first apply inserts a new entry");
        assert_eq!(store.get(c).unwrap().synced.block_count(), 1);

        let created2 = store
            .apply_sync(c, &sync_state(c, 2), PrincipalId::new(), || "ctx".into())
            .unwrap();
        assert!(!created2, "second apply updates the existing entry");
        assert_eq!(store.get(c).unwrap().synced.block_count(), 2);
    }

    #[test]
    fn generation_drives_active_staleness() {
        let mut store = DocumentStore::default();
        let c = ctx();
        store
            .apply_sync(c, &sync_state(c, 1), PrincipalId::new(), || "ctx".into())
            .unwrap();
        store.set_active(c);
        assert_eq!(store.stale_active(), None, "freshly synced active is fresh");

        store.bump_generation();
        assert_eq!(store.stale_active(), Some(c), "a bump makes the active doc stale");

        store.mark_synced(c);
        assert_eq!(store.stale_active(), None, "mark_synced clears staleness");
    }

    #[test]
    fn apply_sync_marks_fresh_at_current_generation() {
        let mut store = DocumentStore::default();
        let c = ctx();
        store
            .apply_sync(c, &sync_state(c, 1), PrincipalId::new(), || "ctx".into())
            .unwrap();
        store.set_active(c);
        store.bump_generation();
        assert_eq!(store.stale_active(), Some(c));

        // Re-applying the fetched state marks the doc fresh again.
        store
            .apply_sync(c, &sync_state(c, 2), PrincipalId::new(), || "ctx".into())
            .unwrap();
        assert_eq!(store.stale_active(), None);
    }

    #[test]
    fn no_active_context_is_never_stale() {
        let mut store = DocumentStore::default();
        store.bump_generation();
        assert_eq!(store.stale_active(), None);
    }

    #[test]
    fn apply_server_event_advances_and_remarks_synced() {
        let mut store = DocumentStore::default();
        let c = ctx();
        let mut server = server_store(c, 1);
        store
            .apply_sync(
                c,
                &SyncState {
                    context_id: c,
                    version: 1,
                    ops: kaijutsu_types::codec::encode(&server.snapshot()).unwrap(),
                },
                PrincipalId::new(),
                || "ctx".into(),
            )
            .unwrap();
        store.set_active(c);
        store.bump_generation();
        assert_eq!(store.stale_active(), Some(c));

        // A streamed insert advances the doc and re-marks it fresh.
        let frontier = server.frontier();
        let id = server
            .insert_block(
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "resp",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let block = server.get_block_snapshot(&id).unwrap();
        let ops = kaijutsu_types::codec::encode(&server.ops_since(&frontier)).unwrap();

        let applied = store.apply_server_event(&ServerEvent::BlockInserted {
            context_id: c,
            block: Box::new(block),
            ops,
        });
        assert_eq!(applied, EventApplied::Updated);
        assert_eq!(store.get(c).unwrap().synced.block_count(), 2);
        assert_eq!(
            store.stale_active(),
            None,
            "applying a stream event re-marks the doc fresh"
        );
    }

    #[test]
    fn apply_server_event_ignored_for_uncached_and_non_document() {
        let mut store = DocumentStore::default();
        // A connection-level event is not a document event.
        assert_eq!(
            store.apply_server_event(&ServerEvent::Reconnected),
            EventApplied::Ignored
        );

        // A block event for a context we don't cache is ignored (no panic).
        let c = ctx();
        let mut server = server_store(c, 1);
        let frontier = server.frontier();
        let id = server
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "x",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let block = server.get_block_snapshot(&id).unwrap();
        let ops = kaijutsu_types::codec::encode(&server.ops_since(&frontier)).unwrap();
        assert_eq!(
            store.apply_server_event(&ServerEvent::BlockInserted {
                context_id: c,
                block: Box::new(block),
                ops,
            }),
            EventApplied::Ignored
        );
    }
}
