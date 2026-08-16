//! Block-based storage using `crate::blocks`.
//!
//! Each document wraps a `crate::blocks::BlockDocument` (each
//! block's content is a plain `String` — see `crate::blocks::content`'s
//! module doc). The durable oplog journals a `SyncPayload` per mutation and
//! replays it on restart; nothing multi-writer merges through it (CLAUDE.md
//! "Durable state and the wire" — the kernel is the sole sequencer).
//!
//! # Concurrency Model
//!
//! - DashMap for per-document concurrent access
//! - FlowBus for typed pub/sub real-time updates
//! - parking_lot for efficient locking

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use parking_lot::RwLock;

use crate::blocks::{BlockDocument, ForkBlockFilter, StoreSnapshot, SyncPayload, TextEdit};
use kaijutsu_types::codec;
use kaijutsu_types::{BlockFilter, BlockQuery};
use kaijutsu_types::{
    BlockId, BlockKind, BlockSnapshot, ContentType, ContextId, DocKind, PrincipalId, Role, Status,
    TaskStatus, Tick, ToolKind, WorkspaceId,
};

use crate::flows::{BlockFlow, OpSource, SharedBlockFlowBus};
use crate::kernel_db::{DocumentRow, KernelDb, KernelDbError};

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

    #[error("document already exists: {0}")]
    DocumentAlreadyExists(ContextId),

    #[error("block not found after insert")]
    BlockNotFoundAfterInsert,

    #[error("no draft in context {0} for principal {1}")]
    NoDraft(ContextId, PrincipalId),

    #[error("draft in context {0} is empty")]
    EmptyDraft(ContextId),

    #[error(transparent)]
    Block(#[from] crate::blocks::BlockDocumentError),

    #[error("database error: {0}")]
    Db(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("no database configured")]
    NoDatabaseConfigured,

    #[error("{0}")]
    Validation(String),

    /// A DB row already exists at this `document_id`, but it does NOT match
    /// the row `create_document`/`create_document_with_path` intended to
    /// write (`doc_kind`, `workspace_id`, or `path` differ) — divergence
    /// must never be silently "recovered" the way a genuine duplicate is.
    /// See `insert_or_reconcile_document`.
    #[error("document {id} diverged from its persisted row: {detail}")]
    DocumentDiverged { id: ContextId, detail: String },

    /// A *different* document already claims this `(workspace, path)` — a
    /// hard error, never recovered.
    #[error("path '{path}' is already claimed by document {existing}")]
    DocumentPathConflict { path: String, existing: ContextId },
}

/// Result type alias for BlockStore operations.
pub type BlockStoreResult<T> = Result<T, BlockStoreError>;

/// Thread-safe database handle (unified KernelDb).
pub type DbHandle = Arc<parking_lot::Mutex<KernelDb>>;

/// Compaction thresholds for the block document oplog.
const COMPACTION_OP_THRESHOLD: u64 = 500;
const COMPACTION_BYTE_THRESHOLD: u64 = 1_048_576; // 1 MiB

/// Entry for a document in the store.
pub struct DocumentEntry {
    /// Per-block store (each block owns its content as a plain `String`).
    pub doc: BlockDocument,
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
        principal_id: PrincipalId,
    ) -> Self {
        Self {
            doc: BlockDocument::new(context_id, principal_id),
            kind,
            language,
            version: AtomicU64::new(0),
            last_agent: RwLock::new(principal_id),
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
        principal_id: PrincipalId,
        journal_seq: u64,
        uncompacted_count: u64,
        uncompacted_bytes: u64,
    ) -> BlockStoreResult<Self> {
        let store = BlockDocument::from_snapshot(snapshot, principal_id)?;
        let version = store.version();
        Ok(Self {
            doc: store,
            kind,
            language,
            version: AtomicU64::new(version),
            last_agent: RwLock::new(principal_id),
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
    pub fn touch(&self, principal_id: PrincipalId) {
        self.version.fetch_add(1, Ordering::SeqCst);
        *self.last_agent.write() = principal_id;
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
    /// Database for persistence (unified KernelDb).
    db: Option<DbHandle>,
    /// Whether this store is expected to persist (kernel-side). When `true`, a
    /// missing `db` at journal time is a fatal invariant violation (crash over
    /// silent durability loss), not a no-op. Replica stores (app scratch,
    /// client sync) set this `false` and journaling legitimately no-ops.
    /// Set together with `db` at construction: `with_db*` → `true`,
    /// `new`/`with_flows` → `false`.
    persistent: bool,
    /// Kernel ID for document rows.
        /// Default workspace ID for new documents.
    default_workspace_id: Option<WorkspaceId>,
    /// Default agent ID for this store.
    principal_id: RwLock<PrincipalId>,
    /// FlowBus for typed pub/sub.
    block_flows: Option<SharedBlockFlowBus>,
    /// Stage 1 (time-well) incremental live-status cache: one
    /// `derive_context_live_status` reduction per context, bumped inside
    /// `journal_op` (the one chokepoint every mutating block op funnels
    /// through) instead of re-scanned on every 5s poll. Empty on a fresh
    /// store (e.g. right after a kernel restart) — `live_status()` lazily
    /// populates a miss with a one-time single-context scan rather than
    /// defaulting wrongly to `Pending`.
    live_status: DashMap<ContextId, Status>,
    /// TEST-ONLY fault injection: when `> 0`, each `insert_from_snapshot_as`
    /// decrements it, and the call on which it hits exactly 1 returns an error
    /// instead of inserting. Lets the per-artifact resumability spine
    /// (`materialize_committed`) be tested without a real journal fault. Always 0
    /// (no-op) in production builds — the field is `#[cfg(test)]`-gated.
    #[cfg(test)]
    fail_insert_countdown: std::sync::atomic::AtomicUsize,
}

impl BlockStore {
    /// Create a new in-memory block store.
    pub fn new(principal_id: PrincipalId) -> Self {
        Self {
            documents: DashMap::new(),
            db: None,
            persistent: false,
                        default_workspace_id: None,
            principal_id: RwLock::new(principal_id),
            block_flows: None,
            live_status: DashMap::new(),
            #[cfg(test)]
            fail_insert_countdown: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Create a new in-memory block store with FlowBus.
    pub fn with_flows(principal_id: PrincipalId, block_flows: SharedBlockFlowBus) -> Self {
        Self {
            documents: DashMap::new(),
            db: None,
            persistent: false,
                        default_workspace_id: None,
            principal_id: RwLock::new(principal_id),
            block_flows: Some(block_flows),
            live_status: DashMap::new(),
            #[cfg(test)]
            fail_insert_countdown: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Create a block store with unified KernelDb persistence.
    pub fn with_db(
        db: DbHandle,
                default_workspace_id: WorkspaceId,
        principal_id: PrincipalId,
    ) -> Self {
        Self {
            documents: DashMap::new(),
            db: Some(db),
            persistent: true,
                        default_workspace_id: Some(default_workspace_id),
            principal_id: RwLock::new(principal_id),
            block_flows: None,
            live_status: DashMap::new(),
            #[cfg(test)]
            fail_insert_countdown: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Create a block store with unified KernelDb persistence and a FlowBus.
    pub fn with_db_and_flows(
        db: DbHandle,
                default_workspace_id: WorkspaceId,
        principal_id: PrincipalId,
        block_flows: SharedBlockFlowBus,
    ) -> Self {
        Self {
            documents: DashMap::new(),
            db: Some(db),
            persistent: true,
                        default_workspace_id: Some(default_workspace_id),
            principal_id: RwLock::new(principal_id),
            block_flows: Some(block_flows),
            live_status: DashMap::new(),
            #[cfg(test)]
            fail_insert_countdown: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// TEST-ONLY: construct a store that *declares* persistence but has no db
    /// handle — the pathological state the journaling guard must reject. In
    /// production this state is unconstructable (`db` and `persistent` are set
    /// together), so it exists only to exercise the fail-loud invariant.
    #[cfg(test)]
    pub fn persistent_without_db(principal_id: PrincipalId) -> Self {
        let mut store = Self::new(principal_id);
        store.persistent = true;
        store
    }

    /// TEST-ONLY: arm the insert fault injector. A value of `n` fails the `n`th
    /// subsequent `insert_from_snapshot_as` (1 = the next insert). 0 disarms.
    #[cfg(test)]
    pub fn arm_insert_fault(&self, n: usize) {
        self.fail_insert_countdown
            .store(n, std::sync::atomic::Ordering::SeqCst);
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
    pub fn principal_id(&self) -> PrincipalId {
        *self.principal_id.read()
    }

    /// Set the agent ID.
    pub fn set_principal_id(&self, principal_id: PrincipalId) {
        *self.principal_id.write() = principal_id;
    }

    /// Insert `row` via `db.insert_document`, reconciling the typed
    /// conflict variants `create_document`/`create_document_with_path` can
    /// see (docs/issues.md:361). Shared by both so the classification logic
    /// exists exactly once.
    ///
    /// - `Ok(())` — inserted cleanly.
    /// - `DuplicateDocument` — a row already exists at this `document_id`.
    ///   Read it back and compare against `row`: if `doc_kind`,
    ///   `workspace_id`, and `path` all match, this is the benign
    ///   already-in-DB-not-in-memory recovery (a differing `language` is
    ///   logged but not divergence). Any of those three differing — or the
    ///   row vanishing between the insert and the read-back — is
    ///   `DocumentDiverged`, which must NOT be silently recovered.
    /// - `DocumentPathConflict` — a *different* document already claims this
    ///   path. Always a hard error.
    /// - anything else — wrapped as `BlockStoreError::Db`, as before.
    fn insert_or_reconcile_document(db: &KernelDb, row: &DocumentRow) -> BlockStoreResult<()> {
        match db.insert_document(row) {
            Ok(()) => Ok(()),
            Err(KernelDbError::DuplicateDocument(id)) => {
                let persisted = db
                    .get_document(id)
                    .map_err(|e| BlockStoreError::Db(e.to_string()))?;
                let Some(persisted) = persisted else {
                    return Err(BlockStoreError::DocumentDiverged {
                        id,
                        detail: "persisted row vanished between insert and read-back"
                            .to_string(),
                    });
                };

                let mut diffs = Vec::new();
                if persisted.doc_kind != row.doc_kind {
                    diffs.push(format!(
                        "doc_kind: persisted={:?} intended={:?}",
                        persisted.doc_kind, row.doc_kind
                    ));
                }
                if persisted.workspace_id != row.workspace_id {
                    diffs.push(format!(
                        "workspace_id: persisted={} intended={}",
                        persisted.workspace_id, row.workspace_id
                    ));
                }
                if persisted.path != row.path {
                    diffs.push(format!(
                        "path: persisted={:?} intended={:?}",
                        persisted.path, row.path
                    ));
                }
                if !diffs.is_empty() {
                    return Err(BlockStoreError::DocumentDiverged {
                        id,
                        detail: diffs.join("; "),
                    });
                }

                // `language` differing is not divergence — log and continue.
                if persisted.language != row.language {
                    tracing::warn!(
                        context_id = %id.to_hex(),
                        persisted_language = ?persisted.language,
                        intended_language = ?row.language,
                        "Document language differs from persisted row; not treated as divergence"
                    );
                }
                tracing::warn!(context_id = %id.to_hex(), "Document already in DB but not in memory, recovering");
                Ok(())
            }
            Err(KernelDbError::DocumentPathConflict { path, existing }) => {
                Err(BlockStoreError::DocumentPathConflict { path, existing })
            }
            Err(e) => Err(BlockStoreError::Db(e.to_string())),
        }
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
                let principal_id = self.principal_id();

                // Persist metadata if we have a DB
                if let Some(db) = &self.db {
                    let db_guard = db.lock();
                    let row = DocumentRow {
                        document_id: context_id,
                                                workspace_id: self.default_workspace_id.unwrap_or_default(),
                        doc_kind: kind,
                        language: language.clone(),
                        path: None,
                        created_at: kaijutsu_types::now_millis() as i64,
                        created_by: principal_id,
                    };
                    Self::insert_or_reconcile_document(&db_guard, &row)?;
                }

                let entry = DocumentEntry::new(context_id, kind, language, principal_id);
                vacant.insert(entry);

                Ok(())
            }
        }
    }

    /// Create a document that carries a filesystem `path` in its `documents`
    /// row. Used by the CRDT-native config/rc backend (`ConfigCrdtFs`): the
    /// path makes the `documents` table double as the readdir manifest
    /// (`list_documents_under_path`), so the doc and its manifest entry are one
    /// write, not two stores to drift. Otherwise identical to
    /// [`create_document`](Self::create_document).
    pub fn create_document_with_path(
        &self,
        context_id: ContextId,
        kind: DocKind,
        language: Option<String>,
        path: String,
    ) -> BlockStoreResult<()> {
        use dashmap::mapref::entry::Entry;

        match self.documents.entry(context_id) {
            Entry::Occupied(_) => Err(BlockStoreError::DocumentAlreadyExists(context_id)),
            Entry::Vacant(vacant) => {
                let principal_id = self.principal_id();

                if let Some(db) = &self.db {
                    let db_guard = db.lock();
                    let row = DocumentRow {
                        document_id: context_id,
                        workspace_id: self.default_workspace_id.unwrap_or_default(),
                        doc_kind: kind,
                        language: language.clone(),
                        path: Some(path),
                        created_at: kaijutsu_types::now_millis() as i64,
                        created_by: principal_id,
                    };
                    Self::insert_or_reconcile_document(&db_guard, &row)?;
                }

                let entry = DocumentEntry::new(context_id, kind, language, principal_id);
                vacant.insert(entry);

                Ok(())
            }
        }
    }

    /// List the persisted `documents` rows whose path falls under `dir`
    /// (the readdir manifest for [`create_document_with_path`]). Empty when
    /// there is no DB. Returns `(path, context_id, doc_kind)` for every
    /// descendant — `doc_kind` lets `ConfigCrdtFs::readdir` emit
    /// `FileType::Symlink` for link docs without a second lookup per entry.
    pub fn documents_under_path(
        &self,
        dir: &str,
    ) -> BlockStoreResult<Vec<(String, ContextId, DocKind)>> {
        let Some(db) = self.db.as_ref() else {
            return Ok(Vec::new());
        };
        let rows = db
            .lock()
            .list_documents_under_path(dir)
            .map_err(|e| BlockStoreError::Db(e.to_string()))?;
        Ok(rows
            .into_iter()
            .filter_map(|r| r.path.map(|p| (p, r.document_id, r.doc_kind)))
            .collect())
    }

    /// Create a document from a serialized store snapshot (for sync from server).
    ///
    /// Reconstructs the document from a CBOR-encoded `StoreSnapshot`.
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

        let snapshot: StoreSnapshot = codec::decode(snapshot_bytes)
            .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;

        let principal_id = self.principal_id();
        let entry = DocumentEntry::from_store_snapshot(snapshot, kind, language, principal_id, 0, 0, 0)?;
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

    /// The [`DocKind`] of a document, or `None` if it does not exist. Used by
    /// `ConfigCrdtFs` to tell a symlink doc (`DocKind::Symlink`, content = link
    /// target) apart from a regular file doc whose content happens to look like
    /// a path — the git-style "mode bit" check.
    pub fn document_kind(&self, context_id: ContextId) -> Option<DocKind> {
        self.documents.get(&context_id).map(|r| r.kind)
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

        let principal_id = self.principal_id();
        let forked_store = source_entry.doc.fork(new_id, principal_id);
        let kind = source_entry.kind;
        let language = source_entry.language.clone();
        drop(source_entry); // Release the read lock

        // Persist metadata if we have a DB
        if let Some(db) = &self.db {
            let db_guard = db.lock();
            let row = DocumentRow {
                document_id: new_id,
                                workspace_id: self.default_workspace_id.unwrap_or_default(),
                doc_kind: kind,
                language: language.clone(),
                path: None,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: principal_id,
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
            last_agent: RwLock::new(principal_id),
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

        let principal_id = self.principal_id();
        let forked_store = source_entry
            .doc
            .fork_at_version(new_id, principal_id, before_timestamp);
        let kind = source_entry.kind;
        let language = source_entry.language.clone();
        drop(source_entry); // Release the read lock

        // Persist metadata if we have a DB
        if let Some(db) = &self.db {
            let db_guard = db.lock();
            let row = DocumentRow {
                document_id: new_id,
                                workspace_id: self.default_workspace_id.unwrap_or_default(),
                doc_kind: kind,
                language: language.clone(),
                path: None,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: principal_id,
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
            last_agent: RwLock::new(principal_id),
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

        let principal_id = self.principal_id();
        let forked_store =
            source_entry
                .doc
                .fork_filtered(new_id, principal_id, before_timestamp, filter);
        let kind = source_entry.kind;
        let language = source_entry.language.clone();
        drop(source_entry);

        // Persist metadata if we have a DB
        if let Some(db) = &self.db {
            let db_guard = db.lock();
            let row = DocumentRow {
                document_id: new_id,
                                workspace_id: self.default_workspace_id.unwrap_or_default(),
                doc_kind: kind,
                language: language.clone(),
                path: None,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: principal_id,
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
            last_agent: RwLock::new(principal_id),
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
        // `block_ids_ordered()`, not `blocks_ordered()`: only the id is
        // needed, so skip building `BlockSnapshot`s (and the `text()`
        // materialization each one carries) entirely.
        entry.doc.block_ids_ordered().last().copied()
    }

    /// Reserve a fresh `BlockId` under `principal` without inserting — the
    /// materialization path mints its id this way (`cell.played_by` becomes the
    /// principal), then inserts via `insert_from_snapshot_as`. Reserve and insert
    /// are two lock acquisitions: reserve atomically claims its seq under the
    /// entry lock, and the single-kernel sole-sequencer invariant covers the rest.
    /// A reserve-then-failed-insert leaves a benign seq gap (monotonic-unique, not
    /// dense). Errors loudly if the document is not resident.
    pub fn reserve_block_id(
        &self,
        context_id: ContextId,
        principal: PrincipalId,
    ) -> BlockStoreResult<BlockId> {
        let mut entry = self
            .get_mut(context_id)
            .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        Ok(entry.doc.reserve_block_id(principal))
    }

    /// The maximum `Tick` over the document's live blocks, or `None` if empty —
    /// the playhead seed on re-arm (design §4). A scan is fine: arm is rare.
    /// Errors loudly if the document is not resident.
    pub fn max_tick(&self, context_id: ContextId) -> BlockStoreResult<Option<Tick>> {
        let entry = self
            .get(context_id)
            .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        Ok(entry.doc.max_tick())
    }

    // =========================================================================
    // Block Operations
    // =========================================================================

    /// Resolve the db handle for a journaling write, enforcing the persistence
    /// invariant. Returns:
    /// - `Ok(Some(db))` — persist to this handle.
    /// - `Ok(None)` — a replica store (`persistent == false`): journaling is a
    ///   legitimate no-op (app scratch store, client sync replica).
    /// - `Err(NoDatabaseConfigured)` — a store that declared itself persistent
    ///   has no db handle. This is the feared silent-durability-loss footgun
    ///   (`block_store.rs` historically `return Ok(())`'d here); we crash over
    ///   corruption rather than drop the op on the floor.
    fn journaling_db(&self) -> BlockStoreResult<Option<&DbHandle>> {
        match self.db.as_ref() {
            Some(db) => Ok(Some(db)),
            None if self.persistent => Err(BlockStoreError::NoDatabaseConfigured),
            None => Ok(None),
        }
    }

    /// Journal an op to the append-only oplog.
    ///
    /// Serializes the SyncPayload, appends it to the `oplog` table, and
    /// triggers compaction if the uncompacted count or bytes exceed thresholds.
    fn journal_op(
        &self,
        context_id: ContextId,
        payload: SyncPayload,
    ) -> BlockStoreResult<()> {
        // Stage 1 (time-well) incremental live_status: recompute + cache this
        // context's live status from its current (just-mutated) block
        // statuses. Unconditional — non-persistent stores (app/client scratch,
        // tests) need a correct cache too; only the DB journaling below is
        // gated on `self.db`. (In practice `get` always hits here: every call
        // site mutates via `get_mut` before calling journal_op, so the
        // document is guaranteed to already exist.)
        //
        // Why not gate this on "the op changed a status" to skip the scan on
        // streaming text deltas? The payload can't tell us: `ops_since` ALWAYS
        // ships every known block's header (block_store.rs `ops_since`, "Always
        // send header for known blocks so metadata changes propagate via LWW"),
        // and `set_status` itself travels *only* as an updated header — so
        // `updated_headers` is non-empty on every op and carries no signal that
        // distinguishes a status change from a status-neutral edit. The scan is
        // the cheapest thing that knows the new status, and it's dwarfed by the
        // `append_op` SQLite write it sits beside, so leave it unconditional.
        //
        // `statuses_ordered()`, not `blocks_ordered()`: the latter builds a
        // full `BlockSnapshot` per block, and `BlockSnapshot::content` calls
        // `BlockContent::text()` — materializing every block's full text out
        // of its DTE document on every journaled op, context-wide, just to
        // read `.status` off the snapshot and throw the rest away. This is
        // the streaming hot path (one call per token), so that cost was paid
        // once per token per block. `statuses_ordered()` reads `.status`
        // directly off each block's header, same document-order sort,
        // without ever touching the text.
        if let Some(entry) = self.get(context_id) {
            let statuses = entry.doc.statuses_ordered();
            drop(entry);
            self.recompute_live_status(context_id, &statuses);
        }

        let Some(db) = self.journaling_db()? else {
            return Ok(());
        };

        let payload_bytes = codec::encode(&payload)
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
            // Stage 1 (time-well) kernel truth: stamp this context's
            // last_activity_at on every mutating block op. `now_millis()` is
            // the SAME Unix-millis clock `created_at` is stamped with
            // (`kaijutsu_types::now_millis`, mirrored by kernel_db's private
            // helper of the same name/formula) - the app computes
            // `now - last_activity_at` directly against it, so the epoch must
            // match exactly. One extra O(1) UPDATE under a lock already held.
            db_guard
                .touch_context_activity(context_id, kaijutsu_types::now_millis() as i64)
                .map_err(|e| BlockStoreError::Db(e.to_string()))?;
        }

        if count >= COMPACTION_OP_THRESHOLD || bytes >= COMPACTION_BYTE_THRESHOLD {
            self.compact_document(context_id)?;
        }
        Ok(())
    }

    /// Run compaction: snapshot the current state and truncate the oplog.
    fn compact_document(&self, context_id: ContextId) -> BlockStoreResult<()> {
        let Some(db) = self.journaling_db()? else {
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
            let snapshot_bytes = codec::encode(&snapshot)
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
            // Flush the just-truncated oplog out of the WAL so the main file
            // stops lagging committed history. Best-effort: a busy checkpoint
            // (a concurrent reader on another connection) is non-fatal.
            if let Ok((busy, _, _)) = db_guard.checkpoint()
                && busy != 0
            {
                tracing::debug!(
                    document_id = %context_id.to_hex(),
                    "wal_checkpoint(TRUNCATE) busy after doc compaction",
                );
            }
        }

        if let Some(entry) = self.get(context_id) {
            entry.uncompacted_count.store(0, Ordering::SeqCst);
            entry.uncompacted_bytes.store(0, Ordering::SeqCst);
        }

        Ok(())
    }

    /// Write an initial snapshot for a newly forked document (no oplog).
    fn write_initial_snapshot(&self, context_id: ContextId) -> BlockStoreResult<()> {
        let Some(db) = self.journaling_db()? else {
            return Ok(());
        };

        let entry = self
            .get(context_id)
            .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        let snapshot = entry.doc.snapshot();
        let content = entry.content();
        let version = entry.version() as i64;

        let snapshot_bytes = codec::encode(&snapshot)
            .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;

        drop(entry);

        let mut db_guard = db.lock();
        db_guard
            .write_snapshot_and_truncate(context_id, 0, version, &snapshot_bytes, &content)
            .map_err(|e| BlockStoreError::Db(e.to_string()))?;

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
    /// If `principal_id` is `Some`, the block will be stamped with that principal.
    /// If `None`, the store's default principal_id is used (backwards compat).
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
        principal_id: Option<PrincipalId>,
    ) -> BlockStoreResult<BlockId> {
        let after_id = after.cloned();
        let (block_id, snapshot, ops, ops_bytes, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = principal_id.unwrap_or_else(|| self.principal_id());

            // Set the agent for this operation so BlockId gets the right author
            entry.doc.set_principal_id(effective_agent);

            let block_id = entry
                .doc
                .insert_block(parent_id, after, role, kind, content, status, content_type)?;
            let snapshot = entry
                .doc
                .get_block_snapshot(&block_id)
                .ok_or(BlockStoreError::BlockNotFoundAfterInsert)?;

            // The journaled payload for a fresh block is just its own
            // snapshot — nothing to diff against, since nobody knew about
            // this block a moment ago.
            let ops = SyncPayload::from_new_block(snapshot.clone());
            let ops_bytes = codec::encode(&ops)
                .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            let version = entry.version();
            (block_id, snapshot, ops, ops_bytes, version)
        };
        self.journal_op(context_id, ops)?;

        // Emit flow event with creation ops
        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(snapshot),
            after_id,
            ops: Arc::from(ops_bytes),
            version,
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
        principal_id: Option<PrincipalId>,
        tool_use_id: Option<String>,
        role: Option<Role>,
    ) -> BlockStoreResult<BlockId> {
        let after_id = after.cloned();
        let (block_id, snapshot, ops, ops_bytes, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = principal_id.unwrap_or_else(|| self.principal_id());
            entry.doc.set_principal_id(effective_agent);

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

            // The journaled payload for a fresh block is just its own
            // snapshot (captured after set_tool_use_id, so it's included).
            let ops = SyncPayload::from_new_block(snapshot.clone());
            let ops_bytes = codec::encode(&ops)
                .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            let version = entry.version();
            (block_id, snapshot, ops, ops_bytes, version)
        };
        self.journal_op(context_id, ops)?;

        // Emit flow event with creation ops
        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(snapshot),
            after_id,
            ops: Arc::from(ops_bytes),
            version,
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
        principal_id: Option<PrincipalId>,
        tool_use_id: Option<String>,
    ) -> BlockStoreResult<BlockId> {
        let after_id = after.cloned();
        let (block_id, snapshot, ops, ops_bytes, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = principal_id.unwrap_or_else(|| self.principal_id());
            entry.doc.set_principal_id(effective_agent);

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

            // The journaled payload for a fresh block is just its own
            // snapshot (captured after set_tool_use_id, so it's included).
            let ops = SyncPayload::from_new_block(snapshot.clone());
            let ops_bytes = codec::encode(&ops)
                .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            let version = entry.version();
            (block_id, snapshot, ops, ops_bytes, version)
        };
        self.journal_op(context_id, ops)?;

        // Emit flow event with creation ops
        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(snapshot),
            after_id,
            ops: Arc::from(ops_bytes),
            version,
            source: OpSource::Local,
        });

        Ok(block_id)
    }

    /// Insert a block from a snapshot (used by drift flush and cross-context injection).
    ///
    /// The snapshot's ID is used as-is if the principal_id matches this store's agent,
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
        principal_id: Option<PrincipalId>,
    ) -> BlockStoreResult<BlockId> {
        #[cfg(test)]
        {
            use std::sync::atomic::Ordering;
            // TEST-ONLY fault injection (see field doc): a countdown of N fails the
            // Nth insert (decrement each call; when the pre-decrement value is 1, the
            // countdown hits 0 on THIS call → error). Single-threaded in the tests
            // that use it, so a plain fetch_sub is sufficient.
            if self.fail_insert_countdown.load(Ordering::SeqCst) > 0 {
                let prev = self.fail_insert_countdown.fetch_sub(1, Ordering::SeqCst);
                if prev == 1 {
                    return Err(BlockStoreError::Db("injected insert fault (test)".into()));
                }
            }
        }
        let after_id = after.cloned();
        let (block_id, final_snapshot, ops, ops_bytes, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = principal_id.unwrap_or_else(|| self.principal_id());
            entry.doc.set_principal_id(effective_agent);

            let block_id = entry.doc.insert_from_snapshot(snapshot, after)?;
            let final_snapshot = entry
                .doc
                .get_block_snapshot(&block_id)
                .ok_or(BlockStoreError::BlockNotFoundAfterInsert)?;

            let ops = SyncPayload::from_new_block(final_snapshot.clone());
            let ops_bytes = codec::encode(&ops)
                .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            let version = entry.version();
            (block_id, final_snapshot, ops, ops_bytes, version)
        };
        self.journal_op(context_id, ops)?;

        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(final_snapshot),
            after_id,
            ops: Arc::from(ops_bytes),
            version,
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
        let (ops, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let principal_id = self.principal_id();
            entry.doc.set_status(block_id, status)?;
            entry.touch(principal_id);
            let version = entry.version();
            let header = entry.doc.get_block_header(block_id).expect(
                "block must exist: the mutation against it just succeeded under this same guard",
            );
            (SyncPayload::from_updated_header(header), version)
        };
        self.journal_op(context_id, ops)?;

        // Emit flow event. Output is not carried here — it is a struct field
        // that can't travel via DTE ops and rides its own `OutputChanged`
        // event (see `set_output`).
        self.emit(BlockFlow::StatusChanged {
            context_id,
            block_id: *block_id,
            status,
            version,
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
            if matches!(
                content_type,
                Some(ContentType::Abc) | Some(ContentType::Svg) | Some(ContentType::Diff)
            ) {
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
        principal_id: Option<PrincipalId>,
    ) -> BlockStoreResult<()> {
        let (ops, change, after_text, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = principal_id.unwrap_or_else(|| self.principal_id());
            entry.doc.set_principal_id(effective_agent);
            // Classify inside the mutation lock (docs/change-feed.md): the
            // before-length is only knowable here, and only here is it stable
            // against another writer.
            let len_before = entry.doc.block_content_len(block_id);
            entry.doc.edit_text(block_id, pos, insert, delete)?;
            entry.touch(effective_agent);
            let len_before = len_before.expect(
                "block must exist: the edit against it just succeeded under this same guard",
            );
            let change = classify_text_edit(len_before, pos, delete);
            // A replace ships the whole after-text; an append ships only the
            // inserted suffix, so it never reads the block back.
            let after_text = match change {
                TextChange::Appended => None,
                TextChange::Replaced => Some(entry.doc.block_text(block_id).expect(
                    "block must exist: the edit against it just succeeded under this same guard",
                )),
            };
            let version = entry.version();
            // The edit we just applied, journaled for the durable oplog
            // exactly as it was applied — never shipped to the wire.
            let ops = SyncPayload::from_text_edit(
                *block_id,
                TextEdit { pos: Some(pos), insert: insert.to_string(), delete },
            );
            (ops, change, after_text, version)
        };
        self.journal_op(context_id, ops)?;

        self.emit_text_change(context_id, block_id, change, after_text, insert, version);

        Ok(())
    }

    /// Publish the classified text change for a mutation that has already been
    /// journaled — commit first, publish second (docs/change-feed.md).
    ///
    /// `after_text` is `Some` exactly when `change` is
    /// [`TextChange::Replaced`]; `inserted` is the suffix for an append.
    fn emit_text_change(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        change: TextChange,
        after_text: Option<String>,
        inserted: &str,
        version: u64,
    ) {
        let flow = match change {
            TextChange::Appended => BlockFlow::TextAppended {
                context_id,
                block_id: *block_id,
                suffix: Arc::from(inserted),
                version,
                source: OpSource::Local,
            },
            TextChange::Replaced => BlockFlow::TextReplaced {
                context_id,
                block_id: *block_id,
                // Moves the owned `String` into the `Arc` rather than copying
                // it: the after-text was just materialized, and a whole extra
                // copy of a block is not free on a large one.
                content: Arc::from(
                    after_text.expect("a replace classification always carries the after-text"),
                ),
                version,
                source: OpSource::Local,
            },
        };
        self.emit(flow);
    }

    /// Set the ephemeral flag on a block (excluded from LLM hydration).
    pub fn set_ephemeral(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        ephemeral: bool,
    ) -> BlockStoreResult<()> {
        let (ops, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            entry.doc.set_ephemeral(block_id, ephemeral)?;
            entry.touch(self.principal_id());
            let version = entry.version();
            let header = entry.doc.get_block_header(block_id).expect(
                "block must exist: the mutation against it just succeeded under this same guard",
            );
            (SyncPayload::from_updated_header(header), version)
        };
        self.journal_op(context_id, ops)?;
        let metadata = self
            .get_block_snapshot(context_id, block_id)
            .ok()
            .flatten()
            .map(|s| s.metadata())
            .unwrap_or_default();
        self.emit(BlockFlow::MetadataChanged {
            context_id,
            block_id: *block_id,
            metadata,
            version,
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
        let (ops, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            entry.doc.set_excluded(block_id, excluded)?;
            entry.touch(self.principal_id());
            let version = entry.version();
            let header = entry.doc.get_block_header(block_id).expect(
                "block must exist: the mutation against it just succeeded under this same guard",
            );
            (SyncPayload::from_updated_header(header), version)
        };
        self.journal_op(context_id, ops)?;
        self.emit(BlockFlow::ExcludedChanged {
            context_id,
            block_id: *block_id,
            excluded,
            version,
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Move a block to a new position.
    ///
    /// `after` is the block to land after, or `None` to land at the beginning.
    /// Wraps the block-document primitive (`crate::blocks::BlockDocument::move_block`)
    /// with FlowBus emission (`BlockFlow::Moved`) and journaling so peers receive ops.
    pub fn move_block(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        after: Option<&BlockId>,
    ) -> BlockStoreResult<()> {
        let after_id = after.cloned();
        let (ops, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            entry.doc.move_block(block_id, after)?;
            entry.touch(self.principal_id());
            let version = entry.version();
            // `order_key` lives on `BlockSnapshot`, not `BlockHeader` — a
            // move was never carried through the journaled payload before
            // this migration either (`ops_since` only ever sent the header
            // for a known block). Unchanged behavior, just reproduced
            // directly instead of via a frontier diff.
            let header = entry.doc.get_block_header(block_id).expect(
                "block must exist: the mutation against it just succeeded under this same guard",
            );
            (SyncPayload::from_updated_header(header), version)
        };
        self.journal_op(context_id, ops)?;
        self.emit(BlockFlow::Moved {
            context_id,
            block_id: *block_id,
            after_id,
            version,
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
        let (ops, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            entry.doc.set_content_type(block_id, content_type)?;
            entry.touch(self.principal_id());
            let version = entry.version();
            let header = entry.doc.get_block_header(block_id).expect(
                "block must exist: the mutation against it just succeeded under this same guard",
            );
            (SyncPayload::from_updated_header(header), version)
        };
        self.journal_op(context_id, ops)?;
        let metadata = self
            .get_block_snapshot(context_id, block_id)
            .ok()
            .flatten()
            .map(|s| s.metadata())
            .unwrap_or_default();
        self.emit(BlockFlow::MetadataChanged {
            context_id,
            block_id: *block_id,
            metadata,
            version,
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Set the task lifecycle status on a `BlockKind::Task` block (household-
    /// agent grooming — docs/tasks.md). Mirrors `set_content_type` exactly:
    /// same per-field LWW clock mechanism, same `MetadataChanged` flow event
    /// (rather than a dedicated flow kind) — a task's status change reaches
    /// every subscribed frontend (app, sibling contexts) the same cheap way
    /// content-type changes already do. Does NOT by itself make the change
    /// visible to the LLM mid-conversation — see `docs/tasks.md`
    /// "Hydration".
    pub fn set_task_status(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        status: TaskStatus,
    ) -> BlockStoreResult<()> {
        let (ops, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            entry.doc.set_task_status(block_id, status)?;
            entry.touch(self.principal_id());
            let version = entry.version();
            let header = entry.doc.get_block_header(block_id).expect(
                "block must exist: the mutation against it just succeeded under this same guard",
            );
            (SyncPayload::from_updated_header(header), version)
        };
        self.journal_op(context_id, ops)?;
        let metadata = self
            .get_block_snapshot(context_id, block_id)
            .ok()
            .flatten()
            .map(|s| s.metadata())
            .unwrap_or_default();
        self.emit(BlockFlow::MetadataChanged {
            context_id,
            block_id: *block_id,
            metadata,
            version,
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Persist the real exit code on a ToolResult block. Shell execution
    /// calls this after the tool returns so `BlockSnapshot::exit_code`
    /// carries the actual value rather than being truncated to the binary
    /// `Status::{Done, Error}`.
    pub fn set_exit_code(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        exit_code: Option<i32>,
    ) -> BlockStoreResult<()> {
        let (ops, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            entry.doc.set_exit_code(block_id, exit_code)?;
            entry.touch(self.principal_id());
            let version = entry.version();
            let header = entry.doc.get_block_header(block_id).expect(
                "block must exist: the mutation against it just succeeded under this same guard",
            );
            (SyncPayload::from_updated_header(header), version)
        };
        self.journal_op(context_id, ops)?;
        let metadata = self
            .get_block_snapshot(context_id, block_id)
            .ok()
            .flatten()
            .map(|s| s.metadata())
            .unwrap_or_default();
        self.emit(BlockFlow::MetadataChanged {
            context_id,
            block_id: *block_id,
            metadata,
            version,
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Persist the standard-error stream on a ToolResult block. The shell
    /// execution path calls this at completion so `BlockSnapshot::stderr`
    /// carries stderr separately from `content` (stdout). Emits
    /// `MetadataChanged` so the value replicates directly to client store
    /// replicas (frontier-independent, reconnect-proof).
    pub fn set_stderr(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        stderr: Option<String>,
    ) -> BlockStoreResult<()> {
        let (ops, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            entry.doc.set_stderr(block_id, stderr)?;
            entry.touch(self.principal_id());
            let version = entry.version();
            // `stderr` is a write-once snapshot field, not part of
            // `BlockHeader` — pre-migration `ops_since` didn't carry it
            // through the journaled payload either (header replay never
            // touched it). Unchanged behavior: the value durably survives
            // the next compaction (a full `BlockSnapshot`), not oplog
            // replay of the window before it. See docs/issues.md.
            let header = entry.doc.get_block_header(block_id).expect(
                "block must exist: the mutation against it just succeeded under this same guard",
            );
            (SyncPayload::from_updated_header(header), version)
        };
        self.journal_op(context_id, ops)?;
        let metadata = self
            .get_block_snapshot(context_id, block_id)
            .ok()
            .flatten()
            .map(|s| s.metadata())
            .unwrap_or_default();
        self.emit(BlockFlow::MetadataChanged {
            context_id,
            block_id: *block_id,
            metadata,
            version,
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Set the reasoning-continuity token on a block (Thinking blocks).
    ///
    /// Write-once at `ThinkingEnd`. Like `stderr`, the value isn't a DTE op and
    /// isn't carried on `BlockMetadata`, so it has no live flow event — it rides
    /// the `StoreSnapshot` (CBOR) used for persistence and fork-copy. That's
    /// all hydration needs: the kernel rebuilds messages from its own block
    /// store, not from the Cap'n Proto wire. See
    /// [`kaijutsu_types::BlockSnapshot::signature`].
    pub fn set_signature(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        signature: Option<String>,
    ) -> BlockStoreResult<()> {
        let ops = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            entry.doc.set_signature(block_id, signature)?;
            entry.touch(self.principal_id());
            // `signature` is a write-once snapshot field, not part of
            // `BlockHeader` — see the same note on `set_stderr` above and
            // docs/issues.md.
            let header = entry.doc.get_block_header(block_id).expect(
                "block must exist: the mutation against it just succeeded under this same guard",
            );
            SyncPayload::from_updated_header(header)
        };
        self.journal_op(context_id, ops)?;
        Ok(())
    }

    /// Set structured output data on a block.
    ///
    /// Output data provides formatting information (tables, trees) for richer output.
    /// Emits the `OutputChanged` flow event — output is not DTE-tracked, so it
    /// rides its own event rather than the block text op stream.
    pub fn set_output(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        output: Option<&kaijutsu_types::OutputData>,
    ) -> BlockStoreResult<()> {
        let (ops, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let principal_id = self.principal_id();
            entry.doc.set_output(block_id, output.cloned())?;
            entry.touch(principal_id);
            let version = entry.version();
            // `output` is a snapshot field, not part of `BlockHeader` — see
            // the same note on `set_stderr` above and docs/issues.md.
            let header = entry.doc.get_block_header(block_id).expect(
                "block must exist: the mutation against it just succeeded under this same guard",
            );
            (SyncPayload::from_updated_header(header), version)
        };
        self.journal_op(context_id, ops)?;
        self.emit(BlockFlow::OutputChanged {
            context_id,
            block_id: *block_id,
            output: output.cloned(),
            version,
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
        let (ops, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let principal_id = self.principal_id();
            entry.doc.set_tool_use_id(block_id, tool_use_id)?;
            entry.touch(principal_id);
            let version = entry.version();
            // `tool_use_id` is a snapshot field, not part of `BlockHeader`
            // — see the same note on `set_stderr` above and docs/issues.md.
            let header = entry.doc.get_block_header(block_id).expect(
                "block must exist: the mutation against it just succeeded under this same guard",
            );
            (SyncPayload::from_updated_header(header), version)
        };
        self.journal_op(context_id, ops)?;
        let metadata = self
            .get_block_snapshot(context_id, block_id)
            .ok()
            .flatten()
            .map(|s| s.metadata())
            .unwrap_or_default();
        self.emit(BlockFlow::MetadataChanged {
            context_id,
            block_id: *block_id,
            metadata,
            version,
            source: OpSource::Local,
        });

        Ok(())
    }

    // =========================================================================
    // Compose drafts (docs/change-feed.md)
    // =========================================================================
    //
    // A draft is a block, not a parallel document. It is an ordinary
    // `Role::User` / `BlockKind::Text` block carrying `Status::Draft` and
    // `ephemeral`, living at the end of its context.
    //
    // The property that matters: **submitting is a status transition, not a
    // copy.** The block a player types into IS the message they send, so there
    // is no window in which the text exists only in a variable — which is
    // exactly where the old `submit_input` could lose it (read draft, clear
    // draft, THEN try to author a block that might fail).
    //
    // One draft per (context, principal): `BlockId` already carries the
    // principal, so two players sharing a context each get their own without
    // any extra key. Their drafts ride the change feed like any other block,
    // which is how a co-player sees you typing.

    /// This principal's draft block in `context_id`, if they have one.
    pub fn draft_block(
        &self,
        context_id: ContextId,
        principal_id: PrincipalId,
    ) -> BlockStoreResult<Option<BlockSnapshot>> {
        let entry = self
            .get(context_id)
            .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        Ok(entry
            .doc
            .blocks_ordered()
            .into_iter()
            .find(|b| b.status == Status::Draft && b.id.principal_id == principal_id))
    }

    /// This principal's draft block, created empty at the end of the document
    /// if they do not have one yet.
    pub fn get_or_create_draft(
        &self,
        context_id: ContextId,
        principal_id: PrincipalId,
    ) -> BlockStoreResult<BlockId> {
        if let Some(existing) = self.draft_block(context_id, principal_id)? {
            return Ok(existing.id);
        }
        let last = self.last_block_id(context_id);
        let id = self.insert_block_as(
            context_id,
            None,
            last.as_ref(),
            Role::User,
            BlockKind::Text,
            "",
            Status::Draft,
            ContentType::Plain,
            Some(principal_id),
        )?;
        // Belt and braces with the `Draft` status: hydration checks both, so a
        // draft cannot reach a model on the strength of one flag.
        self.set_ephemeral(context_id, &id, true)?;
        Ok(id)
    }

    /// Edit this principal's draft, creating it if absent. Character-indexed,
    /// preserving the contract the compose box was already written against.
    pub fn edit_draft(
        &self,
        context_id: ContextId,
        principal_id: PrincipalId,
        pos: usize,
        insert: &str,
        delete: usize,
    ) -> BlockStoreResult<BlockId> {
        let id = self.get_or_create_draft(context_id, principal_id)?;
        self.edit_text_as(context_id, &id, pos, insert, delete, Some(principal_id))?;
        Ok(id)
    }

    /// Promote this principal's draft into a submitted user message.
    ///
    /// Returns the block id and its text. The id is the SAME block they typed
    /// into — nothing is copied, so nothing can be dropped between reading the
    /// text and authoring the message.
    ///
    /// Refuses an empty or whitespace-only draft, leaving it untouched, so a
    /// stray Enter neither sends nothing nor clears what is there.
    pub fn submit_draft(
        &self,
        context_id: ContextId,
        principal_id: PrincipalId,
    ) -> BlockStoreResult<(BlockId, String)> {
        let draft = self
            .draft_block(context_id, principal_id)?
            .ok_or(BlockStoreError::NoDraft(context_id, principal_id))?;
        let text = draft.content.trim().to_string();
        if text.is_empty() {
            return Err(BlockStoreError::EmptyDraft(context_id));
        }
        // Order matters on a crash: `Draft` is the status hydration refuses, so
        // clearing `ephemeral` first and the status second means an interrupted
        // submit leaves a block that is still hidden from the model rather than
        // one that is visible to it but unfinished.
        self.set_ephemeral(context_id, &draft.id, false)?;
        self.set_status(context_id, &draft.id, Status::Done)?;
        Ok((draft.id, text))
    }

    /// Discard this principal's draft. Returns the text that was thrown away,
    /// so a caller can offer it back.
    pub fn clear_draft(
        &self,
        context_id: ContextId,
        principal_id: PrincipalId,
    ) -> BlockStoreResult<String> {
        let Some(draft) = self.draft_block(context_id, principal_id)? else {
            return Ok(String::new());
        };
        let text = draft.content.clone();
        self.delete_block(context_id, &draft.id)?;
        Ok(text)
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
        principal_id: Option<PrincipalId>,
    ) -> BlockStoreResult<()> {
        let (ops, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = principal_id.unwrap_or_else(|| self.principal_id());
            entry.doc.set_principal_id(effective_agent);
            entry.doc.append_text(block_id, text)?;
            entry.touch(effective_agent);
            let version = entry.version();
            // Journaled for the durable oplog — never shipped to the wire.
            // `pos: None` defers "where does this land" to replay time
            // (this block's own length then), so appending never pays for
            // a `content_len()` scan on the streaming hot path.
            let ops = SyncPayload::from_text_edit(
                *block_id,
                TextEdit { pos: None, insert: text.to_string(), delete: 0 },
            );
            (ops, version)
        };
        self.journal_op(context_id, ops)?;

        // Classified as an append *by construction*, not by this function's
        // name (docs/change-feed.md rules 4-5): the primitive underneath
        // computes the end position and deletes nothing, so it satisfies
        // `classify_text_edit`'s predicate for every input.
        //
        // Measuring the before-length here would materialize the whole block a
        // SECOND time per streamed token. Not a second time in place of none:
        // `BlockContent::append_text` already materializes it once to find the
        // end (`blocks/content.rs`), which is a real per-token O(n)
        // this classification neither causes nor cures — it is filed in
        // docs/issues.md and belongs in the text engine. What this avoids is
        // doubling it. `append_emits_exact_suffix` pins the by-construction
        // claim against the engine's real behavior.
        self.emit_text_change(
            context_id,
            block_id,
            TextChange::Appended,
            None,
            text,
            version,
        );

        Ok(())
    }

    /// Set collapsed state for a thinking block.
    pub fn set_collapsed(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        collapsed: bool,
    ) -> BlockStoreResult<()> {
        let (ops, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let principal_id = self.principal_id();
            entry.doc.set_collapsed(block_id, collapsed)?;
            entry.touch(principal_id);
            let version = entry.version();
            let header = entry.doc.get_block_header(block_id).expect(
                "block must exist: the mutation against it just succeeded under this same guard",
            );
            (SyncPayload::from_updated_header(header), version)
        };
        self.journal_op(context_id, ops)?;

        // Emit flow event
        self.emit(BlockFlow::CollapsedChanged {
            context_id,
            block_id: *block_id,
            collapsed,
            version,
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Delete a block from a document.
    pub fn delete_block(&self, context_id: ContextId, block_id: &BlockId) -> BlockStoreResult<()> {
        let (ops, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let principal_id = self.principal_id();
            entry.doc.delete_block(block_id)?;
            entry.touch(principal_id);
            let version = entry.version();
            (SyncPayload::from_deletion(*block_id), version)
        };
        self.journal_op(context_id, ops)?;

        // Emit flow event
        self.emit(BlockFlow::Deleted {
            context_id,
            block_id: *block_id,
            version,
            source: OpSource::Local,
        });

        Ok(())
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

    /// Answer a block query **and** report the context version the answer was
    /// read at, both under one guard.
    ///
    /// The change feed's recovery protocol (docs/change-feed.md rules 21-26)
    /// rests on this atomicity: a client subscribes, fetches this snapshot, and
    /// discards every buffered delivery at or below `version`. Reading the
    /// blocks and the version through two separate calls would let a mutation
    /// land between them — and the client would then either drop a change it
    /// never had or apply one it already has.
    pub fn query_versioned(
        &self,
        context_id: ContextId,
        query: &BlockQuery,
    ) -> BlockStoreResult<(Vec<BlockSnapshot>, u64)> {
        let entry = self
            .get(context_id)
            .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        let blocks = match query {
            BlockQuery::All => entry.doc.blocks_ordered(),
            BlockQuery::ByIds(ids) => snapshots_by_ids(&entry.doc, ids),
            BlockQuery::ByFilter(filter) => snapshots_by_filter(&entry.doc, filter),
        };
        Ok((blocks, entry.version()))
    }

    /// Stage 1 (time-well) incremental live-status read: the cached
    /// per-context reducer over block statuses that drives the time-well
    /// pulse (Running = working, Error = last turn failed), bumped as a side
    /// effect of every mutating block op (see `journal_op`) instead of
    /// re-derived by scanning every block of every context on each 5s poll.
    ///
    /// Cold-cache landmine: the cache is a `DashMap`, empty on a fresh store
    /// (e.g. right after a kernel restart, before any context has been
    /// touched again). A miss lazily computes the correct answer with a
    /// one-time single-context scan and populates the cache — it does NOT
    /// default to `Pending` for an unread context. A context with no
    /// document at all (never existed, or not yet hydrated from the DB) has
    /// no blocks to be `Running`/`Error` about, so `Pending` is correct there.
    pub fn live_status(&self, context_id: ContextId) -> Status {
        if let Some(status) = self.live_status.get(&context_id) {
            return *status;
        }
        let computed = match self.get(context_id) {
            Some(entry) => {
                // Status-only, same reasoning as `journal_op`'s recompute:
                // no need to materialize every block's text for a one-time
                // cold-cache fill.
                derive_context_live_status(&entry.doc.statuses_ordered())
            }
            None => Status::Pending,
        };
        self.live_status.insert(context_id, computed);
        computed
    }

    /// Recompute and cache `context_id`'s live status from its current block
    /// statuses. Called from `journal_op` — the one chokepoint every
    /// mutating block op (insert, status change, edit, merge...) funnels
    /// through — so the cache can never silently drift from the CRDT state
    /// no matter which of the ~20 mutator functions was the actual caller.
    fn recompute_live_status(&self, context_id: ContextId, statuses: &[Status]) {
        self.live_status
            .insert(context_id, derive_context_live_status(statuses));
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
        Ok(snapshots_by_ids(&entry.doc, ids))
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
        Ok(snapshots_by_filter(&entry.doc, filter))
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
        let mut db_guard = db.lock();

        // One-time, dated cleanup for the 2026-08-16 diamond-types-extended
        // cutover, which left a day of oplog rows in a shape this binary cannot
        // decode. It runs here rather than in `KernelDb::open` because deciding
        // "does this row decode?" needs `SyncPayload`, and the payload format is
        // the block store's business, not the SQL layer's — so the SQL layer
        // owns the scan, the transaction and the marker, and we hand it the
        // decoder. It must also run before the replay loop below, which would
        // otherwise poison those documents again on this very boot.
        //
        // This is NOT a general repair: the poison-and-skip branch further down
        // is untouched and still refuses to serve a document with an
        // undecodable op. See `KernelDb::purge_dte_cutover_oplog_rows`.
        let purge = db_guard
            .purge_dte_cutover_oplog_rows(&|bytes| codec::decode::<SyncPayload>(bytes).is_ok())
            .map_err(|e| BlockStoreError::Db(e.to_string()))?;
        if !purge.already_applied {
            tracing::info!(
                examined = purge.examined,
                deleted = purge.deleted,
                documents = purge.documents,
                "One-time 2026-08-16 DTE-cutover oplog cleanup: dropped undecodable oplog rows so their documents can load"
            );
        }

        let docs = db_guard
            .list_documents()
            .map_err(|e| BlockStoreError::Db(e.to_string()))?;
        let principal_id = self.principal_id();

        for doc in docs {
            let context_id = doc.document_id;

            // Load base snapshot if available. `snap_row.version` is the
            // context version the snapshot was taken AT — the durable half of
            // the version this document resumes from (see `base_version`
            // below).
            let (mut crdt_store, base_seq, base_version) = match db_guard.load_latest_snapshot(context_id) {
                Ok(Some(snap_row)) => {
                    match codec::decode::<StoreSnapshot>(&snap_row.state) {
                        Ok(store_snapshot) => {
                            tracing::debug!(
                                document_id = %context_id.to_hex(),
                                blocks = store_snapshot.blocks.len(),
                                snap_seq = snap_row.seq,
                                snap_version = snap_row.version,
                                "Restored document from snapshot"
                            );
                            match BlockDocument::from_snapshot(store_snapshot, principal_id) {
                                Ok(store) => (store, snap_row.seq, snap_row.version.max(0) as u64),
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
                Ok(None) => (BlockDocument::new(context_id, principal_id), 0, 0),
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
            let mut replayed: u64 = 0;
            let mut poisoned = false;
            for (seq, payload_bytes) in &oplog_entries {
                match codec::decode::<SyncPayload>(payload_bytes) {
                    Ok(payload) => {
                        if let Err(e) = crdt_store.merge_ops(payload) {
                            tracing::error!(
                                document_id = %context_id.to_hex(),
                                seq = seq,
                                error = %e,
                                "Failed to replay oplog entry; skipping document so partial state is not served and unreplayed ops are not truncated"
                            );
                            poisoned = true;
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            document_id = %context_id.to_hex(),
                            seq = seq,
                            error = %e,
                            "Failed to deserialize oplog entry; skipping document so partial state is not served and unreplayed ops are not truncated"
                        );
                        poisoned = true;
                        break;
                    }
                }
                // Advance bookkeeping only for entries that actually applied, so
                // `next_journal_seq` never points past an unreplayed entry — a
                // later compaction truncates the oplog up to `next_journal_seq`
                // and would otherwise permanently drop the ops we couldn't replay.
                max_seq = max_seq.max(*seq);
                total_bytes += payload_bytes.len() as u64;
                replayed += 1;
            }

            // A document whose oplog could not be fully replayed is left out of
            // the in-memory store entirely (matching the snapshot-failure paths
            // above) rather than served as coherent-but-partial. Its oplog rows
            // stay intact on disk for recovery.
            if poisoned {
                continue;
            }

            if !oplog_entries.is_empty() {
                tracing::debug!(
                    document_id = %context_id.to_hex(),
                    replayed = replayed,
                    max_seq = max_seq,
                    "Replayed oplog entries"
                );
            }

            // The context version RESUMES; it does not restart. Every
            // mutator bumps the version once under the document guard and
            // journals exactly one op, so the version at the snapshot plus the
            // ops replayed since it is the version this document was at when
            // the kernel stopped. `crdt_store.version()` counts merged
            // payloads instead, which after a restart is "how many ops
            // survived past the last compaction" — a number that starts near
            // zero on a context that had run for weeks.
            //
            // It matters because the version is a client's recovery anchor
            // (docs/change-feed.md rules 21-26) and, in time, the coordinate a
            // repair replay is addressed by. A version that silently rewinds
            // makes both meaningless.
            let version = base_version + replayed;
            let entry = DocumentEntry {
                doc: crdt_store,
                kind: doc.doc_kind,
                language: doc.language.clone(),
                version: AtomicU64::new(version),
                last_agent: RwLock::new(principal_id),
                sync_generation: AtomicU64::new(0),
                next_journal_seq: AtomicU64::new(max_seq as u64),
                uncompacted_count: AtomicU64::new(replayed),
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

        let principal_id = self.principal_id();

        // Load base snapshot if available. `snap_row.version` is the context
        // version it was taken at — see the version note in `load_from_db`.
        let (mut crdt_store, base_seq, base_version) = match db_guard.load_latest_snapshot(context_id) {
            Ok(Some(snap_row)) => {
                match codec::decode::<StoreSnapshot>(&snap_row.state) {
                    Ok(store_snapshot) => {
                        tracing::debug!(
                            document_id = %context_id.to_hex(),
                            blocks = store_snapshot.blocks.len(),
                            snap_seq = snap_row.seq,
                            snap_version = snap_row.version,
                            "Hydrated document from snapshot"
                        );
                        match BlockDocument::from_snapshot(store_snapshot, principal_id) {
                            Ok(store) => (store, snap_row.seq, snap_row.version.max(0) as u64),
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
            Ok(None) => (BlockDocument::new(context_id, principal_id), 0, 0),
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
            match codec::decode::<SyncPayload>(payload_bytes) {
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

        // Resume the version rather than restarting it — see `load_from_db`.
        // Every replayed entry applied (any failure returned above), so the
        // entry count is the number of mutations since the snapshot.
        let version = base_version + oplog_entries.len() as u64;
        let entry = DocumentEntry {
            doc: crdt_store,
            kind: doc.doc_kind,
            language: doc.language.clone(),
            version: AtomicU64::new(version),
            last_agent: RwLock::new(principal_id),
            sync_generation: AtomicU64::new(0),
            next_journal_seq: AtomicU64::new(max_seq as u64),
            uncompacted_count: AtomicU64::new(oplog_entries.len() as u64),
            uncompacted_bytes: AtomicU64::new(total_bytes),
        };

        vacant.insert(entry);
        Ok(true)
    }

    // =========================================================================
    // History (oplog replay) — the `kj diff --from` substrate
    // =========================================================================

    /// The oplog sequence range this document can still be replayed over:
    /// `(oldest, head)`.
    ///
    /// `oldest` is the seq of the latest compaction snapshot (0 when there is
    /// none) — everything before it has been folded into the snapshot and is
    /// gone. `head` is the newest journalled seq. Both ends are inclusive and
    /// reconstructable; `oldest == head` means there is no history to diff yet.
    pub fn oplog_seq_range(&self, context_id: ContextId) -> BlockStoreResult<(i64, i64)> {
        let db = self
            .db
            .as_ref()
            .ok_or(BlockStoreError::NoDatabaseConfigured)?;
        let head = self
            .get(context_id)
            .map(|e| e.next_journal_seq.load(Ordering::SeqCst) as i64)
            .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        let oldest = db
            .lock()
            .load_latest_snapshot(context_id)
            .map_err(|e| BlockStoreError::Db(e.to_string()))?
            .map(|s| s.seq)
            .unwrap_or(0);
        Ok((oldest, head))
    }

    /// Reconstruct one block's text as it stood at oplog sequence `seq`.
    ///
    /// Replays the document's durable history — the latest compaction snapshot
    /// at or before `seq`, then every journalled op up to and including it —
    /// into a throwaway store. Nothing about the live document is touched.
    /// This is what makes any historical pair of a file document derivable, and
    /// therefore what `kj diff --from` stands on.
    ///
    /// It fails loud rather than approximating, because every approximation
    /// here produces a diff against a version that never existed:
    /// - no database → [`BlockStoreError::NoDatabaseConfigured`]: a kernel
    ///   without persistence keeps no history at all.
    /// - a compaction snapshot *newer* than `seq` → [`BlockStoreError::Validation`]:
    ///   the requested point has been compacted away, and the snapshot is not
    ///   a substitute for it.
    /// - the block absent at that point → [`BlockStoreError::Validation`]: it
    ///   had not been created yet (an empty string would read as "the file was
    ///   empty then", which is a different and false claim).
    /// - a `seq` past the journal head → [`BlockStoreError::Validation`]:
    ///   replaying everything and calling the result "seq 99999" would answer a
    ///   question about a version that does not exist with the current one.
    pub fn block_content_at_seq(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        seq: i64,
    ) -> BlockStoreResult<String> {
        let db = self
            .db
            .as_ref()
            .ok_or(BlockStoreError::NoDatabaseConfigured)?;
        let head = self
            .get(context_id)
            .map(|e| e.next_journal_seq.load(Ordering::SeqCst) as i64)
            .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        if seq > head {
            return Err(BlockStoreError::Validation(format!(
                "seq {seq} is past document {}'s journal head ({head})",
                context_id.short()
            )));
        }
        let db_guard = db.lock();
        let principal_id = self.principal_id();

        let (mut replay, base_seq) = match db_guard
            .load_latest_snapshot(context_id)
            .map_err(|e| BlockStoreError::Db(e.to_string()))?
        {
            Some(snap_row) if snap_row.seq > seq => {
                return Err(BlockStoreError::Validation(format!(
                    "history at seq {seq} for document {} has been compacted away \
                     (oldest reconstructable seq is {})",
                    context_id.short(),
                    snap_row.seq
                )));
            }
            Some(snap_row) => {
                let store_snapshot = codec::decode::<StoreSnapshot>(&snap_row.state)
                    .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
                let store = BlockDocument::from_snapshot(store_snapshot, principal_id)?;
                (store, snap_row.seq)
            }
            None => (BlockDocument::new(context_id, principal_id), 0),
        };

        // Replay must be gapless from the snapshot to the requested point.
        // `next_journal_seq` is claimed BEFORE the row commits under the db
        // lock (`journal_op`), so a reader can pass the head bounds check
        // while the row it needs is still in flight — and a failed
        // `append_op` leaves the same hole permanently. Either way, folding
        // whatever rows exist and labeling the result "seq N" would be a
        // version that never existed. Enforce contiguity and fail loud.
        let mut expected = base_seq + 1;
        for (entry_seq, payload_bytes) in db_guard
            .load_oplog_since(context_id, base_seq)
            .map_err(|e| BlockStoreError::Db(e.to_string()))?
        {
            if entry_seq > seq {
                break;
            }
            if entry_seq != expected {
                return Err(BlockStoreError::Validation(format!(
                    "journal for document {} is missing seq {expected} (found {entry_seq}); \
                     seq {seq} cannot be reconstructed — a write may be in flight \
                     (retry), or the row was lost",
                    context_id.short()
                )));
            }
            expected += 1;
            let payload = codec::decode::<SyncPayload>(&payload_bytes)
                .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            replay.merge_ops(payload)?;
        }
        if expected <= seq {
            return Err(BlockStoreError::Validation(format!(
                "journal for document {} ends at seq {} but seq {seq} was requested — \
                 a write may be in flight (retry), or the row was lost",
                context_id.short(),
                expected - 1
            )));
        }

        replay
            .get_block_snapshot(block_id)
            .map(|s| s.content)
            .ok_or_else(|| {
                BlockStoreError::Validation(format!(
                    "block {} did not exist in document {} at seq {seq}",
                    block_id.to_key(),
                    context_id.short()
                ))
            })
    }

    /// Insert a drift block with an explicit author identity.
    ///
    /// There is deliberately no `None`-defaulting wrapper here anymore: the
    /// old `insert_drift_block()` silently fell back to
    /// `BlockStore::principal_id()` (the kernel's own identity), and that
    /// silent default is *how* the drift-authorship smear stayed invisible
    /// for months — no call site ever had to think about who a block
    /// belonged to. `principal_id: None` is still allowed, but every call
    /// site must now write it out and say why (see the identity-smear split,
    /// `docs/issues.md` / commit `b356fc45`).
    pub fn insert_drift_block_as(
        &self,
        context_id: ContextId,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        content: impl Into<String>,
        source_context: ContextId,
        source_model: Option<String>,
        drift_kind: kaijutsu_types::DriftKind,
        principal_id: Option<PrincipalId>,
    ) -> BlockStoreResult<BlockId> {
        let after_id = after.cloned();
        let (block_id, snapshot, ops, ops_bytes, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = principal_id.unwrap_or_else(|| self.principal_id());
            entry.doc.set_principal_id(effective_agent);

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

            let ops = SyncPayload::from_new_block(snapshot.clone());
            let ops_bytes = codec::encode(&ops)
                .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            let version = entry.version();
            (block_id, snapshot, ops, ops_bytes, version)
        };
        self.journal_op(context_id, ops)?;

        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(snapshot),
            after_id,
            ops: Arc::from(ops_bytes),
            version,
            source: OpSource::Local,
        });

        Ok(block_id)
    }

    /// Validate content and attach/update Error child blocks.
    ///
    /// Called when a block's status transitions to Done and its content_type
    /// is Abc, Svg, or Diff. Runs the appropriate parser, compares results
    /// against existing Error children, and inserts/compacts to stay in sync.
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
                .ok_or(BlockStoreError::BlockNotFoundAfterInsert)?
        };

        let new_errors = match snap.content_type {
            ContentType::Abc => validate_abc(&snap.content),
            ContentType::Svg => validate_svg(&snap.content),
            ContentType::Diff => validate_diff(&snap.content),
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
                .filter(|b| b.kind == BlockKind::Error && b.parent_id == Some(*block_id))
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

    /// Insert a notification block (broker-emitted tool/log event).
    ///
    /// Wraps `BlockDocument::insert_notification_block()` with FlowBus
    /// emission and journaling. `parent_id` is typically
    /// `None` (root-level notification tied to the context) but may point at
    /// a specific block when the notification is about that block.
    pub fn insert_notification_block_as(
        &self,
        context_id: ContextId,
        parent_id: Option<&BlockId>,
        payload: &kaijutsu_types::NotificationPayload,
        summary: impl Into<String>,
        principal_id: Option<PrincipalId>,
    ) -> BlockStoreResult<BlockId> {
        let after_id = parent_id.copied();
        let (block_id, snapshot, ops, ops_bytes, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = principal_id.unwrap_or_else(|| self.principal_id());
            entry.doc.set_principal_id(effective_agent);

            let block_id =
                entry
                    .doc
                    .insert_notification_block(parent_id, None, payload, summary)?;
            let snapshot = entry
                .doc
                .get_block_snapshot(&block_id)
                .ok_or(BlockStoreError::BlockNotFoundAfterInsert)?;

            let ops = SyncPayload::from_new_block(snapshot.clone());
            let ops_bytes = codec::encode(&ops)
                .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            let version = entry.version();
            (block_id, snapshot, ops, ops_bytes, version)
        };
        self.journal_op(context_id, ops)?;

        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(snapshot),
            after_id,
            ops: Arc::from(ops_bytes),
            version,
            source: OpSource::Local,
        });

        Ok(block_id)
    }

    /// Insert a resource block (MCP resource read-through — Phase 3, D-43).
    ///
    /// Wraps `BlockDocument::insert_resource_block()` with FlowBus emission
    /// and journaling. `parent_id` is `None` for the initial
    /// read (root block) and `Some(root)` for subscription-update children
    /// emitted by the broker on `ResourceUpdated` flush.
    pub fn insert_resource_block_as(
        &self,
        context_id: ContextId,
        parent_id: Option<&BlockId>,
        payload: &kaijutsu_types::ResourcePayload,
        summary: impl Into<String>,
        principal_id: Option<PrincipalId>,
    ) -> BlockStoreResult<BlockId> {
        let after_id = parent_id.copied();
        let (block_id, snapshot, ops, ops_bytes, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = principal_id.unwrap_or_else(|| self.principal_id());
            entry.doc.set_principal_id(effective_agent);

            let block_id =
                entry
                    .doc
                    .insert_resource_block(parent_id, None, payload, summary)?;
            let snapshot = entry
                .doc
                .get_block_snapshot(&block_id)
                .ok_or(BlockStoreError::BlockNotFoundAfterInsert)?;

            let ops = SyncPayload::from_new_block(snapshot.clone());
            let ops_bytes = codec::encode(&ops)
                .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            let version = entry.version();
            (block_id, snapshot, ops, ops_bytes, version)
        };
        self.journal_op(context_id, ops)?;

        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(snapshot),
            after_id,
            ops: Arc::from(ops_bytes),
            version,
            source: OpSource::Local,
        });

        Ok(block_id)
    }

    /// Insert an error block attached to a parent.
    ///
    /// Wraps `BlockDocument::insert_error_block()` with FlowBus emission
    /// and journaling.
    pub fn insert_error_block_as(
        &self,
        context_id: ContextId,
        parent_id: &BlockId,
        payload: &kaijutsu_types::ErrorPayload,
        summary: impl Into<String>,
        principal_id: Option<PrincipalId>,
    ) -> BlockStoreResult<BlockId> {
        let (block_id, snapshot, ops, ops_bytes, version) = {
            let mut entry = self
                .get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = principal_id.unwrap_or_else(|| self.principal_id());
            entry.doc.set_principal_id(effective_agent);

            let block_id =
                entry
                    .doc
                    .insert_error_block(parent_id, Some(parent_id), payload, summary)?;
            let snapshot = entry
                .doc
                .get_block_snapshot(&block_id)
                .ok_or(BlockStoreError::BlockNotFoundAfterInsert)?;

            let ops = SyncPayload::from_new_block(snapshot.clone());
            let ops_bytes = codec::encode(&ops)
                .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            let version = entry.version();
            (block_id, snapshot, ops, ops_bytes, version)
        };
        self.journal_op(context_id, ops)?;

        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(snapshot),
            after_id: Some(*parent_id),
            ops: Arc::from(ops_bytes),
            version,
            source: OpSource::Local,
        });

        Ok(block_id)
    }
}

/// Derive a context's *live* status from its block statuses in timeline order
/// (as returned by `block_snapshots`, i.e. `blocks_ordered`):
///
/// - any block `Running` → `Running` (the context is actively working);
/// - else the tail block `Error` → `Error` (its most recent turn failed);
/// - else `Pending` (idle — no rim in the time well).
///
/// Non-sticky by construction: a new turn appends a `Running` block, so a past
/// error is superseded the moment work resumes. Pure over the ordered statuses
/// so it is unit-testable without a block store.
///
/// Shared single-context reducer: `BlockStore::live_status` /
/// `recompute_live_status` (this crate) and the `listContexts` poll site
/// (`kaijutsu-server`, which re-exports this fn) both call the same logic —
/// there is exactly one place this derivation is written.
pub fn derive_context_live_status(statuses_in_order: &[Status]) -> Status {
    if statuses_in_order.contains(&Status::Running) {
        Status::Running
    // The tail check skips drafts deliberately. A compose draft is a block and
    // it lives at the END of the document, so a naive `.last()` would find the
    // draft instead of the failed turn behind it and report a broken context as
    // idle. A draft is not the context's work; it is someone mid-sentence.
    } else if statuses_in_order
        .iter()
        .rev()
        .find(|s| **s != Status::Draft)
        == Some(&Status::Error)
    {
        Status::Error
    } else {
        Status::Pending
    }
}

/// Snapshots for specific block IDs, in the order asked for; unknown IDs are
/// skipped. Takes the document so a caller holding one guard can answer a query
/// and read the version without releasing it.
fn snapshots_by_ids(doc: &BlockDocument, ids: &[BlockId]) -> Vec<BlockSnapshot> {
    let mut result = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(snap) = doc.get_block_snapshot(id) {
            result.push(snap);
        }
    }
    result
}

/// Snapshots matching a filter, in document order.
///
/// If `filter.parent_id` is set, only descendants (up to `max_depth`) are
/// considered. Otherwise iterates all blocks in order, applying the predicate.
fn snapshots_by_filter(doc: &BlockDocument, filter: &BlockFilter) -> Vec<BlockSnapshot> {
    // If parent_id is set, compute descendant set via BFS
    let descendant_ids = filter
        .parent_id
        .as_ref()
        .map(|root_id| compute_descendants(doc, root_id, filter.max_depth));

    let mut result = Vec::new();
    let limit = if filter.limit > 0 {
        filter.limit as usize
    } else {
        usize::MAX
    };

    for block in doc.blocks_ordered() {
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

    result
}

/// How a text mutation changed a block's text (docs/change-feed.md).
///
/// Decided while the mutation lock is held, because that is the only place the
/// before-text and the edit coordinates are both in hand. Downstream the change
/// is opaque operation bytes, and a bridge that classified those bytes would
/// have to link the text engine this migration exists to remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextChange {
    /// The after-text starts with the before-text; the feed ships the suffix.
    Appended,
    /// Anything else; the feed ships the whole after-text.
    Replaced,
}

/// Classify a text edit from its coordinates against the block's pre-edit
/// character length.
///
/// **Never classify by the name of the function or the tool that made the
/// edit** (docs/change-feed.md rules 4 and 5). `edit_text_as` makes appends —
/// MCP `block_append` is one — and an earlier draft that listed producers named
/// one of five. Coordinates cannot miss a producer.
///
/// Deliberately conservative in one direction: an edit that deletes text and
/// re-inserts it verbatim (`pos=0, delete=3, insert="abc"` over `"abc…"`) is
/// reported as a replace even though the after-text does start with the
/// before-text. That costs bandwidth and can never corrupt. The opposite
/// mistake — reporting a replace as an append — is the one that corrupts a
/// client's text, and these coordinates cannot produce it.
pub(crate) fn classify_text_edit(len_before: usize, pos: usize, delete: usize) -> TextChange {
    if delete == 0 && pos == len_before {
        TextChange::Appended
    } else {
        TextChange::Replaced
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

/// Validate unified-diff content via `kaijutsu_diff::parse`, returning an
/// `ErrorPayload` when it does not parse.
///
/// The *producers* already refuse bad input — `diff_block` pre-validates and
/// `kj diff` only ever emits `format()` output — so this arm exists for the
/// block typed `Diff` by some other route: a hand-rolled `block_create` +
/// content-type set, a client, or a concurrent LWW type flip (content and
/// content_type are separate registers, so "declared a diff, holds something
/// else" is a legitimate CRDT state, not a bug to be assumed away). Such a
/// block used to land with nothing visible saying so.
///
/// One error at most: [`kaijutsu_diff::parse`] refuses at the first construct
/// it cannot model rather than accumulating diagnostics, and that is
/// deliberate — a parser that skips a section produces a smaller diff that
/// still looks complete.
///
/// Default options are fine here even though the app parses with an explicit
/// `DiffProfile`: options only steer word-span refinement, never whether text
/// parses, so validity is profile-independent by construction.
fn validate_diff(content: &str) -> Vec<(kaijutsu_types::ErrorPayload, String)> {
    let Err(e) = kaijutsu_diff::parse(content) else {
        return vec![];
    };
    let summary = format!("Diff parse error: {e}");
    let payload = kaijutsu_types::ErrorPayload {
        category: kaijutsu_types::ErrorCategory::Parse,
        severity: kaijutsu_types::ErrorSeverity::Error,
        code: Some(format!("diff.{}", e.variant_name())),
        detail: Some(e.to_string()),
        // `DiffError::line` is 1-based in the *normalized* text (CRLF and bare
        // CR both become LF on ingest), which is the position the app's model
        // holds too. Column is not tracked — a diff rejection is about a whole
        // line's shape, never a character within it.
        span: e.line().map(|line| kaijutsu_types::ErrorSpan {
            line: line as u32,
            column: 0,
            length: 0,
        }),
        source_kind: Some(BlockKind::Text),
    };
    vec![(payload, summary)]
}

fn compute_descendants(
    doc: &BlockDocument,
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
pub fn shared_block_store(principal_id: PrincipalId) -> SharedBlockStore {
    Arc::new(BlockStore::new(principal_id))
}

/// Create a shared block store with database persistence.
pub fn shared_block_store_with_db(
    db: DbHandle,
        default_workspace_id: WorkspaceId,
    principal_id: PrincipalId,
) -> SharedBlockStore {
    Arc::new(BlockStore::with_db(
        db,
                default_workspace_id,
        principal_id,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_agent() -> PrincipalId {
        PrincipalId::new()
    }

    // ========================================================================
    // Stage 1 (time-well) incremental live_status — cached per-context
    // reducer over block statuses, bumped on every mutating op (journal_op
    // is the one chokepoint), instead of the every-poll full block scan.
    // ========================================================================

    #[test]
    fn live_status_defaults_to_pending_for_unknown_context() {
        let store = BlockStore::new(test_agent());
        let missing = ContextId::new();
        assert_eq!(store.live_status(missing), Status::Pending);
    }

    #[test]
    fn live_status_running_after_insert_running_block() {
        let store = BlockStore::new(test_agent());
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        // This test never calls block_snapshots itself — live_status must be
        // correct off the cache alone, bumped as a side effect of insert_block.
        store
            .insert_block(
                ctx,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "working...",
                Status::Running,
                ContentType::Plain,
            )
            .unwrap();
        assert_eq!(store.live_status(ctx), Status::Running);
    }

    #[test]
    fn live_status_done_reverts_running_block_to_pending() {
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
                BlockKind::Text,
                "working...",
                Status::Running,
                ContentType::Plain,
            )
            .unwrap();
        assert_eq!(store.live_status(ctx), Status::Running);

        store.set_status(ctx, &block_id, Status::Done).unwrap();
        assert_eq!(
            store.live_status(ctx),
            Status::Pending,
            "the only Running block finishing must revert live_status to Pending"
        );
    }

    #[test]
    fn live_status_error_on_last_block() {
        let store = BlockStore::new(test_agent());
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let first = store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text, "ok",
                Status::Done, ContentType::Plain,
            )
            .unwrap();
        let last = store
            .insert_block(
                ctx, None, Some(&first), Role::Model, BlockKind::Text, "boom",
                Status::Running, ContentType::Plain,
            )
            .unwrap();
        assert_eq!(store.live_status(ctx), Status::Running);

        store.set_status(ctx, &last, Status::Error).unwrap();
        assert_eq!(
            store.live_status(ctx),
            Status::Error,
            "Error on the tail (last) block must surface as live_status Error"
        );
    }

    /// silently default to `Pending` for everything.
    #[test]
    fn live_status_cold_cache_after_boot_is_correct_not_pending_default() {
        use crate::kernel_db::KernelDb;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cold_cache.db");
        let creator = PrincipalId::system();

        let ctx = {
            let db = Arc::new(parking_lot::Mutex::new(KernelDb::open(&db_path).unwrap()));
            let ws_id = db.lock().get_or_create_default_workspace(creator).unwrap();
            let store1 = BlockStore::with_db(db.clone(), ws_id, creator);
            let ctx = ContextId::new();
            store1
                .create_document(ctx, DocumentKind::Conversation, None)
                .unwrap();
            store1
                .insert_block(
                    ctx, None, None, Role::Model, BlockKind::Text, "still going",
                    Status::Running, ContentType::Plain,
                )
                .unwrap();
            assert_eq!(store1.live_status(ctx), Status::Running);
            // Drop store1 + db (unclean, simulating a kernel restart) before
            // ever touching store2's cache.
            ctx
        };

        // "Boot": a brand-new BlockStore, fresh (empty) live_status DashMap,
        // backed by the same on-disk DB.
        let db2 = Arc::new(parking_lot::Mutex::new(KernelDb::open(&db_path).unwrap()));
        let ws_id2 = db2.lock().get_or_create_default_workspace(creator).unwrap();
        let store2 = BlockStore::with_db(db2, ws_id2, creator);
        store2.load_from_db().expect("load_from_db");

        // First read after boot, no mutation yet — must lazily compute the
        // correct answer (Running), not a stale/default Pending.
        assert_eq!(
            store2.live_status(ctx),
            Status::Running,
            "cold cache must lazily populate the correct status on first read"
        );
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
    fn reserve_block_id_and_max_tick_passthrough() {
        // Kernel passthrough for the materialization path (design §3, §4):
        // reserve_block_id claims a fresh seq under an explicit principal (the
        // player, or beat() for fallbacks), and max_tick reports the high-water
        // tick over live blocks so arm can seed the playhead.
        use kaijutsu_types::Tick;

        let store = BlockStore::new(test_agent());
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        // A foreign principal (e.g. a player on a chair) — reserve claims and
        // advances its own lane, independent of the store principal.
        let player = PrincipalId::new();
        let first = store.reserve_block_id(ctx, player).unwrap();
        assert_eq!(first.seq, 0);
        assert_eq!(first.principal_id, player);
        let second = store.reserve_block_id(ctx, player).unwrap();
        assert_eq!(second.seq, 1, "second reserve mints +1 even without an insert");

        // Missing document is a loud error, not a silent default.
        let missing = ContextId::new();
        match store.reserve_block_id(missing, player) {
            Err(BlockStoreError::DocumentNotFound(id)) => assert_eq!(id, missing),
            other => panic!("expected DocumentNotFound, got {:?}", other),
        }

        // max_tick: empty doc → None.
        assert_eq!(store.max_tick(ctx).unwrap(), None);

        // Insert blocks carrying ticks; max_tick reports the high-water.
        for t in [0i64, 3, 1] {
            let snap = kaijutsu_types::BlockSnapshotBuilder::new(
                BlockId::new(ctx, player, store.reserve_block_id(ctx, player).unwrap().seq),
                BlockKind::Text,
            )
            .tick(Tick::new(t))
            .order_key(format!("V{:0>11}AAAA", t))
            .content("c")
            .build();
            store
                .insert_from_snapshot_as(ctx, snap, None, Some(player))
                .unwrap();
        }
        assert_eq!(store.max_tick(ctx).unwrap(), Some(Tick::new(3)), "max_tick is the live high-water");
    }

    #[test]
    fn test_move_block_reorders_and_emits_flow() {
        // Move primitive at the kernel block-store layer (M2-B1):
        // CRDT layer already implements `move_block`; verify the wrapper
        // updates ordering, bumps the version, and journals an op.
        let store = BlockStore::new(test_agent());
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();
        let a = store
            .insert_block(
                ctx,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "A",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let b = store
            .insert_block(
                ctx,
                None,
                Some(&a),
                Role::User,
                BlockKind::Text,
                "B",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let c = store
            .insert_block(
                ctx,
                None,
                Some(&b),
                Role::User,
                BlockKind::Text,
                "C",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let v_before = store.version(ctx).unwrap();
        // Move c to the front (no `after` → beginning of the doc).
        store.move_block(ctx, &c, None).unwrap();
        let v_after = store.version(ctx).unwrap();
        assert!(v_after > v_before, "version should advance after move");

        let order: Vec<_> = store
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .map(|b| b.id)
            .collect();
        assert_eq!(
            order,
            vec![c, a, b],
            "moved block should appear at the beginning"
        );
    }

    #[test]
    fn test_block_store_basic_ops() {
        let store = BlockStore::new(test_agent());
        let ctx = ContextId::new();

        store
            .create_document(ctx, DocumentKind::File, Some("rust".into()))
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
            .create_document(ctx, DocumentKind::File, Some("rust".into()))
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
        let source = ContextId::new();
        let config = ContextId::new();

        store
            .create_document(conv1, DocumentKind::Conversation, None)
            .unwrap();
        store
            .create_document(conv2, DocumentKind::Conversation, None)
            .unwrap();
        store
            .create_document(source, DocumentKind::File, Some("rust".into()))
            .unwrap();
        store
            .create_document(config, DocumentKind::File, None)
            .unwrap();

        assert_eq!(store.list_ids().len(), 4);

        let convs = store.list_ids_by_kind(DocumentKind::Conversation);
        assert_eq!(convs.len(), 2);
        assert!(convs.contains(&conv1));
        assert!(convs.contains(&conv2));

        // A rust source file and a config file are both `File` — the
        // `language` field carries the difference, so the filter returns both.
        let files = store.list_ids_by_kind(DocumentKind::File);
        assert_eq!(files.len(), 2);
        assert!(files.contains(&source));
        assert!(files.contains(&config));

        assert!(store.list_ids_by_kind(DocumentKind::Symlink).is_empty());
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
            .create_document(ctx, DocumentKind::File, None)
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
                .create_document(ctx, DocumentKind::File, None)
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
            .create_document(ctx, DocumentKind::File, None)
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
    // - Client (BlockDocument) can merge these payloads after initial snapshot sync

    use crate::flows::{BlockFlow, FlowBus, SharedBlockFlowBus};
    use std::sync::Arc;

    /// Helper to create a BlockStore with FlowBus for testing.
    fn store_with_flows() -> (BlockStore, SharedBlockFlowBus) {
        let bus: SharedBlockFlowBus = Arc::new(FlowBus::new(256));
        let store = BlockStore::with_flows(test_agent(), bus.clone());
        (store, bus)
    }

    /// `set_output` must publish a dedicated `OutputChanged` flow event — not
    /// piggyback the output on `StatusChanged`. A subscriber on the bus should
    /// see the structured output ride its own event, which is what the server
    /// bridge encodes onto the wire as `onBlockOutputChanged`.
    #[tokio::test]
    async fn test_set_output_emits_output_changed() {
        use kaijutsu_types::{OutputData, OutputNode};

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
                Role::Tool,
                BlockKind::ToolResult,
                "",
                Status::Running,
                ContentType::Plain,
            )
            .unwrap();
        // Drain the BlockInserted event.
        while sub.try_recv().is_some() {}

        let output = OutputData::nodes(vec![OutputNode::text("row-one")]);
        store.set_output(ctx, &block_id, Some(&output)).unwrap();

        // The next event must be OutputChanged carrying our output — never a
        // StatusChanged with a piggybacked output field (that field is gone).
        let msg = sub.try_recv().expect("set_output should emit a flow event");
        match msg.payload {
            BlockFlow::OutputChanged { output: got, .. } => {
                assert_eq!(
                    got,
                    Some(output),
                    "OutputChanged must carry the structured output",
                );
            }
            other => panic!("expected OutputChanged, got: {other:?}"),
        }
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
        let mut client = BlockDocument::from_snapshot(snapshot, PrincipalId::new()).unwrap();
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
            codec::decode(&ops).expect("should deserialize SyncPayload");
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

    /// e2e: a coder turn gets monotonic per-context timeline ticks that survive a
    /// persistence (snapshot → reload) roundtrip — the recovery path the kernel
    /// runs on restart. Exercises CRDT tick assignment → BlockSnapshot → CBOR
    /// StoreSnapshot → from_snapshot (+ normalize).
    #[test]
    fn test_coder_turn_ticks_survive_persistence_roundtrip() {
        let store = BlockStore::new(test_agent());
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        // A coder turn: user prompt, model reply, then two streamed model lines —
        // each appended after the previous (the streaming hot path).
        let turn = ["fix the bug", "on it", "patching foo.rs", "done"];
        let roles = [Role::User, Role::Model, Role::Model, Role::Model];
        let mut prev: Option<BlockId> = None;
        for (text, role) in turn.iter().zip(roles) {
            prev = Some(
                store
                    .insert_block(
                        ctx,
                        None,
                        prev.as_ref(),
                        role,
                        BlockKind::Text,
                        *text,
                        Status::Done,
                        ContentType::Plain,
                    )
                    .unwrap(),
            );
        }

        // Ticks are monotonic, gap-free, in turn order.
        let live = store.block_snapshots(ctx).unwrap();
        let ticks: Vec<i64> = live.iter().map(|b| b.tick.unwrap().get()).collect();
        assert_eq!(ticks, vec![0, 1, 2, 3], "coder turn gets monotonic ticks");

        // Persist → reload, exactly as the kernel recovers a context on restart.
        let snapshot = store.get(ctx).unwrap().doc.snapshot();
        let bytes = codec::encode(&snapshot).unwrap();
        let restored: StoreSnapshot = codec::decode(&bytes).unwrap();
        let reloaded = BlockDocument::from_snapshot(restored, test_agent()).unwrap();

        let rblocks = reloaded.blocks_ordered();
        let rticks: Vec<i64> = rblocks.iter().map(|b| b.tick.unwrap().get()).collect();
        assert_eq!(rticks, vec![0, 1, 2, 3], "ticks survive the persistence roundtrip");
        let rtexts: Vec<String> = rblocks.iter().map(|b| b.content.clone()).collect();
        assert_eq!(rtexts, turn.to_vec(), "order preserved across reload");
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
        let mut client = BlockDocument::from_snapshot(snapshot, PrincipalId::new()).unwrap();

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

        let payload: SyncPayload = codec::decode(&ops).unwrap();
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
        let mut client = BlockDocument::from_snapshot(snapshot, PrincipalId::new()).unwrap();

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

            let payload: SyncPayload = codec::decode(&ops).unwrap();
            client
                .merge_ops(payload)
                .unwrap_or_else(|e| panic!("should merge block {i}: {e}"));
        }

        assert_eq!(client.block_count(), 5);

        let server_blocks = store.block_snapshots(ctx).unwrap();
        let client_blocks = client.blocks_ordered();
        for (sb, cb) in server_blocks.iter().zip(client_blocks.iter()) {
            assert_eq!(sb.content, cb.content);
        }
    }

    /// Streaming append (`append_text`) does two independent things per
    /// chunk: publish the classified `TextAppended` flow event (the live
    /// path), and journal a `TextEdit` to the oplog (the durable-recovery
    /// path — `test_drop_reload_after_append_chain` covers many-chunk
    /// correctness of that path end to end via `load_from_db`). This test
    /// checks both from one streaming run: the events carry the right
    /// suffixes, and replaying the REAL journaled oplog rows (not a
    /// hand-built payload) into a fresh store reconstructs the same
    /// content — the property that used to be named "mergeable
    /// SyncPayload".
    #[tokio::test]
    async fn test_text_streaming_sync_payload() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("stream.db");
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::open(&db_path).unwrap()));
        let creator = PrincipalId::system();
        let ws_id = db.lock().get_or_create_default_workspace(creator).unwrap();
        let bus: SharedBlockFlowBus = Arc::new(FlowBus::new(256));
        let store = BlockStore::with_db_and_flows(db.clone(), ws_id, creator, bus.clone());
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

        let chunks = ["Hello", " ", "World", "!"];
        for chunk in chunks {
            store.append_text(ctx, &block_id, chunk).unwrap();

            // An append publishes the classified `TextAppended` event (the
            // `TextOps` wire event this test used to check alongside it was
            // deleted in the 2026-08-15 flag day, docs/change-feed.md).
            let msg = sub.try_recv().expect("should receive the classified event");
            match msg.payload {
                BlockFlow::TextAppended { ref suffix, .. } => assert_eq!(&**suffix, chunk),
                _ => panic!("expected TextAppended event, got {:?}", msg.payload),
            }
        }

        // Replay the REAL journaled oplog (insert + 4 appends) into a fresh
        // store, the same primitive `load_from_db` uses.
        let mut client = BlockDocument::new(ctx, PrincipalId::new());
        let oplog = db.lock().load_oplog_since(ctx, 0).unwrap();
        assert_eq!(oplog.len(), 5, "insert + 4 appends should each journal one entry");
        for (_seq, payload_bytes) in &oplog {
            let payload: SyncPayload = codec::decode(payload_bytes).expect("decode oplog entry");
            client.merge_ops(payload).expect("replay oplog entry");
        }

        let snapshot = client.get_block_snapshot(&block_id).unwrap();
        assert_eq!(snapshot.content, "Hello World!");
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

    /// Stage 1 (time-well) kernel truth: any mutating block op — funneled
    /// through the one `journal_op` chokepoint — must stamp the context row's
    /// `last_activity_at`, strictly after `created_at` and no earlier than a
    /// `t0` recorded just before the op. Proves the wiring from
    /// `BlockStore::journal_op` into `KernelDb::touch_context_activity`.
    #[test]
    fn journal_op_stamps_context_last_activity_at() {
        use crate::kernel_db::{ContextRow, KernelDb};
        use kaijutsu_types::{ConsentMode, ContextState};

        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let creator = PrincipalId::system();
        let ws_id = db.lock().get_or_create_default_workspace(creator).unwrap();

        let store = BlockStore::with_db(db.clone(), ws_id, creator);
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        // Context row, created safely in the past so `t0` (below) can't tie
        // with it at millisecond resolution.
        let created_at = kaijutsu_types::now_millis() as i64 - 60_000;
        db.lock()
            .insert_context(&ContextRow {
                context_id: ctx,
                label: None,
                provider: None,
                model: None,
                system_prompt: None,
                consent_mode: ConsentMode::Collaborative,
                context_state: ContextState::Live,
                context_type: "default".to_string(),
                created_at,
                created_by: creator,
                forked_from: None,
                fork_kind: None,
                archived_at: None,
                workspace_id: Some(ws_id),
                preset_id: None,
                concluded_at: None,
                last_activity_at: None,
                promoted_at: None,
                demoted_at: None,
                paused_at: None,
                cast_id: None,
                origin_host: None,
            })
            .unwrap();

        let row = db.lock().get_context(ctx).unwrap().unwrap();
        assert_eq!(row.last_activity_at, None, "untouched row starts unstamped");

        let t0 = kaijutsu_types::now_millis() as i64;
        store
            .insert_block(
                ctx,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "activity stamp",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        let row = db.lock().get_context(ctx).unwrap().unwrap();
        let stamped = row
            .last_activity_at
            .expect("insert_block must stamp last_activity_at via journal_op");
        assert!(stamped >= t0, "stamp {stamped} should be >= t0 {t0}");
        assert!(
            stamped > created_at,
            "stamp {stamped} should be after created_at {created_at}"
        );

        // A second mutating op (set_status) re-stamps forward.
        let block_id = {
            let entry = store.get(ctx).unwrap();
            entry.doc.blocks_ordered().last().unwrap().id
        };
        std::thread::sleep(std::time::Duration::from_millis(2));
        let t1 = kaijutsu_types::now_millis() as i64;
        store.set_status(ctx, &block_id, Status::Done).unwrap();
        let row2 = db.lock().get_context(ctx).unwrap().unwrap();
        assert!(row2.last_activity_at.unwrap() >= t1);
    }

    /// A store that declares persistence (`with_db*`) but reaches a journaling
    /// write with no db handle must FAIL LOUD, not silently drop the op — the
    /// historical `return Ok(())` footgun, crash over corruption. Replica
    /// stores (`new`/`with_flows`) keep their legitimate no-op, exercised
    /// implicitly by every db-less test in this module.
    #[test]
    fn persistent_store_journaling_without_db_fails_loud() {
        let store = BlockStore::persistent_without_db(PrincipalId::system());
        let ctx = ContextId::new();
        // create_document is metadata-only (no journaling write), so it
        // succeeds even db-less; it sets the doc up so insert_block reaches
        // journal_op.
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .expect("create_document is metadata-only; no journaling write");

        let err = store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "must not be silently dropped", Status::Done, ContentType::Plain,
            )
            .expect_err("a persistent store with no db must fail loud at journal time");
        assert!(
            matches!(err, BlockStoreError::NoDatabaseConfigured),
            "expected NoDatabaseConfigured, got {err:?}",
        );
    }

    /// `block_content_at_seq` promises to fail loud rather than reconstruct a
    /// version that never existed. A gapped oplog is exactly that case, and it
    /// has two real producers: the TOCTOU window in `journal_op` (seq claimed
    /// from `next_journal_seq` before the row commits under the db lock) and a
    /// row lost to a failed `append_op`. Both leave the counter pointing past
    /// rows the db doesn't hold; replay must refuse, not silently return the
    /// state minus the missing op.
    #[test]
    fn content_at_seq_refuses_a_gapped_oplog() {
        use crate::kernel_db::KernelDb;

        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let creator = PrincipalId::system();
        let ws_id = db.lock().get_or_create_default_workspace(creator).unwrap();
        let store = BlockStore::with_db(db.clone(), ws_id, creator);
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let first = store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "one", Status::Done, ContentType::Plain,
            )
            .unwrap();
        for content in ["two", "three"] {
            store
                .insert_block(
                    ctx, None, None, Role::User, BlockKind::Text,
                    content, Status::Done, ContentType::Plain,
                )
                .unwrap();
        }

        // Sanity: with an intact journal, every seq reconstructs.
        assert_eq!(store.block_content_at_seq(ctx, &first, 1).unwrap(), "one");
        assert_eq!(store.block_content_at_seq(ctx, &first, 3).unwrap(), "one");

        // A mid-journal hole: seqs at or past it are unreconstructable...
        db.lock().delete_oplog_row_for_test(ctx, 2).unwrap();
        let err = store.block_content_at_seq(ctx, &first, 3).unwrap_err();
        assert!(
            matches!(err, BlockStoreError::Validation(_)),
            "a gapped replay must be a loud Validation error, got {err:?}",
        );
        let err = store.block_content_at_seq(ctx, &first, 2).unwrap_err();
        assert!(
            matches!(err, BlockStoreError::Validation(_)),
            "the missing seq itself must be a loud Validation error, got {err:?}",
        );

        // ...but history before the hole is still honestly answerable.
        assert_eq!(store.block_content_at_seq(ctx, &first, 1).unwrap(), "one");
    }

    /// The head-row variant of the gap: `next_journal_seq` says N exists but
    /// the row hasn't committed (or was lost). This is the observable state of
    /// the `journal_op` race from a reader's chair — the bounds check passes,
    /// the row is absent — and it must be a loud error, not the state at N−1
    /// wearing N's label.
    #[test]
    fn content_at_seq_refuses_an_uncommitted_head_row() {
        use crate::kernel_db::KernelDb;

        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let creator = PrincipalId::system();
        let ws_id = db.lock().get_or_create_default_workspace(creator).unwrap();
        let store = BlockStore::with_db(db.clone(), ws_id, creator);
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let block = store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "committed", Status::Done, ContentType::Plain,
            )
            .unwrap();
        store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "in flight", Status::Done, ContentType::Plain,
            )
            .unwrap();

        db.lock().delete_oplog_row_for_test(ctx, 2).unwrap();
        let err = store.block_content_at_seq(ctx, &block, 2).unwrap_err();
        assert!(
            matches!(err, BlockStoreError::Validation(_)),
            "an absent head row must be a loud Validation error, got {err:?}",
        );
        // The last fully-journalled point still answers.
        assert_eq!(
            store.block_content_at_seq(ctx, &block, 1).unwrap(),
            "committed",
        );
    }

    /// Durability across an *unclean* kill: a live store commits via the real
    /// insert/append paths, then both the store and its SQLite connection are
    /// leaked (never closed, never checkpointed) to simulate SIGKILL. A brand
    /// new connection opening the same files must recover everything from the
    /// on-disk main file + WAL via `load_from_db`. Distinct from the
    /// `test_drop_reload_*` tests above, which reuse the live connection and so
    /// never exercise fresh-open WAL recovery.
    #[test]
    fn test_durability_across_kill() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("kill.db");
        let creator = PrincipalId::system();
        let ctx = ContextId::new();

        // Phase 1 — a "live kernel" writes, then dies without a clean close.
        let ws_id = {
            let db = Arc::new(parking_lot::Mutex::new(
                KernelDb::open(&db_path).expect("open DB"),
            ));
            let ws_id = db
                .lock()
                .get_or_create_default_workspace(creator)
                .expect("create workspace");
            let store = BlockStore::with_db(db.clone(), ws_id, creator);
            store
                .create_document(ctx, DocumentKind::Conversation, None)
                .expect("create document");
            let block_id = store
                .insert_block(
                    ctx, None, None, Role::User, BlockKind::Text,
                    "before the kill", Status::Done, ContentType::Plain,
                )
                .expect("insert block");
            store
                .append_text(ctx, &block_id, " — appended")
                .expect("append text");

            // SIGKILL simulation: leak both the in-memory CRDT state and the
            // SQLite connection so neither is cleanly closed or checkpointed.
            // Whatever was committed must survive in the on-disk files alone.
            std::mem::forget(store);
            std::mem::forget(db);
            ws_id
        };

        // Phase 2 — a fresh connection opens the same files and recovers.
        let db2 = Arc::new(parking_lot::Mutex::new(
            KernelDb::open(&db_path).expect("reopen DB"),
        ));
        let store2 = BlockStore::with_db(db2, ws_id, creator);
        store2.load_from_db().expect("load_from_db");

        let content = store2.get_content(ctx).expect("get content");
        assert_eq!(
            content, "before the kill — appended",
            "committed blocks must survive an unclean kill (recovered from WAL)",
        );
    }

    // ========================================================================
    // OPLOG PERSISTENCE TESTS — drop-and-reload, per-mutation journal, compaction
    // ========================================================================

    /// Helper: unique KernelId for test isolation.
    /// Create a DB-backed store backed by an on-disk SQLite file inside `dir`.
    /// Returns (db_handle, block_store, context_id, workspace_id).
    fn fresh_db_store(
        dir: &std::path::Path,
    ) -> (DbHandle, BlockStore, ContextId, WorkspaceId) {
        let db_path = dir.join("test.db");
        let db = Arc::new(parking_lot::Mutex::new(
            KernelDb::open(&db_path).expect("open DB"),
        ));
        let creator = PrincipalId::system();

        let ws_id = {
            let db_guard = db.lock();
            db_guard
                .get_or_create_default_workspace(creator)
                .expect("create workspace")
        };

        let store = BlockStore::with_db(db.clone(), ws_id, creator);
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .expect("create document");

        (db, store, ctx, ws_id)
    }

    /// Drop the store, create a new one from the same DB, call load_from_db.
    fn drop_and_reload(
        db: DbHandle,
                ws_id: WorkspaceId,
    ) -> BlockStore {
        let creator = PrincipalId::system();
        let store2 = BlockStore::with_db(db, ws_id, creator);
        store2.load_from_db().expect("load_from_db");
        store2
    }

    // ====================================================================
    // 1. Crash-Recovery: drop + reload
    // ====================================================================

    #[test]
    fn test_drop_reload_simple() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, ws) = fresh_db_store(dir.path());

        store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "hello world", Status::Done, ContentType::Plain,
            )
            .unwrap();

        drop(store); // destroy in-memory state

        let store2 = drop_and_reload(db, ws);
        let content = store2.get_content(ctx).unwrap();
        assert_eq!(content, "hello world", "content should survive drop+reload");
    }

    #[test]
    fn test_drop_reload_after_append_chain() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, ws) = fresh_db_store(dir.path());

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

        let store2 = drop_and_reload(db, ws);
        let content_after = store2.get_content(ctx).unwrap();
        assert_eq!(
            content_after, expected,
            "100 single-char appends should survive drop+reload"
        );
    }

    #[test]
    fn test_drop_reload_after_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, ws) = fresh_db_store(dir.path());

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

        let store2 = drop_and_reload(db, ws);
        let content_after = store2.get_content(ctx).unwrap();
        assert_eq!(
            content_after, content_before,
            "all 550 chars should survive compaction + drop + reload"
        );
    }

    /// Header metadata — not just text — survives a kernel restart. A
    /// `set_status`/`set_collapsed` journal entry replays through
    /// `merge_ops`'s header-apply path (`BlockDocument::replace_header`) on
    /// reload; this pins that the replayed header lands, not just that the
    /// oplog decodes.
    #[test]
    fn test_status_and_collapsed_survive_drop_reload() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, ws) = fresh_db_store(dir.path());

        let block_id = store
            .insert_block(
                ctx, None, None, Role::Model, BlockKind::Thinking,
                "reasoning", Status::Running, ContentType::Plain,
            )
            .unwrap();
        store.set_status(ctx, &block_id, Status::Done).unwrap();
        store.set_collapsed(ctx, &block_id, true).unwrap();

        drop(store);

        let store2 = drop_and_reload(db, ws);
        let snap = {
            let entry = store2.get(ctx).unwrap();
            entry.doc.get_block_snapshot(&block_id).unwrap()
        };
        assert_eq!(snap.status, Status::Done, "status must survive drop + reload");
        assert!(snap.collapsed, "collapsed must survive drop + reload");
    }

    // ====================================================================
    // 2. Per-Mutation Journal Verification
    // ====================================================================

    #[test]
    fn test_journal_row_per_insert_block() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, _ws) = fresh_db_store(dir.path());

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
            let payload: SyncPayload = codec::decode(payload_bytes)
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
        let (db, store, ctx, _ws) = fresh_db_store(dir.path());

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
        let (db, store, ctx, _ws) = fresh_db_store(dir.path());

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
        let (db, store, ctx, _ws) = fresh_db_store(dir.path());

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
        let payload: SyncPayload = codec::decode(payload_bytes).unwrap();
        assert!(
            !payload.updated_headers.is_empty(),
            "set_status oplog entry should have updated_headers"
        );
    }

    // ====================================================================
    // 3. Compaction
    // ====================================================================

    #[test]
    fn test_compaction_trigger_at_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, _ws) = fresh_db_store(dir.path());

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
        let (db, store, ctx, ws) = fresh_db_store(dir.path());

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

        let store2 = drop_and_reload(db, ws);
        let content_after = store2.get_content(ctx).unwrap();
        assert_eq!(
            content_after, expected,
            "compacted + post-compaction ops should all survive reload"
        );
    }

    #[test]
    fn test_block_order_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, ws) = fresh_db_store(dir.path());

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

        let store2 = drop_and_reload(db, ws);
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

    /// T8 (design §8 Phase 3) — the locked ordering+tick regression, exercised
    /// through the REAL restore path (load_from_db → from_snapshot → merge_ops
    /// per oplog row). `test_block_order_preserved` never appends after reload —
    /// the exact gap this pins. A fresh append after reload must (a) sort LAST in
    /// the ordered log (successor-derived order key beats any stale next_tick) and
    /// (b) stamp a tick strictly greater than every pre-reload tick (merge_ops
    /// restores the tick high-water, §2.3). Both fail under the old tick-driven
    /// calc_order_key + stale-counter behavior.
    #[test]
    fn test_reload_then_append_sorts_last() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, ws) = fresh_db_store(dir.path());

        // A handful of structural inserts seed the keyspace and the tick lane.
        let mut seed_ids = Vec::new();
        let mut prev: Option<BlockId> = None;
        for i in 0..5 {
            let bid = store
                .insert_block(
                    ctx, None, prev.as_ref(), Role::User, BlockKind::Text,
                    format!("seed-{}", i), Status::Done, ContentType::Plain,
                )
                .unwrap();
            seed_ids.push(bid);
            prev = Some(bid);
        }

        // Force compaction: 500 appends to the first block pushes the op count
        // past COMPACTION_OP_THRESHOLD and writes a snapshot row.
        let first = seed_ids[0];
        for i in 0..500 {
            let ch = (b'a' + (i % 26) as u8) as char;
            store.append_text(ctx, &first, &ch.to_string()).unwrap();
        }
        {
            let db_guard = db.lock();
            let snap = db_guard.load_latest_snapshot(ctx).unwrap();
            assert!(snap.is_some(), "500+ ops must have written a snapshot");
        }

        // More STRUCTURAL inserts AFTER compaction — these live in the oplog past
        // the snapshot, so the reload path must replay them via merge_ops (the
        // arm that restores next_tick, §2.3).
        for i in 0..3 {
            let bid = store
                .insert_block(
                    ctx, None, prev.as_ref(), Role::User, BlockKind::Text,
                    format!("post-compact-{}", i), Status::Done, ContentType::Plain,
                )
                .unwrap();
            prev = Some(bid);
        }

        // Record the pre-reload ordering and the max tick across all live blocks.
        let snaps_before = store.block_snapshots(ctx).unwrap();
        let order_before: Vec<BlockId> = snaps_before.iter().map(|s| s.id).collect();
        let max_tick_before = snaps_before
            .iter()
            .filter_map(|s| s.tick)
            .max()
            .expect("blocks carry ticks");

        drop(store); // destroy in-memory state — only the DB survives

        // Reload through the REAL path, then append a fresh block.
        let store2 = drop_and_reload(db, ws);

        // Sanity: the reload reconstructed every pre-reload block in order.
        let order_reloaded: Vec<BlockId> = store2
            .block_snapshots(ctx)
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(
            order_before, order_reloaded,
            "reload must reconstruct the pre-reload ordering exactly"
        );

        let tail = store2.last_block_id(ctx).expect("store has a tail after reload");
        let appended = store2
            .insert_block(
                ctx, None, Some(&tail), Role::User, BlockKind::Text,
                "appended-after-reload", Status::Done, ContentType::Plain,
            )
            .unwrap();

        // (a) The fresh append sorts LAST — successor order key, not a stale
        //     tick-derived mid-document key.
        let snaps_after = store2.block_snapshots(ctx).unwrap();
        assert_eq!(
            snaps_after.last().map(|s| s.id),
            Some(appended),
            "a post-reload append must sort last in the ordered log; \
             ordering = {:?}",
            snaps_after.iter().map(|s| s.content.clone()).collect::<Vec<_>>(),
        );

        // (b) The fresh append's tick strictly exceeds every pre-reload tick —
        //     merge_ops restored the tick high-water across the snapshot boundary.
        let appended_tick = snaps_after
            .iter()
            .find(|s| s.id == appended)
            .and_then(|s| s.tick)
            .expect("the appended block carries a tick");
        assert!(
            appended_tick > max_tick_before,
            "post-reload append tick {:?} must exceed pre-reload max tick {:?}",
            appended_tick, max_tick_before,
        );
    }

    // ====================================================================
    // 6. Forks
    // ====================================================================

    #[test]
    fn test_fork_creates_clean_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, _ws) = fresh_db_store(dir.path());

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

    // ========================================================================
    // Declared-Diff validation on Done (docs/diff.md slice 4)
    //
    // The producers (`diff_block`, `kj diff`) can't emit a bad diff, but
    // content and content_type are separate LWW registers — a block CAN
    // legitimately declare itself a diff while holding text that doesn't
    // parse. That block used to land with nothing visible saying so.
    // ========================================================================

    /// Insert `content` as a `Diff`-typed Running block, flip it to Done, and
    /// return the Error children the validator attached.
    fn diff_errors_after_done(content: &str) -> Vec<BlockSnapshot> {
        let store = BlockStore::new(test_agent());
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();
        let block = store
            .insert_block(
                ctx,
                None,
                None,
                Role::Tool,
                BlockKind::Text,
                content,
                Status::Running,
                ContentType::Diff,
            )
            .unwrap();
        store.set_status(ctx, &block, Status::Done).unwrap();
        store
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .filter(|b| b.kind == BlockKind::Error && b.parent_id == Some(block))
            .collect()
    }

    #[test]
    fn a_valid_diff_block_attaches_no_errors_on_done() {
        let text = kaijutsu_diff::fixtures::read("canonical/multi_file.diff");
        assert!(
            diff_errors_after_done(&text).is_empty(),
            "a canonical fixture must validate clean — the app and the kernel \
             share this corpus precisely so they cannot invent divergent dialects"
        );
    }

    #[test]
    fn a_declared_diff_that_does_not_parse_attaches_a_visible_error() {
        // The whole point of the arm: this block was NOT produced by
        // `diff_block` (which refuses) — it is what a hand-rolled create or a
        // concurrent content/content_type race leaves behind.
        let errors = diff_errors_after_done("this is not a diff at all\n");
        assert_eq!(errors.len(), 1, "expected exactly one parse error child");
        let err = errors[0].error.as_ref().expect("Error block carries payload");
        assert_eq!(err.category, kaijutsu_types::ErrorCategory::Parse);
        assert_eq!(err.code.as_deref(), Some("diff.ExpectedFileHeader"));
        assert_eq!(
            err.span.as_ref().map(|s| s.line),
            Some(1),
            "the diagnostic must point at the offending line, not line 0"
        );
        assert!(
            errors[0].content.contains("Diff parse error"),
            "summary should read as a diff problem: {:?}",
            errors[0].content
        );
    }

    /// The dialect rejects binary patches loudly (`DiffError::BinaryPatch`),
    /// and that rejection must reach the reader as an Error child rather than
    /// being swallowed as "well, it's still text".
    #[test]
    fn a_binary_patch_declared_as_a_diff_is_reported() {
        let text = kaijutsu_diff::fixtures::read("invalid/binary_git.diff");
        let errors = diff_errors_after_done(&text);
        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].error.as_ref().unwrap().code.as_deref(),
            Some("diff.BinaryPatch"),
        );
    }

    /// Re-running the validator over an unchanged bad block must not stack a
    /// second identical Error child — the dedup path is shared with Abc/Svg,
    /// but nothing pinned it for Diff.
    #[test]
    fn revalidating_an_unchanged_bad_diff_does_not_duplicate_the_error() {
        let store = BlockStore::new(test_agent());
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();
        let block = store
            .insert_block(
                ctx,
                None,
                None,
                Role::Tool,
                BlockKind::Text,
                "not a diff\n",
                Status::Running,
                ContentType::Diff,
            )
            .unwrap();
        store.set_status(ctx, &block, Status::Done).unwrap();
        store
            .validate_content_and_attach_errors(ctx, &block)
            .unwrap();

        let errors: Vec<_> = store
            .block_snapshots(ctx)
            .unwrap()
            .into_iter()
            .filter(|b| b.kind == BlockKind::Error && b.parent_id == Some(block))
            .collect();
        assert_eq!(errors.len(), 1, "validation must be idempotent");
    }

    // ========================================================================
    // docs/issues.md:361 — create_document / create_document_with_path must
    // classify an insert_document failure via the typed KernelDbError
    // variants (DuplicateDocument / DocumentPathConflict), not by matching
    // error message text, and must tell a genuine benign duplicate apart
    // from a divergent row claiming the same id or path.
    // ========================================================================

    /// 5. A matching row already in the DB (same id, kind, path=None) is the
    ///    genuine benign-recovery case: `create_document` must still return
    ///    `Ok` and the in-memory entry must exist. `test_merge_ops_persists_to_db`
    ///    already depends on this staying `Ok` — this test pins it directly.
    #[test]
    fn create_document_recovers_when_db_row_matches() {
        use crate::kernel_db::{DocumentRow, KernelDb};
        use kaijutsu_types::now_millis;

        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let creator = PrincipalId::system();
        let ws_id = {
            let db_guard = db.lock();
            db_guard.get_or_create_default_workspace(creator).unwrap()
        };

        let ctx = ContextId::new();
        {
            let db_guard = db.lock();
            db_guard
                .insert_document(&DocumentRow {
                    document_id: ctx,
                    workspace_id: ws_id,
                    doc_kind: DocumentKind::Conversation,
                    language: None,
                    path: None,
                    created_at: now_millis() as i64,
                    created_by: creator,
                })
                .unwrap();
        }

        let store = BlockStore::with_db(db.clone(), ws_id, creator);
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .expect("matching DB row must recover, not error");
        assert!(store.get(ctx).is_some(), "recovered document must be resident in memory");
    }

    /// 6. A DB row with the SAME id but a DIFFERENT `doc_kind` is divergence,
    ///    not benign duplication — `create_document` must return
    ///    `DocumentDiverged` and must NOT insert the in-memory entry.
    #[test]
    fn create_document_errors_on_diverged_kind() {
        use crate::kernel_db::{DocumentRow, KernelDb};
        use kaijutsu_types::now_millis;

        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let creator = PrincipalId::system();
        let ws_id = {
            let db_guard = db.lock();
            db_guard.get_or_create_default_workspace(creator).unwrap()
        };

        let ctx = ContextId::new();
        {
            let db_guard = db.lock();
            db_guard
                .insert_document(&DocumentRow {
                    document_id: ctx,
                    workspace_id: ws_id,
                    doc_kind: DocumentKind::File, // diverges from Conversation below
                    language: None,
                    path: None,
                    created_at: now_millis() as i64,
                    created_by: creator,
                })
                .unwrap();
        }

        let store = BlockStore::with_db(db.clone(), ws_id, creator);
        let err = store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap_err();
        assert!(
            matches!(err, BlockStoreError::DocumentDiverged { id, .. } if id == ctx),
            "expected DocumentDiverged({ctx}), got: {err}"
        );
        assert!(
            store.get(ctx).is_none(),
            "a diverged document must not be inserted into memory"
        );
    }

    /// 7. `create_document_with_path` where a DIFFERENT document already
    ///    owns that `(workspace, path)` must return `DocumentPathConflict` —
    ///    always a hard error, never recovered — and must NOT insert the
    ///    in-memory entry.
    #[test]
    fn create_document_with_path_errors_on_path_conflict() {
        use crate::kernel_db::{DocumentRow, KernelDb};
        use kaijutsu_types::now_millis;

        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let creator = PrincipalId::system();
        let ws_id = {
            let db_guard = db.lock();
            db_guard.get_or_create_default_workspace(creator).unwrap()
        };

        let existing_id = ContextId::new();
        {
            let db_guard = db.lock();
            db_guard
                .insert_document(&DocumentRow {
                    document_id: existing_id,
                    workspace_id: ws_id,
                    doc_kind: DocumentKind::Conversation,
                    language: None,
                    path: Some("/etc/rc/shared.kai".into()),
                    created_at: now_millis() as i64,
                    created_by: creator,
                })
                .unwrap();
        }

        let store = BlockStore::with_db(db.clone(), ws_id, creator);
        let new_id = ContextId::new();
        let err = store
            .create_document_with_path(
                new_id,
                DocumentKind::Conversation,
                None,
                "/etc/rc/shared.kai".to_string(),
            )
            .unwrap_err();
        match &err {
            BlockStoreError::DocumentPathConflict { path, existing } => {
                assert_eq!(path, "/etc/rc/shared.kai");
                assert_eq!(*existing, existing_id);
            }
            other => panic!("expected DocumentPathConflict, got: {other}"),
        }
        assert!(
            store.get(new_id).is_none(),
            "a path-conflicting document must not be inserted into memory"
        );
    }

    // ========================================================================
    // CHANGE FEED — text mutation classification (docs/change-feed.md)
    // ========================================================================
    //
    // The kernel decides append-vs-replace inside its mutation lock and
    // publishes an already-classified event. These tests hold that line: they
    // compare the events against the block's *real* text, never against the
    // name of the call that produced them.

    /// What a classified text event says happened, flattened for assertions.
    #[derive(Debug, PartialEq, Eq)]
    enum SeenChange {
        Appended { suffix: String, version: u64 },
        Replaced { content: String, version: u64 },
    }

    /// Pull every classified text event a subscription has queued (ignoring
    /// any other `BlockFlow` variant on the same subscription).
    fn drain_text_changes(
        sub: &mut crate::flows::Subscription<BlockFlow>,
        block_id: &BlockId,
    ) -> Vec<SeenChange> {
        let mut seen = Vec::new();
        while let Some(msg) = sub.try_recv() {
            match msg.payload {
                BlockFlow::TextAppended {
                    block_id: id,
                    suffix,
                    version,
                    ..
                } if id == *block_id => seen.push(SeenChange::Appended {
                    suffix: suffix.to_string(),
                    version,
                }),
                BlockFlow::TextReplaced {
                    block_id: id,
                    content,
                    version,
                    ..
                } if id == *block_id => seen.push(SeenChange::Replaced {
                    content: content.to_string(),
                    version,
                }),
                _ => {}
            }
        }
        seen
    }

    /// A context with one empty text block, and a subscription drained of the
    /// insert event.
    fn feed_fixture() -> (
        BlockStore,
        crate::flows::Subscription<BlockFlow>,
        ContextId,
        BlockId,
    ) {
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
                Status::Running,
                ContentType::Plain,
            )
            .unwrap();
        while sub.try_recv().is_some() {}
        (store, sub, ctx, block_id)
    }

    /// The predicate itself, in the four shapes that matter. Coordinates only —
    /// no function or tool ever enters this decision (rules 4 and 5).
    #[test]
    fn classify_text_edit_reads_coordinates_only() {
        // An insert at the end with nothing deleted is the append.
        assert_eq!(classify_text_edit(5, 5, 0), TextChange::Appended);
        // Into an empty block, the end is position 0.
        assert_eq!(classify_text_edit(0, 0, 0), TextChange::Appended);
        // Anywhere before the end is not.
        assert_eq!(classify_text_edit(5, 4, 0), TextChange::Replaced);
        assert_eq!(classify_text_edit(5, 0, 0), TextChange::Replaced);
        // A delete is never an append, not even one that ends at the tail.
        assert_eq!(classify_text_edit(5, 5, 1), TextChange::Replaced);
        assert_eq!(classify_text_edit(5, 0, 5), TextChange::Replaced);
    }

    /// `append_text_as` claims append-by-construction rather than measuring the
    /// before-length on every streamed token. This is the test that keeps the
    /// claim honest: the suffix it publishes must be exactly what the text
    /// engine actually added.
    #[tokio::test]
    async fn append_emits_exact_suffix() {
        let (store, mut sub, ctx, block_id) = feed_fixture();

        let mut expected = String::new();
        for chunk in ["Hello", ", ", "世界", "!"] {
            store.append_text(ctx, &block_id, chunk).unwrap();
            let seen = drain_text_changes(&mut sub, &block_id);
            let version = store.version(ctx).unwrap();
            assert_eq!(
                seen,
                vec![SeenChange::Appended {
                    suffix: chunk.to_string(),
                    version,
                }],
                "an append must publish exactly the characters it added"
            );
            expected.push_str(chunk);
        }

        let text = store
            .get_block_snapshot(ctx, &block_id)
            .unwrap()
            .unwrap()
            .content;
        assert_eq!(text, expected);
    }

    /// The MCP `block_append` shape: an *edit* whose position is the current
    /// character count. A provenance rule would call this a splice; the
    /// coordinates say append, and the coordinates are right.
    #[tokio::test]
    async fn edit_at_the_end_is_an_append() {
        let (store, mut sub, ctx, block_id) = feed_fixture();
        store.append_text(ctx, &block_id, "日本語").unwrap();
        let _ = drain_text_changes(&mut sub, &block_id);

        let len = store
            .get_block_snapshot(ctx, &block_id)
            .unwrap()
            .unwrap()
            .content
            .chars()
            .count();
        store.edit_text(ctx, &block_id, len, "です", 0).unwrap();

        assert_eq!(
            drain_text_changes(&mut sub, &block_id),
            vec![SeenChange::Appended {
                suffix: "です".to_string(),
                version: store.version(ctx).unwrap(),
            }],
            "an edit landing at the character count is an append, whatever tool made it"
        );
    }

    /// Every non-append shape publishes the whole after-text. The mid-insert
    /// case is the one that corrupts a client if it is called an append.
    #[tokio::test]
    async fn non_append_edits_publish_the_whole_text() {
        for (before, pos, insert, delete, after) in [
            // insert in the middle
            ("abcdef", 3, "XY", 0, "abcXYdef"),
            // insert at the very start
            ("abcdef", 0, "Z", 0, "Zabcdef"),
            // pure delete at the tail — ends at the length, still not an append
            ("abcdef", 4, "", 2, "abcd"),
            // splice: delete and insert together
            ("abcdef", 1, "QQ", 3, "aQQef"),
            // a replace that keeps the character count — the case a
            // count-comparing client reports as "no change" (rule 19)
            ("abcdef", 0, "ABC", 3, "ABCdef"),
        ] {
            let (store, mut sub, ctx, block_id) = feed_fixture();
            store.append_text(ctx, &block_id, before).unwrap();
            let _ = drain_text_changes(&mut sub, &block_id);

            store
                .edit_text(ctx, &block_id, pos, insert, delete)
                .unwrap();

            let text = store
                .get_block_snapshot(ctx, &block_id)
                .unwrap()
                .unwrap()
                .content;
            assert_eq!(text, after, "fixture disagrees with the text engine");
            assert_eq!(
                drain_text_changes(&mut sub, &block_id),
                vec![SeenChange::Replaced {
                    content: after.to_string(),
                    version: store.version(ctx).unwrap(),
                }],
                "edit(pos={pos}, insert={insert:?}, delete={delete}) must publish the after-text"
            );
        }
    }

    /// The property the whole feed exists for: a subscriber that only appends
    /// suffixes and swaps in replacements — never touching operation bytes —
    /// ends up with the kernel's text, character for character. Multibyte
    /// throughout, because the addressing is characters and the trap is bytes.
    #[tokio::test]
    async fn replaying_classified_events_reproduces_the_text() {
        let (store, mut sub, ctx, block_id) = feed_fixture();
        let mut mirror = String::new();
        let mut last_version = 0u64;

        // A deliberately mixed sequence: streaming appends, a correction in the
        // middle, a deletion, an append onto the corrected text.
        store.append_text(ctx, &block_id, "こんにちは").unwrap();
        store.append_text(ctx, &block_id, "、世界").unwrap();
        store.edit_text(ctx, &block_id, 0, "はい、", 0).unwrap();
        store.edit_text(ctx, &block_id, 3, "", 5).unwrap();
        store.append_text(ctx, &block_id, "！").unwrap();
        store.edit_text(ctx, &block_id, 2, "🎵", 1).unwrap();

        for change in drain_text_changes(&mut sub, &block_id) {
            match change {
                SeenChange::Appended { suffix, version } => {
                    mirror.push_str(&suffix);
                    assert!(
                        version > last_version,
                        "versions must strictly increase: {version} after {last_version}"
                    );
                    last_version = version;
                }
                SeenChange::Replaced { content, version } => {
                    mirror = content;
                    assert!(
                        version > last_version,
                        "versions must strictly increase: {version} after {last_version}"
                    );
                    last_version = version;
                }
            }
        }

        let (blocks, version) = store.query_versioned(ctx, &BlockQuery::All).unwrap();
        assert_eq!(
            mirror, blocks[0].content,
            "replaying the classified feed must reproduce the kernel's text exactly"
        );
        assert_eq!(
            version, last_version,
            "the version a snapshot reports must be the version the last event delivered"
        );
    }

    /// `query_versioned` is what joins a snapshot to the feed, so its version
    /// must be the same counter the events carry — and must move with every
    /// mutation, text or not.
    #[tokio::test]
    async fn query_versioned_tracks_every_mutation() {
        let (store, _sub, ctx, block_id) = feed_fixture();

        let (_, v0) = store.query_versioned(ctx, &BlockQuery::All).unwrap();
        store.append_text(ctx, &block_id, "x").unwrap();
        let (_, v1) = store.query_versioned(ctx, &BlockQuery::All).unwrap();
        store.set_status(ctx, &block_id, Status::Done).unwrap();
        let (blocks, v2) = store.query_versioned(ctx, &BlockQuery::All).unwrap();

        assert!(v0 < v1, "a text append must move the version");
        assert!(v1 < v2, "a status change must move the version too");
        assert_eq!(
            v2,
            store.version(ctx).unwrap(),
            "the versioned query must report the same counter as `version`"
        );
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].content, "x");
        assert_eq!(blocks[0].status, Status::Done);
    }

    // ── Compose drafts as blocks (Lane C) ────────────────────────────────

    /// The property the whole design exists for: **submit does not copy.**
    ///
    /// The old path read the draft, cleared it, and only then tried to author a
    /// block — so a failure or a crash after the clear destroyed what someone
    /// had typed. Here the block they typed into becomes the message, so there
    /// is no interval in which the text lives nowhere.
    #[test]
    fn submitting_a_draft_keeps_the_same_block() {
        let (store, _bus) = store_with_flows();
        let ctx = ContextId::new();
        let me = PrincipalId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let drafted = store.edit_draft(ctx, me, 0, "hello world", 0).unwrap();
        let (submitted, text) = store.submit_draft(ctx, me).unwrap();

        assert_eq!(
            submitted, drafted,
            "the submitted message must BE the drafted block, not a copy of it"
        );
        assert_eq!(text, "hello world");

        let snap = store.get_block_snapshot(ctx, &submitted).unwrap().unwrap();
        assert_eq!(snap.status, Status::Done);
        assert!(!snap.ephemeral, "a submitted message is no longer hidden");
        assert_eq!(snap.content, "hello world");
        assert_eq!(snap.role, Role::User);
    }

    /// A draft is invisible to the model until it is sent — checked on both
    /// flags independently, because submit clears them one at a time and a
    /// crash between the two must fail toward silence.
    #[test]
    fn a_draft_is_hidden_from_hydration_by_status_and_by_ephemeral() {
        let (store, _bus) = store_with_flows();
        let ctx = ContextId::new();
        let me = PrincipalId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let id = store.edit_draft(ctx, me, 0, "unsent thought", 0).unwrap();
        let snap = store.get_block_snapshot(ctx, &id).unwrap().unwrap();
        assert_eq!(snap.status, Status::Draft);
        assert!(snap.ephemeral);
    }

    /// An empty or whitespace-only draft is refused and left alone: a stray
    /// Enter must neither send nothing nor destroy what is sitting there.
    #[test]
    fn an_empty_draft_is_refused_without_being_cleared() {
        let (store, _bus) = store_with_flows();
        let ctx = ContextId::new();
        let me = PrincipalId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let id = store.edit_draft(ctx, me, 0, "   \n  ", 0).unwrap();
        assert!(matches!(
            store.submit_draft(ctx, me),
            Err(BlockStoreError::EmptyDraft(_))
        ));

        let snap = store.get_block_snapshot(ctx, &id).unwrap().unwrap();
        assert_eq!(snap.status, Status::Draft, "the draft survives a refused submit");
        assert_eq!(snap.content, "   \n  ");
    }

    /// Two players in one context each get their own draft. `BlockId` already
    /// carries the principal, so this needs no extra key — and it is the thing
    /// a single shared input document could never do.
    #[test]
    fn two_principals_hold_independent_drafts() {
        let (store, _bus) = store_with_flows();
        let ctx = ContextId::new();
        let amy = PrincipalId::new();
        let model = PrincipalId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        store.edit_draft(ctx, amy, 0, "mine", 0).unwrap();
        store.edit_draft(ctx, model, 0, "theirs", 0).unwrap();

        assert_eq!(
            store.draft_block(ctx, amy).unwrap().unwrap().content,
            "mine"
        );
        assert_eq!(
            store.draft_block(ctx, model).unwrap().unwrap().content,
            "theirs"
        );

        // Submitting one leaves the other alone and still a draft.
        store.submit_draft(ctx, amy).unwrap();
        assert!(store.draft_block(ctx, amy).unwrap().is_none());
        assert_eq!(
            store.draft_block(ctx, model).unwrap().unwrap().status,
            Status::Draft
        );
    }

    /// Clearing returns the discarded text, so a caller can offer it back
    /// rather than swallowing it.
    #[test]
    fn clearing_a_draft_returns_what_it_threw_away() {
        let (store, _bus) = store_with_flows();
        let ctx = ContextId::new();
        let me = PrincipalId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        store.edit_draft(ctx, me, 0, "second thoughts", 0).unwrap();
        assert_eq!(store.clear_draft(ctx, me).unwrap(), "second thoughts");
        assert!(store.draft_block(ctx, me).unwrap().is_none());
        // Clearing nothing is not an error.
        assert_eq!(store.clear_draft(ctx, me).unwrap(), "");
    }

    /// A draft sits at the END of the document, which is exactly where a naive
    /// tail check looks for the last real status. A half-typed message must not
    /// make a context that just failed a turn read as idle.
    #[test]
    fn a_draft_does_not_mask_a_failed_turn_in_live_status() {
        assert_eq!(
            derive_context_live_status(&[Status::Done, Status::Error, Status::Draft]),
            Status::Error,
            "the draft is not the context's work — the failed turn behind it is"
        );
        assert_eq!(
            derive_context_live_status(&[Status::Done, Status::Draft]),
            Status::Pending
        );
        assert_eq!(
            derive_context_live_status(&[Status::Running, Status::Draft]),
            Status::Running
        );
    }

    // ── Version durability (docs/change-feed.md rules 21-26) ─────────────

    /// The context version RESUMES across a restart.
    ///
    /// It used to restart: the loader seeded it from the number of replayed
    /// oplog rows, so a long-lived context came back at a small number and
    /// every client's recovery anchor silently rewound. A version that goes
    /// backwards makes "discard deliveries at or below the snapshot version"
    /// mean the opposite of what it says.
    ///
    /// **This test agrees with the old, wrong formula by construction** — with
    /// no snapshot, replaying from zero counts exactly the mutations that
    /// happened. It pins the property; the test with teeth is
    /// `version_resumes_across_a_restart_after_compaction`, which is the only
    /// one of these four that fails against the old code (verified by
    /// reverting it).
    #[test]
    fn version_resumes_across_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, ws) = fresh_db_store(dir.path());

        let block_id = store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "", Status::Done, ContentType::Plain,
            )
            .unwrap();
        for chunk in ["a", "b", "c", "d"] {
            store.append_text(ctx, &block_id, chunk).unwrap();
        }
        let before = store.version(ctx).unwrap();
        assert!(before >= 5, "fixture should have made several mutations");

        drop(store);
        let store2 = drop_and_reload(db, ws);

        assert_eq!(
            store2.version(ctx).unwrap(),
            before,
            "the version a context resumes at must be the version it stopped at"
        );
    }

    /// The same, across a compaction — the case the old code got closest to
    /// getting right, since compaction is what persists the version at all.
    /// After compaction the oplog is truncated, so a loader that counts rows
    /// sees almost none and reports a version near zero for a context that had
    /// taken hundreds of mutations.
    #[test]
    fn version_resumes_across_a_restart_after_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, ws) = fresh_db_store(dir.path());

        let block_id = store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "", Status::Done, ContentType::Plain,
            )
            .unwrap();
        // Past COMPACTION_OP_THRESHOLD, so a snapshot is written and the
        // oplog behind it is truncated.
        for i in 0..(COMPACTION_OP_THRESHOLD + 20) {
            store
                .append_text(ctx, &block_id, &(i % 10).to_string())
                .unwrap();
        }
        let before = store.version(ctx).unwrap();
        assert!(
            before > COMPACTION_OP_THRESHOLD,
            "fixture must cross the compaction threshold, got {before}"
        );

        drop(store);
        let store2 = drop_and_reload(db, ws);

        assert_eq!(
            store2.version(ctx).unwrap(),
            before,
            "a compacted context must resume at its real version, not at the \
             number of oplog rows that survived truncation"
        );
    }

    /// Resuming is not enough on its own: the next mutation has to continue
    /// from the resumed value, so versions stay strictly increasing across the
    /// restart boundary as well as within a session.
    #[test]
    fn a_mutation_after_a_restart_continues_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, ws) = fresh_db_store(dir.path());

        let block_id = store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "before", Status::Done, ContentType::Plain,
            )
            .unwrap();
        let before = store.version(ctx).unwrap();

        drop(store);
        let store2 = drop_and_reload(db, ws);
        store2.append_text(ctx, &block_id, "-after").unwrap();

        assert!(
            store2.version(ctx).unwrap() > before,
            "a post-restart mutation must advance past every pre-restart version"
        );
        assert_eq!(
            store2
                .get_block_snapshot(ctx, &block_id)
                .unwrap()
                .unwrap()
                .content,
            "before-after"
        );
    }

    /// Hydrating one document on demand must agree with hydrating the whole
    /// store — two loaders, one version rule.
    #[test]
    fn load_one_agrees_with_load_all_on_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, ws) = fresh_db_store(dir.path());

        let block_id = store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "", Status::Done, ContentType::Plain,
            )
            .unwrap();
        for chunk in ["x", "y", "z"] {
            store.append_text(ctx, &block_id, chunk).unwrap();
        }
        let before = store.version(ctx).unwrap();
        drop(store);

        let creator = PrincipalId::system();
        let lazy = BlockStore::with_db(db.clone(), ws, creator);
        assert!(lazy.load_one_from_db(ctx).unwrap(), "document hydrates");
        assert_eq!(lazy.version(ctx).unwrap(), before);

        let eager = drop_and_reload(db, ws);
        assert_eq!(eager.version(ctx).unwrap(), before);
    }

    /// The version rides on the event, not on the delivery, so a subscriber can
    /// tell whether a buffered change is already in a snapshot it holds
    /// (rule 25). An empty append is still a mutation and still moves it.
    #[tokio::test]
    async fn every_text_event_carries_the_post_mutation_version() {
        let (store, mut sub, ctx, block_id) = feed_fixture();

        store.append_text(ctx, &block_id, "a").unwrap();
        let after_first = store.version(ctx).unwrap();
        store.append_text(ctx, &block_id, "").unwrap();
        let after_empty = store.version(ctx).unwrap();

        assert!(
            after_empty > after_first,
            "an empty append is still an accepted mutation"
        );
        assert_eq!(
            drain_text_changes(&mut sub, &block_id),
            vec![
                SeenChange::Appended {
                    suffix: "a".to_string(),
                    version: after_first,
                },
                SeenChange::Appended {
                    suffix: String::new(),
                    version: after_empty,
                },
            ]
        );
    }

    /// Drain every event the bus has queued right now, assert they all carry
    /// the SAME version and that it exceeds `last_version`, and return it.
    ///
    /// A mutation that emits more than one event legitimately shares one
    /// version between them — they are captured under a single
    /// `entry.touch()` inside a single mutation-lock guard, i.e. one accepted
    /// mutation, not two. This helper pins that behavior rather than a
    /// version-per-event assumption that would be wrong.
    fn drain_one_mutation(
        sub: &mut crate::flows::Subscription<BlockFlow>,
        last_version: u64,
        label: &str,
    ) -> u64 {
        let mut versions = Vec::new();
        while let Some(msg) = sub.try_recv() {
            let v = msg
                .payload
                .version()
                .unwrap_or_else(|| panic!("{label}: event with no version: {:?}", msg.payload));
            versions.push(v);
        }
        assert!(!versions.is_empty(), "{label}: emitted no events");
        let v = versions[0];
        assert!(
            versions.iter().all(|&x| x == v),
            "{label}: every event from one mutation must carry the same version, got {versions:?}"
        );
        assert!(
            v > last_version,
            "{label}: version must strictly increase across mutations: {v} did not exceed {last_version}"
        );
        v
    }

    /// The property the change-feed ordering fix depends on: every
    /// `BlockFlow` mutation variant carries the context's post-mutation
    /// version, captured inside the mutation lock, and that version moves
    /// strictly forward from one mutation to the next — never backward,
    /// never stalled — because the server bridge sorts a delivery by this
    /// field to recover publish order across concurrent writers (the kernel
    /// captures the version under the document guard but publishes after
    /// releasing it, so publish order and lock order can disagree).
    ///
    /// Drives one of every mutating op this store exposes (insert twice,
    /// since a move needs a second block) on a single context, and checks
    /// the resulting stream against `store.version()`.
    #[tokio::test]
    async fn block_flow_version_tracks_mutation_order() {
        let (store, bus) = store_with_flows();
        let mut sub = bus.subscribe("block.>");
        let ctx = ContextId::new();
        store
            .create_document(ctx, DocumentKind::Conversation, None)
            .unwrap();

        let mut last_version = 0u64;

        let a = store
            .insert_block(
                ctx,
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "hello",
                Status::Running,
                ContentType::Plain,
            )
            .unwrap();
        last_version = drain_one_mutation(&mut sub, last_version, "insert_block a");

        let b = store
            .insert_block(
                ctx,
                None,
                Some(&a),
                Role::Model,
                BlockKind::Text,
                "world",
                Status::Running,
                ContentType::Plain,
            )
            .unwrap();
        last_version = drain_one_mutation(&mut sub, last_version, "insert_block b");

        store.set_status(ctx, &a, Status::Done).unwrap();
        last_version = drain_one_mutation(&mut sub, last_version, "set_status");

        store.set_collapsed(ctx, &a, true).unwrap();
        last_version = drain_one_mutation(&mut sub, last_version, "set_collapsed");

        store.set_excluded(ctx, &a, true).unwrap();
        last_version = drain_one_mutation(&mut sub, last_version, "set_excluded");

        let output =
            kaijutsu_types::OutputData::nodes(vec![kaijutsu_types::OutputNode::text("row")]);
        store.set_output(ctx, &a, Some(&output)).unwrap();
        last_version = drain_one_mutation(&mut sub, last_version, "set_output");

        // Ends at the current character count — classified as an append.
        store.append_text(ctx, &a, "!").unwrap();
        last_version = drain_one_mutation(&mut sub, last_version, "append_text");

        // Inserts at position 0, not the end — classified as a replace.
        store.edit_text(ctx, &a, 0, "X", 0).unwrap();
        last_version = drain_one_mutation(&mut sub, last_version, "edit_text");

        store.move_block(ctx, &b, None).unwrap();
        last_version = drain_one_mutation(&mut sub, last_version, "move_block");

        store.delete_block(ctx, &b).unwrap();
        last_version = drain_one_mutation(&mut sub, last_version, "delete_block");

        assert_eq!(
            last_version,
            store.version(ctx).unwrap(),
            "the last mutation's version must equal what `store.version()` reports"
        );
    }

    // ========================================================================
    // 2026-08-16 DTE-CUTOVER OPLOG CLEANUP
    //
    // One-time migration, see `KernelDb::purge_dte_cutover_oplog_rows`. These
    // tests exist to prove the cleanup is SURGICAL — it drops rows that
    // genuinely fail to decode and nothing else — because the failure mode
    // that would matter (truncating a healthy oplog) is silent.
    // ========================================================================

    /// The pre-`fc616aa6` shape of `TextEdit`: no `insert` field, because
    /// block text was still a diamond-types-extended CRDT. Encoding this
    /// through the same `codec` the real journal uses reproduces exactly what
    /// the 2026-08-16 boot hit — well-formed CBOR that cannot become a
    /// `SyncPayload` (`missing field \`insert\``). Serialize-only: nothing in
    /// the tree can read the real DTE ops any more, and this must not pretend
    /// otherwise.
    #[derive(serde::Serialize)]
    struct LegacyTextEdit {
        pos: Option<usize>,
        delete: usize,
    }

    #[derive(serde::Serialize)]
    struct LegacySyncPayload {
        block_ops: Vec<(BlockId, LegacyTextEdit)>,
        // Empty in this fixture; the missing `insert` inside `block_ops` is
        // what makes the payload undecodable, and typing these as `Vec<String>`
        // keeps the encoded CBOR an empty array either way.
        new_blocks: Vec<String>,
        updated_headers: Vec<String>,
        deleted_blocks: Vec<String>,
    }

    /// The predicate `load_from_db` hands the migration.
    fn sync_payload_decodable(bytes: &[u8]) -> bool {
        codec::decode::<SyncPayload>(bytes).is_ok()
    }

    /// Encode one old-shape payload naming `block_id`.
    fn legacy_dte_payload(block_id: &BlockId) -> Vec<u8> {
        let bytes = codec::encode(&LegacySyncPayload {
            block_ops: vec![(
                block_id.clone(),
                LegacyTextEdit { pos: Some(0), delete: 0 },
            )],
            new_blocks: Vec::new(),
            updated_headers: Vec::new(),
            deleted_blocks: Vec::new(),
        })
        .expect("encode legacy payload");
        let err = codec::decode::<SyncPayload>(&bytes)
            .err()
            .expect("fixture is only meaningful if it fails to decode as the CURRENT SyncPayload");
        assert!(
            err.to_string().contains("missing field `insert`"),
            "fixture must reproduce the 2026-08-16 boot's actual error, got {err}"
        );
        bytes
    }

    /// The whole point: an undecodable row is dropped, the valid row beside it
    /// survives byte-for-byte, and the document loads instead of staying dark.
    #[test]
    fn dte_cutover_cleanup_drops_bad_row_keeps_good_row_and_document_loads() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, ws) = fresh_db_store(dir.path());

        let doomed = store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "authored before the cutover", Status::Done, ContentType::Plain,
            )
            .unwrap();
        store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "authored after the cutover", Status::Done, ContentType::Plain,
            )
            .unwrap();

        let rows = db.lock().load_oplog_since(ctx, 0).expect("load oplog");
        assert!(
            rows.len() >= 2,
            "fixture needs at least two journalled ops, got {}",
            rows.len()
        );
        let bad_seq = rows[0].0;
        let (good_seq, good_payload) = rows[rows.len() - 1].clone();

        // Rewrite the FIRST row in the old shape, in place — the real
        // situation was old rows followed by new ones in the same journal.
        let legacy = legacy_dte_payload(&doomed);
        {
            let db_guard = db.lock();
            db_guard.delete_oplog_row_for_test(ctx, bad_seq).unwrap();
            db_guard.append_op(ctx, bad_seq, &legacy).unwrap();
        }

        drop(store);
        let store2 = drop_and_reload(db.clone(), ws);

        let content = store2
            .get_content(ctx)
            .expect("document must load after the cleanup instead of being skipped");
        assert!(
            content.contains("authored after the cutover"),
            "the surviving op must have replayed, got {content:?}"
        );

        let after = db.lock().load_oplog_since(ctx, 0).expect("load oplog");
        assert!(
            !after.iter().any(|(seq, _)| *seq == bad_seq),
            "the undecodable row must be gone"
        );
        let survivor = after
            .iter()
            .find(|(seq, _)| *seq == good_seq)
            .expect("the valid row must survive — this is not a truncation");
        assert_eq!(
            survivor.1, good_payload,
            "the valid row's payload must be untouched"
        );
    }

    /// A healthy oplog is not a repair target. Nothing is deleted, the row set
    /// is identical afterwards, and `examined` still reports the full scan so
    /// the INFO line is honest about what it looked at.
    #[test]
    fn dte_cutover_cleanup_leaves_a_healthy_oplog_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, _ws) = fresh_db_store(dir.path());

        store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "healthy", Status::Done, ContentType::Plain,
            )
            .unwrap();
        store
            .insert_block(
                ctx, None, None, Role::Model, BlockKind::Text,
                "also healthy", Status::Done, ContentType::Plain,
            )
            .unwrap();

        let before = db.lock().load_oplog_since(ctx, 0).expect("load oplog");
        assert!(!before.is_empty(), "fixture must journal something");

        let report = db
            .lock()
            .purge_dte_cutover_oplog_rows(&sync_payload_decodable)
            .expect("migration");

        assert!(!report.already_applied, "first run must actually scan");
        assert_eq!(report.examined, before.len() as u64, "every row is examined");
        assert_eq!(report.deleted, 0, "nothing decodes badly, so nothing goes");
        assert_eq!(report.documents, 0, "no document was affected");

        let after = db.lock().load_oplog_since(ctx, 0).expect("load oplog");
        assert_eq!(before, after, "a healthy oplog must be byte-identical after");
    }

    /// Running twice is safe: the marker gates the second run, so it neither
    /// rescans the (large) oplog nor deletes anything further.
    #[test]
    fn dte_cutover_cleanup_is_gated_and_safe_to_run_twice() {
        let dir = tempfile::tempdir().unwrap();
        let (db, store, ctx, _ws) = fresh_db_store(dir.path());

        let doomed = store
            .insert_block(
                ctx, None, None, Role::User, BlockKind::Text,
                "authored before the cutover", Status::Done, ContentType::Plain,
            )
            .unwrap();
        let rows = db.lock().load_oplog_since(ctx, 0).expect("load oplog");
        let bad_seq = rows[0].0;
        let legacy = legacy_dte_payload(&doomed);
        {
            let db_guard = db.lock();
            db_guard.delete_oplog_row_for_test(ctx, bad_seq).unwrap();
            db_guard.append_op(ctx, bad_seq, &legacy).unwrap();
        }

        let first = db
            .lock()
            .purge_dte_cutover_oplog_rows(&sync_payload_decodable)
            .expect("first run");
        assert!(!first.already_applied, "first run must scan");
        assert_eq!(first.deleted, 1, "the one bad row goes");
        assert_eq!(first.documents, 1, "one document was affected");

        let between = db.lock().load_oplog_since(ctx, 0).expect("load oplog");

        let second = db
            .lock()
            .purge_dte_cutover_oplog_rows(&sync_payload_decodable)
            .expect("second run");
        assert!(second.already_applied, "the marker must gate the second run");
        assert_eq!(second.deleted, 0, "second run deletes nothing");
        assert_eq!(
            second.examined, 0,
            "a gated run must not scan the oplog at all"
        );

        let after = db.lock().load_oplog_since(ctx, 0).expect("load oplog");
        assert_eq!(between, after, "second run must not touch a single row");
    }
}
