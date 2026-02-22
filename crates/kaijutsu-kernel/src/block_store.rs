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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use diamond_types_extended::Frontier;
use parking_lot::RwLock;

use kaijutsu_crdt::block_store::{BlockStore as CrdtBlockStore, StoreSnapshot, SyncPayload};
use kaijutsu_crdt::{BlockId, BlockKind, BlockSnapshot, Role, Status};
use kaijutsu_types::{ContextId, PrincipalId};

use crate::db::{DocumentDb, DocumentKind, DocumentMeta};
use crate::flows::{BlockFlow, OpSource, SharedBlockFlowBus};

/// Thread-safe database handle.
type DbHandle = Arc<std::sync::Mutex<DocumentDb>>;

/// Entry for a document in the store.
pub struct DocumentEntry {
    /// Per-block CRDT store (each block owns its own DTE Document).
    pub doc: CrdtBlockStore,
    /// Document metadata.
    pub kind: DocumentKind,
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
    fn new(context_id: ContextId, kind: DocumentKind, language: Option<String>, agent_id: PrincipalId) -> Self {
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
        kind: DocumentKind,
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
    /// Database for persistence.
    db: Option<DbHandle>,
    /// Default agent ID for this store.
    agent_id: RwLock<PrincipalId>,
    /// FlowBus for typed pub/sub.
    block_flows: Option<SharedBlockFlowBus>,
}

impl BlockStore {
    /// Create a new in-memory block store.
    pub fn new(agent_id: PrincipalId) -> Self {
        Self {
            documents: DashMap::new(),
            db: None,
            agent_id: RwLock::new(agent_id),
            block_flows: None,
        }
    }

    /// Create a new in-memory block store with FlowBus.
    pub fn with_flows(agent_id: PrincipalId, block_flows: SharedBlockFlowBus) -> Self {
        Self {
            documents: DashMap::new(),
            db: None,
            agent_id: RwLock::new(agent_id),
            block_flows: Some(block_flows),
        }
    }

    /// Create a block store with SQLite persistence.
    pub fn with_db(db: DocumentDb, agent_id: PrincipalId) -> Self {
        Self {
            documents: DashMap::new(),
            db: Some(Arc::new(std::sync::Mutex::new(db))),
            agent_id: RwLock::new(agent_id),
            block_flows: None,
        }
    }

    /// Create a block store with SQLite persistence and FlowBus.
    pub fn with_db_and_flows(
        db: DocumentDb,
        agent_id: PrincipalId,
        block_flows: SharedBlockFlowBus,
    ) -> Self {
        Self {
            documents: DashMap::new(),
            db: Some(Arc::new(std::sync::Mutex::new(db))),
            agent_id: RwLock::new(agent_id),
            block_flows: Some(block_flows),
        }
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
        kind: DocumentKind,
        language: Option<String>,
    ) -> Result<(), String> {
        use dashmap::mapref::entry::Entry;

        let id_hex = context_id.to_hex();
        match self.documents.entry(context_id) {
            Entry::Occupied(_) => {
                Err(format!("Document {} already exists", id_hex))
            }
            Entry::Vacant(vacant) => {
                // Persist metadata if we have a DB
                if let Some(db) = &self.db {
                    let db_guard = db.lock().map_err(|e| format!("DB lock error: {}", e))?;
                    let meta = DocumentMeta {
                        id: id_hex.clone(),
                        kind,
                        language: language.clone(),
                        position_col: None,
                        position_row: None,
                        parent_document: None,
                        created_at: 0, // Unused - DB default (unixepoch()) handles timestamp
                    };
                    match db_guard.create_document(&meta) {
                        Ok(()) => {}
                        Err(e) if e.to_string().contains("UNIQUE constraint") => {
                            // Document exists in DB (e.g., load_from_db skipped a
                            // corrupted snapshot) but not in memory. Proceed with
                            // DashMap insert so the document becomes usable again.
                            tracing::warn!(context_id = %id_hex, "Document already in DB but not in memory, recovering");
                        }
                        Err(e) => return Err(format!("DB error: {}", e)),
                    }
                }

                let agent_id = self.agent_id();
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
        kind: DocumentKind,
        language: Option<String>,
        snapshot_bytes: &[u8],
    ) -> Result<(), String> {
        if self.documents.contains_key(&context_id) {
            return Err(format!("Document {} already exists", context_id.to_hex()));
        }

        let snapshot: StoreSnapshot = serde_json::from_slice(snapshot_bytes)
            .map_err(|e| format!("Failed to deserialize store snapshot: {}", e))?;

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

    /// Check if a document exists.
    pub fn contains(&self, context_id: ContextId) -> bool {
        self.documents.contains_key(&context_id)
    }

    /// Delete a document.
    pub fn delete_document(&self, context_id: ContextId) -> Result<(), String> {
        if let Some(db) = &self.db {
            let db_guard = db.lock().map_err(|e| format!("DB lock error: {}", e))?;
            db_guard
                .delete_document(&context_id.to_hex())
                .map_err(|e| format!("DB error: {}", e))?;
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
    ) -> Result<(), String> {
        if self.documents.contains_key(&new_id) {
            return Err(format!("Document {} already exists", new_id.to_hex()));
        }

        let source_entry = self.get(source_id)
            .ok_or_else(|| format!("Source document {} not found", source_id.to_hex()))?;

        let agent_id = self.agent_id();
        let forked_store = source_entry.doc.fork(new_id, agent_id);
        let kind = source_entry.kind;
        let language = source_entry.language.clone();
        drop(source_entry); // Release the read lock

        // Persist metadata if we have a DB
        if let Some(db) = &self.db {
            let db_guard = db.lock().map_err(|e| format!("DB lock error: {}", e))?;
            let meta = DocumentMeta {
                id: new_id.to_hex(),
                kind,
                language: language.clone(),
                position_col: None,
                position_row: None,
                parent_document: Some(source_id.to_hex()),
                created_at: 0, // DB default handles timestamp
            };
            db_guard
                .create_document(&meta)
                .map_err(|e| format!("DB error: {}", e))?;
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
    ) -> Result<(), String> {
        if self.documents.contains_key(&new_id) {
            return Err(format!("Document {} already exists", new_id.to_hex()));
        }

        let source_entry = self.get(source_id)
            .ok_or_else(|| format!("Source document {} not found", source_id.to_hex()))?;

        // Validate version
        let current_version = source_entry.version();
        if at_version > current_version {
            return Err(format!(
                "Requested version {} is in the future (current: {})",
                at_version, current_version
            ));
        }

        let agent_id = self.agent_id();
        let forked_store = source_entry.doc.fork_at_version(new_id, agent_id, at_version);
        let kind = source_entry.kind;
        let language = source_entry.language.clone();
        drop(source_entry); // Release the read lock

        // Persist metadata if we have a DB
        if let Some(db) = &self.db {
            let db_guard = db.lock().map_err(|e| format!("DB lock error: {}", e))?;
            let meta = DocumentMeta {
                id: new_id.to_hex(),
                kind,
                language: language.clone(),
                position_col: None,
                position_row: None,
                parent_document: Some(source_id.to_hex()),
                created_at: 0, // DB default handles timestamp
            };
            db_guard
                .create_document(&meta)
                .map_err(|e| format!("DB error: {}", e))?;
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
    ) -> Result<BlockId, String> {
        self.insert_block_as(context_id, parent_id, after, role, kind, content, None)
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
        agent_id: Option<PrincipalId>,
    ) -> Result<BlockId, String> {
        let after_id = after.cloned();
        let (block_id, snapshot, ops) = {
            let mut entry = self.get_mut(context_id).ok_or_else(|| format!("Document {} not found", context_id.to_hex()))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());

            // Set the agent for this operation so BlockId gets the right author
            entry.doc.set_agent_id(effective_agent);

            // Capture frontier before the operation for incremental ops.
            // Clients that are in sync can merge these directly.
            // Clients that are out of sync will get full oplog via get_document_state.
            let frontier_before = entry.doc.frontier();

            let block_id = entry.doc.insert_block(parent_id, after, role, kind, content)
                .map_err(|e| e.to_string())?;
            let snapshot = entry.doc.get_block_snapshot(&block_id)
                .ok_or_else(|| "Block not found after insert".to_string())?;

            // Send incremental ops (just this operation) for efficient sync.
            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = serde_json::to_vec(&ops).map_err(|e| format!("serialize ops: {e}"))?;
            entry.touch(effective_agent);
            (block_id, snapshot, ops_bytes)
        };
        self.auto_save(context_id);

        // Emit flow event with creation ops
        self.emit(BlockFlow::Inserted {
            context_id,
            block: snapshot,
            after_id,
            ops,
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
    ) -> Result<BlockId, String> {
        self.insert_tool_call_as(context_id, parent_id, after, tool_name, tool_input, None)
    }

    /// Insert a tool call block with an explicit author identity.
    pub fn insert_tool_call_as(
        &self,
        context_id: ContextId,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        tool_name: impl Into<String>,
        tool_input: serde_json::Value,
        agent_id: Option<PrincipalId>,
    ) -> Result<BlockId, String> {
        let after_id = after.cloned();
        let (block_id, snapshot, ops) = {
            let mut entry = self.get_mut(context_id).ok_or_else(|| format!("Document {} not found", context_id.to_hex()))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);

            // Capture frontier before the operation for incremental ops
            let frontier_before = entry.doc.frontier();

            let block_id = entry.doc.insert_tool_call(parent_id, after, tool_name, tool_input)
                .map_err(|e| e.to_string())?;
            let snapshot = entry.doc.get_block_snapshot(&block_id)
                .ok_or_else(|| "Block not found after insert".to_string())?;

            // Send incremental ops (just this operation) for efficient sync
            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = serde_json::to_vec(&ops).map_err(|e| format!("serialize ops: {e}"))?;
            entry.touch(effective_agent);
            (block_id, snapshot, ops_bytes)
        };
        self.auto_save(context_id);

        // Emit flow event with creation ops
        self.emit(BlockFlow::Inserted {
            context_id,
            block: snapshot,
            after_id,
            ops,
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
    ) -> Result<BlockId, String> {
        self.insert_tool_result_as(context_id, tool_call_id, after, content, is_error, exit_code, None)
    }

    /// Insert a tool result block with an explicit author identity.
    pub fn insert_tool_result_as(
        &self,
        context_id: ContextId,
        tool_call_id: &BlockId,
        after: Option<&BlockId>,
        content: impl Into<String>,
        is_error: bool,
        exit_code: Option<i32>,
        agent_id: Option<PrincipalId>,
    ) -> Result<BlockId, String> {
        let after_id = after.cloned();
        let (block_id, snapshot, ops) = {
            let mut entry = self.get_mut(context_id).ok_or_else(|| format!("Document {} not found", context_id.to_hex()))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);

            // Capture frontier before the operation for incremental ops
            let frontier_before = entry.doc.frontier();

            let block_id = entry.doc.insert_tool_result_block(tool_call_id, after, content, is_error, exit_code)
                .map_err(|e| e.to_string())?;
            let snapshot = entry.doc.get_block_snapshot(&block_id)
                .ok_or_else(|| "Block not found after insert".to_string())?;

            // Send incremental ops (just this operation) for efficient sync
            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = serde_json::to_vec(&ops).map_err(|e| format!("serialize ops: {e}"))?;
            entry.touch(effective_agent);
            (block_id, snapshot, ops_bytes)
        };
        self.auto_save(context_id);

        // Emit flow event with creation ops
        self.emit(BlockFlow::Inserted {
            context_id,
            block: snapshot,
            after_id,
            ops,
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
    ) -> Result<BlockId, String> {
        self.insert_from_snapshot_as(context_id, snapshot, after, None)
    }

    /// Insert a block from a snapshot with an explicit author identity.
    pub fn insert_from_snapshot_as(
        &self,
        context_id: ContextId,
        snapshot: BlockSnapshot,
        after: Option<&BlockId>,
        agent_id: Option<PrincipalId>,
    ) -> Result<BlockId, String> {
        let after_id = after.cloned();
        let (block_id, final_snapshot, ops) = {
            let mut entry = self.get_mut(context_id)
                .ok_or_else(|| format!("Document {} not found", context_id.to_hex()))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);

            let frontier_before = entry.doc.frontier();

            let block_id = entry.doc.insert_from_snapshot(snapshot, after)
                .map_err(|e| e.to_string())?;
            let final_snapshot = entry.doc.get_block_snapshot(&block_id)
                .ok_or_else(|| "Block not found after insert".to_string())?;

            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = serde_json::to_vec(&ops).map_err(|e| format!("serialize ops: {e}"))?;
            entry.touch(effective_agent);
            (block_id, final_snapshot, ops_bytes)
        };
        self.auto_save(context_id);

        self.emit(BlockFlow::Inserted {
            context_id,
            block: final_snapshot,
            after_id,
            ops,
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
    ) -> Result<(), String> {
        {
            let mut entry = self.get_mut(context_id).ok_or_else(|| format!("Document {} not found", context_id.to_hex()))?;
            let agent_id = self.agent_id();
            entry.doc.set_status(block_id, status).map_err(|e| e.to_string())?;
            entry.touch(agent_id);
        }
        self.auto_save(context_id);

        // Emit flow event
        self.emit(BlockFlow::StatusChanged {
            context_id,
            block_id: block_id.clone(),
            status,
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
    ) -> Result<(), String> {
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
    ) -> Result<(), String> {
        let ops = {
            let mut entry = self.get_mut(context_id).ok_or_else(|| format!("Document {} not found", context_id.to_hex()))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);
            // Capture frontier before edit
            let frontier = entry.doc.frontier();
            entry.doc.edit_text(block_id, pos, insert, delete).map_err(|e| e.to_string())?;
            entry.touch(effective_agent);
            // Get ops since frontier (the edit we just applied)
            let ops = entry.doc.ops_since(&frontier);
            serde_json::to_vec(&ops).map_err(|e| format!("serialize ops: {e}"))?
        };
        // Note: No auto-save for text edits (high frequency during streaming)

        // Emit CRDT ops for proper sync
        self.emit(BlockFlow::TextOps {
            context_id,
            block_id: block_id.clone(),
            ops,
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Set the display hint for a block.
    ///
    /// Display hints provide formatting information (tables, trees) for richer output.
    /// The hint is stored as a JSON string in the CRDT block.
    pub fn set_display_hint(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        hint: Option<&str>,
    ) -> Result<(), String> {
        let mut entry = self.get_mut(context_id).ok_or_else(|| format!("Document {} not found", context_id.to_hex()))?;
        let agent_id = self.agent_id();
        entry.doc.set_display_hint(block_id, hint).map_err(|e| e.to_string())?;
        entry.touch(agent_id);
        // Note: No event emission for display hint changes for now - they're synced with full state
        Ok(())
    }

    /// Append text to a block.
    ///
    /// Note: Does not auto-save to avoid excessive I/O during streaming.
    /// Call `save_snapshot()` explicitly when streaming is complete.
    pub fn append_text(&self, context_id: ContextId, block_id: &BlockId, text: &str) -> Result<(), String> {
        self.append_text_as(context_id, block_id, text, None)
    }

    /// Append text to a block with an explicit author identity.
    pub fn append_text_as(&self, context_id: ContextId, block_id: &BlockId, text: &str, agent_id: Option<PrincipalId>) -> Result<(), String> {
        let ops = {
            let mut entry = self.get_mut(context_id).ok_or_else(|| format!("Document {} not found", context_id.to_hex()))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);
            // Capture frontier before append
            let frontier = entry.doc.frontier();
            entry.doc.append_text(block_id, text).map_err(|e| e.to_string())?;
            entry.touch(effective_agent);
            // Get ops since frontier (the append we just applied)
            let ops = entry.doc.ops_since(&frontier);
            serde_json::to_vec(&ops).map_err(|e| format!("serialize ops: {e}"))?
        };
        // Note: No auto-save for text appends (high frequency during streaming)

        // Emit CRDT ops for proper sync
        self.emit(BlockFlow::TextOps {
            context_id,
            block_id: block_id.clone(),
            ops,
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Set collapsed state for a thinking block.
    pub fn set_collapsed(&self, context_id: ContextId, block_id: &BlockId, collapsed: bool) -> Result<(), String> {
        {
            let mut entry = self.get_mut(context_id).ok_or_else(|| format!("Document {} not found", context_id.to_hex()))?;
            let agent_id = self.agent_id();
            entry.doc.set_collapsed(block_id, collapsed).map_err(|e| e.to_string())?;
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
    pub fn delete_block(&self, context_id: ContextId, block_id: &BlockId) -> Result<(), String> {
        {
            let mut entry = self.get_mut(context_id).ok_or_else(|| format!("Document {} not found", context_id.to_hex()))?;
            let agent_id = self.agent_id();
            entry.doc.delete_block(block_id).map_err(|e| e.to_string())?;
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
    pub fn ops_since(&self, context_id: ContextId, frontier: &HashMap<BlockId, Frontier>) -> Result<SyncPayload, String> {
        let entry = self.get(context_id).ok_or_else(|| format!("Document {} not found", context_id.to_hex()))?;
        Ok(entry.doc.ops_since(frontier))
    }

    /// Merge a sync payload into a document.
    pub fn merge_ops(&self, context_id: ContextId, payload: SyncPayload) -> Result<u64, String> {
        let (version, events) = {
            let mut entry = self.get_mut(context_id).ok_or_else(|| format!("Document {} not found", context_id.to_hex()))?;
            let before = entry.doc.blocks_ordered();
            let frontier_before = entry.doc.frontier();
            entry.doc.merge_ops(payload).map_err(|e| e.to_string())?;
            let version = entry.doc.version();
            entry.version.store(version, Ordering::SeqCst);
            let after = entry.doc.blocks_ordered();
            let ops_bytes = serde_json::to_vec(&entry.doc.ops_since(&frontier_before))
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

        let mut events = Vec::new();

        // New blocks
        for (i, snap) in after.iter().enumerate() {
            if !before_map.contains_key(&snap.id) {
                let after_id = if i > 0 { Some(after[i - 1].id) } else { None };
                events.push(BlockFlow::Inserted {
                    context_id,
                    block: snap.clone(),
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
    pub fn frontier(&self, context_id: ContextId) -> Result<HashMap<BlockId, Frontier>, String> {
        let entry = self.get(context_id).ok_or_else(|| format!("Document {} not found", context_id.to_hex()))?;
        Ok(entry.doc.frontier())
    }

    // =========================================================================
    // Query Operations
    // =========================================================================

    /// Get block snapshots for a document.
    pub fn block_snapshots(&self, context_id: ContextId) -> Result<Vec<BlockSnapshot>, String> {
        let entry = self.get(context_id).ok_or_else(|| format!("Document {} not found", context_id.to_hex()))?;
        Ok(entry.doc.blocks_ordered())
    }

    /// Get the full text content of a document.
    pub fn get_content(&self, context_id: ContextId) -> Result<String, String> {
        let entry = self.get(context_id).ok_or_else(|| format!("Document {} not found", context_id.to_hex()))?;
        Ok(entry.content())
    }

    /// Get document metadata and version.
    pub fn get_document_state(
        &self,
        context_id: ContextId,
    ) -> Result<(DocumentKind, Option<String>, Vec<BlockSnapshot>, u64), String> {
        let entry = self.get(context_id).ok_or_else(|| format!("Document {} not found", context_id.to_hex()))?;
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
    pub fn load_from_db(&self) -> Result<(), String> {
        let db = self.db.as_ref().ok_or("No database configured")?;
        let db_guard = db.lock().map_err(|e| format!("DB lock error: {}", e))?;

        let document_metas = db_guard
            .list_documents()
            .map_err(|e| format!("DB error: {}", e))?;

        let agent_id = self.agent_id();
        for meta in document_metas {
            // Parse the DB string ID back to ContextId
            let context_id = match ContextId::parse(&meta.id) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(
                        document_id = %meta.id,
                        error = %e,
                        "Failed to parse document ID as ContextId, skipping"
                    );
                    continue;
                }
            };

            // Try to load snapshot for this document
            let entry = if let Ok(Some(snapshot_record)) = db_guard.get_snapshot(&meta.id) {
                if let Some(oplog_bytes) = snapshot_record.oplog_bytes {
                    match serde_json::from_slice::<StoreSnapshot>(&oplog_bytes) {
                        Ok(store_snapshot) => {
                            tracing::debug!(
                                document_id = %meta.id,
                                blocks = store_snapshot.blocks.len(),
                                "Restored document from store snapshot"
                            );
                            DocumentEntry::from_store_snapshot(store_snapshot, meta.kind, meta.language.clone(), agent_id)
                        }
                        Err(e) => {
                            tracing::error!(
                                document_id = %meta.id,
                                error = %e,
                                "Failed to deserialize store snapshot, skipping corrupted document"
                            );
                            continue;
                        }
                    }
                } else {
                    // Snapshot exists but no oplog_bytes, start empty
                    DocumentEntry::new(context_id, meta.kind, meta.language.clone(), agent_id)
                }
            } else {
                // No snapshot, start empty
                DocumentEntry::new(context_id, meta.kind, meta.language.clone(), agent_id)
            };

            self.documents.insert(context_id, entry);
        }

        Ok(())
    }

    /// Save a document's content to the database as a snapshot.
    ///
    /// Stores the StoreSnapshot as postcard binary in the `oplog_bytes` column.
    pub fn save_snapshot(&self, context_id: ContextId) -> Result<(), String> {
        let db = self.db.as_ref().ok_or("No database configured")?;

        let entry = self.get(context_id).ok_or_else(|| format!("Document {} not found", context_id.to_hex()))?;
        let snapshot = entry.doc.snapshot();
        let version = entry.version() as i64;
        let content = entry.content();

        // Serialize snapshot as JSON (StoreSnapshot contains BlockSnapshot
        // with skip_serializing_if, which requires a self-describing format)
        let snapshot_bytes = serde_json::to_vec(&snapshot)
            .map_err(|e| format!("Failed to serialize store snapshot: {}", e))?;

        drop(entry); // Release the read lock before acquiring DB lock

        let db_guard = db.lock().map_err(|e| format!("DB lock error: {}", e))?;
        db_guard
            .save_snapshot(&context_id.to_hex(), version, &content, Some(&snapshot_bytes))
            .map_err(|e| format!("DB error: {}", e))?;

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
    ) -> Result<BlockId, String> {
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
    ) -> Result<BlockId, String> {
        let after_id = after.cloned();
        let (block_id, snapshot, ops) = {
            let mut entry = self.get_mut(context_id).ok_or_else(|| format!("Document {} not found", context_id.to_hex()))?;
            let effective_agent = agent_id.unwrap_or_else(|| self.agent_id());
            entry.doc.set_agent_id(effective_agent);

            let frontier_before = entry.doc.frontier();

            let block_id = entry.doc.insert_drift_block(parent_id, after, content, source_context, source_model, drift_kind)
                .map_err(|e| e.to_string())?;
            let snapshot = entry.doc.get_block_snapshot(&block_id)
                .ok_or_else(|| "Block not found after insert".to_string())?;

            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = serde_json::to_vec(&ops).map_err(|e| format!("serialize ops: {e}"))?;
            entry.touch(effective_agent);
            (block_id, snapshot, ops_bytes)
        };
        self.auto_save(context_id);

        self.emit(BlockFlow::Inserted {
            context_id,
            block: snapshot,
            after_id,
            ops,
            source: OpSource::Local,
        });

        Ok(block_id)
    }
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
pub fn shared_block_store_with_db(db: DocumentDb, agent_id: PrincipalId) -> SharedBlockStore {
    Arc::new(BlockStore::with_db(db, agent_id))
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
        let block_id = store.insert_block(ctx, None, None, Role::User, BlockKind::Text, "hello world").unwrap();
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
        let thinking_id = store.insert_block(ctx, None, None, Role::Model, BlockKind::Thinking, "Let me think...").unwrap();

        // Insert text block after thinking (as child of root, after thinking in order)
        let text_id = store.insert_block(ctx, None, Some(&thinking_id), Role::Model, BlockKind::Text, "Here's my answer").unwrap();

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

        store.insert_block(ctx, None, None, Role::User, BlockKind::Text, "fn main() {}").unwrap();

        assert_eq!(store.get_content(ctx).unwrap(), "fn main() {}");

        store.delete_document(ctx).unwrap();
        assert!(store.get(ctx).is_none());
    }

    #[test]
    fn test_block_snapshots() {
        let store = BlockStore::new(test_agent());
        let ctx = ContextId::new();

        store.create_document(ctx, DocumentKind::Conversation, None).unwrap();

        let thinking_id = store.insert_block(ctx, None, None, Role::Model, BlockKind::Thinking, "thinking...").unwrap();
        store.insert_block(ctx, None, Some(&thinking_id), Role::Model, BlockKind::Text, "response").unwrap();

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

        let block_id = store.insert_block(ctx, None, None, Role::Model, BlockKind::ToolCall, "{}").unwrap();

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
                    let _ = store_clone.insert_block(ctx, None, None, Role::User, BlockKind::Text, &text);
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
                    let _ = store_clone.insert_block(ctx, None, None, Role::User, BlockKind::Text, &text);
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
        let block_id = store.insert_block(ctx, None, None, Role::User, BlockKind::Text, "initial content").unwrap();

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
            ctx, None, None, Role::User, BlockKind::Text, "Hello from server"
        ).unwrap();

        // Get the BlockInserted event with ops
        let msg = sub.try_recv().expect("should receive BlockInserted event");
        let ops = match msg.payload {
            BlockFlow::Inserted { ops, .. } => ops,
            _ => panic!("expected BlockInserted event"),
        };

        // Deserialize SyncPayload and merge on client
        let payload: SyncPayload = serde_json::from_slice(&ops).expect("should deserialize SyncPayload");
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
            ctx, None, None, "bash", serde_json::json!({"command": "ls -la"})
        ).unwrap();

        let msg = sub.try_recv().expect("should receive event");
        let ops = match msg.payload {
            BlockFlow::Inserted { ops, .. } => ops,
            _ => panic!("expected BlockInserted"),
        };

        let payload: SyncPayload = serde_json::from_slice(&ops).unwrap();
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
                ctx, None, None, Role::User, BlockKind::Text, format!("Message {}", i)
            ).unwrap();

            let msg = sub.try_recv().expect("should receive event");
            let ops = match msg.payload {
                BlockFlow::Inserted { ops, .. } => ops,
                _ => panic!("expected BlockInserted"),
            };

            let payload: SyncPayload = serde_json::from_slice(&ops).unwrap();
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
            ctx, None, None, Role::Model, BlockKind::Text, ""
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
            serde_json::to_vec(&entry.doc.snapshot()).unwrap()
        };

        let frontier_before = server.frontier(ctx).unwrap();

        let _block_id = server.insert_block(
            ctx, None, None, Role::User, BlockKind::Text, "hello from remote",
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
            ctx, None, None, Role::Model, BlockKind::Text, "initial",
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
            ctx, None, None, Role::Model, BlockKind::Text, "",
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
