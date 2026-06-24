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

use kaijutsu_types::ContextId;

use crate::{SyncedDocument, SyncedInput};

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
    /// Saved scroll offset (restored on switch-back). View state today; kept on
    /// the entry for now to avoid churning consumers — a later increment lifts it
    /// to an app-side companion.
    pub scroll_offset: f32,
    /// Set after submit/escape×3 to suppress late-arriving TextOps and
    /// SyncedInput restoration. Cleared when `InputCleared` arrives from
    /// the server and triggers a clean re-fetch.
    pub input_pending_clear: bool,
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
}

impl Default for DocumentStore {
    fn default() -> Self {
        Self {
            documents: HashMap::new(),
            active_id: None,
            mru: Vec::new(),
            max_cached: 8,
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
