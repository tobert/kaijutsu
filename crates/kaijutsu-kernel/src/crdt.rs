//! CRDT-based cell storage using diamond-types.
//!
//! Each cell is a separate CRDT document. The CellStore manages multiple
//! documents and handles persistence via the db module.

use diamond_types::list::encoding::{ENCODE_FULL, ENCODE_PATCH};
use diamond_types::list::ListCRDT;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::db::{CellDb, CellKind, CellMeta};

/// Unique identifier for a cell.
pub type CellId = String;

/// A single cell document with CRDT state.
pub struct CellDoc {
    pub id: CellId,
    pub kind: CellKind,
    pub language: Option<String>,
    pub crdt: ListCRDT,
}

impl CellDoc {
    /// Create a new cell document.
    pub fn new(id: CellId, kind: CellKind, language: Option<String>) -> Self {
        Self {
            id,
            kind,
            language,
            crdt: ListCRDT::new(),
        }
    }

    /// Get the current text content.
    pub fn content(&self) -> String {
        self.crdt.branch.content().to_string()
    }

    /// Get the length of the content in chars.
    pub fn len(&self) -> usize {
        self.crdt.branch.content().len_chars()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Insert text at position as a specific agent.
    pub fn insert(&mut self, agent_name: &str, pos: usize, text: &str) {
        let agent_id = self.crdt.oplog.get_or_create_agent_id(agent_name);
        self.crdt.insert(agent_id, pos, text);
    }

    /// Delete a range of text as a specific agent.
    pub fn delete(&mut self, agent_name: &str, start: usize, end: usize) {
        if start >= end {
            return;
        }
        let agent_id = self.crdt.oplog.get_or_create_agent_id(agent_name);
        self.crdt.delete_without_content(agent_id, start..end);
    }

    /// Replace a range with new text.
    pub fn replace(&mut self, agent_name: &str, start: usize, end: usize, text: &str) {
        if start < end {
            self.delete(agent_name, start, end);
        }
        if !text.is_empty() {
            self.insert(agent_name, start, text);
        }
    }

    /// Encode the full oplog for storage or transmission.
    pub fn encode_full(&self) -> Vec<u8> {
        self.crdt.oplog.encode(ENCODE_FULL)
    }

    /// Encode changes since a specific version (for incremental sync).
    pub fn encode_patch_from(&self, from_version: &[usize]) -> Vec<u8> {
        self.crdt.oplog.encode_from(ENCODE_PATCH, from_version)
    }

    /// Merge encoded operations from another agent.
    pub fn merge(&mut self, encoded: &[u8]) -> Result<(), String> {
        self.crdt
            .oplog
            .decode_and_add(encoded)
            .map_err(|e| format!("Failed to decode ops: {:?}", e))?;
        self.crdt
            .branch
            .merge(&self.crdt.oplog, self.crdt.oplog.local_version_ref());
        Ok(())
    }

    /// Get the current frontier (version) as a vector for comparison.
    pub fn frontier(&self) -> Vec<usize> {
        self.crdt.oplog.local_version().to_vec()
    }

    /// Get the frontier as a serializable format.
    pub fn frontier_version(&self) -> u64 {
        // Use the maximum LV (local version) as the version number
        self.crdt.oplog.local_version().iter().copied().max().unwrap_or(0) as u64
    }
}

/// Store for multiple cell documents with persistence.
pub struct CellStore {
    cells: HashMap<CellId, CellDoc>,
    db: Option<CellDb>,
}

impl CellStore {
    /// Create a new in-memory cell store.
    pub fn new() -> Self {
        Self {
            cells: HashMap::new(),
            db: None,
        }
    }

    /// Create a cell store with SQLite persistence.
    pub fn with_db(db: CellDb) -> Self {
        Self {
            cells: HashMap::new(),
            db: Some(db),
        }
    }

    /// Create a new cell.
    pub fn create_cell(
        &mut self,
        id: CellId,
        kind: CellKind,
        language: Option<String>,
    ) -> Result<&mut CellDoc, String> {
        if self.cells.contains_key(&id) {
            return Err(format!("Cell {} already exists", id));
        }

        // Persist metadata if we have a DB
        if let Some(db) = &self.db {
            let meta = CellMeta {
                id: id.clone(),
                kind,
                language: language.clone(),
                position_col: None,
                position_row: None,
                parent_cell: None,
                created_at: 0,
            };
            db.create_cell(&meta)
                .map_err(|e| format!("DB error: {}", e))?;
        }

        let doc = CellDoc::new(id.clone(), kind, language);
        self.cells.insert(id.clone(), doc);
        Ok(self.cells.get_mut(&id).unwrap())
    }

    /// Get a cell by ID.
    pub fn get(&self, id: &str) -> Option<&CellDoc> {
        self.cells.get(id)
    }

    /// Get a mutable cell by ID.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut CellDoc> {
        self.cells.get_mut(id)
    }

    /// List all cell IDs.
    pub fn list_ids(&self) -> Vec<CellId> {
        self.cells.keys().cloned().collect()
    }

    /// Iterate over all cells.
    pub fn iter(&self) -> impl Iterator<Item = &CellDoc> {
        self.cells.values()
    }

    /// Delete a cell.
    pub fn delete_cell(&mut self, id: &str) -> Result<(), String> {
        if let Some(db) = &self.db {
            db.delete_cell(id).map_err(|e| format!("DB error: {}", e))?;
        }
        self.cells.remove(id);
        Ok(())
    }

    /// Persist an operation to the database.
    /// This should be called after applying operations to a cell.
    pub fn persist_op(
        &self,
        cell_id: &str,
        agent_id: &str,
        op_bytes: &[u8],
    ) -> Result<(), String> {
        if let Some(db) = &self.db {
            db.append_op(cell_id, agent_id, op_bytes, None)
                .map_err(|e| format!("DB error: {}", e))?;
        }
        Ok(())
    }

    /// Check if the store is empty (no cells).
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Apply an operation from a remote agent.
    pub fn apply_remote_op(
        &mut self,
        cell_id: &str,
        agent_id: &str,
        encoded_op: &[u8],
    ) -> Result<(), String> {
        let cell = self
            .get_mut(cell_id)
            .ok_or_else(|| format!("Cell {} not found", cell_id))?;

        cell.merge(encoded_op)?;

        // Persist the operation
        if let Some(db) = &self.db {
            db.append_op(cell_id, agent_id, encoded_op, None)
                .map_err(|e| format!("DB error: {}", e))?;
        }

        Ok(())
    }

    /// Get delta operations for a cell since a given version.
    pub fn get_delta(&self, cell_id: &str, since_version: u64) -> Result<Vec<u8>, String> {
        let cell = self
            .get(cell_id)
            .ok_or_else(|| format!("Cell {} not found", cell_id))?;

        // If version is 0, send full state
        if since_version == 0 {
            return Ok(cell.encode_full());
        }

        // Otherwise encode from the version
        Ok(cell.encode_patch_from(&[since_version as usize]))
    }

    /// Snapshot a cell to the database.
    pub fn snapshot(&self, cell_id: &str) -> Result<(), String> {
        let cell = self
            .get(cell_id)
            .ok_or_else(|| format!("Cell {} not found", cell_id))?;

        if let Some(db) = &self.db {
            let content = cell.content();
            let oplog_bytes = cell.encode_full();
            let version = cell.frontier_version() as i64;

            db.save_snapshot(cell_id, version, &content, Some(&oplog_bytes))
                .map_err(|e| format!("DB error: {}", e))?;
        }

        Ok(())
    }

    /// Load cells from database on startup.
    pub fn load_from_db(&mut self) -> Result<(), String> {
        let db = self.db.as_ref().ok_or("No database configured")?;

        let cell_metas = db.list_cells().map_err(|e| format!("DB error: {}", e))?;

        for meta in cell_metas {
            let mut doc = CellDoc::new(meta.id.clone(), meta.kind, meta.language);

            // Try to load snapshot
            if let Ok(Some(snapshot)) = db.get_snapshot(&meta.id) {
                if let Some(oplog_bytes) = snapshot.oplog_bytes {
                    let _ = doc.merge(&oplog_bytes);
                }
            }

            // Apply any ops after the snapshot
            if let Ok(ops) = db.get_all_ops(&meta.id) {
                for op in ops {
                    let _ = doc.merge(&op.op_bytes);
                }
            }

            self.cells.insert(meta.id, doc);
        }

        Ok(())
    }
}

impl Default for CellStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe handle to a CellStore.
pub type SharedCellStore = Arc<RwLock<CellStore>>;

/// Create a new shared cell store.
pub fn shared_cell_store() -> SharedCellStore {
    Arc::new(RwLock::new(CellStore::new()))
}

/// Create a shared cell store with database persistence.
pub fn shared_cell_store_with_db(db: CellDb) -> SharedCellStore {
    Arc::new(RwLock::new(CellStore::with_db(db)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cell_doc_basic_ops() {
        let mut doc = CellDoc::new("test".into(), CellKind::Code, Some("rust".into()));

        doc.insert("alice", 0, "hello");
        assert_eq!(doc.content(), "hello");

        doc.insert("alice", 5, " world");
        assert_eq!(doc.content(), "hello world");

        doc.delete("alice", 5, 6);
        assert_eq!(doc.content(), "helloworld");

        doc.replace("alice", 5, 10, " rust");
        assert_eq!(doc.content(), "hello rust");
    }

    #[test]
    fn test_cell_doc_concurrent_edits() {
        let mut doc1 = CellDoc::new("test".into(), CellKind::Code, None);
        let mut doc2 = CellDoc::new("test".into(), CellKind::Code, None);

        // Both start empty, make concurrent edits
        doc1.insert("alice", 0, "hello");
        doc2.insert("bob", 0, "world");

        // Exchange encoded states
        let encoded1 = doc1.encode_full();
        let encoded2 = doc2.encode_full();

        doc1.merge(&encoded2).unwrap();
        doc2.merge(&encoded1).unwrap();

        // Both should converge to the same content
        assert_eq!(doc1.content(), doc2.content());
    }

    #[test]
    fn test_cell_store_crud() {
        let mut store = CellStore::new();

        store
            .create_cell("cell-1".into(), CellKind::Code, Some("rust".into()))
            .unwrap();

        {
            let cell = store.get_mut("cell-1").unwrap();
            cell.insert("alice", 0, "fn main() {}");
        }

        let cell = store.get("cell-1").unwrap();
        assert_eq!(cell.content(), "fn main() {}");

        store.delete_cell("cell-1").unwrap();
        assert!(store.get("cell-1").is_none());
    }

    #[test]
    fn test_cell_store_with_db() {
        let db = CellDb::in_memory().unwrap();
        let mut store = CellStore::with_db(db);

        store
            .create_cell("cell-1".into(), CellKind::Markdown, None)
            .unwrap();

        {
            let cell = store.get_mut("cell-1").unwrap();
            cell.insert("alice", 0, "# Hello");
        }

        store.snapshot("cell-1").unwrap();

        // Verify snapshot exists in DB
        let db_ref = store.db.as_ref().unwrap();
        let snapshot = db_ref.get_snapshot("cell-1").unwrap().unwrap();
        assert_eq!(snapshot.content, "# Hello");
    }
}
