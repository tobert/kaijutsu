//! Block-based CRDT storage using kaijutsu-crdt.
//!
//! Each cell has a BlockDocument that manages block ordering (Fugue)
//! and per-block text CRDTs (diamond-types).
//!
//! This replaces the old flat-text CellStore with structured content.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use kaijutsu_crdt::{BlockContentSnapshot, BlockDocOp, BlockDocument, BlockId};

use crate::db::{CellDb, CellKind, CellMeta};

/// Thread-safe database handle.
/// Wrapped in Mutex because rusqlite::Connection is !Sync.
type DbHandle = Arc<Mutex<CellDb>>;

/// Unique identifier for a cell.
pub type CellId = String;

/// A cell with block-based CRDT document.
pub struct BlockCell {
    pub id: CellId,
    pub kind: CellKind,
    pub language: Option<String>,
    pub doc: BlockDocument,
}

impl BlockCell {
    /// Create a new block cell.
    pub fn new(id: CellId, kind: CellKind, language: Option<String>, agent_id: &str) -> Self {
        Self {
            id: id.clone(),
            kind,
            language,
            doc: BlockDocument::new(&id, agent_id),
        }
    }

    /// Get the full text content (all blocks concatenated).
    pub fn content(&self) -> String {
        self.doc.full_text()
    }

    /// Get the current version.
    pub fn version(&self) -> u64 {
        self.doc.version()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.doc.is_empty()
    }

    /// Insert a text block.
    pub fn insert_text_block(
        &mut self,
        after: Option<&BlockId>,
        text: impl Into<String>,
    ) -> Result<BlockId, String> {
        self.doc.insert_text_block(after, text).map_err(|e| e.to_string())
    }

    /// Insert a thinking block.
    pub fn insert_thinking_block(
        &mut self,
        after: Option<&BlockId>,
        text: impl Into<String>,
    ) -> Result<BlockId, String> {
        self.doc.insert_thinking_block(after, text).map_err(|e| e.to_string())
    }

    /// Insert a tool use block.
    pub fn insert_tool_use(
        &mut self,
        after: Option<&BlockId>,
        id: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
    ) -> Result<BlockId, String> {
        self.doc
            .insert_tool_use(after, id, name, input)
            .map_err(|e| e.to_string())
    }

    /// Insert a tool result block.
    pub fn insert_tool_result(
        &mut self,
        after: Option<&BlockId>,
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Result<BlockId, String> {
        self.doc
            .insert_tool_result(after, tool_use_id, content, is_error)
            .map_err(|e| e.to_string())
    }

    /// Edit text within a block.
    pub fn edit_text(
        &mut self,
        block_id: &BlockId,
        pos: usize,
        insert: &str,
        delete: usize,
    ) -> Result<(), String> {
        self.doc.edit_text(block_id, pos, insert, delete).map_err(|e| e.to_string())
    }

    /// Append text to a block.
    pub fn append_text(&mut self, block_id: &BlockId, text: &str) -> Result<(), String> {
        self.doc.append_text(block_id, text).map_err(|e| e.to_string())
    }

    /// Set collapsed state.
    pub fn set_collapsed(&mut self, block_id: &BlockId, collapsed: bool) -> Result<(), String> {
        self.doc.set_collapsed(block_id, collapsed).map_err(|e| e.to_string())
    }

    /// Delete a block.
    pub fn delete_block(&mut self, block_id: &BlockId) -> Result<(), String> {
        self.doc.delete_block(block_id).map_err(|e| e.to_string())
    }

    /// Apply a remote operation.
    pub fn apply_remote_op(&mut self, op: &BlockDocOp) -> Result<(), String> {
        self.doc.apply_remote_op(op).map_err(|e| e.to_string())
    }

    /// Take pending operations for sync.
    pub fn take_pending_ops(&mut self) -> Vec<BlockDocOp> {
        self.doc.take_pending_ops()
    }

    /// Check if there are pending operations.
    pub fn has_pending_ops(&self) -> bool {
        self.doc.has_pending_ops()
    }

    /// Get blocks as snapshots for serialization.
    pub fn block_snapshots(&self) -> Vec<(BlockId, BlockContentSnapshot)> {
        self.doc
            .blocks_ordered()
            .iter()
            .map(|block| (block.id.clone(), block.content.snapshot()))
            .collect()
    }
}

/// Store for block-based cell documents.
pub struct BlockStore {
    cells: HashMap<CellId, BlockCell>,
    db: Option<DbHandle>,
    /// Default agent ID for this store.
    agent_id: String,
}

impl BlockStore {
    /// Create a new in-memory block store.
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            cells: HashMap::new(),
            db: None,
            agent_id: agent_id.into(),
        }
    }

    /// Create a block store with SQLite persistence.
    pub fn with_db(db: CellDb, agent_id: impl Into<String>) -> Self {
        Self {
            cells: HashMap::new(),
            db: Some(Arc::new(Mutex::new(db))),
            agent_id: agent_id.into(),
        }
    }

    /// Create a new cell.
    pub fn create_cell(
        &mut self,
        id: CellId,
        kind: CellKind,
        language: Option<String>,
    ) -> Result<&mut BlockCell, String> {
        if self.cells.contains_key(&id) {
            return Err(format!("Cell {} already exists", id));
        }

        // Persist metadata if we have a DB
        if let Some(db) = &self.db {
            let db_guard = db.lock().map_err(|e| format!("DB lock error: {}", e))?;
            let meta = CellMeta {
                id: id.clone(),
                kind,
                language: language.clone(),
                position_col: None,
                position_row: None,
                parent_cell: None,
                created_at: 0,
            };
            db_guard.create_cell(&meta)
                .map_err(|e| format!("DB error: {}", e))?;
        }

        let cell = BlockCell::new(id.clone(), kind, language, &self.agent_id);
        self.cells.insert(id.clone(), cell);
        Ok(self.cells.get_mut(&id).unwrap())
    }

    /// Get a cell by ID.
    pub fn get(&self, id: &str) -> Option<&BlockCell> {
        self.cells.get(id)
    }

    /// Get a mutable cell by ID.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut BlockCell> {
        self.cells.get_mut(id)
    }

    /// List all cell IDs.
    pub fn list_ids(&self) -> Vec<CellId> {
        self.cells.keys().cloned().collect()
    }

    /// Iterate over all cells.
    pub fn iter(&self) -> impl Iterator<Item = &BlockCell> {
        self.cells.values()
    }

    /// Delete a cell.
    pub fn delete_cell(&mut self, id: &str) -> Result<(), String> {
        if let Some(db) = &self.db {
            let db_guard = db.lock().map_err(|e| format!("DB lock error: {}", e))?;
            db_guard.delete_cell(id).map_err(|e| format!("DB error: {}", e))?;
        }
        self.cells.remove(id);
        Ok(())
    }

    /// Check if the store is empty.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Apply a block operation from a remote agent.
    pub fn apply_block_op(
        &mut self,
        cell_id: &str,
        _agent_id: &str,
        op: &BlockDocOp,
    ) -> Result<u64, String> {
        let cell = self
            .get_mut(cell_id)
            .ok_or_else(|| format!("Cell {} not found", cell_id))?;

        cell.apply_remote_op(op)?;

        // TODO: Persist the operation to the database
        // This will require new block-specific tables

        Ok(cell.version())
    }

    /// Get the current state of a cell for sync.
    pub fn get_cell_state(
        &self,
        cell_id: &str,
    ) -> Result<(CellKind, Option<String>, Vec<(BlockId, BlockContentSnapshot)>, u64), String> {
        let cell = self
            .get(cell_id)
            .ok_or_else(|| format!("Cell {} not found", cell_id))?;

        Ok((
            cell.kind,
            cell.language.clone(),
            cell.block_snapshots(),
            cell.version(),
        ))
    }

    /// Load cells from database on startup.
    /// Note: Currently only loads cell metadata, not block content.
    /// Full block persistence will be added in a future phase.
    pub fn load_from_db(&mut self) -> Result<(), String> {
        let db = self.db.as_ref().ok_or("No database configured")?;
        let db_guard = db.lock().map_err(|e| format!("DB lock error: {}", e))?;

        let cell_metas = db_guard.list_cells().map_err(|e| format!("DB error: {}", e))?;

        for meta in cell_metas {
            let cell = BlockCell::new(meta.id.clone(), meta.kind, meta.language, &self.agent_id);
            // TODO: Load block content from database
            // For now, cells start empty after restart
            self.cells.insert(meta.id, cell);
        }

        Ok(())
    }
}

impl Default for BlockStore {
    fn default() -> Self {
        Self::new("server")
    }
}

/// Thread-safe handle to a BlockStore.
pub type SharedBlockStore = Arc<RwLock<BlockStore>>;

/// Create a new shared block store.
pub fn shared_block_store(agent_id: impl Into<String>) -> SharedBlockStore {
    Arc::new(RwLock::new(BlockStore::new(agent_id)))
}

/// Create a shared block store with database persistence.
pub fn shared_block_store_with_db(db: CellDb, agent_id: impl Into<String>) -> SharedBlockStore {
    Arc::new(RwLock::new(BlockStore::with_db(db, agent_id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_cell_basic_ops() {
        let mut cell = BlockCell::new("test".into(), CellKind::Code, Some("rust".into()), "alice");

        // Insert a text block
        let block_id = cell.insert_text_block(None, "hello world").unwrap();
        assert_eq!(cell.content(), "hello world");

        // Append to the block
        cell.append_text(&block_id, "!").unwrap();
        assert_eq!(cell.content(), "hello world!");

        // Edit the block
        cell.edit_text(&block_id, 6, "rust ", 0).unwrap();
        assert_eq!(cell.content(), "hello rust world!");
    }

    #[test]
    fn test_block_cell_multiple_blocks() {
        let mut cell = BlockCell::new("test".into(), CellKind::AgentMessage, None, "agent");

        // Insert thinking block
        let thinking_id = cell.insert_thinking_block(None, "Let me think...").unwrap();

        // Insert text block after thinking
        let text_id = cell.insert_text_block(Some(&thinking_id), "Here's my answer").unwrap();

        // Should have both blocks
        let content = cell.content();
        assert!(content.contains("Let me think..."));
        assert!(content.contains("Here's my answer"));

        // Collapse thinking
        cell.set_collapsed(&thinking_id, true).unwrap();

        // Delete text block
        cell.delete_block(&text_id).unwrap();
        let content = cell.content();
        assert!(content.contains("Let me think..."));
        assert!(!content.contains("Here's my answer"));
    }

    #[test]
    fn test_block_store_crud() {
        let mut store = BlockStore::new("server");

        store
            .create_cell("cell-1".into(), CellKind::Code, Some("rust".into()))
            .unwrap();

        {
            let cell = store.get_mut("cell-1").unwrap();
            cell.insert_text_block(None, "fn main() {}").unwrap();
        }

        let cell = store.get("cell-1").unwrap();
        assert_eq!(cell.content(), "fn main() {}");

        store.delete_cell("cell-1").unwrap();
        assert!(store.get("cell-1").is_none());
    }

    #[test]
    fn test_block_snapshots() {
        let mut cell = BlockCell::new("test".into(), CellKind::AgentMessage, None, "agent");

        let thinking_id = cell.insert_thinking_block(None, "thinking...").unwrap();
        cell.insert_text_block(Some(&thinking_id), "response").unwrap();

        let snapshots = cell.block_snapshots();
        assert_eq!(snapshots.len(), 2);

        // Collect snapshot types
        let mut has_thinking = false;
        let mut has_text = false;

        for (_, snapshot) in &snapshots {
            match snapshot {
                BlockContentSnapshot::Thinking { text, .. } => {
                    assert_eq!(text, "thinking...");
                    has_thinking = true;
                }
                BlockContentSnapshot::Text { text } => {
                    assert_eq!(text, "response");
                    has_text = true;
                }
                _ => {}
            }
        }

        assert!(has_thinking, "Expected a thinking block");
        assert!(has_text, "Expected a text block");
    }
}
