//! Block-based CRDT storage using kaijutsu-crdt.
//!
//! Each document wraps a `kaijutsu_crdt::block_store::BlockStore` (per-block DTE).
//! Multi-client sync uses `SyncPayload` exchange.
//!
//! # Concurrency Model
//!
//! - DashMap for per-document concurrent access
//! - FlowBus for typed pub/sub real-time updates
//! - parking_lot for efficient locking

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use diamond_types_extended::Frontier;
use parking_lot::RwLock;

use kaijutsu_crdt::block_store::{
    BlockStore as CrdtBlockStore, ForkBlockFilter, StoreSnapshot, SyncPayload,
};
use kaijutsu_crdt::{BlockId, BlockKind, BlockSnapshot, ContentType, Role, Status, ToolKind};
use kaijutsu_types::BlockFilter;
use kaijutsu_types::{ContextId, DocKind, KernelId, PrincipalId, WorkspaceId};

use crate::flows::{BlockFlow, InputDocFlow, OpSource, SharedBlockFlowBus, SharedInputDocFlowBus};
use crate::input_doc::InputDocEntry;
use crate::kernel_db::{DocumentRow, KernelDb};

/// Backward-compatible alias during migration.
pub type DocumentKind = DocKind;

// ============================================================================
// Error types
// ============================================================================

/// Structured error for BlockStore operations.
#[derive(Debug, thiserror::Error)]
pub enum BlockStoreError {
    #[error("document not found: {0}")]
    DocumentNotFound(ContextId),

    #[error("input document not found: {0}")]
    InputDocNotFound(ContextId),

    #[error("document already exists: {0}")]
    DocumentAlreadyExists(ContextId),

    #[error("block not found after insert")]
    BlockNotFoundAfterInsert,

    #[error(transparent)]
    Crdt(#[from] kaijutsu_crdt::CrdtError),

    #[error("database error: {0}")]
    Db(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("no database configured")]
    NoDatabaseConfigured,

    #[error("{0}")]
    Validation(String),
}

/// Result type alias for BlockStore operations.
pub type BlockStoreResult<T> = Result<T, BlockStoreError>;

/// Thread-safe database handle (unified KernelDb).
pub type DbHandle = Arc<parking_lot::Mutex<KernelDb>>;

/// Compaction thresholds for the block document oplog.
const COMPACTION_OP_THRESHOLD: u64 = 500;
const COMPACTION_BYTE_THRESHOLD: u64 = 1_048_576; // 1 MiB

/// Compaction threshold for input document oplog (lower — scratchpads are small).
const INPUT_COMPACTION_OP_THRESHOLD: u64 = 200;

/// Entry for a document in the store.
pub struct DocumentEntry {
    /// Per-block CRDT store (each block owns its own DTE Document).
    pub doc: CrdtBlockStore,
    /// Document metadata.
    pub kind: DocKind,
    /// Programming language (if code).
    pub language: Option<String>,
    /// Version counter (incremented on each modification).
    version: AtomicU64,
    /// Last agent to modify.
    last_agent: RwLock<PrincipalId>,
    /// Sync generation — bumped on reset to force client re-sync.
    sync_generation: AtomicU64,
    /// Next oplog sequence number (monotonic per document).
    next_journal_seq: AtomicU64,
    /// Ops appended since last compaction (for trigger check).
    uncompacted_count: AtomicU64,
    /// Bytes appended since last compaction (for trigger check).
    uncompacted_bytes: AtomicU64,
}

impl DocumentEntry {
    /// Create a new document entry.
    fn new(
        context_id: ContextId,
        kind: DocKind,
        language: Option<String>,
        agent_id: PrincipalId,
    ) -> Self {
        Self {
            doc: CrdtBlockStore::new(context_id, agent_id),
            kind,
            language,
            version: AtomicU64::new(0),
            last_agent: RwLock::new(agent_id),
            sync_generation: AtomicU64::new(0),
            next_journal_seq: AtomicU64::new(0),
            uncompacted_count: AtomicU64::new(0),
            uncompacted_bytes: AtomicU64::new(0),
        }
    }

    /// Create a document entry from a store snapshot.
    /// Create from a snapshot, optionally seeding the journal seq from an oplog.
    fn from_store_snapshot(
        snapshot: StoreSnapshot,
        kind: DocKind,
        language: Option<String>,
        agent_id: PrincipalId,
        journal_seq: u64,
        uncompacted_count: u64,
        uncompacted_bytes: u64,
    ) -> BlockStoreResult<Self> {
        let store = CrdtBlockStore::from_snapshot(snapshot, agent_id)?;
        let version = store.version();
        Ok(Self {
            doc: store,
            kind,
            language,
            version: AtomicU64::new(version),
            last_agent: RwLock::new(agent_id),
            sync_generation: AtomicU64::new(0),
            next_journal_seq: AtomicU64::new(journal_seq),
            uncompacted_count: AtomicU64::new(uncompacted_count),
            uncompacted_bytes: AtomicU64::new(uncompacted_bytes),
        })
    }

    /// Get the current version.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// Increment version and record agent.
    pub fn touch(&self, agent_id: PrincipalId) {
        self.version.fetch_add(1, Ordering::SeqCst);
        *self.last_agent.write() = agent_id;
    }

    /// Get the full text content.
    pub fn content(&self) -> String {
        self.doc.full_text()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.doc.is_empty()
    }

    /// Get the current sync generation.
    pub fn sync_generation(&self) -> u64 {
        self.sync_generation.load(Ordering::SeqCst)
    }
}

/// Store for block-based documents with per-document locking.
pub struct BlockStore {
    /// Concurrent document storage.
    documents: DashMap<ContextId, DocumentEntry>,
    /// Per-context input documents (compose scratchpads).
    input_docs: DashMap<ContextId, InputDocEntry>,
    /// Per-input-doc journal sequence counters.
    input_journal_seqs: DashMap<ContextId, AtomicU64>,
    /// Per-input-doc uncompacted op counts (for compaction trigger).
    input_uncompacted: DashMap<ContextId, AtomicU64>,
    /// Database for persistence (unified KernelDb).
    db: Option<DbHandle>,
    /// Kernel ID for document rows.
    kernel_id: Option<KernelId>,
    /// Default workspace ID for new documents.
    default_workspace_id: Option<WorkspaceId>,
    /// Default agent ID for this store.
    agent_id: RwLock<PrincipalId>,
    /// FlowBus for typed pub/sub.
    block_flows: Option<SharedBlockFlowBus>,
    /// FlowBus for input doc events.
    input_flows: Option<SharedInputDocFlowBus>,
}

impl BlockStore {
    /// Create a new in-memory block store.
    pub fn new(agent_id: PrincipalId) -> Self {
        Self {
            documents: DashMap::new(),
            input_docs: DashMap::new(),
            input_journal_seqs: DashMap::new(),
            input_uncompacted: DashMap::new(),
            db: None,
            kernel_id: None,
            default_workspace_id: None,
            agent_id: RwLock::new(agent_id),
            block_flows: None,
            input_flows: None,
        }
    }

    /// Create a new in-memory block store with FlowBus.
    pub fn with_flows(agent_id: PrincipalId, block_flows: SharedBlockFlowBus) -> Self {
        Self {
            documents: DashMap::new(),
            input_docs: DashMap::new(),
            input_journal_seqs: DashMap::new(),
            input_uncompacted: DashMap::new(),
            db: None,
            kernel_id: None,
            default_workspace_id: None,
            agent_id: RwLock::new(agent_id),
            block_flows: Some(block_flows),
            input_flows: None,
        }
    }

    /// Create a block store with unified KernelDb persistence.
    pub fn with_db(
        db: DbHandle,
        kernel_id: KernelId,
        default_workspace_id: WorkspaceId,
        agent_id: PrincipalId,
    ) -> Self {
        Self {
            documents: DashMap::new(),
            input_docs: DashMap::new(),
            input_journal_seqs: DashMap::new(),
            input_uncompacted: DashMap::new(),
            db: Some(db),
            kernel_id: Some(kernel_id),
            default_workspace_id: Some(default_workspace_id),
            agent_id: RwLock::new(agent_id),
            block_flows: None,
            input_flows: None,
        }
    }

    /// Create a block store with unified KernelDb persistence and FlowBus.
    pub fn with_db_and_flows(
        db: DbHandle,
        kernel_id: KernelId,
        default_workspace_id: WorkspaceId,
        agent_id: PrincipalId,
        block_flows: SharedBlockFlowBus,
    ) -> Self {
        Self {
            documents: DashMap::new(),
            input_docs: DashMap::new(),
            input_journal_seqs: DashMap::new(),
            input_uncompacted: DashMap::new(),
            db: Some(db),
            kernel_id: Some(kernel_id),
            default_workspace_id: Some(default_workspace_id),
            agent_id: RwLock::new(agent_id),
            block_flows: Some(block_flows),
            input_flows: None,
        }
    }

    /// Get a reference to the database handle, if one is configured.
    pub fn db(&self) -> Option<&DbHandle> {
        self.db.as_ref()
    }

    /// Get the FlowBus for typed pub/sub.
    pub fn block_flows(&self) -> Option<&SharedBlockFlowBus> {
        self.block_flows.as_ref()
    }

    /// Emit a block flow event if the bus is configured.
    fn emit(&self, flow: BlockFlow) {
        if let Some(bus) = &self.block_flows {
            bus.publish(flow);
        }
    }

    /// Get the current agent ID.
    pub fn agent_id(&self) -> PrincipalId {
        *self.agent_id.read()
    }

    /// Set the agent ID.
    pub fn set_agent_id(&self, agent_id: PrincipalId) {
        *self.agent_id.write() = agent_id;
    }

    /// Create a new document.
    ///
    /// Uses DashMap `entry()` for atomicity — the DB INSERT only runs in the
    /// `Vacant` branch, so concurrent callers can't race past the check.
    pub fn create_document(
        &self,
        context_id: ContextId,
        kind: DocKind,
        language: Option<String>,
    ) -> BlockStoreResult<()> {
        use dashmap::mapref::entry::Entry;

        match self.documents.entry(context_id) {
            Entry::Occupied(_) => Err(BlockStoreError::DocumentAlreadyExists(context_id)),
            Entry::Vacant(vacant) => {
                let agent_id = self.agent_id();

                // Persist metadata if we have a DB
                if let Some(db) = &self.db {
                    let db_guard = db.lock();
                    let row = DocumentRow {
                        document_id: context_id,
                        kernel_id: self.kernel_id.unwrap_or_default(),
                        workspace_id: self.default_workspace_id.unwrap_or_default(),
                        doc_kind: kind,
                        language: language.clone(),
                        path: None,
                        created_at: kaijutsu_types::now_millis() as i64,
                        created_by: agent_id,
                    };
                    match db_guard.insert_document(&row) {
                        Ok(()) => {}
                        Err(e)
                            if e.to_string().contains("UNIQUE constraint")
                                || e.to_string().contains("already exists") =>
                        {
                            tracing::warn!(context_id = %context_id.to_hex(), "Document already in DB but not in memory, recovering");
                        }
                        Err(e) => return Err(BlockStoreError::Db(e.to_string())),
                    }
                }

                let entry = DocumentEntry::new(context_id, kind, language, agent_id);
                vacant.insert(entry);

                Ok(())
            }
        }
    }

    /// Create a document from a serialized store snapshot (for sync from server).
    ///
    /// Reconstructs the document from a postcard-encoded `StoreSnapshot`.
    /// Used for initial sync when connecting to a kaijutsu-server.
    pub fn create_document_from_snapshot(
        &self,
        context_id: ContextId,
        kind: DocKind,
        language: Option<String>,
        snapshot_bytes: &[u8],
    ) -> BlockStoreResult<()> {
        if self.documents.contains_key(&context_id) {
            return Err(BlockStoreError::DocumentAlreadyExists(context_id));
        }

        let snapshot: StoreSnapshot = postcard::from_bytes(snapshot_bytes)
            .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;

        let agent_id = self.agent_id();
        let entry = DocumentEntry::from_store_snapshot(snapshot, kind, language, agent_id, 0, 0, 0)?;
        self.documents.insert(context_id, entry);

        Ok(())
    }

    /// Get a document for reading.
    pub fn get(
        &self,
        context_id: ContextId,
    ) -> Option<dashmap::mapref::one::Ref<'_, ContextId, DocumentEntry>> {
        self.documents.get(&context_id)
    }

    /// Current CRDT version for a context, or `DocumentNotFound` if the
    /// context is not resident. Prefer this over `get(..).map(|e| e.version())`
    /// when a missing document should be an error rather than silently
    /// collapsing to 0 — RPC acknowledgements, for example.
    pub fn version(&self, context_id: ContextId) -> BlockStoreResult<u64> {
        self.documents
            .get(&context_id)
            .map(|entry| entry.version())
            .ok_or(BlockStoreError::DocumentNotFound(context_id))
    }

    /// Get a document for writing.
    pub fn get_mut(
        &self,
        context_id: ContextId,
    ) -> Option<dashmap::mapref::one::RefMut<'_, ContextId, DocumentEntry>> {
        self.documents.get_mut(&context_id)
    }

    /// List all document IDs.
    pub fn list_ids(&self) -> Vec<ContextId> {
        self.documents.iter().map(|r| *r.key()).collect()
    }

    /// List document IDs filtered by kind.
    pub fn list_ids_by_kind(&self, kind: DocKind) -> Vec<ContextId> {
        self.documents
            .iter()
            .filter(|r| r.kind == kind)
            .map(|r| *r.key())
            .collect()
    }

    /// Check if a document exists.
    pub fn contains(&self, context_id: ContextId) -> bool {
        self.documents.contains_key(&context_id)
    }

    /// Delete a document.
    pub fn delete_document(&self, context_id: ContextId) -> BlockStoreResult<()> {
        if let Some(db) = &self.db {
            let db_guard = db.lock();
            db_guard
                .delete_document(context_id)
                .map_err(|e| BlockStoreError::Db(e.to_string()))?;
        }

        self.documents.remove(&context_id);

        Ok(())
    }

    /// Check if the store is empty.
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Fork a document, creating a copy with a new document ID.
    ///
    /// All blocks and their content are copied to the new document.
    /// The new document gets a fresh CRDT oplog.
    ///
    /// # Arguments
    ///
    /// * `source_id` - ID of the document to fork
    /// * `new_id` - ID for the forked document
    ///
    /// # Returns
    ///
    /// Ok(()) on success, Err if source not found or target exists.
    pub fn fork_document(&self, source_id: ContextId, new_id: ContextId) -> BlockStoreResult<()> {
        if self.documents.contains_key(&new_id) {
            return Err(BlockStoreError::DocumentAlreadyExists(new_id));
        }

        let source_entry = self
            .get(source_id)
            .ok_or(BlockStoreError::DocumentNotFound(source_id))?;

        let agent_id = self.agent_id();
        let forked_store = source_entry.doc.fork(new_id, agent_id);
        let kind = source_entry.kind;
        let language = source_entry.language.clone();
        drop(source_entry); // Release the read lock

        // Persist metadata if we have a DB
        if let Some(db) = &self.db {
            let db_guard = db.lock();
            let row = DocumentRow {
                document_id: new_id,
                kernel_id: self.kernel_id.unwrap_or_default(),
                workspace_id: self.default_workspace_id.unwrap_or_default(),
                doc_kind: kind,
                language: language.clone(),
                path: None,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: agent_id,
            };
            db_guard
                .insert_document(&row)
                .map_err(|e| BlockStoreError::Db(e.to_string()))?;
        }

        let version = forked_store.version();
        let entry = DocumentEntry {
            doc: forked_store,
            kind,
            language,
            version: AtomicU64::new(version),
            last_agent: RwLock::new(agent_id),
            sync_generation: AtomicU64::new(0),
            next_journal_seq: AtomicU64::new(0),
            uncompacted_count: AtomicU64::new(0),
            uncompacted_bytes: AtomicU64::new(0),
        };
        self.documents.insert(new_id, entry);
        self.write_initial_snapshot(new_id)?;

        Ok(())
    }

    /// Fork a document at a specific timestamp, creating a copy with only blocks up to that time.
    ///
    /// This creates a new document containing only blocks with `created_at <= before_timestamp`,
    /// useful for timeline branching and "what if" explorations.
    ///
    /// # Arguments
    ///
    /// * `source_id` - ID of the document to fork
    /// * `new_id` - ID for the forked document
    /// * `before_timestamp` - Only include blocks with `created_at` <= this value (wall-clock millis)
    ///
    /// # Returns
    ///
    /// Ok(()) on success, Err if source not found, target exists, or timestamp in the future.
    pub fn fork_document_at_version(
        &self,
        source_id: ContextId,
        new_id: ContextId,
        before_timestamp: u64,
    ) -> BlockStoreResult<()> {
        if self.documents.contains_key(&new_id) {
            return Err(BlockStoreError::DocumentAlreadyExists(new_id));
        }

        let source_entry = self
            .get(source_id)
            .ok_or(BlockStoreError::DocumentNotFound(source_id))?;

        // Validate timestamp is not in the future
        let now = kaijutsu_types::now_millis();
        if before_timestamp > now {
            return Err(BlockStoreError::Validation(format!(
                "Requested timestamp {} is in the future (now: {})",
                before_timestamp, now
            )));
        }

        let agent_id = self.agent_id();
        let forked_store = source_entry
            .doc
            .fork_at_version(new_id, agent_id, before_timestamp);
        let kind = source_entry.kind;
        let language = source_entry.language.clone();
        drop(source_entry); // Release the read lock

        // Persist metadata if we have a DB
        if let Some(db) = &self.db {
            let db_guard = db.lock();
            let row = DocumentRow {
                document_id: new_id,
                kernel_id: self.kernel_id.unwrap_or_default(),
                workspace_id: self.default_workspace_id.unwrap_or_default(),
                doc_kind: kind,
                language: language.clone(),
                path: None,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: agent_id,
            };
            db_guard
                .insert_document(&row)
                .map_err(|e| BlockStoreError::Db(e.to_string()))?;
        }

        let version = forked_store.version();
        let entry = DocumentEntry {
            doc: forked_store,
            kind,
            language,
            version: AtomicU64::new(version),
            last_agent: RwLock::new(agent_id),
            sync_generation: AtomicU64::new(0),
            next_journal_seq: AtomicU64::new(0),
            uncompacted_count: AtomicU64::new(0),
            uncompacted_bytes: AtomicU64::new(0),
        };
        self.documents.insert(new_id, entry);
        self.write_initial_snapshot(new_id)?;

        Ok(())
    }

    /// Fork a document at a specific timestamp with block filtering.
    ///
    /// Like [`fork_document_at_version`] but additionally filters blocks via `ForkBlockFilter`.
    /// Blocks that don't pass the filter are excluded from the fork.
    pub fn fork_document_filtered(
        &self,
        source_id: ContextId,
        new_id: ContextId,
        before_timestamp: u64,
        filter: &ForkBlockFilter,
    ) -> BlockStoreResult<()> {
        if self.documents.contains_key(&new_id) {
            return Err(BlockStoreError::DocumentAlreadyExists(new_id));
        }

        let source_entry = self
            .get(source_id)
            .ok_or(BlockStoreError::DocumentNotFound(source_id))?;

        // Validate timestamp is not in the future
        let now = kaijutsu_types::now_millis();
        if before_timestamp > now {
            return Err(BlockStoreError::Validation(format!(
                "Requested timestamp {} is in the future (now: {})",
                before_timestamp, now
            )));
        }

        let agent_id = self.agent_id();
        let forked_store =
            source_entry
                .doc
                .fork_filtered(new_id, agent_id, before_timestamp, filter);
        let kind = source_entry.kind;
        let language = source_entry.language.clone();
        drop(source_entry);

        // Persist metadata if we have a DB
        if let Some(db) = &self.db {
            let db_guard = db.lock();
            let row = DocumentRow {
                document_id: new_id,
                kernel_id: self.kernel_id.unwrap_or_default(),
                workspace_id: self.default_workspace_id.unwrap_or_default(),
                doc_kind: kind,
                language: language.clone(),
                path: None,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: agent_id,
            };
            db_guard
                .insert_document(&row)
                .map_err(|e| BlockStoreError::Db(e.to_string()))?;
        }

        let version = forked_store.version();
        let entry = DocumentEntry {
            doc: forked_store,
            kind,
            language,
            version: AtomicU64::new(version),
            last_agent: RwLock::new(agent_id),
            sync_generation: AtomicU64::new(0),
            next_journal_seq: AtomicU64::new(0),
            uncompacted_count: AtomicU64::new(0),
            uncompacted_bytes: AtomicU64::new(0),
        };
        self.documents.insert(new_id, entry);
        self.write_initial_snapshot(new_id)?;

        Ok(())
    }

    /// Get the number of documents.
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Get the last block ID in a document (for ordering new blocks at the end).
    pub fn last_block_id(&self, context_id: ContextId) -> Option<BlockId> {
        let entry = self.get(context_id)?;
        entry.doc.blocks_ordered().last().map(|b| b.id)
    }

    // =========================================================================
    // Block Operations
    // =========================================================================

    /// Journal an op to the append-only oplog.
    ///
    /// Serializes the SyncPayload, appends it to the `oplog` table, and
    /// triggers compaction if the uncompacted count or bytes exceed thresholds.
    fn journal_op(
        &self,
        context_id: ContextId,
        payload: SyncPayload,
    ) -> BlockStoreResult<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };

        let payload_bytes = postcard::to_allocvec(&payload)
            .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
        let payload_len = payload_bytes.len() as u64;

        let (seq, count, bytes) = {
            let entry = self
                .get(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let seq = entry.next_journal_seq.fetch_add(1, Ordering::SeqCst) + 1;
            let count = entry.uncompacted_count.fetch_add(1, Ordering::SeqCst) + 1;
            let bytes = entry
                .uncompacted_bytes
                .fetch_add(payload_len, Ordering::SeqCst)
                + payload_len;
            (seq, count, bytes)
        };

        {
            let db_guard = db.lock();
            db_guard
                .append_op(context_id, seq as i64, &payload_bytes)
                .map_err(|e| BlockStoreError::Db(e.to_string()))?;
        }

        if count >= COMPACTION_OP_THRESHOLD || bytes >= COMPACTION_BYTE_THRESHOLD {
            self.compact_document(context_id)?;
        }
        Ok(())
    }

    /// Run compaction: snapshot the current state and truncate the oplog.
    fn compact_document(&self, context_id: ContextId) -> BlockStoreResult<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };

        let (snapshot_bytes, content, version, max_seq) = {
            let entry = self
                .get(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let snapshot = entry.doc.snapshot();
            let content = entry.content();
            let version = entry.version() as i64;
            let max_seq = entry.next_journal_seq.load(Ordering::SeqCst);
            let snapshot_bytes = postcard::to_allocvec(&snapshot)
                .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            (snapshot_bytes, content, version, max_seq)
        };

        {
            let mut db_guard = db.lock();
            db_guard
                .write_snapshot_and_truncate(
                    context_id,
                    max_seq as i64,
                    version,
                    &snapshot_bytes,
                    &content,
                )
                .map_err(|e| BlockStoreError::Db(e.to_string()))?;
        }

        if let Some(entry) = self.get(context_id) {
            entry.uncompacted_count.store(0, Ordering::SeqCst);
            entry.uncompacted_bytes.store(0, Ordering::SeqCst);
        }

        Ok(())
    }

    /// Write an initial snapshot for a newly forked document (no oplog).
    fn write_initial_snapshot(&self, context_id: ContextId) -> BlockStoreResult<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };

        let entry = self
            .get(context_id)
            .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        let snapshot = entry.doc.snapshot();
        let content = entry.content();
        let version = entry.version() as i64;

        let snapshot_bytes = postcard::to_allocvec(&snapshot)
            .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;

        drop(entry);

        let mut db_guard = db.lock();
        db_guard
            .write_snapshot_and_truncate(context_id, 0, version, &snapshot_bytes, &content)
            .map_err(|e| BlockStoreError::Db(e.to_string()))?;

        Ok(())
    }

    /// Journal an input doc op and trigger compaction if needed.
    fn journal_and_maybe_compact_input(
        &self,
        context_id: ContextId,
        ops: &[u8],
    ) -> BlockStoreResult<()> {
        let Some(db) = &self.db else {
            return Ok(());
        };

        let seq_entry = self
            .input_journal_seqs
            .entry(context_id)
            .or_insert_with(|| AtomicU64::new(0));
        let seq = seq_entry.fetch_add(1, Ordering::SeqCst) + 1;

        let count_entry = self
            .input_uncompacted
            .entry(context_id)
            .or_insert_with(|| AtomicU64::new(0));
        let count = count_entry.fetch_add(1, Ordering::SeqCst) + 1;

        {
            let db_guard = db.lock();
            db_guard
                .append_input_op(context_id, seq as i64, ops)
                .map_err(|e| BlockStoreError::Db(e.to_string()))?;
        }

        if count >= INPUT_COMPACTION_OP_THRESHOLD {
            self.compact_input_doc(context_id)?;
        }
        Ok(())
    }

    /// Compact an input document: snapshot + truncate oplog.
    fn compact_input_doc(&self, context_id: ContextId) -> BlockStoreResult<()> {
        let Some(db) = &self.db else {
            return Ok(());
        };

        let (state_bytes, content, max_seq) = {
            let entry = self
                .input_docs
                .get(&context_id)
                .ok_or(BlockStoreError::InputDocNotFound(context_id))?;
            let state_bytes = entry.all_ops().map_err(BlockStoreError::Serialization)?;
            let content = entry.get_text();
            let max_seq = self
                .input_journal_seqs
                .get(&context_id)
                .map(|s| s.load(Ordering::SeqCst))
                .unwrap_or(0);
            (state_bytes, content, max_seq)
        };

        {
            let mut db_guard = db.lock();
            db_guard
                .write_input_snapshot_and_truncate(
                    context_id,
                    max_seq as i64,
                    &state_bytes,
                    &content,
                )
                .map_err(|e| BlockStoreError::Db(e.to_string()))?;
        }

        if let Some(count) = self.input_uncompacted.get(&context_id) {
            count.store(0, Ordering::SeqCst);
        }

        Ok(())
    }

    /// Insert a block into a document.
    ///
    /// This is the primary block creation API.
    ///
    /// # Arguments
    ///
    /// * `document_id` - The document to insert into
    /// * `parent_id` - Parent block ID for DAG relationship (None for root)
    /// * `after` - Block ID to insert after in document order (None for beginning)
    /// * `role` - Role of the block author (Human, Agent, System, Tool)
    /// * `kind` - Content type (Text, Thinking, ToolCall, ToolResult)
    /// * `content` - Initial text content
    pub fn insert_block(
        &self,
        context_id: ContextId,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        role: Role,
        kind: BlockKind,
        content: impl Into<String>,
        status: Status,
        content_type: ContentType,
    ) -> BlockStoreResult<BlockId> {
        self.insert_block_as(
            context_id, parent_id, after, role, kind, content, status, content_type, None,
        )
    }

    /// Insert a block with an explicit author identity.
    ///
    /// If `agent_id` is `Some`, the block will be stamped with that principal.
    /// If `None`, the store's default agent_id is used (backwards compat).
    pub fn insert_block_as(
        &self,
        context_id: ContextId,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        role: Role,
        kind: BlockKind,
        content: impl Into<String>,
        status: Status,
        content_type: ContentType,
        agent_id: Option<PrincipalId>,
    ) -> BlockStoreResult<BlockId> {
        let after_id = after.cloned();
        let (block_id, snapshot, ops, ops_bytes) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());

            // Set the agent for this operation so BlockId gets the right author
            entry.doc.set_agent_id(effective_agent);

            // Capture frontier before the operation for incremental ops.
            // Clients that are in sync can merge these directly.
            // Clients that are out of sync will get full oplog via get_document_state.
            let frontier_before = entry.doc.frontier();

            let block_id = entry
                .doc
                .insert_block(parent_id, after, role, kind, content, status, content_type)?;
            let snapshot = entry
                .doc
                .get_block_snapshot(&block_id)
                .ok_or(BlockStoreError::BlockNotFoundAfterInsert)?;

            // Send incremental ops (just this operation) for efficient sync.
            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = postcard::to_allocvec(&ops)
                .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            (block_id, snapshot, ops, ops_bytes)
        };
        self.journal_op(context_id, ops)?;

        // Emit flow event with creation ops
        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(snapshot),
            after_id,
            ops: Arc::from(ops_bytes),
            source: OpSource::Local,
        });

        Ok(block_id)
    }

    /// Insert a tool call block into a document.
    pub fn insert_tool_call(
        &self,
        context_id: ContextId,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        tool_name: impl Into<String>,
        tool_input: serde_json::Value,
        tool_kind: Option<ToolKind>,
    ) -> BlockStoreResult<BlockId> {
        self.insert_tool_call_as(
            context_id, parent_id, after, tool_name, tool_input, tool_kind, None, None, None,
        )
    }

    /// Insert a tool call block with an explicit author identity.
    ///
    /// `tool_use_id` is the LLM-assigned tool invocation ID (e.g., "toolu_01ABC...").
    /// Pass `Some(id)` when capturing from LLM stream events, `None` for shell/manual calls.
    ///
    /// `role` overrides the block role (default: `Role::Model`). Pass `Some(Role::User)`
    /// for human-initiated shell commands.
    pub fn insert_tool_call_as(
        &self,
        context_id: ContextId,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        tool_name: impl Into<String>,
        tool_input: serde_json::Value,
        tool_kind: Option<ToolKind>,
        agent_id: Option<PrincipalId>,
        tool_use_id: Option<String>,
        role: Option<Role>,
    ) -> BlockStoreResult<BlockId> {
        let after_id = after.cloned();
        let (block_id, snapshot, ops, ops_bytes) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);

            // Capture frontier before the operation for incremental ops
            let frontier_before = entry.doc.frontier();

            let block_id = entry
                .doc
                .insert_tool_call(parent_id, after, tool_name, tool_input, tool_kind, role)?;

            // Persist tool_use_id to BlockContent so it survives snapshot round-trips
            if let Some(ref tui) = tool_use_id {
                entry.doc.set_tool_use_id(&block_id, Some(tui.clone()))?;
            }

            let snapshot = entry
                .doc
                .get_block_snapshot(&block_id)
                .ok_or(BlockStoreError::BlockNotFoundAfterInsert)?;

            // Send incremental ops (just this operation) for efficient sync
            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = postcard::to_allocvec(&ops)
                .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            (block_id, snapshot, ops, ops_bytes)
        };
        self.journal_op(context_id, ops)?;

        // Emit flow event with creation ops
        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(snapshot),
            after_id,
            ops: Arc::from(ops_bytes),
            source: OpSource::Local,
        });

        Ok(block_id)
    }

    /// Insert a tool result block into a document.
    pub fn insert_tool_result(
        &self,
        context_id: ContextId,
        tool_call_id: &BlockId,
        after: Option<&BlockId>,
        content: impl Into<String>,
        is_error: bool,
        exit_code: Option<i32>,
        tool_kind: Option<ToolKind>,
    ) -> BlockStoreResult<BlockId> {
        self.insert_tool_result_as(
            context_id,
            tool_call_id,
            after,
            content,
            is_error,
            exit_code,
            tool_kind,
            None,
            None,
        )
    }

    /// Insert a tool result block with an explicit author identity.
    ///
    /// `tool_use_id` is the LLM-assigned tool invocation ID for correlating
    /// tool calls with results during hydration.
    pub fn insert_tool_result_as(
        &self,
        context_id: ContextId,
        tool_call_id: &BlockId,
        after: Option<&BlockId>,
        content: impl Into<String>,
        is_error: bool,
        exit_code: Option<i32>,
        tool_kind: Option<ToolKind>,
        agent_id: Option<PrincipalId>,
        tool_use_id: Option<String>,
    ) -> BlockStoreResult<BlockId> {
        let after_id = after.cloned();
        let (block_id, snapshot, ops, ops_bytes) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);

            // Capture frontier before the operation for incremental ops
            let frontier_before = entry.doc.frontier();

            let block_id = entry.doc.insert_tool_result_block(
                tool_call_id,
                after,
                content,
                is_error,
                exit_code,
                tool_kind,
            )?;

            // Persist tool_use_id to BlockContent so it survives snapshot round-trips
            if let Some(ref tui) = tool_use_id {
                entry.doc.set_tool_use_id(&block_id, Some(tui.clone()))?;
            }

            let snapshot = entry
                .doc
                .get_block_snapshot(&block_id)
                .ok_or(BlockStoreError::BlockNotFoundAfterInsert)?;

            // Send incremental ops (just this operation) for efficient sync
            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = postcard::to_allocvec(&ops)
                .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            (block_id, snapshot, ops, ops_bytes)
        };
        self.journal_op(context_id, ops)?;

        // Emit flow event with creation ops
        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(snapshot),
            after_id,
            ops: Arc::from(ops_bytes),
            source: OpSource::Local,
        });

        Ok(block_id)
    }

    /// Insert a block from a snapshot (used by drift flush and cross-context injection).
    ///
    /// The snapshot's ID is used as-is if the agent_id matches this store's agent,
    /// otherwise a new ID is assigned. Emits FlowBus events for real-time sync.
    pub fn insert_from_snapshot(
        &self,
        context_id: ContextId,
        snapshot: BlockSnapshot,
        after: Option<&BlockId>,
    ) -> BlockStoreResult<BlockId> {
        self.insert_from_snapshot_as(context_id, snapshot, after, None)
    }

    /// Insert a block from a snapshot with an explicit author identity.
    pub fn insert_from_snapshot_as(
        &self,
        context_id: ContextId,
        snapshot: BlockSnapshot,
        after: Option<&BlockId>,
        agent_id: Option<PrincipalId>,
    ) -> BlockStoreResult<BlockId> {
        let after_id = after.cloned();
        let (block_id, final_snapshot, ops, ops_bytes) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);

            let frontier_before = entry.doc.frontier();

            let block_id = entry.doc.insert_from_snapshot(snapshot, after)?;
            let final_snapshot = entry
                .doc
                .get_block_snapshot(&block_id)
                .ok_or(BlockStoreError::BlockNotFoundAfterInsert)?;

            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = postcard::to_allocvec(&ops)
                .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            (block_id, final_snapshot, ops, ops_bytes)
        };
        self.journal_op(context_id, ops)?;

        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(final_snapshot),
            after_id,
            ops: Arc::from(ops_bytes),
            source: OpSource::Local,
        });

        Ok(block_id)
    }

    /// Set the status of a block.
    pub fn set_status(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        status: Status,
    ) -> BlockStoreResult<()> {
        let ops = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let agent_id = self.agent_id();
            let frontier_before = entry.doc.frontier();
            entry.doc.set_status(block_id, status)?;
            entry.touch(agent_id);
            entry.doc.ops_since(&frontier_before)
        };
        self.journal_op(context_id, ops)?;

        // Emit flow event
        // Include output data if present — output is a struct field that can't
        // travel via DTE ops, so we piggyback it on StatusChanged
        let output = {
            let entry = self.get(context_id);
            entry
                .and_then(|e| e.doc.get_block_snapshot(block_id))
                .and_then(|s| s.output)
        };
        self.emit(BlockFlow::StatusChanged {
            context_id,
            block_id: *block_id,
            status,
            output,
            source: OpSource::Local,
        });

        // Validate content when a block transitions to Done with a rich content type.
        // This is the primary hook for kernel-side ABC/SVG validation — it runs once
        // when streaming completes, not on every keystroke.
        if status == Status::Done {
            let content_type = self
                .get(context_id)
                .and_then(|e| e.doc.get_block_snapshot(block_id))
                .map(|s| s.content_type);
            if matches!(content_type, Some(ContentType::Abc) | Some(ContentType::Svg)) {
                let _ = self.validate_content_and_attach_errors(context_id, block_id);
            }
        }

        Ok(())
    }

    /// Edit text within a block.
    pub fn edit_text(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        pos: usize,
        insert: &str,
        delete: usize,
    ) -> BlockStoreResult<()> {
        self.edit_text_as(context_id, block_id, pos, insert, delete, None)
    }

    /// Edit text within a block with an explicit author identity.
    pub fn edit_text_as(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        pos: usize,
        insert: &str,
        delete: usize,
        agent_id: Option<PrincipalId>,
    ) -> BlockStoreResult<()> {
        let (ops, ops_bytes) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);
            // Capture frontier before edit
            let frontier = entry.doc.frontier();
            entry.doc.edit_text(block_id, pos, insert, delete)?;
            entry.touch(effective_agent);
            // Get ops since frontier (the edit we just applied)
            let ops = entry.doc.ops_since(&frontier);
            let ops_bytes = postcard::to_allocvec(&ops)
                .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            (ops, ops_bytes)
        };
        self.journal_op(context_id, ops)?;

        // Emit CRDT ops for proper sync
        self.emit(BlockFlow::TextOps {
            context_id,
            block_id: *block_id,
            ops: Arc::from(ops_bytes),
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Set the ephemeral flag on a block (excluded from LLM hydration).
    pub fn set_ephemeral(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        ephemeral: bool,
    ) -> BlockStoreResult<()> {
        let ops = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let frontier_before = entry.doc.frontier();
            entry.doc.set_ephemeral(block_id, ephemeral)?;
            entry.touch(self.agent_id());
            entry.doc.ops_since(&frontier_before)
        };
        self.journal_op(context_id, ops)?;
        self.emit(BlockFlow::MetadataChanged {
            context_id,
            block_id: *block_id,
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Set the excluded flag on a block (user-curated exclusion during staging).
    pub fn set_excluded(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        excluded: bool,
    ) -> BlockStoreResult<()> {
        let ops = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let frontier_before = entry.doc.frontier();
            entry.doc.set_excluded(block_id, excluded)?;
            entry.touch(self.agent_id());
            entry.doc.ops_since(&frontier_before)
        };
        self.journal_op(context_id, ops)?;
        self.emit(BlockFlow::ExcludedChanged {
            context_id,
            block_id: *block_id,
            excluded,
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Set the content_type hint on a block (e.g., Markdown, Svg, Abc).
    pub fn set_content_type(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        content_type: ContentType,
    ) -> BlockStoreResult<()> {
        let ops = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let frontier_before = entry.doc.frontier();
            entry.doc.set_content_type(block_id, content_type)?;
            entry.touch(self.agent_id());
            entry.doc.ops_since(&frontier_before)
        };
        self.journal_op(context_id, ops)?;
        self.emit(BlockFlow::MetadataChanged {
            context_id,
            block_id: *block_id,
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Set structured output data on a block.
    ///
    /// Output data provides formatting information (tables, trees) for richer output.
    /// Emits `OutputChanged` flow event. Also piggybacked on `StatusChanged` for
    /// wire compat — see `set_status`.
    pub fn set_output(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        output: Option<&kaijutsu_types::OutputData>,
    ) -> BlockStoreResult<()> {
        let ops = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let agent_id = self.agent_id();
            let frontier_before = entry.doc.frontier();
            entry.doc.set_output(block_id, output.cloned())?;
            entry.touch(agent_id);
            entry.doc.ops_since(&frontier_before)
        };
        self.journal_op(context_id, ops)?;
        self.emit(BlockFlow::OutputChanged {
            context_id,
            block_id: *block_id,
            output: output.cloned(),
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Set the LLM-assigned tool invocation ID on a block.
    pub fn set_tool_use_id(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        tool_use_id: Option<String>,
    ) -> BlockStoreResult<()> {
        let ops = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let agent_id = self.agent_id();
            let frontier_before = entry.doc.frontier();
            entry.doc.set_tool_use_id(block_id, tool_use_id)?;
            entry.touch(agent_id);
            entry.doc.ops_since(&frontier_before)
        };
        self.journal_op(context_id, ops)?;
        self.emit(BlockFlow::MetadataChanged {
            context_id,
            block_id: *block_id,
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Append text to a block.
    pub fn append_text(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        text: &str,
    ) -> BlockStoreResult<()> {
        self.append_text_as(context_id, block_id, text, None)
    }

    /// Append text to a block with an explicit author identity.
    pub fn append_text_as(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        text: &str,
        agent_id: Option<PrincipalId>,
    ) -> BlockStoreResult<()> {
        let (ops, ops_bytes) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);
            // Capture frontier before append
            let frontier = entry.doc.frontier();
            entry.doc.append_text(block_id, text)?;
            entry.touch(effective_agent);
            // Get ops since frontier (the append we just applied)
            let ops = entry.doc.ops_since(&frontier);
            let ops_bytes = postcard::to_allocvec(&ops)
                .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            (ops, ops_bytes)
        };
        self.journal_op(context_id, ops)?;

        // Emit CRDT ops for proper sync
        self.emit(BlockFlow::TextOps {
            context_id,
            block_id: *block_id,
            ops: Arc::from(ops_bytes),
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Set collapsed state for a thinking block.
    pub fn set_collapsed(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        collapsed: bool,
    ) -> BlockStoreResult<()> {
        let ops = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let agent_id = self.agent_id();
            let frontier_before = entry.doc.frontier();
            entry.doc.set_collapsed(block_id, collapsed)?;
            entry.touch(agent_id);
            entry.doc.ops_since(&frontier_before)
        };
        self.journal_op(context_id, ops)?;

        // Emit flow event
        self.emit(BlockFlow::CollapsedChanged {
            context_id,
            block_id: *block_id,
            collapsed,
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Delete a block from a document.
    pub fn delete_block(&self, context_id: ContextId, block_id: &BlockId) -> BlockStoreResult<()> {
        let ops = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let agent_id = self.agent_id();
            let frontier_before = entry.doc.frontier();
            entry.doc.delete_block(block_id)?;
            entry.touch(agent_id);
            entry.doc.ops_since(&frontier_before)
        };
        self.journal_op(context_id, ops)?;

        // Emit flow event
        self.emit(BlockFlow::Deleted {
            context_id,
            block_id: *block_id,
            source: OpSource::Local,
        });

        Ok(())
    }

    // =========================================================================
    // Sync Operations
    // =========================================================================

    /// Get sync payload since a frontier for a document.
    pub fn ops_since(
        &self,
        context_id: ContextId,
        frontier: &HashMap<BlockId, Frontier>,
    ) -> BlockStoreResult<SyncPayload> {
        let entry = self
            .get(context_id)
            .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        Ok(entry.doc.ops_since(frontier))
    }

    /// Merge a sync payload into a document.
    pub fn merge_ops(&self, context_id: ContextId, payload: SyncPayload) -> BlockStoreResult<u64> {
        let (version, events, ops) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let before = entry.doc.blocks_ordered();
            let frontier_before = entry.doc.frontier();
            entry.doc.merge_ops(payload)?;
            let version = entry.doc.version();
            entry.version.store(version, Ordering::SeqCst);
            let after = entry.doc.blocks_ordered();
            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes =
                postcard::to_allocvec(&ops).unwrap_or_default();
            (
                version,
                Self::diff_block_events(context_id, &before, &after, ops_bytes),
                ops,
            )
        };
        for event in events {
            self.emit(event);
        }
        self.journal_op(context_id, ops)?;
        Ok(version)
    }

    /// Compare block snapshots before/after a merge and produce BlockFlow events.
    ///
    /// Detects new blocks (Inserted), removed blocks (Deleted), status changes,
    /// collapsed changes, and text changes. All events carry `OpSource::Remote`
    /// and share the same ops blob (CRDT dedup handles multiple merges).
    fn diff_block_events(
        context_id: ContextId,
        before: &[BlockSnapshot],
        after: &[BlockSnapshot],
        ops: Vec<u8>,
    ) -> Vec<BlockFlow> {
        use std::collections::HashMap;

        let before_map: HashMap<&BlockId, &BlockSnapshot> =
            before.iter().map(|b| (&b.id, b)).collect();
        let after_map: HashMap<&BlockId, &BlockSnapshot> =
            after.iter().map(|b| (&b.id, b)).collect();

        // Shared Arc for ops — all events from this diff share the same allocation
        let ops: Arc<[u8]> = Arc::from(ops);
        let mut events = Vec::new();

        // New blocks
        for (i, snap) in after.iter().enumerate() {
            if !before_map.contains_key(&snap.id) {
                let after_id = if i > 0 { Some(after[i - 1].id) } else { None };
                events.push(BlockFlow::Inserted {
                    context_id,
                    block: Arc::new(snap.clone()),
                    after_id,
                    ops: ops.clone(),
                    source: OpSource::Remote,
                });
            }
        }

        // Deleted blocks
        for snap in before {
            if !after_map.contains_key(&snap.id) {
                events.push(BlockFlow::Deleted {
                    context_id,
                    block_id: snap.id,
                    source: OpSource::Remote,
                });
            }
        }

        // Changes to existing blocks
        for snap in after {
            if let Some(old) = before_map.get(&snap.id) {
                if old.status != snap.status {
                    events.push(BlockFlow::StatusChanged {
                        context_id,
                        block_id: snap.id,
                        status: snap.status,
                        output: snap.output.clone(),
                        source: OpSource::Remote,
                    });
                }
                if old.collapsed != snap.collapsed {
                    events.push(BlockFlow::CollapsedChanged {
                        context_id,
                        block_id: snap.id,
                        collapsed: snap.collapsed,
                        source: OpSource::Remote,
                    });
                }
                if old.content != snap.content {
                    events.push(BlockFlow::TextOps {
                        context_id,
                        block_id: snap.id,
                        ops: ops.clone(),
                        source: OpSource::Remote,
                    });
                }
            }
        }

        events
    }

    /// Get the current frontier for a document (per-block frontiers).
    pub fn frontier(&self, context_id: ContextId) -> BlockStoreResult<HashMap<BlockId, Frontier>> {
        let entry = self
            .get(context_id)
            .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        Ok(entry.doc.frontier())
    }

    // =========================================================================
    // Query Operations
    // =========================================================================

    /// Get block snapshots for a document.
    pub fn block_snapshots(&self, context_id: ContextId) -> BlockStoreResult<Vec<BlockSnapshot>> {
        let entry = self
            .get(context_id)
            .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        Ok(entry.doc.blocks_ordered())
    }

    /// Get a single block snapshot by ID.
    pub fn get_block_snapshot(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
    ) -> BlockStoreResult<Option<BlockSnapshot>> {
        let entry = self
            .get(context_id)
            .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        Ok(entry.doc.get_block_snapshot(block_id))
    }

    /// Get multiple block snapshots by ID. Missing blocks are silently skipped.
    pub fn get_blocks_by_ids(
        &self,
        context_id: ContextId,
        ids: &[BlockId],
    ) -> BlockStoreResult<Vec<BlockSnapshot>> {
        let entry = self
            .get(context_id)
            .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(snap) = entry.doc.get_block_snapshot(id) {
                result.push(snap);
            }
        }
        Ok(result)
    }

    /// Query blocks matching a filter.
    ///
    /// If `filter.parent_id` is set, only descendants (up to `max_depth`) are considered.
    /// Otherwise iterates all blocks in order, applying the filter predicate.
    pub fn query_blocks(
        &self,
        context_id: ContextId,
        filter: &BlockFilter,
    ) -> BlockStoreResult<Vec<BlockSnapshot>> {
        let entry = self
            .get(context_id)
            .ok_or(BlockStoreError::DocumentNotFound(context_id))?;

        // If parent_id is set, compute descendant set via BFS
        let descendant_ids = if let Some(ref root_id) = filter.parent_id {
            Some(compute_descendants(&entry.doc, root_id, filter.max_depth))
        } else {
            None
        };

        let mut result = Vec::new();
        let limit = if filter.limit > 0 {
            filter.limit as usize
        } else {
            usize::MAX
        };

        for block in entry.doc.blocks_ordered() {
            // If we have a descendant set, check membership
            if let Some(ref descendants) = descendant_ids
                && !descendants.contains(&block.id)
            {
                continue;
            }

            if filter.matches(&block) {
                result.push(block);
                if result.len() >= limit {
                    break;
                }
            }
        }

        Ok(result)
    }

    /// Get CRDT sync state (serialized ops + version) without blocks.
    pub fn context_sync_state(&self, context_id: ContextId) -> BlockStoreResult<(Vec<u8>, u64)> {
        let entry = self
            .get(context_id)
            .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        let snapshot = entry.doc.snapshot();
        let bytes = postcard::to_allocvec(&snapshot)
            .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
        Ok((bytes, entry.version()))
    }

    /// Get the full text content of a document.
    pub fn get_content(&self, context_id: ContextId) -> BlockStoreResult<String> {
        let entry = self
            .get(context_id)
            .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        Ok(entry.content())
    }

    /// Get document metadata and version.
    pub fn get_document_state(
        &self,
        context_id: ContextId,
    ) -> BlockStoreResult<(DocumentKind, Option<String>, Vec<BlockSnapshot>, u64)> {
        let entry = self
            .get(context_id)
            .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        Ok((
            entry.kind,
            entry.language.clone(),
            entry.doc.blocks_ordered(),
            entry.version(),
        ))
    }

    // =========================================================================
    // Persistence
    // =========================================================================

    /// Load documents from database on startup.
    ///
    /// For each document, loads the latest compaction snapshot (if any) then
    /// replays oplog entries written after that snapshot.
    pub fn load_from_db(&self) -> BlockStoreResult<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        let db_guard = db.lock();
        let kernel_id = self
            .kernel_id
            .ok_or_else(|| BlockStoreError::Db("no kernel_id configured".into()))?;
        let docs = db_guard
            .list_documents(kernel_id)
            .map_err(|e| BlockStoreError::Db(e.to_string()))?;
        let agent_id = self.agent_id();

        for doc in docs {
            let context_id = doc.document_id;

            // Load base snapshot if available
            let (mut crdt_store, base_seq) = match db_guard.load_latest_snapshot(context_id) {
                Ok(Some(snap_row)) => {
                    match postcard::from_bytes::<StoreSnapshot>(&snap_row.state) {
                        Ok(store_snapshot) => {
                            tracing::debug!(
                                document_id = %context_id.to_hex(),
                                blocks = store_snapshot.blocks.len(),
                                snap_seq = snap_row.seq,
                                "Restored document from snapshot"
                            );
                            match CrdtBlockStore::from_snapshot(store_snapshot, agent_id) {
                                Ok(store) => (store, snap_row.seq),
                                Err(e) => {
                                    tracing::error!(document_id = %context_id.to_hex(), error = %e, "Failed to restore snapshot, skipping");
                                    continue;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(document_id = %context_id.to_hex(), error = %e, "Failed to deserialize snapshot, skipping");
                            continue;
                        }
                    }
                }
                Ok(None) => (CrdtBlockStore::new(context_id, agent_id), 0),
                Err(e) => {
                    tracing::error!(document_id = %context_id.to_hex(), error = %e, "Failed to load snapshot, skipping");
                    continue;
                }
            };

            // Replay oplog entries since the snapshot
            let oplog_entries = db_guard
                .load_oplog_since(context_id, base_seq)
                .map_err(|e| BlockStoreError::Db(e.to_string()))?;

            let mut max_seq = base_seq;
            let mut total_bytes: u64 = 0;
            for (seq, payload_bytes) in &oplog_entries {
                max_seq = max_seq.max(*seq);
                total_bytes += payload_bytes.len() as u64;
                match postcard::from_bytes::<SyncPayload>(payload_bytes) {
                    Ok(payload) => {
                        if let Err(e) = crdt_store.merge_ops(payload) {
                            tracing::error!(
                                document_id = %context_id.to_hex(),
                                seq = seq,
                                error = %e,
                                "Failed to replay oplog entry, halting replay"
                            );
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            document_id = %context_id.to_hex(),
                            seq = seq,
                            error = %e,
                            "Failed to deserialize oplog entry, halting replay"
                        );
                        break;
                    }
                }
            }

            if !oplog_entries.is_empty() {
                tracing::debug!(
                    document_id = %context_id.to_hex(),
                    replayed = oplog_entries.len(),
                    max_seq = max_seq,
                    "Replayed oplog entries"
                );
            }

            let version = crdt_store.version();
            let entry = DocumentEntry {
                doc: crdt_store,
                kind: doc.doc_kind,
                language: doc.language.clone(),
                version: AtomicU64::new(version),
                last_agent: RwLock::new(agent_id),
                sync_generation: AtomicU64::new(0),
                next_journal_seq: AtomicU64::new(max_seq as u64),
                uncompacted_count: AtomicU64::new(oplog_entries.len() as u64),
                uncompacted_bytes: AtomicU64::new(total_bytes),
            };

            self.documents.insert(context_id, entry);
        }

        Ok(())
    }

    /// Load a single document from the database into the in-memory store.
    ///
    /// Returns `true` if the document was loaded, `false` if it was already
    /// present or not found in the database. This is an explicit hydration
    /// path — not called automatically on `get()`.
    pub fn load_one_from_db(&self, context_id: ContextId) -> BlockStoreResult<bool> {
        use dashmap::mapref::entry::Entry;

        let db = self
            .db
            .as_ref()
            .ok_or(BlockStoreError::NoDatabaseConfigured)?;

        // Use entry() for atomicity — only proceed if the slot is vacant.
        let vacant = match self.documents.entry(context_id) {
            Entry::Occupied(_) => return Ok(false), // already loaded
            Entry::Vacant(v) => v,
        };

        let db_guard = db.lock();

        let doc = db_guard
            .get_document(context_id)
            .map_err(|e| BlockStoreError::Db(e.to_string()))?;

        let Some(doc) = doc else {
            return Ok(false);
        };

        let agent_id = self.agent_id();

        // Load base snapshot if available
        let (mut crdt_store, base_seq) = match db_guard.load_latest_snapshot(context_id) {
            Ok(Some(snap_row)) => {
                match postcard::from_bytes::<StoreSnapshot>(&snap_row.state) {
                    Ok(store_snapshot) => {
                        tracing::debug!(
                            document_id = %context_id.to_hex(),
                            blocks = store_snapshot.blocks.len(),
                            snap_seq = snap_row.seq,
                            "Hydrated document from snapshot"
                        );
                        match CrdtBlockStore::from_snapshot(store_snapshot, agent_id) {
                            Ok(store) => (store, snap_row.seq),
                            Err(e) => {
                                tracing::warn!(document_id = %context_id.to_hex(), error = %e, "Failed to restore snapshot");
                                return Ok(false);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(document_id = %context_id.to_hex(), error = %e, "Failed to deserialize snapshot");
                        return Ok(false);
                    }
                }
            }
            Ok(None) => (CrdtBlockStore::new(context_id, agent_id), 0),
            Err(e) => {
                tracing::warn!(document_id = %context_id.to_hex(), error = %e, "Failed to load snapshot");
                return Ok(false);
            }
        };

        // Replay oplog entries since the snapshot
        let oplog_entries = db_guard
            .load_oplog_since(context_id, base_seq)
            .map_err(|e| BlockStoreError::Db(e.to_string()))?;

        let mut max_seq = base_seq;
        let mut total_bytes: u64 = 0;
        for (seq, payload_bytes) in &oplog_entries {
            max_seq = max_seq.max(*seq);
            total_bytes += payload_bytes.len() as u64;
            match postcard::from_bytes::<SyncPayload>(payload_bytes) {
                Ok(payload) => {
                    if let Err(e) = crdt_store.merge_ops(payload) {
                        tracing::warn!(
                            document_id = %context_id.to_hex(),
                            seq = seq,
                            error = %e,
                            "Failed to replay oplog entry"
                        );
                        return Ok(false);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        document_id = %context_id.to_hex(),
                        seq = seq,
                        error = %e,
                        "Failed to deserialize oplog entry"
                    );
                    return Ok(false);
                }
            }
        }

        if !oplog_entries.is_empty() {
            tracing::debug!(
                document_id = %context_id.to_hex(),
                replayed = oplog_entries.len(),
                max_seq = max_seq,
                "Replayed oplog entries"
            );
        }

        let version = crdt_store.version();
        let entry = DocumentEntry {
            doc: crdt_store,
            kind: doc.doc_kind,
            language: doc.language.clone(),
            version: AtomicU64::new(version),
            last_agent: RwLock::new(agent_id),
            sync_generation: AtomicU64::new(0),
            next_journal_seq: AtomicU64::new(max_seq as u64),
            uncompacted_count: AtomicU64::new(oplog_entries.len() as u64),
            uncompacted_bytes: AtomicU64::new(total_bytes),
        };

        vacant.insert(entry);
        Ok(true)
    }

    // =========================================================================
    // Input Document Operations
    // =========================================================================

    /// Set the input document flow bus.
    pub fn set_input_flows(&mut self, bus: SharedInputDocFlowBus) {
        self.input_flows = Some(bus);
    }

    /// Get the input flow bus.
    pub fn input_flows(&self) -> Option<&SharedInputDocFlowBus> {
        self.input_flows.as_ref()
    }

    /// Emit an input doc flow event if the bus is configured.
    fn emit_input(&self, flow: InputDocFlow) {
        if let Some(bus) = &self.input_flows {
            bus.publish(flow);
        }
    }

    /// Create an input document for a context.
    ///
    /// Idempotent — returns Ok if the input doc already exists.
    /// The input doc is persisted to DB lazily when the first edit is journaled.
    pub fn create_input_doc(&self, context_id: ContextId) -> BlockStoreResult<()> {
        use dashmap::mapref::entry::Entry;

        match self.input_docs.entry(context_id) {
            Entry::Occupied(_) => Ok(()), // Already exists
            Entry::Vacant(vacant) => {
                let agent_id = self.agent_id();
                let entry = InputDocEntry::new(agent_id);
                vacant.insert(entry);
                Ok(())
            }
        }
    }

    /// Edit the input document for a context.
    ///
    /// Returns serialized ops for broadcasting.
    pub fn edit_input(
        &self,
        context_id: ContextId,
        pos: usize,
        insert: &str,
        delete: usize,
    ) -> BlockStoreResult<Vec<u8>> {
        let mut entry = self
            .input_docs
            .get_mut(&context_id)
            .ok_or(BlockStoreError::InputDocNotFound(context_id))?;

        let ops = entry
            .edit_text(pos, insert, delete)
            .map_err(BlockStoreError::Serialization)?;
        drop(entry);

        self.journal_and_maybe_compact_input(context_id, &ops)?;

        self.emit_input(InputDocFlow::TextOps {
            context_id,
            ops: Arc::from(ops.clone()),
            source: crate::flows::OpSource::Local,
        });

        Ok(ops)
    }

    /// Get the current input text for a context.
    pub fn get_input_text(&self, context_id: ContextId) -> BlockStoreResult<String> {
        let entry = self
            .input_docs
            .get(&context_id)
            .ok_or(BlockStoreError::InputDocNotFound(context_id))?;
        Ok(entry.get_text())
    }

    /// Get the full input document state (text + ops + version) for sync.
    pub fn get_input_state(
        &self,
        context_id: ContextId,
    ) -> BlockStoreResult<(String, Vec<u8>, u64)> {
        let entry = self
            .input_docs
            .get(&context_id)
            .ok_or(BlockStoreError::InputDocNotFound(context_id))?;
        let text = entry.get_text();
        let ops = entry.all_ops().map_err(BlockStoreError::Serialization)?;
        let version = entry.version();
        Ok((text, ops, version))
    }

    /// Get input ops since a frontier (for incremental sync).
    pub fn input_ops_since(
        &self,
        context_id: ContextId,
        frontier: &Frontier,
    ) -> BlockStoreResult<Vec<u8>> {
        let entry = self
            .input_docs
            .get(&context_id)
            .ok_or(BlockStoreError::InputDocNotFound(context_id))?;
        entry
            .ops_since(frontier)
            .map_err(BlockStoreError::Serialization)
    }

    /// Merge remote ops into an input document.
    pub fn merge_input_ops(
        &self,
        context_id: ContextId,
        ops_bytes: &[u8],
    ) -> BlockStoreResult<u64> {
        let mut entry = self
            .input_docs
            .get_mut(&context_id)
            .ok_or(BlockStoreError::InputDocNotFound(context_id))?;

        entry
            .merge_ops(ops_bytes)
            .map_err(BlockStoreError::Serialization)?;

        let version = entry.version();
        drop(entry);

        self.journal_and_maybe_compact_input(context_id, ops_bytes)?;

        self.emit_input(InputDocFlow::TextOps {
            context_id,
            ops: Arc::from(ops_bytes.to_vec()),
            source: crate::flows::OpSource::Remote,
        });

        Ok(version)
    }

    /// Clear the input document for a context.
    ///
    /// Returns the text that was in the input doc before clearing.
    pub fn clear_input(&self, context_id: ContextId) -> BlockStoreResult<String> {
        let mut entry = self
            .input_docs
            .get_mut(&context_id)
            .ok_or(BlockStoreError::InputDocNotFound(context_id))?;

        let (text, ops) = entry.clear().map_err(BlockStoreError::Serialization)?;
        drop(entry);

        self.emit_input(InputDocFlow::Cleared { context_id });

        if !ops.is_empty() {
            self.journal_and_maybe_compact_input(context_id, &ops)?;
        }

        Ok(text)
    }

    /// Load input documents from database on startup.
    ///
    /// Restores each input doc from its latest snapshot (if any) then replays
    /// oplog entries written after that snapshot.
    pub fn load_input_docs_from_db(&self) -> BlockStoreResult<()> {
        let db = self
            .db
            .as_ref()
            .ok_or(BlockStoreError::NoDatabaseConfigured)?;
        let db_guard = db.lock();
        let kernel_id = self
            .kernel_id
            .ok_or_else(|| BlockStoreError::Db("no kernel_id configured".into()))?;

        let doc_ids = db_guard
            .list_input_doc_ids(kernel_id)
            .map_err(|e| BlockStoreError::Db(e.to_string()))?;

        let agent_id = self.agent_id();

        for context_id in doc_ids {
            // Load base snapshot if available
            let (mut input_entry, base_seq) =
                match db_guard.load_latest_input_snapshot(context_id) {
                    Ok(Some(snap_row)) => {
                        match InputDocEntry::from_ops(&snap_row.state, agent_id) {
                            Ok(entry) => {
                                tracing::debug!(
                                    context_id = %context_id.to_hex(),
                                    snap_seq = snap_row.seq,
                                    "Restored input doc from snapshot"
                                );
                                (entry, snap_row.seq)
                            }
                            Err(e) => {
                                tracing::warn!(context_id = %context_id.to_hex(), error = %e, "Failed to restore input snapshot, creating empty");
                                (InputDocEntry::new(agent_id), 0)
                            }
                        }
                    }
                    Ok(None) => (InputDocEntry::new(agent_id), 0),
                    Err(e) => {
                        tracing::warn!(context_id = %context_id.to_hex(), error = %e, "Failed to load input snapshot, creating empty");
                        (InputDocEntry::new(agent_id), 0)
                    }
                };

            // Replay oplog entries since the snapshot
            let oplog_entries = db_guard
                .load_input_oplog_since(context_id, base_seq)
                .map_err(|e| BlockStoreError::Db(e.to_string()))?;

            let mut max_seq = base_seq;
            for (seq, payload_bytes) in &oplog_entries {
                max_seq = max_seq.max(*seq);
                if let Err(e) = input_entry.merge_ops(payload_bytes) {
                    tracing::warn!(
                        context_id = %context_id.to_hex(),
                        seq = seq,
                        error = %e,
                        "Failed to replay input oplog entry, skipping"
                    );
                }
            }

            if !oplog_entries.is_empty() {
                tracing::debug!(
                    context_id = %context_id.to_hex(),
                    replayed = oplog_entries.len(),
                    text_len = input_entry.get_text().len(),
                    "Replayed input oplog entries"
                );
            }

            self.input_docs.insert(context_id, input_entry);
            self.input_journal_seqs
                .insert(context_id, AtomicU64::new(max_seq as u64));
        }

        Ok(())
    }

    /// Insert a drift block into a document.
    ///
    /// Wraps `CrdtBlockStore::insert_drift_block()` with FlowBus emission,
    /// auto-save, and frontier tracking.
    pub fn insert_drift_block(
        &self,
        context_id: ContextId,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        content: impl Into<String>,
        source_context: ContextId,
        source_model: Option<String>,
        drift_kind: kaijutsu_crdt::DriftKind,
    ) -> BlockStoreResult<BlockId> {
        self.insert_drift_block_as(
            context_id,
            parent_id,
            after,
            content,
            source_context,
            source_model,
            drift_kind,
            None,
        )
    }

    /// Insert a drift block with an explicit author identity.
    pub fn insert_drift_block_as(
        &self,
        context_id: ContextId,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        content: impl Into<String>,
        source_context: ContextId,
        source_model: Option<String>,
        drift_kind: kaijutsu_crdt::DriftKind,
        agent_id: Option<PrincipalId>,
    ) -> BlockStoreResult<BlockId> {
        let after_id = after.cloned();
        let (block_id, snapshot, ops, ops_bytes) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);

            let frontier_before = entry.doc.frontier();

            let block_id = entry.doc.insert_drift_block(
                parent_id,
                after,
                content,
                source_context,
                source_model,
                drift_kind,
            )?;
            let snapshot = entry
                .doc
                .get_block_snapshot(&block_id)
                .ok_or(BlockStoreError::BlockNotFoundAfterInsert)?;

            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = postcard::to_allocvec(&ops)
                .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            (block_id, snapshot, ops, ops_bytes)
        };
        self.journal_op(context_id, ops)?;

        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(snapshot),
            after_id,
            ops: Arc::from(ops_bytes),
            source: OpSource::Local,
        });

        Ok(block_id)
    }

    /// Validate content and attach/update Error child blocks.
    ///
    /// Called when a block's status transitions to Done and its content_type
    /// is Abc or Svg. Runs the appropriate parser, compares results against
    /// existing Error children, and inserts/compacts to stay in sync.
    pub fn validate_content_and_attach_errors(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
    ) -> BlockStoreResult<()> {
        // Read the block snapshot
        let snap = {
            let entry = self
                .get(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            entry
                .doc
                .get_block_snapshot(block_id)
                .ok_or_else(|| BlockStoreError::BlockNotFoundAfterInsert)?
        };

        let new_errors = match snap.content_type {
            ContentType::Abc => validate_abc(&snap.content),
            ContentType::Svg => validate_svg(&snap.content),
            _ => return Ok(()),
        };

        // Get existing Error children of this block
        let existing_errors: Vec<BlockSnapshot> = {
            let entry = self
                .get(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            entry
                .doc
                .blocks_ordered()
                .into_iter()
                .filter(|b| {
                    b.kind == BlockKind::Error
                        && b.parent_id == Some(*block_id)
                        && !b.compacted
                })
                .collect()
        };

        // Dedup: compare new errors against existing by (code, line, message)
        let existing_keys: HashSet<(Option<&str>, u32, &str)> = existing_errors
            .iter()
            .filter_map(|b| {
                b.error.as_ref().map(|e| {
                    let line = e.span.as_ref().map(|s| s.line).unwrap_or(0);
                    (e.code.as_deref(), line, b.content.as_str())
                })
            })
            .collect();

        let new_keys: HashSet<(Option<&str>, u32, &str)> = new_errors
            .iter()
            .map(|(payload, summary)| {
                let line = payload.span.as_ref().map(|s| s.line).unwrap_or(0);
                (payload.code.as_deref(), line, summary.as_str())
            })
            .collect();

        // Delete stale errors (present in existing, absent in new)
        for existing in &existing_errors {
            if let Some(ref e) = existing.error {
                let line = e.span.as_ref().map(|s| s.line).unwrap_or(0);
                let key = (e.code.as_deref(), line, existing.content.as_str());
                if !new_keys.contains(&key) {
                    let _ = self.delete_block(context_id, &existing.id);
                }
            }
        }

        // Insert new errors (present in new, absent in existing)
        for (payload, summary) in &new_errors {
            let line = payload.span.as_ref().map(|s| s.line).unwrap_or(0);
            let key = (payload.code.as_deref(), line, summary.as_str());
            if !existing_keys.contains(&key) {
                let _ = self.insert_error_block_as(
                    context_id,
                    block_id,
                    payload,
                    summary.clone(),
                    Some(PrincipalId::system()),
                );
            }
        }

        Ok(())
    }

    /// Insert an error block attached to a parent.
    ///
    /// Wraps `CrdtBlockStore::insert_error_block()` with FlowBus emission,
    /// journal, and frontier tracking.
    pub fn insert_error_block_as(
        &self,
        context_id: ContextId,
        parent_id: &BlockId,
        payload: &kaijutsu_types::ErrorPayload,
        summary: impl Into<String>,
        agent_id: Option<PrincipalId>,
    ) -> BlockStoreResult<BlockId> {
        let (block_id, snapshot, ops, ops_bytes) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);

            let frontier_before = entry.doc.frontier();

            let block_id =
                entry
                    .doc
                    .insert_error_block(parent_id, Some(parent_id), payload, summary)?;
            let snapshot = entry
                .doc
                .get_block_snapshot(&block_id)
                .ok_or(BlockStoreError::BlockNotFoundAfterInsert)?;

            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = postcard::to_allocvec(&ops)
                .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            (block_id, snapshot, ops, ops_bytes)
        };
        self.journal_op(context_id, ops)?;

        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(snapshot),
            after_id: Some(*parent_id),
            ops: Arc::from(ops_bytes),
            source: OpSource::Local,
        });

        Ok(block_id)
    }
}

/// BFS from `root_id` collecting all descendant block IDs up to `max_depth` levels.
/// Depth 0 = unlimited. The root itself is included in the result set.
/// Validate ABC notation content, returning ErrorPayloads for each diagnostic.
fn validate_abc(content: &str) -> Vec<(kaijutsu_types::ErrorPayload, String)> {
    let result = kaijutsu_abc::parse(content);
    result
        .feedback
        .into_iter()
        .filter(|f| {
            matches!(
                f.level,
                kaijutsu_abc::feedback::FeedbackLevel::Error
                    | kaijutsu_abc::feedback::FeedbackLevel::Warning
            )
        })
        .map(|f| {
            let severity = match f.level {
                kaijutsu_abc::feedback::FeedbackLevel::Error => {
                    kaijutsu_types::ErrorSeverity::Error
                }
                _ => kaijutsu_types::ErrorSeverity::Warning,
            };
            let summary = if let Some(ref suggestion) = f.suggestion {
                format!("{} (hint: {})", f.message, suggestion)
            } else {
                f.message.clone()
            };
            let payload = kaijutsu_types::ErrorPayload {
                category: kaijutsu_types::ErrorCategory::Parse,
                severity,
                code: None,
                detail: Some(f.message),
                span: Some(kaijutsu_types::ErrorSpan {
                    line: f.line as u32,
                    column: f.column as u32,
                    length: f
                        .span
                        .map(|(start, end)| (end - start) as u32)
                        .unwrap_or(0),
                }),
                source_kind: Some(BlockKind::Text),
            };
            (payload, summary)
        })
        .collect()
}

/// Validate SVG content via usvg, returning ErrorPayloads on failure.
fn validate_svg(content: &str) -> Vec<(kaijutsu_types::ErrorPayload, String)> {
    // Wrap in catch_unwind to prevent parser panics from killing the kernel
    let result = std::panic::catch_unwind(|| {
        usvg::Tree::from_str(content, &usvg::Options::default())
    });
    match result {
        Ok(Ok(_)) => vec![],
        Ok(Err(e)) => {
            let summary = format!("SVG parse error: {}", e);
            let payload = kaijutsu_types::ErrorPayload {
                category: kaijutsu_types::ErrorCategory::Parse,
                severity: kaijutsu_types::ErrorSeverity::Error,
                code: None,
                detail: Some(e.to_string()),
                span: None,
                source_kind: Some(BlockKind::Text),
            };
            vec![(payload, summary)]
        }
        Err(_panic) => {
            let summary = "SVG validator panicked (malformed input)".to_string();
            let payload = kaijutsu_types::ErrorPayload {
                category: kaijutsu_types::ErrorCategory::Kernel,
                severity: kaijutsu_types::ErrorSeverity::Fatal,
                code: Some("svg.validator_panic".into()),
                detail: Some("usvg::Tree::from_str panicked — the SVG content is likely severely malformed".into()),
                span: None,
                source_kind: Some(BlockKind::Text),
            };
            vec![(payload, summary)]
        }
    }
}

fn compute_descendants(
    doc: &CrdtBlockStore,
    root_id: &BlockId,
    max_depth: u32,
) -> HashSet<BlockId> {
    let mut result = HashSet::new();
    let mut queue = VecDeque::new();
    queue.push_back((*root_id, 0u32));
    result.insert(*root_id);

    while let Some((current, depth)) = queue.pop_front() {
        if max_depth > 0 && depth >= max_depth {
            continue;
        }
        for child_id in doc.get_children(&current) {
            if result.insert(child_id) {
                queue.push_back((child_id, depth + 1));
            }
        }
    }
    result
}

impl Default for BlockStore {
    fn default() -> Self {
        Self::new(PrincipalId::system())
    }
}

/// Thread-safe handle to a BlockStore.
/// With DashMap, the store itself doesn't need RwLock.
pub type SharedBlockStore = Arc<BlockStore>;

/// Create a new shared block store.
pub fn shared_block_store(agent_id: PrincipalId) -> SharedBlockStore {
    Arc::new(BlockStore::new(agent_id))
}

/// Create a shared block store with database persistence.
pub fn shared_block_store_with_db(
    db: DbHandle,
    kernel_id: KernelId,
    default_workspace_id: WorkspaceId,
    agent_id: PrincipalId,
) -> SharedBlockStore {
    Arc::new(BlockStore::with_db(
        db,
        kernel_id,
        default_workspace_id,
        agent_id,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_agent() -> PrincipalId {
        PrincipalId::new()
    }

    #[test]
    fn test_version_errors_on_missing_context() {
        // Regression for silent-0 acks: BlockStore::version must surface an
        // error for a missing context, not collapse to 0 the way the old
        // `get(ctx).map(|e| e.version()).unwrap_or(0)` pattern did.
        let store = BlockStore::new(test_agent());
        let missing = ContextId::new();
        match store.version(missing) {
            Err(BlockStoreError::DocumentNotFound(id)) => assert_eq!(id, missing),
            other => panic!("expected DocumentNotFound, got {:?}", other),
        }
    }

    #[test]
    fn test_version_monotonic_across_mutations() {
        // After creation, version starts at a baseline; each block insert
        // bumps it. Used by RPC acks so clients can track sync state.
        let store = BlockStore::new(test_agent());
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();
        let v0 = store.version(ctx).unwrap();
        store
            .insert_block(
                ctx,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "first",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let v1 = store.version(ctx).unwrap();
        assert!(v1 > v0, "version should advance after insert (v0={}, v1={})", v0, v1);
        store
            .insert_block(
                ctx,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "second",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let v2 = store.version(ctx).unwrap();
        assert!(v2 > v1, "version should advance again (v1={}, v2={})", v1, v2);
    }

    #[test]
    fn test_block_store_basic_ops() {
        let store = BlockStore::new(test_agent());
        let ctx = ContextId::new();

        store
            .create_document(ctx, DocumentKind::Code, Some("rust".into()))
            .unwrap();

        // Insert a text block using new API
        let block_id = store
            .insert_block(
                ctx,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "hello world",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        assert_eq!(store.get_content(ctx).unwrap(), "hello world");

        // Append to the block
        store.append_text(ctx, &block_id, "!").unwrap();
        assert_eq!(store.get_content(ctx).unwrap(), "hello world!");

        // Edit the block
        store.edit_text(ctx, &block_id, 6, "rust ", 0).unwrap();
        assert_eq!(store.get_content(ctx).unwrap(), "hello rust world!");
    }

    #[test]
    fn test_block_store_multiple_blocks() {
        let store = BlockStore::new(test_agent());
        let ctx = ContextId::new();

        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        // Insert thinking block
        let thinking_id = store
            .insert_block(
                ctx,
                None,
                None,
                Role::Model,
                BlockKind::Thinking,
                "Let me think...",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Insert text block after thinking (as child of root, after thinking in order)
        let text_id = store
            .insert_block(
                ctx,
                None,
                Some(&thinking_id),
                Role::Model,
                BlockKind::Text,
                "Here's my answer",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Should have both blocks
        let content = store.get_content(ctx).unwrap();
        assert!(content.contains("Let me think..."));
        assert!(content.contains("Here's my answer"));

        // Collapse thinking
        store.set_collapsed(ctx, &thinking_id, true).unwrap();

        // Delete text block
        store.delete_block(ctx, &text_id).unwrap();
        let content = store.get_content(ctx).unwrap();
        assert!(content.contains("Let me think..."));
        assert!(!content.contains("Here's my answer"));
    }

    #[test]
    fn test_block_store_crud() {
        let store = BlockStore::new(test_agent());
        let ctx = ContextId::new();

        store
            .create_document(ctx, DocumentKind::Code, Some("rust".into()))
            .unwrap();

        store
            .insert_block(
                ctx,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "fn main() {}",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        assert_eq!(store.get_content(ctx).unwrap(), "fn main() {}");

        store.delete_document(ctx).unwrap();
        assert!(store.get(ctx).is_none());
    }

    #[test]
    fn test_list_ids_by_kind() {
        let store = BlockStore::new(test_agent());
        let conv1 = ContextId::new();
        let conv2 = ContextId::new();
        let code1 = ContextId::new();
        let config1 = ContextId::new();

        store
            .create_document(conv1, DocumentKind::Conversation, None)
            .unwrap();
        store
            .create_document(conv2, DocumentKind::Conversation, None)
            .unwrap();
        store
            .create_document(code1, DocumentKind::Code, Some("rust".into()))
            .unwrap();
        store
            .create_document(config1, DocumentKind::Config, None)
            .unwrap();

        assert_eq!(store.list_ids().len(), 4);

        let convs = store.list_ids_by_kind(DocumentKind::Conversation);
        assert_eq!(convs.len(), 2);
        assert!(convs.contains(&conv1));
        assert!(convs.contains(&conv2));

        let codes = store.list_ids_by_kind(DocumentKind::Code);
        assert_eq!(codes.len(), 1);
        assert!(codes.contains(&code1));

        let configs = store.list_ids_by_kind(DocumentKind::Config);
        assert_eq!(configs.len(), 1);
        assert!(configs.contains(&config1));

        let texts = store.list_ids_by_kind(DocumentKind::Text);
        assert!(texts.is_empty());
    }

    #[test]
    fn test_block_snapshots() {
        let store = BlockStore::new(test_agent());
        let ctx = ContextId::new();

        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let thinking_id = store
            .insert_block(
                ctx,
                None,
                None,
                Role::Model,
                BlockKind::Thinking,
                "thinking...",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        store
            .insert_block(
                ctx,
                None,
                Some(&thinking_id),
                Role::Model,
                BlockKind::Text,
                "response",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let snapshots = store.block_snapshots(ctx).unwrap();
        assert_eq!(snapshots.len(), 2);

        // Check snapshot types using new flat struct
        let mut has_thinking = false;
        let mut has_text = false;

        for snapshot in &snapshots {
            match snapshot.kind {
                BlockKind::Thinking => {
                    assert_eq!(snapshot.content, "thinking...");
                    has_thinking = true;
                }
                BlockKind::Text => {
                    assert_eq!(snapshot.content, "response");
                    has_text = true;
                }
                _ => {}
            }
        }

        assert!(has_thinking, "Expected a thinking block");
        assert!(has_text, "Expected a text block");
    }

    #[test]
    fn test_set_status() {
        let store = BlockStore::new(test_agent());
        let ctx = ContextId::new();

        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let block_id = store
            .insert_block(
                ctx,
                None,
                None,
                Role::Model,
                BlockKind::ToolCall,
                "{}",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Set status to Running
        store.set_status(ctx, &block_id, Status::Running).unwrap();

        let snapshots = store.block_snapshots(ctx).unwrap();
        assert_eq!(snapshots[0].status, Status::Running);

        // Set status to Done
        store.set_status(ctx, &block_id, Status::Done).unwrap();

        let snapshots = store.block_snapshots(ctx).unwrap();
        assert_eq!(snapshots[0].status, Status::Done);
    }

    #[tokio::test]
    async fn test_concurrent_document_access() {
        use std::sync::Arc;
        use tokio::task::JoinSet;

        let store = Arc::new(BlockStore::new(test_agent()));
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Code, None)
            .unwrap();

        let mut tasks = JoinSet::new();
        let num_tasks = 4;
        let ops_per_task = 10;

        // Spawn multiple tasks that concurrently insert blocks to the same document
        for i in 0..num_tasks {
            let store_clone = Arc::clone(&store);
            tasks.spawn(async move {
                for j in 0..ops_per_task {
                    // Each task inserts a uniquely identifiable block
                    let text = format!("[task-{}-op-{}]", i, j);
                    let _ = store_clone.insert_block(
                        ctx,
                        None,
                        None,
                        Role::User,
                        BlockKind::Text,
                        &text,
                        Status::Done,
                        ContentType::Plain,
                    );
                }
            });
        }

        // Wait for all tasks to complete
        while let Some(result) = tasks.join_next().await {
            result.expect("Task panicked");
        }

        // Verify the document has content from all tasks
        let content = store.get_content(ctx).unwrap();

        // Should have at least some content (exact ordering is non-deterministic)
        assert!(!content.is_empty());

        // Count how many blocks we have - should be num_tasks * ops_per_task
        let snapshots = store.block_snapshots(ctx).unwrap();
        assert_eq!(
            snapshots.len(),
            num_tasks * ops_per_task,
            "Expected {} blocks, got {}",
            num_tasks * ops_per_task,
            snapshots.len()
        );
    }

    #[tokio::test]
    async fn test_concurrent_multi_document_access() {
        use std::sync::Arc;
        use tokio::task::JoinSet;

        let store = Arc::new(BlockStore::new(test_agent()));

        // Create multiple documents
        let num_docs = 3;
        let doc_ids: Vec<ContextId> = (0..num_docs).map(|_| ContextId::new()).collect();
        for &ctx in &doc_ids {
            store
                .create_document(ctx, DocumentKind::Code, None)
                .unwrap();
        }

        let mut tasks = JoinSet::new();
        let num_tasks = 6;

        // Each task works on different documents
        for i in 0..num_tasks {
            let store_clone = Arc::clone(&store);
            let ctx = doc_ids[i % num_docs];
            tasks.spawn(async move {
                for j in 0..5 {
                    let text = format!("task-{}-op-{}", i, j);
                    let _ = store_clone.insert_block(
                        ctx,
                        None,
                        None,
                        Role::User,
                        BlockKind::Text,
                        &text,
                        Status::Done,
                        ContentType::Plain,
                    );
                }
            });
        }

        // Wait for all tasks
        while let Some(result) = tasks.join_next().await {
            result.expect("Task panicked");
        }

        // Each document should have content
        for &ctx in &doc_ids {
            let content = store.get_content(ctx).unwrap();
            assert!(
                !content.is_empty(),
                "Document {} should have content",
                ctx.to_hex()
            );
        }
    }

    #[tokio::test]
    async fn test_concurrent_read_write() {
        use std::sync::Arc;
        use tokio::task::JoinSet;

        let store = Arc::new(BlockStore::new(test_agent()));
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Code, None)
            .unwrap();

        // Insert initial content
        let block_id = store
            .insert_block(
                ctx,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "initial content",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let mut tasks = JoinSet::new();

        // Spawn writer tasks
        for i in 0..3 {
            let store_clone = Arc::clone(&store);
            let bid = block_id;
            tasks.spawn(async move {
                for j in 0..5 {
                    // Append text
                    let text = format!(" [w{}:{}]", i, j);
                    let _ = store_clone.append_text(ctx, &bid, &text);
                }
            });
        }

        // Spawn reader tasks
        for _ in 0..3 {
            let store_clone = Arc::clone(&store);
            tasks.spawn(async move {
                for _ in 0..10 {
                    // Read content
                    let _ = store_clone.get_content(ctx);
                }
            });
        }

        // Wait for all tasks
        while let Some(result) = tasks.join_next().await {
            result.expect("Task panicked");
        }

        // Content should still be valid
        let content = store.get_content(ctx).unwrap();
        assert!(content.starts_with("initial content"));
    }

    // ============================================================================
    // SYNC PAYLOAD TESTS
    // ============================================================================
    //
    // These tests verify SyncPayload-based sync:
    // - Server sends incremental SyncPayload for block insertions
    // - Client (CrdtBlockStore) can merge these payloads after initial snapshot sync

    use crate::flows::{BlockFlow, FlowBus, SharedBlockFlowBus};
    use std::sync::Arc;

    /// Helper to create a BlockStore with FlowBus for testing.
    fn store_with_flows() -> (BlockStore, SharedBlockFlowBus) {
        let bus: SharedBlockFlowBus = Arc::new(FlowBus::new(256));
        let store = BlockStore::with_flows(test_agent(), bus.clone());
        (store, bus)
    }

    /// Test that insert_block emits SyncPayload that can be merged by a client store.
    #[tokio::test]
    async fn test_insert_block_emits_sync_payload() {
        let (store, bus) = store_with_flows();
        let mut sub = bus.subscribe("block.>");
        let ctx = ContextId::new();

        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        // Client syncs from snapshot
        let snapshot = store.get(ctx).unwrap().doc.snapshot();
        let mut client = CrdtBlockStore::from_snapshot(snapshot, PrincipalId::new()).unwrap();
        assert_eq!(client.block_count(), 0);

        // Server inserts a block
        let block_id = store
            .insert_block(
                ctx,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Hello from server",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Get the BlockInserted event with ops
        let msg = sub.try_recv().expect("should receive BlockInserted event");
        let ops = match msg.payload {
            BlockFlow::Inserted { ops, .. } => ops,
            _ => panic!("expected BlockInserted event"),
        };

        // Deserialize SyncPayload and merge on client
        let payload: SyncPayload =
            postcard::from_bytes(&ops).expect("should deserialize SyncPayload");
        client
            .merge_ops(payload)
            .expect("client should merge sync payload");

        // Verify client has the block
        assert_eq!(client.block_count(), 1);
        let snapshot = client
            .get_block_snapshot(&block_id)
            .expect("block should exist on client");
        assert_eq!(snapshot.content, "Hello from server");
    }

    /// Test that insert_tool_call emits mergeable SyncPayload.
    #[tokio::test]
    async fn test_insert_tool_call_emits_sync_payload() {
        let (store, bus) = store_with_flows();
        let mut sub = bus.subscribe("block.>");
        let ctx = ContextId::new();

        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let snapshot = store.get(ctx).unwrap().doc.snapshot();
        let mut client = CrdtBlockStore::from_snapshot(snapshot, PrincipalId::new()).unwrap();

        let block_id = store
            .insert_tool_call(
                ctx,
                None,
                None,
                "bash",
                serde_json::json!({"command": "ls -la"}),
                None,
            )
            .unwrap();

        let msg = sub.try_recv().expect("should receive event");
        let ops = match msg.payload {
            BlockFlow::Inserted { ops, .. } => ops,
            _ => panic!("expected BlockInserted"),
        };

        let payload: SyncPayload = postcard::from_bytes(&ops).unwrap();
        client
            .merge_ops(payload)
            .expect("should merge tool_call sync payload");

        let snapshot = client.get_block_snapshot(&block_id).unwrap();
        assert_eq!(snapshot.kind, BlockKind::ToolCall);
        assert_eq!(snapshot.tool_name.as_deref(), Some("bash"));
    }

    /// Test multiple sequential block inserts all produce mergeable SyncPayloads.
    #[tokio::test]
    async fn test_multiple_incremental_syncs() {
        let (store, bus) = store_with_flows();
        let mut sub = bus.subscribe("block.>");
        let ctx = ContextId::new();

        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let snapshot = store.get(ctx).unwrap().doc.snapshot();
        let mut client = CrdtBlockStore::from_snapshot(snapshot, PrincipalId::new()).unwrap();

        for i in 0..5 {
            let _ = store
                .insert_block(
                    ctx,
                    None,
                    None,
                    Role::User,
                    BlockKind::Text,
                    format!("Message {}", i),
                    Status::Done,
                    ContentType::Plain,
                )
                .unwrap();

            let msg = sub.try_recv().expect("should receive event");
            let ops = match msg.payload {
                BlockFlow::Inserted { ops, .. } => ops,
                _ => panic!("expected BlockInserted"),
            };

            let payload: SyncPayload = postcard::from_bytes(&ops).unwrap();
            client
                .merge_ops(payload)
                .expect(&format!("should merge block {i}"));
        }

        assert_eq!(client.block_count(), 5);

        let server_blocks = store.block_snapshots(ctx).unwrap();
        let client_blocks = client.blocks_ordered();
        for (sb, cb) in server_blocks.iter().zip(client_blocks.iter()) {
            assert_eq!(sb.content, cb.content);
        }
    }

    /// Test that text streaming (append_text) produces mergeable SyncPayload.
    #[tokio::test]
    async fn test_text_streaming_sync_payload() {
        let (store, bus) = store_with_flows();
        let mut sub = bus.subscribe("block.>");
        let ctx = ContextId::new();

        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let block_id = store
            .insert_block(
                ctx,
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let _ = sub.try_recv(); // drain insert event

        // Sync via proper protocol: ops_since with empty frontier sends full DTE
        // ops for new blocks, establishing shared causal history on the client.
        let mut client = CrdtBlockStore::new(ctx, PrincipalId::new());
        let initial_payload = store.ops_since(ctx, &HashMap::new()).unwrap();
        client.merge_ops(initial_payload).expect("initial sync");
        assert_eq!(client.block_count(), 1);

        let chunks = ["Hello", " ", "World", "!"];
        for chunk in chunks {
            let client_frontier = client.frontier();
            store.append_text(ctx, &block_id, chunk).unwrap();

            let msg = sub.try_recv().expect("should receive event");
            match msg.payload {
                BlockFlow::TextOps { .. } => {}
                _ => panic!("expected TextOps event, got {:?}", msg.payload),
            }

            // Use frontier-based sync for incremental ops
            let payload = store.ops_since(ctx, &client_frontier).unwrap();
            client
                .merge_ops(payload)
                .expect(&format!("should merge chunk '{chunk}'"));
        }

        let snapshot = client.get_block_snapshot(&block_id).unwrap();
        assert_eq!(snapshot.content, "Hello World!");
    }

    /// Test that merge_ops emits BlockFlow events for new blocks.
    #[tokio::test]
    async fn test_merge_ops_emits_inserted_event() {
        let (server, bus) = store_with_flows();
        let mut sub = bus.subscribe("block.>");
        let ctx = ContextId::new();

        server
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        // Snapshot before insert
        let initial_snapshot = {
            let entry = server.get(ctx).unwrap();
            postcard::to_allocvec(&entry.doc.snapshot()).unwrap()
        };

        let frontier_before = server.frontier(ctx).unwrap();

        let _block_id = server
            .insert_block(
                ctx,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "hello from remote",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let msg = sub
            .try_recv()
            .expect("should get Inserted from insert_block");
        assert!(matches!(msg.payload, BlockFlow::Inserted { .. }));

        let ops = server.ops_since(ctx, &frontier_before).unwrap();

        // Create receiver from initial snapshot
        let (receiver, recv_bus) = store_with_flows();
        let mut recv_sub = recv_bus.subscribe("block.>");

        receiver
            .create_document_from_snapshot(ctx, DocumentKind::Conversation, None, &initial_snapshot)
            .unwrap();

        receiver.merge_ops(ctx, ops).unwrap();

        let msg = recv_sub
            .try_recv()
            .expect("merge_ops should emit Inserted event");
        match msg.payload {
            BlockFlow::Inserted {
                context_id, block, ..
            } => {
                assert_eq!(context_id, ctx);
                assert_eq!(block.content, "hello from remote");
                assert_eq!(block.kind, BlockKind::Text);
            }
            other => panic!("expected Inserted, got {:?}", other),
        }
    }

    /// Test that merge_ops emits StatusChanged and TextOps events for existing blocks.
    #[tokio::test]
    async fn test_merge_ops_emits_status_and_text_events() {
        let ctx = ContextId::new();

        let (server_a, _) = store_with_flows();
        server_a
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let block_id = server_a
            .insert_block(
                ctx,
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "initial",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Receiver syncs via proper protocol: empty document + ops_since
        let (receiver, recv_bus) = store_with_flows();
        receiver
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();
        let initial_ops = server_a.ops_since(ctx, &HashMap::new()).unwrap();
        receiver.merge_ops(ctx, initial_ops).unwrap();

        let recv_frontier = receiver.frontier(ctx).unwrap();

        // Modify block on A
        server_a
            .set_status(ctx, &block_id, Status::Running)
            .unwrap();
        server_a
            .edit_text(ctx, &block_id, 7, " content", 0)
            .unwrap();

        // Compute diff — frontier types differ (per-block vs per-block), but both stores
        // use HashMap<BlockId, Frontier>, so we can pass receiver's frontier to server A
        let diff_ops = server_a.ops_since(ctx, &recv_frontier).unwrap();

        let mut recv_sub = recv_bus.subscribe("block.>");
        receiver.merge_ops(ctx, diff_ops).unwrap();

        let mut events = Vec::new();
        while let Some(msg) = recv_sub.try_recv() {
            events.push(msg.payload);
        }

        let has_status = events.iter().any(|e| {
            matches!(
                e,
                BlockFlow::StatusChanged {
                    status: Status::Running,
                    ..
                }
            )
        });
        let has_text = events
            .iter()
            .any(|e| matches!(e, BlockFlow::TextOps { .. }));

        assert!(has_status, "should emit StatusChanged, got: {:?}", events);
        assert!(has_text, "should emit TextOps, got: {:?}", events);
    }

    /// Integration test: stream → finalize → verify content preserved.
    #[tokio::test]
    async fn test_streaming_lifecycle() {
        let (store, _bus) = store_with_flows();
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let block_id = store
            .insert_block(
                ctx,
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        store.set_status(ctx, &block_id, Status::Running).unwrap();

        let streaming_text = "The quick brown fox jumps over the lazy dog. ".repeat(20);
        for (i, ch) in streaming_text.chars().enumerate() {
            store
                .edit_text(ctx, &block_id, i, &ch.to_string(), 0)
                .unwrap();
        }

        store.set_status(ctx, &block_id, Status::Done).unwrap();

        let entry = store.get(ctx).unwrap();
        let snap = entry.doc.get_block_snapshot(&block_id).unwrap();
        assert_eq!(snap.content, streaming_text);
        assert_eq!(snap.status, Status::Done);
    }

    /// Prove that merge_ops persists merged content to the database.
    ///
    /// This simulates the push_ops RPC flow: a remote client builds a
    /// SyncPayload from its local mutations and the server merges it.
    /// The server's DB must contain the merged content afterward.
    #[test]
    fn test_merge_ops_persists_to_db() {
        use crate::kernel_db::{DocumentRow, KernelDb};
        use kaijutsu_types::{KernelId, now_millis};

        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let creator = PrincipalId::system();
        let kernel_id = KernelId::new();

        let ws_id = {
            let db_guard = db.lock();
            db_guard
                .get_or_create_default_workspace(kernel_id, creator)
                .unwrap()
        };

        // "Server" store — DB-backed, will receive merged ops.
        let server_store = BlockStore::with_db(db.clone(), kernel_id, ws_id, creator);
        let ctx = ContextId::new();
        {
            let db_guard = db.lock();
            db_guard
                .insert_document(&DocumentRow {
                    document_id: ctx,
                    kernel_id,
                    workspace_id: ws_id,
                    doc_kind: DocumentKind::Conversation,
                    language: None,
                    path: None,
                    created_at: now_millis() as i64,
                    created_by: creator,
                })
                .unwrap();
        }
        server_store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        // "Client" store — no DB, generates mutations.
        let client_store = BlockStore::new(creator);
        client_store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        // Client inserts a block with content.
        let empty_frontier = HashMap::new();
        client_store
            .insert_block(
                ctx,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "merge_ops persistence test",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        // Build sync payload from client (all ops since empty frontier).
        let payload = client_store.ops_since(ctx, &empty_frontier).unwrap();
        assert!(!payload.new_blocks.is_empty(), "payload should contain the new block");

        // Server merges the payload — this is the code path under test.
        let version = server_store.merge_ops(ctx, payload).unwrap();
        assert!(version > 0);

        // In-memory content should have the merged block.
        let content = server_store.get_content(ctx).unwrap();
        assert!(
            content.contains("merge_ops persistence test"),
            "in-memory content after merge_ops should contain the block, got: {:?}",
            content,
        );

        // DB should have oplog entries from journal_op.
        let db_guard = db.lock();
        let oplog_entries = db_guard.load_oplog_since(ctx, 0).unwrap();
        assert!(
            !oplog_entries.is_empty(),
            "merge_ops should journal ops to the oplog",
        );

        // Verify the oplog can be replayed to reconstruct the merged content.
        let mut replay_store = CrdtBlockStore::new(ctx, creator);
        for (_seq, payload_bytes) in &oplog_entries {
            let payload: SyncPayload = postcard::from_bytes(payload_bytes).unwrap();
            replay_store.merge_ops(payload).unwrap();
        }
        let replayed_content = replay_store.full_text();
        assert!(
            replayed_content.contains("merge_ops persistence test"),
            "Replayed oplog should produce merged content, got: {:?}",
            replayed_content,
        );
    }

    // ========================================================================
    // OPLOG PERSISTENCE TESTS — drop-and-reload, per-mutation journal, compaction
    // ========================================================================

    /// Helper: unique KernelId for test isolation.
    fn test_kernel_id() -> KernelId {
        KernelId::new()
    }

    /// Create a DB-backed store backed by an on-disk SQLite file inside `dir`.
    /// Returns (db_handle, block_store, context_id, kernel_id, workspace_id).
    fn fresh_db_store(
        dir: &std::path::Path,
    ) -> (DbHandle, BlockStore, ContextId, KernelId, WorkspaceId) {
        let db_path = dir.join("test.db");
        let db = Arc::new(parking_lot::Mutex::new(
            KernelDb::open(&db_path).expect("open DB"),
        ));
        let creator = PrincipalId::system();
        let kernel_id = test_kernel_id();

        let ws_id = {
            let db_guard = db.lock();
            db_guard
                .get_or_create_default_workspace(kernel_id, creator)
                .expect("create workspace")
        };

        let store = BlockStore::with_db(db.clone(), kernel_id, ws_id, creator);
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .expect("create document");

        (db, store, ctx, kernel_id, ws_id)
    }

    /// Drop the store, create a new one from the same DB, call load_from_db.
    fn drop_and_reload(
        db: DbHandle,
        kernel_id: KernelId,
        ws_id: WorkspaceId,
    ) -> BlockStore {
        let creator = PrincipalId::system();
        let store2 = BlockStore::with_db(db, kernel_id, ws_id, creator);
        store2.load_from_db().expect("load_from_db");
        store2
    }

    // ====================================================================
    // 1. Crash-Recovery: drop + reload
    // ====================================================================

    #[test]
    fn test_drop_reload_simple() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, kid, ws) = fresh_db_store(dir.path());

        store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "hello world", Status::Done, ContentType::Plain,
            )
            .unwrap();

        drop(store); // destroy in-memory state

        let store2 = drop_and_reload(db, kid, ws);
        let content = store2.get_content(ctx).unwrap();
        assert_eq!(content, "hello world", "content should survive drop+reload");
    }

    #[test]
    fn test_drop_reload_after_append_chain() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, kid, ws) = fresh_db_store(dir.path());

        let block_id = store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "", Status::Done, ContentType::Plain,
            )
            .unwrap();

        let expected: String = (0..100).map(|i| (b'a' + (i % 26)) as char).collect();
        for ch in expected.chars() {
            store.append_text(ctx, &block_id, &ch.to_string()).unwrap();
        }

        let content_before = store.get_content(ctx).unwrap();
        assert_eq!(content_before, expected);

        drop(store);

        let store2 = drop_and_reload(db, kid, ws);
        let content_after = store2.get_content(ctx).unwrap();
        assert_eq!(
            content_after, expected,
            "100 single-char appends should survive drop+reload"
        );
    }

    #[test]
    fn test_drop_reload_after_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, kid, ws) = fresh_db_store(dir.path());

        let block_id = store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "", Status::Done, ContentType::Plain,
            )
            .unwrap();

        // 500 appends (+ 1 insert = 501 journal entries) should trigger compaction
        for i in 0..500 {
            let ch = (b'a' + (i % 26) as u8) as char;
            store.append_text(ctx, &block_id, &ch.to_string()).unwrap();
        }

        // Verify compaction happened: snapshot row should exist
        {
            let db_guard = db.lock();
            let snap = db_guard.load_latest_snapshot(ctx).unwrap();
            assert!(snap.is_some(), "compaction should have written a snapshot after 501 ops");
        }

        // Do 50 more appends after compaction
        for i in 0..50 {
            let ch = (b'A' + (i % 26) as u8) as char;
            store.append_text(ctx, &block_id, &ch.to_string()).unwrap();
        }

        let content_before = store.get_content(ctx).unwrap();
        assert_eq!(content_before.len(), 550);

        drop(store);

        let store2 = drop_and_reload(db, kid, ws);
        let content_after = store2.get_content(ctx).unwrap();
        assert_eq!(
            content_after, content_before,
            "all 550 chars should survive compaction + drop + reload"
        );
    }

    // ====================================================================
    // 2. Per-Mutation Journal Verification
    // ====================================================================

    #[test]
    fn test_journal_row_per_insert_block() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, _kid, _ws) = fresh_db_store(dir.path());

        store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "first", Status::Done, ContentType::Plain,
            )
            .unwrap();
        store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "second", Status::Done, ContentType::Plain,
            )
            .unwrap();

        let db_guard = db.lock();
        let entries = db_guard.load_oplog_since(ctx, 0).unwrap();
        assert_eq!(entries.len(), 2, "should have 2 oplog rows for 2 inserts");

        for (i, (_seq, payload_bytes)) in entries.iter().enumerate() {
            let payload: SyncPayload = postcard::from_bytes(payload_bytes)
                .unwrap_or_else(|e| panic!("deserialize oplog entry {}: {}", i, e));
            assert!(
                !payload.new_blocks.is_empty(),
                "insert oplog entry {} should have new_blocks",
                i
            );
        }
    }

    #[test]
    fn test_journal_row_per_append() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, _kid, _ws) = fresh_db_store(dir.path());

        let block_id = store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "", Status::Done, ContentType::Plain,
            )
            .unwrap();

        for i in 0..5 {
            let ch = (b'a' + i) as char;
            store.append_text(ctx, &block_id, &ch.to_string()).unwrap();
        }

        let db_guard = db.lock();
        let entries = db_guard.load_oplog_since(ctx, 0).unwrap();
        assert_eq!(
            entries.len(),
            6,
            "1 insert + 5 appends = 6 oplog rows, got {}",
            entries.len()
        );
    }

    #[test]
    fn test_journal_row_per_edit() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, _kid, _ws) = fresh_db_store(dir.path());

        let block_id = store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "hello", Status::Done, ContentType::Plain,
            )
            .unwrap();

        // Replace "hello" → "helloX" (insert at pos 5, delete 0)
        store.edit_text(ctx, &block_id, 5, "X", 0).unwrap();

        let db_guard = db.lock();
        let entries = db_guard.load_oplog_since(ctx, 0).unwrap();
        assert_eq!(
            entries.len(),
            2,
            "1 insert + 1 edit = 2 oplog rows, got {}",
            entries.len()
        );
    }

    #[test]
    fn test_journal_row_per_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, _kid, _ws) = fresh_db_store(dir.path());

        let block_id = store
            .insert_block(
                ctx, None, None, Role::Model, BlockKind::Text,
                "thinking", Status::Running, ContentType::Plain,
            )
            .unwrap();

        store.set_status(ctx, &block_id, Status::Done).unwrap();

        let db_guard = db.lock();
        let entries = db_guard.load_oplog_since(ctx, 0).unwrap();
        assert_eq!(
            entries.len(),
            2,
            "1 insert + 1 set_status = 2 oplog rows, got {}",
            entries.len()
        );

        // Decode the second entry and verify it has updated_headers
        let (_seq, payload_bytes) = &entries[1];
        let payload: SyncPayload = postcard::from_bytes(payload_bytes).unwrap();
        assert!(
            !payload.updated_headers.is_empty(),
            "set_status oplog entry should have updated_headers"
        );
    }

    #[test]
    fn test_journal_row_per_merge_ops() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, _kid, _ws) = fresh_db_store(dir.path());

        // Create a second (non-DB) store and insert a block in it
        let client = BlockStore::new(PrincipalId::system());
        client
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();
        client
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "from remote", Status::Done, ContentType::Plain,
            )
            .unwrap();

        let payload = client.ops_since(ctx, &HashMap::new()).unwrap();
        store.merge_ops(ctx, payload).unwrap();

        let db_guard = db.lock();
        let entries = db_guard.load_oplog_since(ctx, 0).unwrap();
        assert_eq!(
            entries.len(),
            1,
            "merge_ops should produce 1 oplog row, got {}",
            entries.len()
        );
    }

    // ====================================================================
    // 3. Compaction
    // ====================================================================

    #[test]
    fn test_compaction_trigger_at_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, _kid, _ws) = fresh_db_store(dir.path());

        let block_id = store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "", Status::Done, ContentType::Plain,
            )
            .unwrap();

        // 500 more appends → total 501 journal entries (insert + 500 appends)
        // This exceeds COMPACTION_OP_THRESHOLD (500).
        for i in 0..500 {
            let ch = (b'a' + (i % 26) as u8) as char;
            store.append_text(ctx, &block_id, &ch.to_string()).unwrap();
        }

        let db_guard = db.lock();

        // Snapshot should exist
        let snap = db_guard.load_latest_snapshot(ctx).unwrap();
        assert!(snap.is_some(), "snapshot should exist after compaction");
        let snap = snap.unwrap();

        // Remaining oplog entries should be only those written after compaction
        let remaining = db_guard.load_oplog_since(ctx, 0).unwrap();
        assert!(
            remaining.len() < 10,
            "oplog should be truncated after compaction, got {} entries",
            remaining.len()
        );

        // All remaining entries should have seq > snap.seq
        for (seq, _) in &remaining {
            assert!(
                *seq > snap.seq,
                "remaining oplog entry seq {} should be > snapshot seq {}",
                seq,
                snap.seq
            );
        }
    }

    #[test]
    fn test_compaction_preserves_state() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, kid, ws) = fresh_db_store(dir.path());

        let block_id = store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "", Status::Done, ContentType::Plain,
            )
            .unwrap();

        // 600 appends — triggers compaction (601 total > 500 threshold)
        let expected: String = (0..600).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        for ch in expected.chars() {
            store.append_text(ctx, &block_id, &ch.to_string()).unwrap();
        }

        let content_before = store.get_content(ctx).unwrap();
        assert_eq!(content_before, expected);

        drop(store);

        let store2 = drop_and_reload(db, kid, ws);
        let content_after = store2.get_content(ctx).unwrap();
        assert_eq!(
            content_after, expected,
            "compacted + post-compaction ops should all survive reload"
        );
    }

    // ====================================================================
    // 4. Mixed Operations
    // ====================================================================

    #[test]
    fn test_mixed_local_and_remote_ops() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store_a, ctx, kid, ws) = fresh_db_store(dir.path());

        // A inserts a block locally
        store_a
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "local-1", Status::Done, ContentType::Plain,
            )
            .unwrap();

        // B (no DB) inserts a block
        let store_b = BlockStore::new(PrincipalId::new());
        store_b
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();
        store_b
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "remote-b", Status::Done, ContentType::Plain,
            )
            .unwrap();

        // Merge B's ops into A
        let payload = store_b.ops_since(ctx, &HashMap::new()).unwrap();
        store_a.merge_ops(ctx, payload).unwrap();

        // A inserts another block locally
        store_a
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "local-2", Status::Done, ContentType::Plain,
            )
            .unwrap();

        let content_before = store_a.get_content(ctx).unwrap();
        assert!(content_before.contains("local-1"));
        assert!(content_before.contains("remote-b"));
        assert!(content_before.contains("local-2"));

        drop(store_a);

        let store2 = drop_and_reload(db, kid, ws);
        let content_after = store2.get_content(ctx).unwrap();
        assert!(
            content_after.contains("local-1"),
            "local-1 missing after reload: {:?}",
            content_after
        );
        assert!(
            content_after.contains("remote-b"),
            "remote-b missing after reload: {:?}",
            content_after
        );
        assert!(
            content_after.contains("local-2"),
            "local-2 missing after reload: {:?}",
            content_after
        );

        let blocks = store2.block_snapshots(ctx).unwrap();
        assert_eq!(blocks.len(), 3, "should have 3 blocks after reload");
    }

    #[test]
    fn test_block_order_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, kid, ws) = fresh_db_store(dir.path());

        // Insert 5 blocks, recording the order
        let mut ids = Vec::new();
        let mut prev: Option<BlockId> = None;
        for i in 0..5 {
            let bid = store
                .insert_block(
                    ctx,
                    None,
                    prev.as_ref(),
                    Role::User,
                    BlockKind::Text,
                    format!("block-{}", i),
                    Status::Done,
                    ContentType::Plain,
                )
                .unwrap();
            ids.push(bid);
            prev = Some(bid);
        }

        let order_before: Vec<BlockId> = store
            .block_snapshots(ctx)
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();

        drop(store);

        let store2 = drop_and_reload(db, kid, ws);
        let order_after: Vec<BlockId> = store2
            .block_snapshots(ctx)
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();

        assert_eq!(
            order_before, order_after,
            "block order should be preserved across drop+reload"
        );

        // Verify content order too
        let blocks = store2.block_snapshots(ctx).unwrap();
        for (i, snap) in blocks.iter().enumerate() {
            assert_eq!(
                snap.content,
                format!("block-{}", i),
                "block {} content mismatch",
                i
            );
        }
    }

    // ====================================================================
    // 5. Lamport Clock
    // ====================================================================

    #[test]
    fn test_lamport_clock_seeded_after_restore() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, kid, ws) = fresh_db_store(dir.path());

        let block_id = store
            .insert_block(
                ctx, None, None, Role::Model, BlockKind::Text,
                "test", Status::Running, ContentType::Plain,
            )
            .unwrap();

        store.set_status(ctx, &block_id, Status::Pending).unwrap();

        let status_at_before = {
            let entry = store.get(ctx).unwrap();
            let snap = entry.doc.get_block_snapshot(&block_id).unwrap();
            snap.status_at
        };
        assert!(status_at_before > 0, "status_at should be nonzero after set_status");

        drop(store);

        let store2 = drop_and_reload(db, kid, ws);

        // Now set_status again — the Lamport clock should have been seeded
        // from the restored state, so the new timestamp must be strictly greater.
        store2.set_status(ctx, &block_id, Status::Done).unwrap();

        let status_at_after = {
            let entry = store2.get(ctx).unwrap();
            let snap = entry.doc.get_block_snapshot(&block_id).unwrap();
            snap.status_at
        };

        assert!(
            status_at_after > status_at_before,
            "Lamport clock after reload should advance: before={}, after={}",
            status_at_before,
            status_at_after
        );
    }

    // ====================================================================
    // 6. Input Documents
    // ====================================================================

    #[test]
    fn test_input_oplog_independent() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, _kid, _ws) = fresh_db_store(dir.path());

        store.create_input_doc(ctx).unwrap();

        // 5 block operations
        let block_id = store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "a", Status::Done, ContentType::Plain,
            )
            .unwrap();
        for _ in 0..4 {
            store.append_text(ctx, &block_id, "x").unwrap();
        }

        // 5 input edits
        for i in 0..5 {
            let ch = (b'A' + i) as char;
            store
                .edit_input(ctx, i as usize, &ch.to_string(), 0)
                .unwrap();
        }

        let db_guard = db.lock();
        let oplog_entries = db_guard.load_oplog_since(ctx, 0).unwrap();
        let input_entries = db_guard.load_input_oplog_since(ctx, 0).unwrap();

        assert_eq!(
            oplog_entries.len(),
            5,
            "block oplog should have 5 rows (1 insert + 4 appends), got {}",
            oplog_entries.len()
        );
        assert_eq!(
            input_entries.len(),
            5,
            "input oplog should have 5 rows, got {}",
            input_entries.len()
        );
    }

    #[test]
    fn test_input_drop_reload() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, kid, ws) = fresh_db_store(dir.path());

        store.create_input_doc(ctx).unwrap();

        // 100 single-char edits
        let expected: String = (0..100).map(|i| (b'a' + (i % 26)) as char).collect();
        for (i, ch) in expected.chars().enumerate() {
            store
                .edit_input(ctx, i, &ch.to_string(), 0)
                .unwrap();
        }

        let text_before = store.get_input_text(ctx).unwrap();
        assert_eq!(text_before, expected);

        drop(store);

        let store2 = drop_and_reload(db, kid, ws);
        store2.load_input_docs_from_db().unwrap();
        let text_after = store2.get_input_text(ctx).unwrap();
        assert_eq!(
            text_after, expected,
            "input doc should survive drop+reload"
        );
    }

    // ====================================================================
    // 7. Forks
    // ====================================================================

    #[test]
    fn test_fork_creates_clean_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, _kid, _ws) = fresh_db_store(dir.path());

        store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "original block 1", Status::Done, ContentType::Plain,
            )
            .unwrap();
        store
            .insert_block(
                ctx, None, None, Role::Model, BlockKind::Text,
                "original block 2", Status::Done, ContentType::Plain,
            )
            .unwrap();

        let fork_id = ContextId::new();
        store.fork_document(ctx, fork_id).unwrap();

        let db_guard = db.lock();

        // Fork should have a snapshot
        let snap = db_guard.load_latest_snapshot(fork_id).unwrap();
        assert!(snap.is_some(), "forked doc should have a snapshot");

        // Fork should have NO oplog entries
        let oplog = db_guard.load_oplog_since(fork_id, 0).unwrap();
        assert!(
            oplog.is_empty(),
            "forked doc should have empty oplog, got {} entries",
            oplog.len()
        );

        // Verify the snapshot contains the right content
        let snap = snap.unwrap();
        assert!(
            snap.content.contains("original block 1"),
            "fork snapshot content missing block 1: {:?}",
            snap.content
        );
        assert!(
            snap.content.contains("original block 2"),
            "fork snapshot content missing block 2: {:?}",
            snap.content
        );
    }
}
