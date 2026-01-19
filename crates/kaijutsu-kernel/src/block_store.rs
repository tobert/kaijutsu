//! Block-based CRDT storage using kaijutsu-crdt.
//!
//! Each cell has a BlockDocument backed by diamond-types OpLog.
//! Multi-client sync is handled via SerializedOps exchange.
//!
//! # Concurrency Model
//!
//! - DashMap for per-cell concurrent access
//! - Event broadcasting for real-time updates
//! - parking_lot for efficient locking

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::RwLock;
use tokio::sync::broadcast;

use kaijutsu_crdt::{
    BlockDocument, BlockId, BlockSnapshot, SerializedOps, SerializedOpsOwned, LV,
};

use crate::db::{CellDb, CellKind, CellMeta};

/// Thread-safe database handle.
type DbHandle = Arc<std::sync::Mutex<CellDb>>;

/// Unique identifier for a cell.
pub type CellId = String;

/// Events broadcast when cells change.
#[derive(Clone, Debug)]
pub enum BlockEvent {
    /// Operations applied to a cell's OpLog.
    OpsApplied {
        cell_id: String,
        ops: SerializedOpsOwned,
        agent_id: String,
    },
    /// A new cell was created.
    CellCreated {
        cell_id: String,
        kind: CellKind,
        agent_id: String,
    },
    /// A cell was deleted.
    CellDeleted {
        cell_id: String,
        agent_id: String,
    },
}

/// Entry for a cell in the store.
pub struct CellEntry {
    /// The block document (owns OpLog).
    pub doc: BlockDocument,
    /// Cell metadata.
    pub kind: CellKind,
    /// Programming language (if code).
    pub language: Option<String>,
    /// Version counter (incremented on each modification).
    version: AtomicU64,
    /// Last agent to modify.
    last_agent: RwLock<String>,
}

impl CellEntry {
    /// Create a new cell entry.
    fn new(id: &str, kind: CellKind, language: Option<String>, agent_id: &str) -> Self {
        Self {
            doc: BlockDocument::new(id, agent_id),
            kind,
            language,
            version: AtomicU64::new(0),
            last_agent: RwLock::new(agent_id.to_string()),
        }
    }

    /// Get the current version.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// Increment version and record agent.
    fn touch(&self, agent_id: &str) {
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

/// Store for block-based cell documents with per-cell locking.
pub struct BlockStore {
    /// Concurrent cell storage.
    cells: DashMap<CellId, CellEntry>,
    /// Database for persistence.
    db: Option<DbHandle>,
    /// Default agent ID for this store.
    agent_id: RwLock<String>,
    /// Event broadcaster.
    event_tx: broadcast::Sender<BlockEvent>,
}

impl BlockStore {
    /// Create a new in-memory block store.
    pub fn new(agent_id: impl Into<String>) -> Self {
        let (event_tx, _) = broadcast::channel(1024);
        Self {
            cells: DashMap::new(),
            db: None,
            agent_id: RwLock::new(agent_id.into()),
            event_tx,
        }
    }

    /// Create a block store with SQLite persistence.
    pub fn with_db(db: CellDb, agent_id: impl Into<String>) -> Self {
        let (event_tx, _) = broadcast::channel(1024);
        Self {
            cells: DashMap::new(),
            db: Some(Arc::new(std::sync::Mutex::new(db))),
            agent_id: RwLock::new(agent_id.into()),
            event_tx,
        }
    }

    /// Get the event receiver for subscribing to changes.
    pub fn subscribe(&self) -> broadcast::Receiver<BlockEvent> {
        self.event_tx.subscribe()
    }

    /// Get the current agent ID.
    pub fn agent_id(&self) -> String {
        self.agent_id.read().clone()
    }

    /// Set the agent ID.
    pub fn set_agent_id(&self, agent_id: impl Into<String>) {
        *self.agent_id.write() = agent_id.into();
    }

    /// Create a new cell.
    pub fn create_cell(
        &self,
        id: CellId,
        kind: CellKind,
        language: Option<String>,
    ) -> Result<(), String> {
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
            db_guard
                .create_cell(&meta)
                .map_err(|e| format!("DB error: {}", e))?;
        }

        let agent_id = self.agent_id();
        let entry = CellEntry::new(&id, kind, language, &agent_id);
        self.cells.insert(id.clone(), entry);

        // Broadcast event
        let _ = self.event_tx.send(BlockEvent::CellCreated {
            cell_id: id,
            kind,
            agent_id,
        });

        Ok(())
    }

    /// Get a cell for reading.
    pub fn get(&self, id: &str) -> Option<dashmap::mapref::one::Ref<'_, CellId, CellEntry>> {
        self.cells.get(id)
    }

    /// Get a cell for writing.
    pub fn get_mut(&self, id: &str) -> Option<dashmap::mapref::one::RefMut<'_, CellId, CellEntry>> {
        self.cells.get_mut(id)
    }

    /// List all cell IDs.
    pub fn list_ids(&self) -> Vec<CellId> {
        self.cells.iter().map(|r| r.key().clone()).collect()
    }

    /// Check if a cell exists.
    pub fn contains(&self, id: &str) -> bool {
        self.cells.contains_key(id)
    }

    /// Delete a cell.
    pub fn delete_cell(&self, id: &str) -> Result<(), String> {
        if let Some(db) = &self.db {
            let db_guard = db.lock().map_err(|e| format!("DB lock error: {}", e))?;
            db_guard
                .delete_cell(id)
                .map_err(|e| format!("DB error: {}", e))?;
        }

        self.cells.remove(id);

        // Broadcast event
        let _ = self.event_tx.send(BlockEvent::CellDeleted {
            cell_id: id.to_string(),
            agent_id: self.agent_id(),
        });

        Ok(())
    }

    /// Check if the store is empty.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Get the number of cells.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    // =========================================================================
    // Block Operations (convenience methods)
    // =========================================================================

    /// Insert a text block into a cell.
    pub fn insert_text_block(
        &self,
        cell_id: &str,
        after: Option<&BlockId>,
        text: impl Into<String>,
    ) -> Result<BlockId, String> {
        let mut entry = self.get_mut(cell_id).ok_or_else(|| format!("Cell {} not found", cell_id))?;
        let agent_id = self.agent_id();
        let result = entry.doc.insert_text_block(after, text).map_err(|e| e.to_string())?;
        entry.touch(&agent_id);
        Ok(result)
    }

    /// Insert a thinking block into a cell.
    pub fn insert_thinking_block(
        &self,
        cell_id: &str,
        after: Option<&BlockId>,
        text: impl Into<String>,
    ) -> Result<BlockId, String> {
        let mut entry = self.get_mut(cell_id).ok_or_else(|| format!("Cell {} not found", cell_id))?;
        let agent_id = self.agent_id();
        let result = entry.doc.insert_thinking_block(after, text).map_err(|e| e.to_string())?;
        entry.touch(&agent_id);
        Ok(result)
    }

    /// Insert a tool use block into a cell.
    pub fn insert_tool_use(
        &self,
        cell_id: &str,
        after: Option<&BlockId>,
        tool_id: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
    ) -> Result<BlockId, String> {
        let mut entry = self.get_mut(cell_id).ok_or_else(|| format!("Cell {} not found", cell_id))?;
        let agent_id = self.agent_id();
        let result = entry.doc.insert_tool_use(after, tool_id, name, input).map_err(|e| e.to_string())?;
        entry.touch(&agent_id);
        Ok(result)
    }

    /// Insert a tool result block into a cell.
    pub fn insert_tool_result(
        &self,
        cell_id: &str,
        after: Option<&BlockId>,
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Result<BlockId, String> {
        let mut entry = self.get_mut(cell_id).ok_or_else(|| format!("Cell {} not found", cell_id))?;
        let agent_id = self.agent_id();
        let result = entry.doc.insert_tool_result(after, tool_use_id, content, is_error).map_err(|e| e.to_string())?;
        entry.touch(&agent_id);
        Ok(result)
    }

    /// Edit text within a block.
    pub fn edit_text(
        &self,
        cell_id: &str,
        block_id: &BlockId,
        pos: usize,
        insert: &str,
        delete: usize,
    ) -> Result<(), String> {
        let mut entry = self.get_mut(cell_id).ok_or_else(|| format!("Cell {} not found", cell_id))?;
        let agent_id = self.agent_id();
        entry.doc.edit_text(block_id, pos, insert, delete).map_err(|e| e.to_string())?;
        entry.touch(&agent_id);
        Ok(())
    }

    /// Append text to a block.
    pub fn append_text(&self, cell_id: &str, block_id: &BlockId, text: &str) -> Result<(), String> {
        let mut entry = self.get_mut(cell_id).ok_or_else(|| format!("Cell {} not found", cell_id))?;
        let agent_id = self.agent_id();
        entry.doc.append_text(block_id, text).map_err(|e| e.to_string())?;
        entry.touch(&agent_id);
        Ok(())
    }

    /// Set collapsed state for a thinking block.
    pub fn set_collapsed(&self, cell_id: &str, block_id: &BlockId, collapsed: bool) -> Result<(), String> {
        let mut entry = self.get_mut(cell_id).ok_or_else(|| format!("Cell {} not found", cell_id))?;
        let agent_id = self.agent_id();
        entry.doc.set_collapsed(block_id, collapsed).map_err(|e| e.to_string())?;
        entry.touch(&agent_id);
        Ok(())
    }

    /// Delete a block from a cell.
    pub fn delete_block(&self, cell_id: &str, block_id: &BlockId) -> Result<(), String> {
        let mut entry = self.get_mut(cell_id).ok_or_else(|| format!("Cell {} not found", cell_id))?;
        let agent_id = self.agent_id();
        entry.doc.delete_block(block_id).map_err(|e| e.to_string())?;
        entry.touch(&agent_id);
        Ok(())
    }

    // =========================================================================
    // Sync Operations
    // =========================================================================

    /// Get operations since a frontier for a cell.
    pub fn ops_since(&self, cell_id: &str, frontier: &[LV]) -> Result<SerializedOpsOwned, String> {
        let entry = self.get(cell_id).ok_or_else(|| format!("Cell {} not found", cell_id))?;
        Ok(entry.doc.ops_since(frontier))
    }

    /// Merge remote operations into a cell.
    pub fn merge_ops(&self, cell_id: &str, ops: SerializedOps<'_>) -> Result<u64, String> {
        let mut entry = self.get_mut(cell_id).ok_or_else(|| format!("Cell {} not found", cell_id))?;
        entry.doc.merge_ops(ops).map_err(|e| e.to_string())?;
        let version = entry.doc.version();
        entry.version.store(version, Ordering::SeqCst);
        Ok(version)
    }

    /// Get the current frontier for a cell.
    pub fn frontier(&self, cell_id: &str) -> Result<Vec<LV>, String> {
        let entry = self.get(cell_id).ok_or_else(|| format!("Cell {} not found", cell_id))?;
        Ok(entry.doc.frontier())
    }

    // =========================================================================
    // Query Operations
    // =========================================================================

    /// Get block snapshots for a cell.
    pub fn block_snapshots(&self, cell_id: &str) -> Result<Vec<BlockSnapshot>, String> {
        let entry = self.get(cell_id).ok_or_else(|| format!("Cell {} not found", cell_id))?;
        Ok(entry.doc.blocks_ordered())
    }

    /// Get the full text content of a cell.
    pub fn get_content(&self, cell_id: &str) -> Result<String, String> {
        let entry = self.get(cell_id).ok_or_else(|| format!("Cell {} not found", cell_id))?;
        Ok(entry.content())
    }

    /// Get cell metadata and version.
    pub fn get_cell_state(
        &self,
        cell_id: &str,
    ) -> Result<(CellKind, Option<String>, Vec<BlockSnapshot>, u64), String> {
        let entry = self.get(cell_id).ok_or_else(|| format!("Cell {} not found", cell_id))?;
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

    /// Load cells from database on startup.
    /// Note: Currently only loads cell metadata, not block content.
    pub fn load_from_db(&self) -> Result<(), String> {
        let db = self.db.as_ref().ok_or("No database configured")?;
        let db_guard = db.lock().map_err(|e| format!("DB lock error: {}", e))?;

        let cell_metas = db_guard
            .list_cells()
            .map_err(|e| format!("DB error: {}", e))?;

        let agent_id = self.agent_id();
        for meta in cell_metas {
            let entry = CellEntry::new(&meta.id, meta.kind, meta.language, &agent_id);
            // TODO: Load block content from database
            // For now, cells start empty after restart
            self.cells.insert(meta.id, entry);
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
/// With DashMap, the store itself doesn't need RwLock.
pub type SharedBlockStore = Arc<BlockStore>;

/// Create a new shared block store.
pub fn shared_block_store(agent_id: impl Into<String>) -> SharedBlockStore {
    Arc::new(BlockStore::new(agent_id))
}

/// Create a shared block store with database persistence.
pub fn shared_block_store_with_db(db: CellDb, agent_id: impl Into<String>) -> SharedBlockStore {
    Arc::new(BlockStore::with_db(db, agent_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_crdt::BlockContentSnapshot;

    #[test]
    fn test_block_store_basic_ops() {
        let store = BlockStore::new("alice");

        store.create_cell("test".into(), CellKind::Code, Some("rust".into())).unwrap();

        // Insert a text block
        let block_id = store.insert_text_block("test", None, "hello world").unwrap();
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

        store.create_cell("test".into(), CellKind::AgentMessage, None).unwrap();

        // Insert thinking block
        let thinking_id = store.insert_thinking_block("test", None, "Let me think...").unwrap();

        // Insert text block after thinking
        let text_id = store.insert_text_block("test", Some(&thinking_id), "Here's my answer").unwrap();

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

        store.create_cell("cell-1".into(), CellKind::Code, Some("rust".into())).unwrap();

        store.insert_text_block("cell-1", None, "fn main() {}").unwrap();

        assert_eq!(store.get_content("cell-1").unwrap(), "fn main() {}");

        store.delete_cell("cell-1").unwrap();
        assert!(store.get("cell-1").is_none());
    }

    #[test]
    fn test_block_snapshots() {
        let store = BlockStore::new("agent");

        store.create_cell("test".into(), CellKind::AgentMessage, None).unwrap();

        let thinking_id = store.insert_thinking_block("test", None, "thinking...").unwrap();
        store.insert_text_block("test", Some(&thinking_id), "response").unwrap();

        let snapshots = store.block_snapshots("test").unwrap();
        assert_eq!(snapshots.len(), 2);

        // Check snapshot types
        let mut has_thinking = false;
        let mut has_text = false;

        for snapshot in &snapshots {
            match &snapshot.content {
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

    #[tokio::test]
    async fn test_event_subscription() {
        let store = BlockStore::new("alice");
        let mut rx = store.subscribe();

        store.create_cell("test".into(), CellKind::Code, None).unwrap();

        // Should receive CellCreated event
        let event = rx.try_recv().unwrap();
        match event {
            BlockEvent::CellCreated { cell_id, kind, agent_id } => {
                assert_eq!(cell_id, "test");
                assert_eq!(kind, CellKind::Code);
                assert_eq!(agent_id, "alice");
            }
            _ => panic!("Expected CellCreated event"),
        }
    }

    #[tokio::test]
    async fn test_concurrent_cell_access() {
        use std::sync::Arc;
        use tokio::task::JoinSet;

        let store = Arc::new(BlockStore::new("test-agent"));
        store
            .create_cell("shared-cell".into(), CellKind::Code, None)
            .unwrap();

        let mut tasks = JoinSet::new();
        let num_tasks = 4;
        let ops_per_task = 10;

        // Spawn multiple tasks that concurrently append text to the same cell
        for i in 0..num_tasks {
            let store_clone = Arc::clone(&store);
            tasks.spawn(async move {
                for j in 0..ops_per_task {
                    // Each task inserts a uniquely identifiable block
                    let text = format!("[task-{}-op-{}]", i, j);
                    let _ = store_clone.insert_text_block("shared-cell", None, &text);
                }
            });
        }

        // Wait for all tasks to complete
        while let Some(result) = tasks.join_next().await {
            result.expect("Task panicked");
        }

        // Verify the cell has content from all tasks
        let content = store.get_content("shared-cell").unwrap();

        // Should have at least some content (exact ordering is non-deterministic)
        assert!(!content.is_empty());

        // Count how many blocks we have - should be num_tasks * ops_per_task
        let snapshots = store.block_snapshots("shared-cell").unwrap();
        assert_eq!(
            snapshots.len(),
            num_tasks * ops_per_task,
            "Expected {} blocks, got {}",
            num_tasks * ops_per_task,
            snapshots.len()
        );
    }

    #[tokio::test]
    async fn test_concurrent_multi_cell_access() {
        use std::sync::Arc;
        use tokio::task::JoinSet;

        let store = Arc::new(BlockStore::new("test-agent"));

        // Create multiple cells
        let num_cells = 3;
        for i in 0..num_cells {
            store
                .create_cell(format!("cell-{}", i), CellKind::Code, None)
                .unwrap();
        }

        let mut tasks = JoinSet::new();
        let num_tasks = 6;

        // Each task works on different cells
        for i in 0..num_tasks {
            let store_clone = Arc::clone(&store);
            let cell_id = format!("cell-{}", i % num_cells);
            tasks.spawn(async move {
                for j in 0..5 {
                    let text = format!("task-{}-op-{}", i, j);
                    let _ = store_clone.insert_text_block(&cell_id, None, &text);
                }
            });
        }

        // Wait for all tasks
        while let Some(result) = tasks.join_next().await {
            result.expect("Task panicked");
        }

        // Each cell should have content
        for i in 0..num_cells {
            let cell_id = format!("cell-{}", i);
            let content = store.get_content(&cell_id).unwrap();
            assert!(!content.is_empty(), "Cell {} should have content", cell_id);
        }
    }

    #[tokio::test]
    async fn test_concurrent_read_write() {
        use std::sync::Arc;
        use tokio::task::JoinSet;

        let store = Arc::new(BlockStore::new("test-agent"));
        store
            .create_cell("rw-cell".into(), CellKind::Code, None)
            .unwrap();

        // Insert initial content
        let block_id = store
            .insert_text_block("rw-cell", None, "initial content")
            .unwrap();

        let mut tasks = JoinSet::new();

        // Spawn writer tasks
        for i in 0..3 {
            let store_clone = Arc::clone(&store);
            let bid = block_id.clone();
            tasks.spawn(async move {
                for j in 0..5 {
                    // Append text
                    let text = format!(" [w{}:{}]", i, j);
                    let _ = store_clone.append_text("rw-cell", &bid, &text);
                }
            });
        }

        // Spawn reader tasks
        for _ in 0..3 {
            let store_clone = Arc::clone(&store);
            tasks.spawn(async move {
                for _ in 0..10 {
                    // Read content
                    let _ = store_clone.get_content("rw-cell");
                }
            });
        }

        // Wait for all tasks
        while let Some(result) = tasks.join_next().await {
            result.expect("Task panicked");
        }

        // Content should still be valid
        let content = store.get_content("rw-cell").unwrap();
        assert!(content.starts_with("initial content"));
    }

    #[tokio::test]
    async fn test_event_subscription_concurrent() {
        use std::sync::Arc;
        use tokio::task::JoinSet;

        let store = Arc::new(BlockStore::new("test-agent"));
        let mut rx = store.subscribe();

        let mut tasks = JoinSet::new();

        // Create multiple cells concurrently
        for i in 0..10 {
            let store_clone = Arc::clone(&store);
            tasks.spawn(async move {
                store_clone
                    .create_cell(format!("event-cell-{}", i), CellKind::Code, None)
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
}
