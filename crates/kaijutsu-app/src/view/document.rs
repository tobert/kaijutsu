//! Multi-context document cache.
//!
//! Holds a `SyncedDocument` per joined context, enabling instant context switching,
//! background sync for inactive contexts, and LRU eviction.

use bevy::prelude::*;
pub use kaijutsu_types::ContextId;

/// A cached document for a single context, including its CRDT doc and sync state.
#[allow(dead_code)]
pub struct CachedDocument {
    /// Synced document — wraps CrdtBlockStore + SyncManager.
    pub synced: kaijutsu_client::SyncedDocument,
    /// CRDT-backed input document (compose scratchpad).
    /// `None` until the input state is fetched from the server.
    pub input: Option<kaijutsu_client::SyncedInput>,
    /// Context name (e.g. the kernel_id or user-supplied name).
    pub context_name: String,
    /// Generation counter at last sync (for staleness detection).
    pub synced_at_generation: u64,
    /// When this document was last accessed (for LRU eviction).
    pub last_accessed: std::time::Instant,
    /// Saved scroll offset (restored on switch-back).
    pub scroll_offset: f32,
}

/// Multi-context document cache — the authoritative source for all document state.
///
/// `sync_main_cell_to_conversation` reads from the active cache entry to
/// rebuild the MainCell's BlockStore for rendering.
#[derive(Resource)]
#[allow(dead_code)]
pub struct DocumentCache {
    /// Map from context_id → cached document state.
    documents: std::collections::HashMap<ContextId, CachedDocument>,
    /// Currently active (rendered) context_id.
    active_id: Option<ContextId>,
    /// Most-recently-used context IDs (front = most recent).
    mru: Vec<ContextId>,
    /// Maximum number of cached documents before LRU eviction.
    max_cached: usize,
}

impl Default for DocumentCache {
    fn default() -> Self {
        Self {
            documents: std::collections::HashMap::new(),
            active_id: None,
            mru: Vec::new(),
            max_cached: 8,
        }
    }
}

#[allow(dead_code)]
impl DocumentCache {
    /// Get the active context ID.
    pub fn active_id(&self) -> Option<ContextId> {
        self.active_id
    }

    /// Get a reference to a cached document by context_id.
    pub fn get(&self, context_id: ContextId) -> Option<&CachedDocument> {
        self.documents.get(&context_id)
    }

    /// Get a mutable reference to a cached document by context_id.
    pub fn get_mut(&mut self, context_id: ContextId) -> Option<&mut CachedDocument> {
        self.documents.get_mut(&context_id)
    }

    /// Check if a document is cached.
    pub fn contains(&self, context_id: ContextId) -> bool {
        self.documents.contains_key(&context_id)
    }

    /// Insert a new cached document. Evicts LRU entry if at capacity.
    pub fn insert(&mut self, context_id: ContextId, cached: CachedDocument) {
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
            doc.last_accessed = std::time::Instant::now();
        }

        previous
    }

    /// Get MRU-ordered context IDs (most recent first).
    pub fn mru_ids(&self) -> &[ContextId] {
        &self.mru
    }

    /// Number of cached documents.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Iterate over all cached documents.
    pub fn iter(&self) -> impl Iterator<Item = (ContextId, &CachedDocument)> {
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
            log::info!("DocumentCache: evicted LRU document {}", id);
        }
    }
}
