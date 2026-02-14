//! Block-based CRDT storage using kaijutsu-crdt.
//!
//! Each document has a BlockDocument backed by diamond-types OpLog.
//! Multi-client sync is handled via SerializedOps exchange.
//!
//! # Concurrency Model
//!
//! - DashMap for per-document concurrent access
//! - Event broadcasting for real-time updates
//! - parking_lot for efficient locking

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::RwLock;
use tokio::sync::broadcast;

use kaijutsu_crdt::{
    BlockDocument, BlockId, BlockKind, BlockSnapshot, DocumentSnapshot, Frontier, Role,
    SerializedOps, SerializedOpsOwned, Status,
};

use crate::db::{DocumentDb, DocumentKind, DocumentMeta};
use crate::flows::{BlockFlow, OpSource, SharedBlockFlowBus};

/// Thread-safe database handle.
type DbHandle = Arc<std::sync::Mutex<DocumentDb>>;

/// Unique identifier for a document.
pub type DocumentId = String;

/// Events broadcast when documents change.
#[derive(Clone, Debug)]
pub enum BlockEvent {
    /// Operations applied to a document's OpLog.
    OpsApplied {
        document_id: String,
        ops: SerializedOpsOwned,
        agent_id: String,
    },
    /// A new document was created.
    DocumentCreated {
        document_id: String,
        kind: DocumentKind,
        agent_id: String,
    },
    /// A document was deleted.
    DocumentDeleted {
        document_id: String,
        agent_id: String,
    },
}

/// Entry for a document in the store.
pub struct DocumentEntry {
    /// The block document (owns OpLog).
    pub doc: BlockDocument,
    /// Document metadata.
    pub kind: DocumentKind,
    /// Programming language (if code).
    pub language: Option<String>,
    /// Version counter (incremented on each modification).
    version: AtomicU64,
    /// Last agent to modify.
    last_agent: RwLock<String>,
}

impl DocumentEntry {
    /// Create a new document entry.
    fn new(id: &str, kind: DocumentKind, language: Option<String>, agent_id: &str) -> Self {
        Self {
            doc: BlockDocument::new(id, agent_id),
            kind,
            language,
            version: AtomicU64::new(0),
            last_agent: RwLock::new(agent_id.to_string()),
        }
    }

    /// Create a document entry from a snapshot.
    fn from_snapshot(
        snapshot: DocumentSnapshot,
        kind: DocumentKind,
        language: Option<String>,
        agent_id: &str,
    ) -> Self {
        let version = snapshot.version;
        Self {
            doc: BlockDocument::from_snapshot(snapshot, agent_id),
            kind,
            language,
            version: AtomicU64::new(version),
            last_agent: RwLock::new(agent_id.to_string()),
        }
    }

    /// Get the current version.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// Increment version and record agent.
    pub fn touch(&self, agent_id: &str) {
        self.version.fetch_add(1, Ordering::SeqCst);
        *self.last_agent.write() = agent_id.to_string();
    }

    /// Get the full text content.
    pub fn content(&self) -> String {
        self.doc.full_text()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.doc.is_empty()
    }
}

/// Store for block-based documents with per-document locking.
pub struct BlockStore {
    /// Concurrent document storage.
    documents: DashMap<DocumentId, DocumentEntry>,
    /// Database for persistence.
    db: Option<DbHandle>,
    /// Default agent ID for this store.
    agent_id: RwLock<String>,
    /// Event broadcaster (legacy, kept for backwards compat).
    event_tx: broadcast::Sender<BlockEvent>,
    /// FlowBus for typed pub/sub (preferred for new code).
    block_flows: Option<SharedBlockFlowBus>,
}

impl BlockStore {
    /// Create a new in-memory block store.
    pub fn new(agent_id: impl Into<String>) -> Self {
        let (event_tx, _) = broadcast::channel(1024);
        Self {
            documents: DashMap::new(),
            db: None,
            agent_id: RwLock::new(agent_id.into()),
            event_tx,
            block_flows: None,
        }
    }

    /// Create a new in-memory block store with FlowBus.
    pub fn with_flows(agent_id: impl Into<String>, block_flows: SharedBlockFlowBus) -> Self {
        let (event_tx, _) = broadcast::channel(1024);
        Self {
            documents: DashMap::new(),
            db: None,
            agent_id: RwLock::new(agent_id.into()),
            event_tx,
            block_flows: Some(block_flows),
        }
    }

    /// Create a block store with SQLite persistence.
    pub fn with_db(db: DocumentDb, agent_id: impl Into<String>) -> Self {
        let (event_tx, _) = broadcast::channel(1024);
        Self {
            documents: DashMap::new(),
            db: Some(Arc::new(std::sync::Mutex::new(db))),
            agent_id: RwLock::new(agent_id.into()),
            event_tx,
            block_flows: None,
        }
    }

    /// Create a block store with SQLite persistence and FlowBus.
    pub fn with_db_and_flows(
        db: DocumentDb,
        agent_id: impl Into<String>,
        block_flows: SharedBlockFlowBus,
    ) -> Self {
        let (event_tx, _) = broadcast::channel(1024);
        Self {
            documents: DashMap::new(),
            db: Some(Arc::new(std::sync::Mutex::new(db))),
            agent_id: RwLock::new(agent_id.into()),
            event_tx,
            block_flows: Some(block_flows),
        }
    }

    /// Get the event receiver for subscribing to changes (legacy API).
    pub fn subscribe(&self) -> broadcast::Receiver<BlockEvent> {
        self.event_tx.subscribe()
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
    pub fn agent_id(&self) -> String {
        self.agent_id.read().clone()
    }

    /// Set the agent ID.
    pub fn set_agent_id(&self, agent_id: impl Into<String>) {
        *self.agent_id.write() = agent_id.into();
    }

    /// Create a new document.
    pub fn create_document(
        &self,
        id: DocumentId,
        kind: DocumentKind,
        language: Option<String>,
    ) -> Result<(), String> {
        if self.documents.contains_key(&id) {
            return Err(format!("Document {} already exists", id));
        }

        // Persist metadata if we have a DB
        if let Some(db) = &self.db {
            let db_guard = db.lock().map_err(|e| format!("DB lock error: {}", e))?;
            let meta = DocumentMeta {
                id: id.clone(),
                kind,
                language: language.clone(),
                position_col: None,
                position_row: None,
                parent_document: None,
                created_at: 0, // Unused - DB default (unixepoch()) handles timestamp
            };
            db_guard
                .create_document(&meta)
                .map_err(|e| format!("DB error: {}", e))?;
        }

        let agent_id = self.agent_id();
        let entry = DocumentEntry::new(&id, kind, language, &agent_id);
        self.documents.insert(id.clone(), entry);

        // Broadcast event
        let _ = self.event_tx.send(BlockEvent::DocumentCreated {
            document_id: id,
            kind,
            agent_id,
        });

        Ok(())
    }

    /// Create a document from serialized oplog bytes (for sync from server).
    ///
    /// This reconstructs a document's full CRDT history from the server's oplog.
    /// Used for initial sync when connecting to a kaijutsu-server.
    pub fn create_document_from_oplog(
        &self,
        id: DocumentId,
        kind: DocumentKind,
        language: Option<String>,
        oplog_bytes: &[u8],
    ) -> Result<(), String> {
        if self.documents.contains_key(&id) {
            return Err(format!("Document {} already exists", id));
        }

        let agent_id = self.agent_id();
        let doc = BlockDocument::from_oplog(&id, &agent_id, oplog_bytes)
            .map_err(|e| format!("Failed to create document from oplog: {}", e))?;

        let version = doc.version();
        let entry = DocumentEntry {
            doc,
            kind,
            language,
            version: AtomicU64::new(version),
            last_agent: RwLock::new(agent_id.clone()),
        };
        self.documents.insert(id.clone(), entry);

        // Broadcast event
        let _ = self.event_tx.send(BlockEvent::DocumentCreated {
            document_id: id,
            kind,
            agent_id,
        });

        Ok(())
    }

    /// Get a document for reading.
    pub fn get(&self, id: &str) -> Option<dashmap::mapref::one::Ref<'_, DocumentId, DocumentEntry>> {
        self.documents.get(id)
    }

    /// Get a document for writing.
    pub fn get_mut(&self, id: &str) -> Option<dashmap::mapref::one::RefMut<'_, DocumentId, DocumentEntry>> {
        self.documents.get_mut(id)
    }

    /// List all document IDs.
    pub fn list_ids(&self) -> Vec<DocumentId> {
        self.documents.iter().map(|r| r.key().clone()).collect()
    }

    /// Check if a document exists.
    pub fn contains(&self, id: &str) -> bool {
        self.documents.contains_key(id)
    }

    /// Delete a document.
    pub fn delete_document(&self, id: &str) -> Result<(), String> {
        if let Some(db) = &self.db {
            let db_guard = db.lock().map_err(|e| format!("DB lock error: {}", e))?;
            db_guard
                .delete_document(id)
                .map_err(|e| format!("DB error: {}", e))?;
        }

        self.documents.remove(id);

        // Broadcast event
        let _ = self.event_tx.send(BlockEvent::DocumentDeleted {
            document_id: id.to_string(),
            agent_id: self.agent_id(),
        });

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
        source_id: &str,
        new_id: DocumentId,
    ) -> Result<(), String> {
        if self.documents.contains_key(&new_id) {
            return Err(format!("Document {} already exists", new_id));
        }

        let source_entry = self.get(source_id)
            .ok_or_else(|| format!("Source document {} not found", source_id))?;

        let agent_id = self.agent_id();
        let forked_doc = source_entry.doc.fork(&new_id, &agent_id);
        let kind = source_entry.kind;
        let language = source_entry.language.clone();
        drop(source_entry); // Release the read lock

        // Persist metadata if we have a DB
        if let Some(db) = &self.db {
            let db_guard = db.lock().map_err(|e| format!("DB lock error: {}", e))?;
            let meta = DocumentMeta {
                id: new_id.clone(),
                kind,
                language: language.clone(),
                position_col: None,
                position_row: None,
                parent_document: Some(source_id.to_string()),
                created_at: 0, // DB default handles timestamp
            };
            db_guard
                .create_document(&meta)
                .map_err(|e| format!("DB error: {}", e))?;
        }

        let version = forked_doc.version();
        let entry = DocumentEntry {
            doc: forked_doc,
            kind,
            language,
            version: AtomicU64::new(version),
            last_agent: RwLock::new(agent_id.clone()),
        };
        self.documents.insert(new_id.clone(), entry);

        // Broadcast event
        let _ = self.event_tx.send(BlockEvent::DocumentCreated {
            document_id: new_id,
            kind,
            agent_id,
        });

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
        source_id: &str,
        new_id: DocumentId,
        at_version: u64,
    ) -> Result<(), String> {
        if self.documents.contains_key(&new_id) {
            return Err(format!("Document {} already exists", new_id));
        }

        let source_entry = self.get(source_id)
            .ok_or_else(|| format!("Source document {} not found", source_id))?;

        // Validate version
        let current_version = source_entry.version();
        if at_version > current_version {
            return Err(format!(
                "Requested version {} is in the future (current: {})",
                at_version, current_version
            ));
        }

        let agent_id = self.agent_id();
        let forked_doc = source_entry.doc.fork_at_version(&new_id, &agent_id, at_version);
        let kind = source_entry.kind;
        let language = source_entry.language.clone();
        drop(source_entry); // Release the read lock

        // Persist metadata if we have a DB
        if let Some(db) = &self.db {
            let db_guard = db.lock().map_err(|e| format!("DB lock error: {}", e))?;
            let meta = DocumentMeta {
                id: new_id.clone(),
                kind,
                language: language.clone(),
                position_col: None,
                position_row: None,
                parent_document: Some(source_id.to_string()),
                created_at: 0, // DB default handles timestamp
            };
            db_guard
                .create_document(&meta)
                .map_err(|e| format!("DB error: {}", e))?;
        }

        let version = forked_doc.version();
        let entry = DocumentEntry {
            doc: forked_doc,
            kind,
            language,
            version: AtomicU64::new(version),
            last_agent: RwLock::new(agent_id.clone()),
        };
        self.documents.insert(new_id.clone(), entry);

        // Broadcast event
        let _ = self.event_tx.send(BlockEvent::DocumentCreated {
            document_id: new_id,
            kind,
            agent_id,
        });

        Ok(())
    }

    /// Get the number of documents.
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Get the last block ID in a document (for ordering new blocks at the end).
    pub fn last_block_id(&self, document_id: &str) -> Option<BlockId> {
        let entry = self.get(document_id)?;
        entry.doc.blocks_ordered().last().map(|b| b.id.clone())
    }

    // =========================================================================
    // Block Operations
    // =========================================================================

    /// Auto-save snapshot if database is configured.
    /// Logs warnings on failure but doesn't propagate errors.
    fn auto_save(&self, document_id: &str) {
        if self.db.is_some() {
            if let Err(e) = self.save_snapshot(document_id) {
                tracing::warn!(document_id = %document_id, error = %e, "Failed to auto-save snapshot");
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
        document_id: &str,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        role: Role,
        kind: BlockKind,
        content: impl Into<String>,
    ) -> Result<BlockId, String> {
        let after_id = after.cloned();
        let (block_id, snapshot, ops) = {
            let mut entry = self.get_mut(document_id).ok_or_else(|| format!("Document {} not found", document_id))?;
            let agent_id = self.agent_id();

            // Capture frontier before the operation for incremental ops.
            // Clients that are in sync can merge these directly.
            // Clients that are out of sync will get full oplog via get_document_state.
            let frontier_before = entry.doc.frontier();

            let block_id = entry.doc.insert_block(parent_id, after, role, kind, content, &agent_id)
                .map_err(|e| e.to_string())?;
            let snapshot = entry.doc.get_block_snapshot(&block_id)
                .ok_or_else(|| "Block not found after insert".to_string())?;

            // Send incremental ops (just this operation) for efficient sync.
            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = serde_json::to_vec(&ops).map_err(|e| format!("serialize ops: {e}"))?;
            entry.touch(&agent_id);
            (block_id, snapshot, ops_bytes)
        };
        self.auto_save(document_id);

        // Emit flow event with creation ops
        self.emit(BlockFlow::Inserted {
            document_id: document_id.to_string(),
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
        document_id: &str,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        tool_name: impl Into<String>,
        tool_input: serde_json::Value,
    ) -> Result<BlockId, String> {
        let after_id = after.cloned();
        let (block_id, snapshot, ops) = {
            let mut entry = self.get_mut(document_id).ok_or_else(|| format!("Document {} not found", document_id))?;
            let agent_id = self.agent_id();

            // Capture frontier before the operation for incremental ops
            let frontier_before = entry.doc.frontier();

            let block_id = entry.doc.insert_tool_call(parent_id, after, tool_name, tool_input, &agent_id)
                .map_err(|e| e.to_string())?;
            let snapshot = entry.doc.get_block_snapshot(&block_id)
                .ok_or_else(|| "Block not found after insert".to_string())?;

            // Send incremental ops (just this operation) for efficient sync
            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = serde_json::to_vec(&ops).map_err(|e| format!("serialize ops: {e}"))?;
            entry.touch(&agent_id);
            (block_id, snapshot, ops_bytes)
        };
        self.auto_save(document_id);

        // Emit flow event with creation ops
        self.emit(BlockFlow::Inserted {
            document_id: document_id.to_string(),
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
        document_id: &str,
        tool_call_id: &BlockId,
        after: Option<&BlockId>,
        content: impl Into<String>,
        is_error: bool,
        exit_code: Option<i32>,
    ) -> Result<BlockId, String> {
        let after_id = after.cloned();
        let (block_id, snapshot, ops) = {
            let mut entry = self.get_mut(document_id).ok_or_else(|| format!("Document {} not found", document_id))?;
            let agent_id = self.agent_id();

            // Capture frontier before the operation for incremental ops
            let frontier_before = entry.doc.frontier();

            let block_id = entry.doc.insert_tool_result_block(tool_call_id, after, content, is_error, exit_code, &agent_id)
                .map_err(|e| e.to_string())?;
            let snapshot = entry.doc.get_block_snapshot(&block_id)
                .ok_or_else(|| "Block not found after insert".to_string())?;

            // Send incremental ops (just this operation) for efficient sync
            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = serde_json::to_vec(&ops).map_err(|e| format!("serialize ops: {e}"))?;
            entry.touch(&agent_id);
            (block_id, snapshot, ops_bytes)
        };
        self.auto_save(document_id);

        // Emit flow event with creation ops
        self.emit(BlockFlow::Inserted {
            document_id: document_id.to_string(),
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
        document_id: &str,
        snapshot: BlockSnapshot,
        after: Option<&BlockId>,
    ) -> Result<BlockId, String> {
        let after_id = after.cloned();
        let (block_id, final_snapshot, ops) = {
            let mut entry = self.get_mut(document_id)
                .ok_or_else(|| format!("Document {} not found", document_id))?;
            let agent_id = self.agent_id();

            let frontier_before = entry.doc.frontier();

            let block_id = entry.doc.insert_from_snapshot(snapshot, after)
                .map_err(|e| e.to_string())?;
            let final_snapshot = entry.doc.get_block_snapshot(&block_id)
                .ok_or_else(|| "Block not found after insert".to_string())?;

            let ops = entry.doc.ops_since(&frontier_before);
            let ops_bytes = serde_json::to_vec(&ops).map_err(|e| format!("serialize ops: {e}"))?;
            entry.touch(&agent_id);
            (block_id, final_snapshot, ops_bytes)
        };
        self.auto_save(document_id);

        self.emit(BlockFlow::Inserted {
            document_id: document_id.to_string(),
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
        document_id: &str,
        block_id: &BlockId,
        status: Status,
    ) -> Result<(), String> {
        {
            let mut entry = self.get_mut(document_id).ok_or_else(|| format!("Document {} not found", document_id))?;
            let agent_id = self.agent_id();
            entry.doc.set_status(block_id, status).map_err(|e| e.to_string())?;
            entry.touch(&agent_id);
        }
        self.auto_save(document_id);

        // Emit flow event
        self.emit(BlockFlow::StatusChanged {
            document_id: document_id.to_string(),
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
        document_id: &str,
        block_id: &BlockId,
        pos: usize,
        insert: &str,
        delete: usize,
    ) -> Result<(), String> {
        let ops = {
            let mut entry = self.get_mut(document_id).ok_or_else(|| format!("Document {} not found", document_id))?;
            let agent_id = self.agent_id();
            // Capture frontier before edit
            let frontier = entry.doc.frontier();
            entry.doc.edit_text(block_id, pos, insert, delete).map_err(|e| e.to_string())?;
            entry.touch(&agent_id);
            // Get ops since frontier (the edit we just applied)
            let ops = entry.doc.ops_since(&frontier);
            serde_json::to_vec(&ops).map_err(|e| format!("serialize ops: {e}"))?
        };
        // Note: No auto-save for text edits (high frequency during streaming)

        // Emit CRDT ops for proper sync
        self.emit(BlockFlow::TextOps {
            document_id: document_id.to_string(),
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
        document_id: &str,
        block_id: &BlockId,
        hint: Option<&str>,
    ) -> Result<(), String> {
        let mut entry = self.get_mut(document_id).ok_or_else(|| format!("Document {} not found", document_id))?;
        let agent_id = self.agent_id();
        entry.doc.set_display_hint(block_id, hint).map_err(|e| e.to_string())?;
        entry.touch(&agent_id);
        // Note: No event emission for display hint changes for now - they're synced with full state
        Ok(())
    }

    /// Append text to a block.
    ///
    /// Note: Does not auto-save to avoid excessive I/O during streaming.
    /// Call `save_snapshot()` explicitly when streaming is complete.
    pub fn append_text(&self, document_id: &str, block_id: &BlockId, text: &str) -> Result<(), String> {
        let ops = {
            let mut entry = self.get_mut(document_id).ok_or_else(|| format!("Document {} not found", document_id))?;
            let agent_id = self.agent_id();
            // Capture frontier before append
            let frontier = entry.doc.frontier();
            entry.doc.append_text(block_id, text).map_err(|e| e.to_string())?;
            entry.touch(&agent_id);
            // Get ops since frontier (the append we just applied)
            let ops = entry.doc.ops_since(&frontier);
            serde_json::to_vec(&ops).map_err(|e| format!("serialize ops: {e}"))?
        };
        // Note: No auto-save for text appends (high frequency during streaming)

        // Emit CRDT ops for proper sync
        self.emit(BlockFlow::TextOps {
            document_id: document_id.to_string(),
            block_id: block_id.clone(),
            ops,
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Set collapsed state for a thinking block.
    pub fn set_collapsed(&self, document_id: &str, block_id: &BlockId, collapsed: bool) -> Result<(), String> {
        {
            let mut entry = self.get_mut(document_id).ok_or_else(|| format!("Document {} not found", document_id))?;
            let agent_id = self.agent_id();
            entry.doc.set_collapsed(block_id, collapsed).map_err(|e| e.to_string())?;
            entry.touch(&agent_id);
        }
        self.auto_save(document_id);

        // Emit flow event
        self.emit(BlockFlow::CollapsedChanged {
            document_id: document_id.to_string(),
            block_id: block_id.clone(),
            collapsed,
            source: OpSource::Local,
        });

        Ok(())
    }

    /// Delete a block from a document.
    pub fn delete_block(&self, document_id: &str, block_id: &BlockId) -> Result<(), String> {
        {
            let mut entry = self.get_mut(document_id).ok_or_else(|| format!("Document {} not found", document_id))?;
            let agent_id = self.agent_id();
            entry.doc.delete_block(block_id).map_err(|e| e.to_string())?;
            entry.touch(&agent_id);
        }
        self.auto_save(document_id);

        // Emit flow event
        self.emit(BlockFlow::Deleted {
            document_id: document_id.to_string(),
            block_id: block_id.clone(),
            source: OpSource::Local,
        });

        Ok(())
    }

    // =========================================================================
    // Sync Operations
    // =========================================================================

    /// Get operations since a frontier for a document.
    pub fn ops_since(&self, document_id: &str, frontier: &Frontier) -> Result<SerializedOpsOwned, String> {
        let entry = self.get(document_id).ok_or_else(|| format!("Document {} not found", document_id))?;
        Ok(entry.doc.ops_since(frontier))
    }

    /// Merge remote operations into a document.
    pub fn merge_ops(&self, document_id: &str, ops: SerializedOps<'_>) -> Result<u64, String> {
        let mut entry = self.get_mut(document_id).ok_or_else(|| format!("Document {} not found", document_id))?;
        entry.doc.merge_ops(ops).map_err(|e| e.to_string())?;
        let version = entry.doc.version();
        entry.version.store(version, Ordering::SeqCst);
        Ok(version)
    }

    /// Merge remote operations into a document (owned variant).
    ///
    /// Use this when receiving serialized ops that have been deserialized
    /// into the owned form (e.g., from network RPC via pushOps).
    pub fn merge_ops_owned(&self, document_id: &str, ops: SerializedOpsOwned) -> Result<u64, String> {
        let mut entry = self.get_mut(document_id).ok_or_else(|| format!("Document {} not found", document_id))?;
        entry.doc.merge_ops_owned(ops).map_err(|e| e.to_string())?;
        let version = entry.doc.version();
        entry.version.store(version, Ordering::SeqCst);
        Ok(version)
    }

    /// Get the current frontier for a document.
    pub fn frontier(&self, document_id: &str) -> Result<Frontier, String> {
        let entry = self.get(document_id).ok_or_else(|| format!("Document {} not found", document_id))?;
        Ok(entry.doc.frontier())
    }

    // =========================================================================
    // Query Operations
    // =========================================================================

    /// Get block snapshots for a document.
    pub fn block_snapshots(&self, document_id: &str) -> Result<Vec<BlockSnapshot>, String> {
        let entry = self.get(document_id).ok_or_else(|| format!("Document {} not found", document_id))?;
        Ok(entry.doc.blocks_ordered())
    }

    /// Get the full text content of a document.
    pub fn get_content(&self, document_id: &str) -> Result<String, String> {
        let entry = self.get(document_id).ok_or_else(|| format!("Document {} not found", document_id))?;
        Ok(entry.content())
    }

    /// Get document metadata and version.
    pub fn get_document_state(
        &self,
        document_id: &str,
    ) -> Result<(DocumentKind, Option<String>, Vec<BlockSnapshot>, u64), String> {
        let entry = self.get(document_id).ok_or_else(|| format!("Document {} not found", document_id))?;
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
    /// The `oplog_bytes` column stores JSON-encoded DocumentSnapshot.
    pub fn load_from_db(&self) -> Result<(), String> {
        let db = self.db.as_ref().ok_or("No database configured")?;
        let db_guard = db.lock().map_err(|e| format!("DB lock error: {}", e))?;

        let document_metas = db_guard
            .list_documents()
            .map_err(|e| format!("DB error: {}", e))?;

        let agent_id = self.agent_id();
        for meta in document_metas {
            // Try to load snapshot for this document
            let entry = if let Ok(Some(snapshot_record)) = db_guard.get_snapshot(&meta.id) {
                // oplog_bytes contains JSON-encoded DocumentSnapshot
                if let Some(oplog_bytes) = snapshot_record.oplog_bytes {
                    match serde_json::from_slice::<DocumentSnapshot>(&oplog_bytes) {
                        Ok(doc_snapshot) => {
                            tracing::debug!(
                                document_id = %meta.id,
                                blocks = doc_snapshot.blocks.len(),
                                "Restored document from snapshot"
                            );
                            DocumentEntry::from_snapshot(doc_snapshot, meta.kind, meta.language.clone(), &agent_id)
                        }
                        Err(e) => {
                            tracing::error!(
                                document_id = %meta.id,
                                error = %e,
                                "Failed to deserialize snapshot, skipping corrupted document"
                            );
                            continue;
                        }
                    }
                } else {
                    // Snapshot exists but no oplog_bytes, start empty
                    DocumentEntry::new(&meta.id, meta.kind, meta.language.clone(), &agent_id)
                }
            } else {
                // No snapshot, start empty
                DocumentEntry::new(&meta.id, meta.kind, meta.language.clone(), &agent_id)
            };

            self.documents.insert(meta.id, entry);
        }

        Ok(())
    }

    /// Save a document's content to the database as a snapshot.
    ///
    /// Stores the DocumentSnapshot as JSON in the `oplog_bytes` column.
    pub fn save_snapshot(&self, document_id: &str) -> Result<(), String> {
        let db = self.db.as_ref().ok_or("No database configured")?;

        let entry = self.get(document_id).ok_or_else(|| format!("Document {} not found", document_id))?;
        let snapshot = entry.doc.snapshot();
        let version = entry.version() as i64;
        let content = entry.content();

        // Serialize snapshot as JSON
        let oplog_bytes = serde_json::to_vec(&snapshot)
            .map_err(|e| format!("Failed to serialize snapshot: {}", e))?;

        drop(entry); // Release the read lock before acquiring DB lock

        let db_guard = db.lock().map_err(|e| format!("DB lock error: {}", e))?;
        db_guard
            .save_snapshot(document_id, version, &content, Some(&oplog_bytes))
            .map_err(|e| format!("DB error: {}", e))?;

        Ok(())
    }
}

impl Default for BlockStore {
    fn default() -> Self {
        Self::new("server")
    }
}

/// Thread-safe handle to a BlockStore.
/// With DashMap, the store itself doesn't need RwLock.
pub type SharedBlockStore = Arc<BlockStore>;

/// Create a new shared block store.
pub fn shared_block_store(agent_id: impl Into<String>) -> SharedBlockStore {
    Arc::new(BlockStore::new(agent_id))
}

/// Create a shared block store with database persistence.
pub fn shared_block_store_with_db(db: DocumentDb, agent_id: impl Into<String>) -> SharedBlockStore {
    Arc::new(BlockStore::with_db(db, agent_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_store_basic_ops() {
        let store = BlockStore::new("alice");

        store.create_document("test".into(), DocumentKind::Code, Some("rust".into())).unwrap();

        // Insert a text block using new API
        let block_id = store.insert_block("test", None, None, Role::User, BlockKind::Text, "hello world").unwrap();
        assert_eq!(store.get_content("test").unwrap(), "hello world");

        // Append to the block
        store.append_text("test", &block_id, "!").unwrap();
        assert_eq!(store.get_content("test").unwrap(), "hello world!");

        // Edit the block
        store.edit_text("test", &block_id, 6, "rust ", 0).unwrap();
        assert_eq!(store.get_content("test").unwrap(), "hello rust world!");
    }

    #[test]
    fn test_block_store_multiple_blocks() {
        let store = BlockStore::new("agent");

        store.create_document("test".into(), DocumentKind::Conversation, None).unwrap();

        // Insert thinking block
        let thinking_id = store.insert_block("test", None, None, Role::Model, BlockKind::Thinking, "Let me think...").unwrap();

        // Insert text block after thinking (as child of root, after thinking in order)
        let text_id = store.insert_block("test", None, Some(&thinking_id), Role::Model, BlockKind::Text, "Here's my answer").unwrap();

        // Should have both blocks
        let content = store.get_content("test").unwrap();
        assert!(content.contains("Let me think..."));
        assert!(content.contains("Here's my answer"));

        // Collapse thinking
        store.set_collapsed("test", &thinking_id, true).unwrap();

        // Delete text block
        store.delete_block("test", &text_id).unwrap();
        let content = store.get_content("test").unwrap();
        assert!(content.contains("Let me think..."));
        assert!(!content.contains("Here's my answer"));
    }

    #[test]
    fn test_block_store_crud() {
        let store = BlockStore::new("server");

        store.create_document("doc-1".into(), DocumentKind::Code, Some("rust".into())).unwrap();

        store.insert_block("doc-1", None, None, Role::User, BlockKind::Text, "fn main() {}").unwrap();

        assert_eq!(store.get_content("doc-1").unwrap(), "fn main() {}");

        store.delete_document("doc-1").unwrap();
        assert!(store.get("doc-1").is_none());
    }

    #[test]
    fn test_block_snapshots() {
        let store = BlockStore::new("agent");

        store.create_document("test".into(), DocumentKind::Conversation, None).unwrap();

        let thinking_id = store.insert_block("test", None, None, Role::Model, BlockKind::Thinking, "thinking...").unwrap();
        store.insert_block("test", None, Some(&thinking_id), Role::Model, BlockKind::Text, "response").unwrap();

        let snapshots = store.block_snapshots("test").unwrap();
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
        let store = BlockStore::new("agent");

        store.create_document("test".into(), DocumentKind::Conversation, None).unwrap();

        let block_id = store.insert_block("test", None, None, Role::Model, BlockKind::ToolCall, "{}").unwrap();

        // Set status to Running
        store.set_status("test", &block_id, Status::Running).unwrap();

        let snapshots = store.block_snapshots("test").unwrap();
        assert_eq!(snapshots[0].status, Status::Running);

        // Set status to Done
        store.set_status("test", &block_id, Status::Done).unwrap();

        let snapshots = store.block_snapshots("test").unwrap();
        assert_eq!(snapshots[0].status, Status::Done);
    }

    #[tokio::test]
    async fn test_event_subscription() {
        let store = BlockStore::new("alice");
        let mut rx = store.subscribe();

        store.create_document("test".into(), DocumentKind::Code, None).unwrap();

        // Should receive DocumentCreated event
        let event = rx.try_recv().unwrap();
        match event {
            BlockEvent::DocumentCreated { document_id, kind, agent_id } => {
                assert_eq!(document_id, "test");
                assert_eq!(kind, DocumentKind::Code);
                assert_eq!(agent_id, "alice");
            }
            _ => panic!("Expected DocumentCreated event"),
        }
    }

    #[tokio::test]
    async fn test_concurrent_document_access() {
        use std::sync::Arc;
        use tokio::task::JoinSet;

        let store = Arc::new(BlockStore::new("test-agent"));
        store
            .create_document("shared-doc".into(), DocumentKind::Code, None)
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
                    let _ = store_clone.insert_block("shared-doc", None, None, Role::User, BlockKind::Text, &text);
                }
            });
        }

        // Wait for all tasks to complete
        while let Some(result) = tasks.join_next().await {
            result.expect("Task panicked");
        }

        // Verify the document has content from all tasks
        let content = store.get_content("shared-doc").unwrap();

        // Should have at least some content (exact ordering is non-deterministic)
        assert!(!content.is_empty());

        // Count how many blocks we have - should be num_tasks * ops_per_task
        let snapshots = store.block_snapshots("shared-doc").unwrap();
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

        let store = Arc::new(BlockStore::new("test-agent"));

        // Create multiple documents
        let num_docs = 3;
        for i in 0..num_docs {
            store
                .create_document(format!("doc-{}", i), DocumentKind::Code, None)
                .unwrap();
        }

        let mut tasks = JoinSet::new();
        let num_tasks = 6;

        // Each task works on different documents
        for i in 0..num_tasks {
            let store_clone = Arc::clone(&store);
            let doc_id = format!("doc-{}", i % num_docs);
            tasks.spawn(async move {
                for j in 0..5 {
                    let text = format!("task-{}-op-{}", i, j);
                    let _ = store_clone.insert_block(&doc_id, None, None, Role::User, BlockKind::Text, &text);
                }
            });
        }

        // Wait for all tasks
        while let Some(result) = tasks.join_next().await {
            result.expect("Task panicked");
        }

        // Each document should have content
        for i in 0..num_docs {
            let doc_id = format!("doc-{}", i);
            let content = store.get_content(&doc_id).unwrap();
            assert!(!content.is_empty(), "Document {} should have content", doc_id);
        }
    }

    #[tokio::test]
    async fn test_concurrent_read_write() {
        use std::sync::Arc;
        use tokio::task::JoinSet;

        let store = Arc::new(BlockStore::new("test-agent"));
        store
            .create_document("rw-doc".into(), DocumentKind::Code, None)
            .unwrap();

        // Insert initial content
        let block_id = store.insert_block("rw-doc", None, None, Role::User, BlockKind::Text, "initial content").unwrap();

        let mut tasks = JoinSet::new();

        // Spawn writer tasks
        for i in 0..3 {
            let store_clone = Arc::clone(&store);
            let bid = block_id.clone();
            tasks.spawn(async move {
                for j in 0..5 {
                    // Append text
                    let text = format!(" [w{}:{}]", i, j);
                    let _ = store_clone.append_text("rw-doc", &bid, &text);
                }
            });
        }

        // Spawn reader tasks
        for _ in 0..3 {
            let store_clone = Arc::clone(&store);
            tasks.spawn(async move {
                for _ in 0..10 {
                    // Read content
                    let _ = store_clone.get_content("rw-doc");
                }
            });
        }

        // Wait for all tasks
        while let Some(result) = tasks.join_next().await {
            result.expect("Task panicked");
        }

        // Content should still be valid
        let content = store.get_content("rw-doc").unwrap();
        assert!(content.starts_with("initial content"));
    }

    #[tokio::test]
    async fn test_event_subscription_concurrent() {
        use std::sync::Arc;
        use tokio::task::JoinSet;

        let store = Arc::new(BlockStore::new("test-agent"));
        let mut rx = store.subscribe();

        let mut tasks = JoinSet::new();

        // Create multiple documents concurrently
        for i in 0..10 {
            let store_clone = Arc::clone(&store);
            tasks.spawn(async move {
                store_clone
                    .create_document(format!("event-doc-{}", i), DocumentKind::Code, None)
                    .unwrap();
            });
        }

        // Wait for all tasks
        while let Some(result) = tasks.join_next().await {
            result.expect("Task panicked");
        }

        // Collect events (may not get all due to broadcast channel)
        let mut events_received = 0;
        while rx.try_recv().is_ok() {
            events_received += 1;
        }

        // Should have received at least some events
        assert!(events_received > 0, "Should have received some events");
    }

    // ============================================================================
    // FRONTIER-BASED INCREMENTAL SYNC TESTS
    // ============================================================================
    //
    // These tests verify Phase 2 of the frontier-based CRDT sync:
    // - Server sends incremental ops (not full oplog) for block insertions
    // - Client can merge these ops after initial full sync

    use crate::flows::{BlockFlow, FlowBus, SharedBlockFlowBus};
    use std::sync::Arc;

    /// Helper to create a BlockStore with FlowBus for testing.
    fn store_with_flows() -> (BlockStore, SharedBlockFlowBus) {
        let bus: SharedBlockFlowBus = Arc::new(FlowBus::new(256));
        let store = BlockStore::with_flows("server-agent", bus.clone());
        (store, bus)
    }

    /// Test that insert_block emits incremental ops that can be merged
    /// by a client that already has the base document state.
    #[tokio::test]
    async fn test_insert_block_emits_incremental_ops() {
        use kaijutsu_crdt::{BlockDocument, SerializedOpsOwned};

        let (store, bus) = store_with_flows();
        let mut sub = bus.subscribe("block.>");

        // Create document on server
        store.create_document("sync-test".into(), DocumentKind::Conversation, None).unwrap();

        // Get full oplog for client initial sync (simulates get_block_cell_state)
        let full_oplog = {
            let entry = store.get("sync-test").unwrap();
            entry.doc.oplog_bytes()
        };

        // Client creates document from full oplog (initial sync)
        let mut client = BlockDocument::from_oplog("sync-test", "client-agent", &full_oplog)
            .expect("client should sync from oplog");
        assert_eq!(client.block_count(), 0, "initially empty");

        // Server inserts a block
        let block_id = store.insert_block(
            "sync-test",
            None,
            None,
            Role::User,
            BlockKind::Text,
            "Hello from server"
        ).unwrap();

        // Get the BlockInserted event with ops
        let msg = sub.try_recv().expect("should receive BlockInserted event");
        let ops = match msg.payload {
            BlockFlow::Inserted { ops, .. } => ops,
            _ => panic!("expected BlockInserted event"),
        };

        // Deserialize and merge incremental ops on client
        let serialized_ops: SerializedOpsOwned = serde_json::from_slice(&ops)
            .expect("should deserialize ops");

        client.merge_ops_owned(serialized_ops)
            .expect("client should merge incremental ops without DataMissing error");

        // Verify client has the block
        assert_eq!(client.block_count(), 1);
        let snapshot = client.get_block_snapshot(&block_id).expect("block should exist on client");
        assert_eq!(snapshot.content, "Hello from server");
    }

    /// Test that insert_tool_call emits incremental ops that can be merged.
    #[tokio::test]
    async fn test_insert_tool_call_emits_incremental_ops() {
        use kaijutsu_crdt::{BlockDocument, SerializedOpsOwned};

        let (store, bus) = store_with_flows();
        let mut sub = bus.subscribe("block.>");

        store.create_document("tool-test".into(), DocumentKind::Conversation, None).unwrap();

        // Full sync
        let full_oplog = store.get("tool-test").unwrap().doc.oplog_bytes();
        let mut client = BlockDocument::from_oplog("tool-test", "client-agent", &full_oplog)
            .expect("initial sync");

        // Server inserts tool call
        let block_id = store.insert_tool_call(
            "tool-test",
            None,
            None,
            "bash",
            serde_json::json!({"command": "ls -la"})
        ).unwrap();

        // Get incremental ops from event
        let msg = sub.try_recv().expect("should receive event");
        let ops = match msg.payload {
            BlockFlow::Inserted { ops, .. } => ops,
            _ => panic!("expected BlockInserted"),
        };

        // Client merges incremental ops
        let serialized_ops: SerializedOpsOwned = serde_json::from_slice(&ops).unwrap();
        client.merge_ops_owned(serialized_ops)
            .expect("should merge tool_call incremental ops");

        // Verify
        let snapshot = client.get_block_snapshot(&block_id).unwrap();
        assert_eq!(snapshot.kind, BlockKind::ToolCall);
        assert_eq!(snapshot.tool_name.as_deref(), Some("bash"));
    }

    /// Test that insert_tool_result emits incremental ops that can be merged.
    #[tokio::test]
    async fn test_insert_tool_result_emits_incremental_ops() {
        use kaijutsu_crdt::{BlockDocument, SerializedOpsOwned};

        let (store, bus) = store_with_flows();
        let mut sub = bus.subscribe("block.>");

        store.create_document("result-test".into(), DocumentKind::Conversation, None).unwrap();

        // Insert a tool call first (parent for tool result)
        let tool_call_id = store.insert_tool_call(
            "result-test",
            None,
            None,
            "bash",
            serde_json::json!({"command": "echo hello"})
        ).unwrap();
        let _ = sub.try_recv(); // drain tool call event

        // Full sync (after tool call exists)
        let full_oplog = store.get("result-test").unwrap().doc.oplog_bytes();
        let mut client = BlockDocument::from_oplog("result-test", "client-agent", &full_oplog)
            .expect("initial sync");
        assert_eq!(client.block_count(), 1, "should have tool call");

        // Server inserts tool result
        let result_id = store.insert_tool_result(
            "result-test",
            &tool_call_id,
            None,
            "hello\n",
            false,
            Some(0)
        ).unwrap();

        // Get incremental ops
        let msg = sub.try_recv().expect("should receive event");
        let ops = match msg.payload {
            BlockFlow::Inserted { ops, .. } => ops,
            _ => panic!("expected BlockInserted"),
        };

        // Client merges
        let serialized_ops: SerializedOpsOwned = serde_json::from_slice(&ops).unwrap();
        client.merge_ops_owned(serialized_ops)
            .expect("should merge tool_result incremental ops");

        // Verify
        assert_eq!(client.block_count(), 2);
        let snapshot = client.get_block_snapshot(&result_id).unwrap();
        assert_eq!(snapshot.kind, BlockKind::ToolResult);
        assert_eq!(snapshot.content, "hello\n");
        assert_eq!(snapshot.exit_code, Some(0));
    }

    /// Test multiple sequential block inserts all produce mergeable incremental ops.
    #[tokio::test]
    async fn test_multiple_incremental_syncs() {
        use kaijutsu_crdt::{BlockDocument, SerializedOpsOwned};

        let (store, bus) = store_with_flows();
        let mut sub = bus.subscribe("block.>");

        store.create_document("multi-test".into(), DocumentKind::Conversation, None).unwrap();

        // Initial sync
        let full_oplog = store.get("multi-test").unwrap().doc.oplog_bytes();
        let mut client = BlockDocument::from_oplog("multi-test", "client-agent", &full_oplog)
            .expect("initial sync");

        // Insert multiple blocks, merging each incrementally
        for i in 0..5 {
            let _ = store.insert_block(
                "multi-test",
                None,
                None,
                Role::User,
                BlockKind::Text,
                format!("Message {}", i)
            ).unwrap();

            let msg = sub.try_recv().expect("should receive event");
            let ops = match msg.payload {
                BlockFlow::Inserted { ops, .. } => ops,
                _ => panic!("expected BlockInserted"),
            };

            let serialized_ops: SerializedOpsOwned = serde_json::from_slice(&ops).unwrap();
            client.merge_ops_owned(serialized_ops)
                .expect(&format!("should merge block {} incrementally", i));
        }

        // Verify all blocks synced
        assert_eq!(client.block_count(), 5);

        // Verify content matches server
        let server_blocks = store.block_snapshots("multi-test").unwrap();
        let client_blocks = client.blocks_ordered();

        for (server_block, client_block) in server_blocks.iter().zip(client_blocks.iter()) {
            assert_eq!(server_block.content, client_block.content);
        }
    }

    /// Test that text streaming (append_text) produces incremental ops.
    #[tokio::test]
    async fn test_text_streaming_incremental_ops() {
        use kaijutsu_crdt::{BlockDocument, SerializedOpsOwned};

        let (store, bus) = store_with_flows();
        let mut sub = bus.subscribe("block.>");

        store.create_document("stream-test".into(), DocumentKind::Conversation, None).unwrap();

        // Insert initial empty block
        let block_id = store.insert_block(
            "stream-test",
            None,
            None,
            Role::Model,
            BlockKind::Text,
            ""  // Start empty
        ).unwrap();
        let _ = sub.try_recv(); // drain insert event

        // Full sync after block created
        let full_oplog = store.get("stream-test").unwrap().doc.oplog_bytes();
        let mut client = BlockDocument::from_oplog("stream-test", "client-agent", &full_oplog)
            .expect("initial sync");

        // Stream text in chunks
        let chunks = ["Hello", " ", "World", "!"];
        for chunk in chunks {
            store.append_text("stream-test", &block_id, chunk).unwrap();

            let msg = sub.try_recv().expect("should receive event");
            let ops = match msg.payload {
                BlockFlow::TextOps { ops, .. } => ops,
                _ => panic!("expected TextOps event, got {:?}", msg.payload),
            };

            let serialized_ops: SerializedOpsOwned = serde_json::from_slice(&ops).unwrap();
            client.merge_ops_owned(serialized_ops)
                .expect(&format!("should merge chunk '{}'", chunk));
        }

        // Verify final content
        let snapshot = client.get_block_snapshot(&block_id).unwrap();
        assert_eq!(snapshot.content, "Hello World!");
    }
}
