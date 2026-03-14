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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use diamond_types_extended::Frontier;
use parking_lot::RwLock;

use kaijutsu_crdt::block_store::{BlockStore as CrdtBlockStore, ForkBlockFilter, StoreSnapshot, SyncPayload};
use kaijutsu_crdt::{BlockId, BlockKind, BlockSnapshot, Role, Status, ToolKind};
use kaijutsu_types::BlockFilter;
use kaijutsu_types::{ContextId, DocKind, KernelId, PrincipalId, WorkspaceId};

use crate::kernel_db::{DocumentRow, KernelDb};
use crate::flows::{BlockFlow, InputDocFlow, OpSource, SharedBlockFlowBus, SharedInputDocFlowBus};
use crate::input_doc::InputDocEntry;

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
pub type DbHandle = Arc<std::sync::Mutex<KernelDb>>;

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
}

impl DocumentEntry {
    /// Create a new document entry.
    fn new(context_id: ContextId, kind: DocKind, language: Option<String>, agent_id: PrincipalId) -> Self {
        Self {
            doc: CrdtBlockStore::new(context_id, agent_id),
            kind,
            language,
            version: AtomicU64::new(0),
            last_agent: RwLock::new(agent_id),
            sync_generation: AtomicU64::new(0),
        }
    }

    /// Create a document entry from a store snapshot.
    fn from_store_snapshot(
        snapshot: StoreSnapshot,
        kind: DocKind,
        language: Option<String>,
        agent_id: PrincipalId,
    ) -> Self {
        let store = CrdtBlockStore::from_snapshot(snapshot, agent_id);
        let version = store.version();
        Self {
            doc: store,
            kind,
            language,
            version: AtomicU64::new(version),
            last_agent: RwLock::new(agent_id),
            sync_generation: AtomicU64::new(0),
        }
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
            Entry::Occupied(_) => {
                Err(BlockStoreError::DocumentAlreadyExists(context_id))
            }
            Entry::Vacant(vacant) => {
                let agent_id = self.agent_id();

                // Persist metadata if we have a DB
                if let Some(db) = &self.db {
                    let db_guard = db.lock().map_err(|e| BlockStoreError::Db(e.to_string()))?;
                    let row = DocumentRow {
                        document_id: context_id,
                        kernel_id: self.kernel_id.unwrap_or_else(KernelId::new),
                        workspace_id: self.default_workspace_id.unwrap_or_else(WorkspaceId::new),
                        doc_kind: kind,
                        language: language.clone(),
                        path: None,
                        created_at: kaijutsu_types::now_millis() as i64,
                        created_by: agent_id,
                    };
                    match db_guard.insert_document(&row) {
                        Ok(()) => {}
                        Err(e) if e.to_string().contains("UNIQUE constraint")
                            || e.to_string().contains("already exists") => {
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
        let entry = DocumentEntry::from_store_snapshot(snapshot, kind, language, agent_id);
        self.documents.insert(context_id, entry);

        Ok(())
    }

    /// Get a document for reading.
    pub fn get(&self, context_id: ContextId) -> Option<dashmap::mapref::one::Ref<'_, ContextId, DocumentEntry>> {
        self.documents.get(&context_id)
    }

    /// Get a document for writing.
    pub fn get_mut(&self, context_id: ContextId) -> Option<dashmap::mapref::one::RefMut<'_, ContextId, DocumentEntry>> {
        self.documents.get_mut(&context_id)
    }

    /// List all document IDs.
    pub fn list_ids(&self) -> Vec<ContextId> {
        self.documents.iter().map(|r| *r.key()).collect()
    }

    /// List document IDs filtered by kind.
    pub fn list_ids_by_kind(&self, kind: DocKind) -> Vec<ContextId> {
        self.documents.iter()
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
            let db_guard = db.lock().map_err(|e| BlockStoreError::Db(e.to_string()))?;
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
    pub fn fork_document(
        &self,
        source_id: ContextId,
        new_id: ContextId,
    ) -> BlockStoreResult<()> {
        if self.documents.contains_key(&new_id) {
            return Err(BlockStoreError::DocumentAlreadyExists(new_id));
        }

        let source_entry = self.get(source_id)
            .ok_or(BlockStoreError::DocumentNotFound(source_id))?;

        let agent_id = self.agent_id();
        let forked_store = source_entry.doc.fork(new_id, agent_id);
        let kind = source_entry.kind;
        let language = source_entry.language.clone();
        drop(source_entry); // Release the read lock

        // Persist metadata if we have a DB
        if let Some(db) = &self.db {
            let db_guard = db.lock().map_err(|e| BlockStoreError::Db(e.to_string()))?;
            let row = DocumentRow {
                document_id: new_id,
                kernel_id: self.kernel_id.unwrap_or_else(KernelId::new),
                workspace_id: self.default_workspace_id.unwrap_or_else(WorkspaceId::new),
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
        };
        self.documents.insert(new_id, entry);

        Ok(())
    }

    /// Fork a document at a specific version, creating a copy with only blocks up to that version.
    ///
    /// This creates a new document containing only blocks that existed at the given version,
    /// useful for timeline branching and "what if" explorations.
    ///
    /// # Arguments
    ///
    /// * `source_id` - ID of the document to fork
    /// * `new_id` - ID for the forked document
    /// * `at_version` - Only include blocks with created_at <= this version
    ///
    /// # Returns
    ///
    /// Ok(()) on success, Err if source not found, target exists, or version invalid.
    pub fn fork_document_at_version(
        &self,
        source_id: ContextId,
        new_id: ContextId,
        at_version: u64,
    ) -> BlockStoreResult<()> {
        if self.documents.contains_key(&new_id) {
            return Err(BlockStoreError::DocumentAlreadyExists(new_id));
        }

        let source_entry = self.get(source_id)
            .ok_or(BlockStoreError::DocumentNotFound(source_id))?;

        // Validate version
        let current_version = source_entry.version();
        if at_version > current_version {
            return Err(BlockStoreError::Validation(format!(
                "Requested version {} is in the future (current: {})",
                at_version, current_version
            )));
        }

        let agent_id = self.agent_id();
        let forked_store = source_entry.doc.fork_at_version(new_id, agent_id, at_version);
        let kind = source_entry.kind;
        let language = source_entry.language.clone();
        drop(source_entry); // Release the read lock

        // Persist metadata if we have a DB
        if let Some(db) = &self.db {
            let db_guard = db.lock().map_err(|e| BlockStoreError::Db(e.to_string()))?;
            let row = DocumentRow {
                document_id: new_id,
                kernel_id: self.kernel_id.unwrap_or_else(KernelId::new),
                workspace_id: self.default_workspace_id.unwrap_or_else(WorkspaceId::new),
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
        };
        self.documents.insert(new_id, entry);

        Ok(())
    }

    /// Fork a document at a specific version with block filtering.
    ///
    /// Like [`fork_document_at_version`] but additionally filters blocks via `ForkBlockFilter`.
    /// Blocks that don't pass the filter are excluded from the fork.
    pub fn fork_document_filtered(
        &self,
        source_id: ContextId,
        new_id: ContextId,
        at_version: u64,
        filter: &ForkBlockFilter,
    ) -> BlockStoreResult<()> {
        if self.documents.contains_key(&new_id) {
            return Err(BlockStoreError::DocumentAlreadyExists(new_id));
        }

        let source_entry = self.get(source_id)
            .ok_or(BlockStoreError::DocumentNotFound(source_id))?;

        let current_version = source_entry.version();
        if at_version > current_version {
            return Err(BlockStoreError::Validation(format!(
                "Requested version {} is in the future (current: {})",
                at_version, current_version
            )));
        }

        let agent_id = self.agent_id();
        let forked_store = source_entry.doc.fork_filtered(new_id, agent_id, at_version, filter);
        let kind = source_entry.kind;
        let language = source_entry.language.clone();
        drop(source_entry);

        // Persist metadata if we have a DB
        if let Some(db) = &self.db {
            let db_guard = db.lock().map_err(|e| BlockStoreError::Db(e.to_string()))?;
            let row = DocumentRow {
                document_id: new_id,
                kernel_id: self.kernel_id.unwrap_or_else(KernelId::new),
                workspace_id: self.default_workspace_id.unwrap_or_else(WorkspaceId::new),
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
        };
        self.documents.insert(new_id, entry);

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

    /// Auto-save snapshot if database is configured.
    /// Logs warnings on failure but doesn't propagate errors.
    fn auto_save(&self, context_id: ContextId) {
        if self.db.is_some() {
            if let Err(e) = self.save_snapshot(context_id) {
                tracing::warn!(context_id = %context_id.to_hex(), error = %e, "Failed to auto-save snapshot");
            }
        }
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
    ) -> BlockStoreResult<BlockId> {
        self.insert_block_as(context_id, parent_id, after, role, kind, content, status, None)
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
        agent_id: Option<PrincipalId>,
    ) -> BlockStoreResult<BlockId> {
        let after_id = after.cloned();
        let (block_id, snapshot, ops) = {
            let mut entry = self.get_mut(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());

            // Set the agent for this operation so BlockId gets the right author
            entry.doc.set_agent_id(effective_agent);

            // Capture frontier before the operation for incremental ops.
            // Clients that are in sync can merge these directly.
            // Clients that are out of sync will get full oplog via get_document_state.
            let frontier_before = entry.doc.frontier();

            let block_id = entry.doc.insert_block(parent_id, after, role, kind, content, status)
                ?;
            let snapshot = entry.doc.get_block_snapshot(&block_id)
                .ok_or(BlockStoreError::BlockNotFoundAfterInsert)?;

            // Send incremental ops (just this operation) for efficient sync.
            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = postcard::to_allocvec(&ops).map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            (block_id, snapshot, ops_bytes)
        };
        self.auto_save(context_id);

        // Emit flow event with creation ops
        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(snapshot),
            after_id,
            ops: Arc::from(ops),
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
        self.insert_tool_call_as(context_id, parent_id, after, tool_name, tool_input, tool_kind, None, None, None)
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
        let (block_id, snapshot, ops) = {
            let mut entry = self.get_mut(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);

            // Capture frontier before the operation for incremental ops
            let frontier_before = entry.doc.frontier();

            let block_id = entry.doc.insert_tool_call(parent_id, after, tool_name, tool_input, tool_kind, role)
                ?;

            // Persist tool_use_id to BlockContent so it survives snapshot round-trips
            if let Some(ref tui) = tool_use_id {
                entry.doc.set_tool_use_id(&block_id, Some(tui.clone()))
                    ?;
            }

            let snapshot = entry.doc.get_block_snapshot(&block_id)
                .ok_or(BlockStoreError::BlockNotFoundAfterInsert)?;

            // Send incremental ops (just this operation) for efficient sync
            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = postcard::to_allocvec(&ops).map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            (block_id, snapshot, ops_bytes)
        };
        self.auto_save(context_id);

        // Emit flow event with creation ops
        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(snapshot),
            after_id,
            ops: Arc::from(ops),
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
        self.insert_tool_result_as(context_id, tool_call_id, after, content, is_error, exit_code, tool_kind, None, None)
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
        let (block_id, snapshot, ops) = {
            let mut entry = self.get_mut(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);

            // Capture frontier before the operation for incremental ops
            let frontier_before = entry.doc.frontier();

            let block_id = entry.doc.insert_tool_result_block(tool_call_id, after, content, is_error, exit_code, tool_kind)
                ?;

            // Persist tool_use_id to BlockContent so it survives snapshot round-trips
            if let Some(ref tui) = tool_use_id {
                entry.doc.set_tool_use_id(&block_id, Some(tui.clone()))
                    ?;
            }

            let snapshot = entry.doc.get_block_snapshot(&block_id)
                .ok_or(BlockStoreError::BlockNotFoundAfterInsert)?;

            // Send incremental ops (just this operation) for efficient sync
            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = postcard::to_allocvec(&ops).map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            (block_id, snapshot, ops_bytes)
        };
        self.auto_save(context_id);

        // Emit flow event with creation ops
        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(snapshot),
            after_id,
            ops: Arc::from(ops),
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
        let (block_id, final_snapshot, ops) = {
            let mut entry = self.get_mut(context_id)
                .ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);

            let frontier_before = entry.doc.frontier();

            let block_id = entry.doc.insert_from_snapshot(snapshot, after)
                ?;
            let final_snapshot = entry.doc.get_block_snapshot(&block_id)
                .ok_or(BlockStoreError::BlockNotFoundAfterInsert)?;

            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = postcard::to_allocvec(&ops).map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            (block_id, final_snapshot, ops_bytes)
        };
        self.auto_save(context_id);

        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(final_snapshot),
            after_id,
            ops: Arc::from(ops),
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
        {
            let mut entry = self.get_mut(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let agent_id = self.agent_id();
            entry.doc.set_status(block_id, status)?;
            entry.touch(agent_id);
        }
        self.auto_save(context_id);

        // Emit flow event
        // Include output data if present — output is a struct field that can't
        // travel via DTE ops, so we piggyback it on StatusChanged
        let output = {
            let entry = self.get(context_id);
            entry.and_then(|e| e.doc.get_block_snapshot(block_id))
                .and_then(|s| s.output)
        };
        self.emit(BlockFlow::StatusChanged {
            context_id,
            block_id: block_id.clone(),
            status,
            output,
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Edit text within a block.
    ///
    /// Note: Does not auto-save to avoid excessive I/O during streaming.
    /// Call `save_snapshot()` explicitly when editing is complete.
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
        let ops = {
            let mut entry = self.get_mut(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);
            // Capture frontier before edit
            let frontier = entry.doc.frontier();
            entry.doc.edit_text(block_id, pos, insert, delete)?;
            entry.touch(effective_agent);
            // Get ops since frontier (the edit we just applied)
            let ops = entry.doc.ops_since(&frontier);
            postcard::to_allocvec(&ops).map_err(|e| BlockStoreError::Serialization(e.to_string()))?
        };
        // Note: No auto-save for text edits (high frequency during streaming)

        // Emit CRDT ops for proper sync
        self.emit(BlockFlow::TextOps {
            context_id,
            block_id: block_id.clone(),
            ops: Arc::from(ops),
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Set structured output data on a block.
    ///
    /// Output data provides formatting information (tables, trees) for richer output.
    /// Emits `OutputChanged` flow event. Also piggybacked on `StatusChanged` for
    /// wire compat — see `set_status`.
    /// Set the content_type hint on a block (e.g., "text/markdown", "image/svg+xml").
    pub fn set_content_type(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        content_type: Option<String>,
    ) -> BlockStoreResult<()> {
        let mut entry = self.get_mut(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        entry.doc.set_content_type(block_id, content_type)?;
        entry.touch(self.agent_id());
        Ok(())
    }

    pub fn set_output(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        output: Option<&kaijutsu_types::OutputData>,
    ) -> BlockStoreResult<()> {
        let mut entry = self.get_mut(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        let agent_id = self.agent_id();
        entry.doc.set_output(block_id, output.cloned())?;
        entry.touch(agent_id);
        drop(entry);

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
        let mut entry = self.get_mut(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        let agent_id = self.agent_id();
        entry.doc.set_tool_use_id(block_id, tool_use_id)?;
        entry.touch(agent_id);
        drop(entry);

        self.emit(BlockFlow::MetadataChanged {
            context_id,
            block_id: *block_id,
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Append text to a block.
    ///
    /// Note: Does not auto-save to avoid excessive I/O during streaming.
    /// Call `save_snapshot()` explicitly when streaming is complete.
    pub fn append_text(&self, context_id: ContextId, block_id: &BlockId, text: &str) -> BlockStoreResult<()> {
        self.append_text_as(context_id, block_id, text, None)
    }

    /// Append text to a block with an explicit author identity.
    pub fn append_text_as(&self, context_id: ContextId, block_id: &BlockId, text: &str, agent_id: Option<PrincipalId>) -> BlockStoreResult<()> {
        let ops = {
            let mut entry = self.get_mut(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);
            // Capture frontier before append
            let frontier = entry.doc.frontier();
            entry.doc.append_text(block_id, text)?;
            entry.touch(effective_agent);
            // Get ops since frontier (the append we just applied)
            let ops = entry.doc.ops_since(&frontier);
            postcard::to_allocvec(&ops).map_err(|e| BlockStoreError::Serialization(e.to_string()))?
        };
        // Note: No auto-save for text appends (high frequency during streaming)

        // Emit CRDT ops for proper sync
        self.emit(BlockFlow::TextOps {
            context_id,
            block_id: block_id.clone(),
            ops: Arc::from(ops),
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Set collapsed state for a thinking block.
    pub fn set_collapsed(&self, context_id: ContextId, block_id: &BlockId, collapsed: bool) -> BlockStoreResult<()> {
        {
            let mut entry = self.get_mut(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let agent_id = self.agent_id();
            entry.doc.set_collapsed(block_id, collapsed)?;
            entry.touch(agent_id);
        }
        self.auto_save(context_id);

        // Emit flow event
        self.emit(BlockFlow::CollapsedChanged {
            context_id,
            block_id: block_id.clone(),
            collapsed,
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Delete a block from a document.
    pub fn delete_block(&self, context_id: ContextId, block_id: &BlockId) -> BlockStoreResult<()> {
        {
            let mut entry = self.get_mut(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let agent_id = self.agent_id();
            entry.doc.delete_block(block_id)?;
            entry.touch(agent_id);
        }
        self.auto_save(context_id);

        // Emit flow event
        self.emit(BlockFlow::Deleted {
            context_id,
            block_id: block_id.clone(),
            source: OpSource::Local,
        });

        Ok(())
    }

    // =========================================================================
    // Sync Operations
    // =========================================================================

    /// Get sync payload since a frontier for a document.
    pub fn ops_since(&self, context_id: ContextId, frontier: &HashMap<BlockId, Frontier>) -> BlockStoreResult<SyncPayload> {
        let entry = self.get(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        Ok(entry.doc.ops_since(frontier))
    }

    /// Merge a sync payload into a document.
    pub fn merge_ops(&self, context_id: ContextId, payload: SyncPayload) -> BlockStoreResult<u64> {
        let (version, events) = {
            let mut entry = self.get_mut(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let before = entry.doc.blocks_ordered();
            let frontier_before = entry.doc.frontier();
            entry.doc.merge_ops(payload)?;
            let version = entry.doc.version();
            entry.version.store(version, Ordering::SeqCst);
            let after = entry.doc.blocks_ordered();
            let ops_bytes = postcard::to_allocvec(&entry.doc.ops_since(&frontier_before))
                .unwrap_or_default();
            (version, Self::diff_block_events(context_id, &before, &after, ops_bytes))
        };
        for event in events {
            self.emit(event);
        }
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
        let entry = self.get(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        Ok(entry.doc.frontier())
    }

    // =========================================================================
    // Query Operations
    // =========================================================================

    /// Get block snapshots for a document.
    pub fn block_snapshots(&self, context_id: ContextId) -> BlockStoreResult<Vec<BlockSnapshot>> {
        let entry = self.get(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        Ok(entry.doc.blocks_ordered())
    }

    /// Get a single block snapshot by ID.
    pub fn get_block_snapshot(&self, context_id: ContextId, block_id: &BlockId) -> BlockStoreResult<Option<BlockSnapshot>> {
        let entry = self.get(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        Ok(entry.doc.get_block_snapshot(block_id))
    }

    /// Get multiple block snapshots by ID. Missing blocks are silently skipped.
    pub fn get_blocks_by_ids(&self, context_id: ContextId, ids: &[BlockId]) -> BlockStoreResult<Vec<BlockSnapshot>> {
        let entry = self.get(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
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
    pub fn query_blocks(&self, context_id: ContextId, filter: &BlockFilter) -> BlockStoreResult<Vec<BlockSnapshot>> {
        let entry = self.get(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;

        // If parent_id is set, compute descendant set via BFS
        let descendant_ids = if let Some(ref root_id) = filter.parent_id {
            Some(compute_descendants(&entry.doc, root_id, filter.max_depth))
        } else {
            None
        };

        let mut result = Vec::new();
        let limit = if filter.limit > 0 { filter.limit as usize } else { usize::MAX };

        for block in entry.doc.blocks_ordered() {
            // If we have a descendant set, check membership
            if let Some(ref descendants) = descendant_ids {
                if !descendants.contains(&block.id) {
                    continue;
                }
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
        let entry = self.get(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        let snapshot = entry.doc.snapshot();
        let bytes = postcard::to_allocvec(&snapshot)
            .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
        Ok((bytes, entry.version()))
    }

    /// Get the full text content of a document.
    pub fn get_content(&self, context_id: ContextId) -> BlockStoreResult<String> {
        let entry = self.get(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        Ok(entry.content())
    }

    /// Get document metadata and version.
    pub fn get_document_state(
        &self,
        context_id: ContextId,
    ) -> BlockStoreResult<(DocumentKind, Option<String>, Vec<BlockSnapshot>, u64)> {
        let entry = self.get(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
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
    /// For each document, loads both metadata and content from the snapshot table.
    /// The `oplog_bytes` column stores postcard-encoded StoreSnapshot.
    pub fn load_from_db(&self) -> BlockStoreResult<()> {
        let db = self.db.as_ref().ok_or(BlockStoreError::NoDatabaseConfigured)?;
        let db_guard = db.lock().map_err(|e| BlockStoreError::Db(e.to_string()))?;

        let kernel_id = self.kernel_id.ok_or_else(|| BlockStoreError::Db("no kernel_id configured".into()))?;
        let documents = db_guard
            .list_documents(kernel_id)
            .map_err(|e| BlockStoreError::Db(e.to_string()))?;

        let agent_id = self.agent_id();
        for doc in documents {
            let context_id = doc.document_id;

            // Try to load snapshot for this document
            let entry = if let Ok(Some(snapshot_record)) = db_guard.get_snapshot(context_id) {
                if let Some(oplog_bytes) = snapshot_record.oplog_bytes {
                    match postcard::from_bytes::<StoreSnapshot>(&oplog_bytes) {
                        Ok(store_snapshot) => {
                            tracing::debug!(
                                document_id = %context_id.to_hex(),
                                blocks = store_snapshot.blocks.len(),
                                "Restored document from store snapshot"
                            );
                            DocumentEntry::from_store_snapshot(store_snapshot, doc.doc_kind, doc.language.clone(), agent_id)
                        }
                        Err(e) => {
                            tracing::error!(
                                document_id = %context_id.to_hex(),
                                error = %e,
                                "Failed to deserialize store snapshot, skipping (wipe DB to recover)"
                            );
                            continue;
                        }
                    }
                } else {
                    DocumentEntry::new(context_id, doc.doc_kind, doc.language.clone(), agent_id)
                }
            } else {
                DocumentEntry::new(context_id, doc.doc_kind, doc.language.clone(), agent_id)
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
        if self.documents.contains_key(&context_id) {
            return Ok(false); // already loaded
        }

        let db = self.db.as_ref().ok_or(BlockStoreError::NoDatabaseConfigured)?;
        let db_guard = db.lock().map_err(|e| BlockStoreError::Db(e.to_string()))?;

        let doc = db_guard
            .get_document(context_id)
            .map_err(|e| BlockStoreError::Db(e.to_string()))?;

        let Some(doc) = doc else {
            return Ok(false);
        };

        let agent_id = self.agent_id();
        let entry = if let Ok(Some(snapshot_record)) = db_guard.get_snapshot(context_id) {
            if let Some(oplog_bytes) = snapshot_record.oplog_bytes {
                match postcard::from_bytes::<StoreSnapshot>(&oplog_bytes) {
                    Ok(store_snapshot) => {
                        tracing::debug!(
                            document_id = %context_id.to_hex(),
                            blocks = store_snapshot.blocks.len(),
                            "Hydrated document from DB"
                        );
                        DocumentEntry::from_store_snapshot(store_snapshot, doc.doc_kind, doc.language.clone(), agent_id)
                    }
                    Err(e) => {
                        tracing::warn!(document_id = %context_id.to_hex(), error = %e, "Failed to deserialize snapshot");
                        return Ok(false);
                    }
                }
            } else {
                return Ok(false);
            }
        } else {
            return Ok(false);
        };

        self.documents.insert(context_id, entry);
        Ok(true)
    }

    /// Save a document's content to the database as a snapshot.
    ///
    /// Stores the StoreSnapshot as postcard binary in the `oplog_bytes` column.
    pub fn save_snapshot(&self, context_id: ContextId) -> BlockStoreResult<()> {
        let db = self.db.as_ref().ok_or(BlockStoreError::NoDatabaseConfigured)?;

        let entry = self.get(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
        let snapshot = entry.doc.snapshot();
        let version = entry.version() as i64;
        let content = entry.content();

        let snapshot_bytes = postcard::to_allocvec(&snapshot)
            .map_err(|e| BlockStoreError::Serialization(e.to_string()))?;

        drop(entry); // Release the read lock before acquiring DB lock

        let db_guard = db.lock().map_err(|e| BlockStoreError::Db(e.to_string()))?;
        db_guard
            .save_snapshot(context_id, version, &content, Some(&snapshot_bytes))
            .map_err(|e| BlockStoreError::Db(e.to_string()))?;

        Ok(())
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
    pub fn create_input_doc(&self, context_id: ContextId) -> BlockStoreResult<()> {
        use dashmap::mapref::entry::Entry;

        match self.input_docs.entry(context_id) {
            Entry::Occupied(_) => Ok(()), // Already exists
            Entry::Vacant(vacant) => {
                let agent_id = self.agent_id();
                let entry = InputDocEntry::new(agent_id);

                // Persist if we have a DB
                if let Some(db) = &self.db {
                    let db_guard = db.lock().map_err(|e| BlockStoreError::Db(e.to_string()))?;
                    let _ = db_guard.create_input_doc(context_id);
                }

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
        let mut entry = self.input_docs.get_mut(&context_id)
            .ok_or(BlockStoreError::InputDocNotFound(context_id))?;

        let ops = entry.edit_text(pos, insert, delete).map_err(BlockStoreError::Serialization)?;

        self.emit_input(InputDocFlow::TextOps {
            context_id,
            ops: Arc::from(ops.clone()),
            source: crate::flows::OpSource::Local,
        });

        Ok(ops)
    }

    /// Get the current input text for a context.
    pub fn get_input_text(&self, context_id: ContextId) -> BlockStoreResult<String> {
        let entry = self.input_docs.get(&context_id)
            .ok_or(BlockStoreError::InputDocNotFound(context_id))?;
        Ok(entry.get_text())
    }

    /// Get the full input document state (text + ops + version) for sync.
    pub fn get_input_state(&self, context_id: ContextId) -> BlockStoreResult<(String, Vec<u8>, u64)> {
        let entry = self.input_docs.get(&context_id)
            .ok_or(BlockStoreError::InputDocNotFound(context_id))?;
        let text = entry.get_text();
        let ops = entry.all_ops().map_err(BlockStoreError::Serialization)?;
        let version = entry.version();
        Ok((text, ops, version))
    }

    /// Get input ops since a frontier (for incremental sync).
    pub fn input_ops_since(&self, context_id: ContextId, frontier: &Frontier) -> BlockStoreResult<Vec<u8>> {
        let entry = self.input_docs.get(&context_id)
            .ok_or(BlockStoreError::InputDocNotFound(context_id))?;
        entry.ops_since(frontier).map_err(BlockStoreError::Serialization)
    }

    /// Merge remote ops into an input document.
    pub fn merge_input_ops(&self, context_id: ContextId, ops_bytes: &[u8]) -> BlockStoreResult<u64> {
        let mut entry = self.input_docs.get_mut(&context_id)
            .ok_or(BlockStoreError::InputDocNotFound(context_id))?;

        entry.merge_ops(ops_bytes).map_err(BlockStoreError::Serialization)?;

        self.emit_input(InputDocFlow::TextOps {
            context_id,
            ops: Arc::from(ops_bytes.to_vec()),
            source: crate::flows::OpSource::Remote,
        });

        Ok(entry.version())
    }

    /// Clear the input document for a context.
    ///
    /// Returns the text that was in the input doc before clearing.
    pub fn clear_input(&self, context_id: ContextId) -> BlockStoreResult<String> {
        let mut entry = self.input_docs.get_mut(&context_id)
            .ok_or(BlockStoreError::InputDocNotFound(context_id))?;

        let (text, _ops) = entry.clear().map_err(BlockStoreError::Serialization)?;

        self.emit_input(InputDocFlow::Cleared { context_id });

        // Persist cleared state
        if let Some(db) = &self.db {
            if let Ok(db_guard) = db.lock() {
                let _ = db_guard.clear_input_doc(context_id);
            }
        }

        Ok(text)
    }

    /// Save the current input document state to the database.
    pub fn save_input_snapshot(&self, context_id: ContextId) -> BlockStoreResult<()> {
        let db = self.db.as_ref().ok_or(BlockStoreError::NoDatabaseConfigured)?;

        let entry = self.input_docs.get(&context_id)
            .ok_or(BlockStoreError::InputDocNotFound(context_id))?;

        let text = entry.get_text();
        let ops_bytes = entry.all_ops().map_err(BlockStoreError::Serialization)?;
        let version = entry.version() as i64;
        drop(entry);

        let db_guard = db.lock().map_err(|e| BlockStoreError::Db(e.to_string()))?;
        db_guard.upsert_input_doc(context_id, &text, Some(&ops_bytes), version)
            .map_err(|e| BlockStoreError::Db(e.to_string()))?;

        Ok(())
    }

    /// Load input documents from database on startup.
    pub fn load_input_docs_from_db(&self) -> BlockStoreResult<()> {
        let db = self.db.as_ref().ok_or(BlockStoreError::NoDatabaseConfigured)?;
        let db_guard = db.lock().map_err(|e| BlockStoreError::Db(e.to_string()))?;

        let rows = db_guard.list_input_docs()
            .map_err(|e| BlockStoreError::Db(e.to_string()))?;
        drop(db_guard);

        let agent_id = self.agent_id();

        for (context_id, oplog_bytes) in rows {
            let entry = if let Some(ops_bytes) = oplog_bytes {
                match InputDocEntry::from_ops(&ops_bytes, agent_id) {
                    Ok(entry) => {
                        tracing::debug!(context_id = %context_id.to_hex(), text_len = entry.get_text().len(), "Restored input doc from oplog");
                        entry
                    }
                    Err(e) => {
                        tracing::warn!(context_id = %context_id.to_hex(), error = %e, "Failed to restore input doc, creating empty");
                        InputDocEntry::new(agent_id)
                    }
                }
            } else {
                InputDocEntry::new(agent_id)
            };

            self.input_docs.insert(context_id, entry);
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
        self.insert_drift_block_as(context_id, parent_id, after, content, source_context, source_model, drift_kind, None)
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
        let (block_id, snapshot, ops) = {
            let mut entry = self.get_mut(context_id).ok_or(BlockStoreError::DocumentNotFound(context_id))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);

            let frontier_before = entry.doc.frontier();

            let block_id = entry.doc.insert_drift_block(parent_id, after, content, source_context, source_model, drift_kind)
                ?;
            let snapshot = entry.doc.get_block_snapshot(&block_id)
                .ok_or(BlockStoreError::BlockNotFoundAfterInsert)?;

            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = postcard::to_allocvec(&ops).map_err(|e| BlockStoreError::Serialization(e.to_string()))?;
            entry.touch(effective_agent);
            (block_id, snapshot, ops_bytes)
        };
        self.auto_save(context_id);

        self.emit(BlockFlow::Inserted {
            context_id,
            block: Arc::new(snapshot),
            after_id,
            ops: Arc::from(ops),
            source: OpSource::Local,
        });

        Ok(block_id)
    }
}

/// BFS from `root_id` collecting all descendant block IDs up to `max_depth` levels.
/// Depth 0 = unlimited. The root itself is included in the result set.
fn compute_descendants(doc: &CrdtBlockStore, root_id: &BlockId, max_depth: u32) -> HashSet<BlockId> {
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
    Arc::new(BlockStore::with_db(db, kernel_id, default_workspace_id, agent_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_agent() -> PrincipalId { PrincipalId::new() }

    #[test]
    fn test_block_store_basic_ops() {
        let store = BlockStore::new(test_agent());
        let ctx = ContextId::new();

        store.create_document(ctx, DocumentKind::Code, Some("rust".into())).unwrap();

        // Insert a text block using new API
        let block_id = store.insert_block(ctx, None, None, Role::User, BlockKind::Text, "hello world", Status::Done).unwrap();
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

        store.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        // Insert thinking block
        let thinking_id = store.insert_block(ctx, None, None, Role::Model, BlockKind::Thinking, "Let me think...", Status::Done).unwrap();

        // Insert text block after thinking (as child of root, after thinking in order)
        let text_id = store.insert_block(ctx, None, Some(&thinking_id), Role::Model, BlockKind::Text, "Here's my answer", Status::Done).unwrap();

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

        store.create_document(ctx, DocumentKind::Code, Some("rust".into())).unwrap();

        store.insert_block(ctx, None, None, Role::User, BlockKind::Text, "fn main() {}", Status::Done).unwrap();

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

        store.create_document(conv1, DocumentKind::Conversation, None).unwrap();
        store.create_document(conv2, DocumentKind::Conversation, None).unwrap();
        store.create_document(code1, DocumentKind::Code, Some("rust".into())).unwrap();
        store.create_document(config1, DocumentKind::Config, None).unwrap();

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

        store.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let thinking_id = store.insert_block(ctx, None, None, Role::Model, BlockKind::Thinking, "thinking...", Status::Done).unwrap();
        store.insert_block(ctx, None, Some(&thinking_id), Role::Model, BlockKind::Text, "response", Status::Done).unwrap();

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

        store.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let block_id = store.insert_block(ctx, None, None, Role::Model, BlockKind::ToolCall, "{}", Status::Done).unwrap();

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
                    let _ = store_clone.insert_block(ctx, None, None, Role::User, BlockKind::Text, &text, Status::Done);
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
                    let _ = store_clone.insert_block(ctx, None, None, Role::User, BlockKind::Text, &text, Status::Done);
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
            assert!(!content.is_empty(), "Document {} should have content", ctx.to_hex());
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
        let block_id = store.insert_block(ctx, None, None, Role::User, BlockKind::Text, "initial content", Status::Done).unwrap();

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

        store.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        // Client syncs from snapshot
        let snapshot = store.get(ctx).unwrap().doc.snapshot();
        let mut client = CrdtBlockStore::from_snapshot(snapshot, PrincipalId::new());
        assert_eq!(client.block_count(), 0);

        // Server inserts a block
        let block_id = store.insert_block(
            ctx, None, None, Role::User, BlockKind::Text, "Hello from server", Status::Done
        ).unwrap();

        // Get the BlockInserted event with ops
        let msg = sub.try_recv().expect("should receive BlockInserted event");
        let ops = match msg.payload {
            BlockFlow::Inserted { ops, .. } => ops,
            _ => panic!("expected BlockInserted event"),
        };

        // Deserialize SyncPayload and merge on client
        let payload: SyncPayload = postcard::from_bytes(&ops).expect("should deserialize SyncPayload");
        client.merge_ops(payload).expect("client should merge sync payload");

        // Verify client has the block
        assert_eq!(client.block_count(), 1);
        let snapshot = client.get_block_snapshot(&block_id).expect("block should exist on client");
        assert_eq!(snapshot.content, "Hello from server");
    }

    /// Test that insert_tool_call emits mergeable SyncPayload.
    #[tokio::test]
    async fn test_insert_tool_call_emits_sync_payload() {
        let (store, bus) = store_with_flows();
        let mut sub = bus.subscribe("block.>");
        let ctx = ContextId::new();

        store.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let snapshot = store.get(ctx).unwrap().doc.snapshot();
        let mut client = CrdtBlockStore::from_snapshot(snapshot, PrincipalId::new());

        let block_id = store.insert_tool_call(
            ctx, None, None, "bash", serde_json::json!({"command": "ls -la"}), None
        ).unwrap();

        let msg = sub.try_recv().expect("should receive event");
        let ops = match msg.payload {
            BlockFlow::Inserted { ops, .. } => ops,
            _ => panic!("expected BlockInserted"),
        };

        let payload: SyncPayload = postcard::from_bytes(&ops).unwrap();
        client.merge_ops(payload).expect("should merge tool_call sync payload");

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

        store.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let snapshot = store.get(ctx).unwrap().doc.snapshot();
        let mut client = CrdtBlockStore::from_snapshot(snapshot, PrincipalId::new());

        for i in 0..5 {
            let _ = store.insert_block(
                ctx, None, None, Role::User, BlockKind::Text, format!("Message {}", i), Status::Done
            ).unwrap();

            let msg = sub.try_recv().expect("should receive event");
            let ops = match msg.payload {
                BlockFlow::Inserted { ops, .. } => ops,
                _ => panic!("expected BlockInserted"),
            };

            let payload: SyncPayload = postcard::from_bytes(&ops).unwrap();
            client.merge_ops(payload).expect(&format!("should merge block {i}"));
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

        store.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let block_id = store.insert_block(
            ctx, None, None, Role::Model, BlockKind::Text, "", Status::Done
        ).unwrap();
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
            client.merge_ops(payload).expect(&format!("should merge chunk '{chunk}'"));
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

        server.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        // Snapshot before insert
        let initial_snapshot = {
            let entry = server.get(ctx).unwrap();
            postcard::to_allocvec(&entry.doc.snapshot()).unwrap()
        };

        let frontier_before = server.frontier(ctx).unwrap();

        let _block_id = server.insert_block(
            ctx, None, None, Role::User, BlockKind::Text, "hello from remote", Status::Done,
        ).unwrap();

        let msg = sub.try_recv().expect("should get Inserted from insert_block");
        assert!(matches!(msg.payload, BlockFlow::Inserted { .. }));

        let ops = server.ops_since(ctx, &frontier_before).unwrap();

        // Create receiver from initial snapshot
        let (receiver, recv_bus) = store_with_flows();
        let mut recv_sub = recv_bus.subscribe("block.>");

        receiver.create_document_from_snapshot(
            ctx, DocumentKind::Conversation, None, &initial_snapshot
        ).unwrap();

        receiver.merge_ops(ctx, ops).unwrap();

        let msg = recv_sub.try_recv().expect("merge_ops should emit Inserted event");
        match msg.payload {
            BlockFlow::Inserted { context_id, block, .. } => {
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
        server_a.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let block_id = server_a.insert_block(
            ctx, None, None, Role::Model, BlockKind::Text, "initial", Status::Done,
        ).unwrap();

        // Receiver syncs via proper protocol: empty document + ops_since
        let (receiver, recv_bus) = store_with_flows();
        receiver.create_document(ctx, DocumentKind::Conversation, None).unwrap();
        let initial_ops = server_a.ops_since(ctx, &HashMap::new()).unwrap();
        receiver.merge_ops(ctx, initial_ops).unwrap();

        let recv_frontier = receiver.frontier(ctx).unwrap();

        // Modify block on A
        server_a.set_status(ctx, &block_id, Status::Running).unwrap();
        server_a.edit_text(ctx, &block_id, 7, " content", 0).unwrap();

        // Compute diff — frontier types differ (per-block vs per-block), but both stores
        // use HashMap<BlockId, Frontier>, so we can pass receiver's frontier to server A
        let diff_ops = server_a.ops_since(ctx, &recv_frontier).unwrap();

        let mut recv_sub = recv_bus.subscribe("block.>");
        receiver.merge_ops(ctx, diff_ops).unwrap();

        let mut events = Vec::new();
        while let Some(msg) = recv_sub.try_recv() {
            events.push(msg.payload);
        }

        let has_status = events.iter().any(|e| matches!(e, BlockFlow::StatusChanged { status: Status::Running, .. }));
        let has_text = events.iter().any(|e| matches!(e, BlockFlow::TextOps { .. }));

        assert!(has_status, "should emit StatusChanged, got: {:?}", events);
        assert!(has_text, "should emit TextOps, got: {:?}", events);
    }

    /// Integration test: stream → finalize → verify content preserved.
    #[tokio::test]
    async fn test_streaming_lifecycle() {
        let (store, _bus) = store_with_flows();
        let ctx = ContextId::new();
        store.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let block_id = store.insert_block(
            ctx, None, None, Role::Model, BlockKind::Text, "", Status::Done,
        ).unwrap();
        store.set_status(ctx, &block_id, Status::Running).unwrap();

        let streaming_text = "The quick brown fox jumps over the lazy dog. ".repeat(20);
        for (i, ch) in streaming_text.chars().enumerate() {
            store.edit_text(ctx, &block_id, i, &ch.to_string(), 0).unwrap();
        }

        store.set_status(ctx, &block_id, Status::Done).unwrap();

        let entry = store.get(ctx).unwrap();
        let snap = entry.doc.get_block_snapshot(&block_id).unwrap();
        assert_eq!(snap.content, streaming_text);
        assert_eq!(snap.status, Status::Done);
    }
}
