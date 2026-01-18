//! Block document with CRDT ordering and per-block text CRDTs.

use std::collections::HashMap;

use cola::Replica;

use crate::{
    Block, BlockContent, BlockContentSnapshot, BlockDocOp, BlockId, CrdtError, Result,
};

/// Block document with Fugue ordering and per-block CRDTs.
///
/// # Two-Level CRDT
///
/// 1. **Block ordering**: Uses `cola::Replica` (Fugue CRDT) for concurrent
///    block insertion, deletion, and reordering. This ensures all agents
///    converge to the same block order.
///
/// 2. **Block content**: Each editable block (Thinking, Text) has its own
///    `diamond_types::ListCRDT` for concurrent text editing within that block.
///
/// # Usage
///
/// ```
/// use kaijutsu_crdt::BlockDocument;
///
/// let mut doc = BlockDocument::new("cell-1", "alice");
///
/// // Insert a text block
/// let block_id = doc.insert_text_block(None, "Hello").unwrap();
///
/// // Edit the block
/// doc.append_text(&block_id, ", world!").unwrap();
///
/// // Get pending ops to send to server
/// let ops = doc.take_pending_ops();
/// ```
pub struct BlockDocument {
    /// Cell ID this document belongs to.
    cell_id: String,

    /// Agent ID for this instance.
    agent_id: String,

    /// Fugue replica for block ordering.
    ordering: Replica,

    /// Blocks by ID.
    blocks: HashMap<BlockId, Block>,

    /// Ordered list of block IDs (derived from Fugue).
    /// This is the linearization of the Fugue tree.
    block_order: Vec<BlockId>,

    /// Next sequence number for block IDs.
    next_seq: u64,

    /// Pending operations to send to server.
    pending_ops: Vec<BlockDocOp>,

    /// Document version (incremented on each operation).
    version: u64,
}

impl BlockDocument {
    /// Create a new empty document.
    pub fn new(cell_id: impl Into<String>, agent_id: impl Into<String>) -> Self {
        Self {
            cell_id: cell_id.into(),
            agent_id: agent_id.into(),
            ordering: Replica::new(1, 0), // Start with ID 1
            blocks: HashMap::new(),
            block_order: Vec::new(),
            next_seq: 0,
            pending_ops: Vec::new(),
            version: 0,
        }
    }

    // =========================================================================
    // Accessors
    // =========================================================================

    /// Get the cell ID.
    pub fn cell_id(&self) -> &str {
        &self.cell_id
    }

    /// Get the agent ID.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Get the current version.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Get the number of blocks.
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Check if the document is empty.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Get a block by ID.
    pub fn get_block(&self, id: &BlockId) -> Option<&Block> {
        self.blocks.get(id)
    }

    /// Get a mutable block by ID.
    pub fn get_block_mut(&mut self, id: &BlockId) -> Option<&mut Block> {
        self.blocks.get_mut(id)
    }

    /// Get blocks in document order.
    pub fn blocks_ordered(&self) -> Vec<&Block> {
        self.block_order
            .iter()
            .filter_map(|id| self.blocks.get(id))
            .collect()
    }

    /// Get full text content (concatenation of all blocks).
    pub fn full_text(&self) -> String {
        self.blocks_ordered()
            .into_iter()
            .map(|b| b.text())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    // =========================================================================
    // Block Operations
    // =========================================================================

    /// Generate a new block ID.
    fn new_block_id(&mut self) -> BlockId {
        let id = BlockId::new(&self.cell_id, &self.agent_id, self.next_seq);
        self.next_seq += 1;
        id
    }

    /// Insert a text block.
    pub fn insert_text_block(
        &mut self,
        after: Option<&BlockId>,
        text: impl Into<String>,
    ) -> Result<BlockId> {
        self.insert_text_block_with_author(after, text, &self.agent_id.clone())
    }

    /// Insert a text block with a specific author.
    pub fn insert_text_block_with_author(
        &mut self,
        after: Option<&BlockId>,
        text: impl Into<String>,
        author: impl Into<String>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let content = BlockContentSnapshot::Text { text: text.into() };
        self.insert_block_internal(id.clone(), after.cloned(), content, author.into())?;
        Ok(id)
    }

    /// Insert a thinking block.
    pub fn insert_thinking_block(
        &mut self,
        after: Option<&BlockId>,
        text: impl Into<String>,
    ) -> Result<BlockId> {
        self.insert_thinking_block_with_author(after, text, &self.agent_id.clone())
    }

    /// Insert a thinking block with a specific author.
    pub fn insert_thinking_block_with_author(
        &mut self,
        after: Option<&BlockId>,
        text: impl Into<String>,
        author: impl Into<String>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let content = BlockContentSnapshot::Thinking {
            text: text.into(),
            collapsed: false,
        };
        self.insert_block_internal(id.clone(), after.cloned(), content, author.into())?;
        Ok(id)
    }

    /// Insert a tool use block.
    pub fn insert_tool_use(
        &mut self,
        after: Option<&BlockId>,
        tool_id: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
    ) -> Result<BlockId> {
        self.insert_tool_use_with_author(after, tool_id, name, input, &self.agent_id.clone())
    }

    /// Insert a tool use block with a specific author.
    pub fn insert_tool_use_with_author(
        &mut self,
        after: Option<&BlockId>,
        tool_id: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
        author: impl Into<String>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let content = BlockContentSnapshot::ToolUse {
            id: tool_id.into(),
            name: name.into(),
            input,
        };
        self.insert_block_internal(id.clone(), after.cloned(), content, author.into())?;
        Ok(id)
    }

    /// Insert a tool result block.
    pub fn insert_tool_result(
        &mut self,
        after: Option<&BlockId>,
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Result<BlockId> {
        self.insert_tool_result_with_author(after, tool_use_id, content, is_error, &self.agent_id.clone())
    }

    /// Insert a tool result block with a specific author.
    pub fn insert_tool_result_with_author(
        &mut self,
        after: Option<&BlockId>,
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
        author: impl Into<String>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let snapshot = BlockContentSnapshot::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error,
        };
        self.insert_block_internal(id.clone(), after.cloned(), snapshot, author.into())?;
        Ok(id)
    }

    /// Internal block insertion.
    fn insert_block_internal(
        &mut self,
        id: BlockId,
        after: Option<BlockId>,
        snapshot: BlockContentSnapshot,
        author: String,
    ) -> Result<()> {
        if self.blocks.contains_key(&id) {
            return Err(CrdtError::DuplicateBlock(id));
        }

        // Validate reference if provided
        if let Some(ref after_id) = after {
            if !self.blocks.contains_key(after_id) {
                return Err(CrdtError::InvalidReference(after_id.clone()));
            }
        }

        // Determine insertion index in block_order
        let insert_idx = match &after {
            Some(after_id) => self
                .block_order
                .iter()
                .position(|id| id == after_id)
                .map(|i| i + 1)
                .unwrap_or(self.block_order.len()),
            None => 0,
        };

        // Insert into Fugue (cola)
        // For cola, we need to convert our position to their offset
        let _ = self.ordering.inserted(insert_idx, 1);

        // Create the block with author
        let content = snapshot.clone().into_content(&self.agent_id);
        let block = Block::new(id.clone(), content, &author);
        let created_at = block.created_at;

        // Update state
        self.blocks.insert(id.clone(), block);
        self.block_order.insert(insert_idx, id.clone());
        self.version += 1;

        // Queue operation
        self.pending_ops.push(BlockDocOp::InsertBlock {
            id,
            after,
            content: snapshot,
            author,
            created_at,
            fugue_meta: None, // TODO: capture Fugue metadata
        });

        Ok(())
    }

    /// Delete a block.
    pub fn delete_block(&mut self, id: &BlockId) -> Result<()> {
        if !self.blocks.contains_key(id) {
            return Err(CrdtError::BlockNotFound(id.clone()));
        }

        // Find position in order
        if let Some(idx) = self.block_order.iter().position(|bid| bid == id) {
            // Update Fugue
            let _ = self.ordering.deleted(idx..idx + 1);

            // Remove from state
            self.block_order.remove(idx);
        }

        self.blocks.remove(id);
        self.version += 1;

        // Queue operation
        self.pending_ops.push(BlockDocOp::DeleteBlock { id: id.clone() });

        Ok(())
    }

    // =========================================================================
    // Text Operations
    // =========================================================================

    /// Edit text within a block.
    pub fn edit_text(
        &mut self,
        id: &BlockId,
        pos: usize,
        insert: &str,
        delete: usize,
    ) -> Result<()> {
        let block = self
            .blocks
            .get_mut(id)
            .ok_or_else(|| CrdtError::BlockNotFound(id.clone()))?;

        // Check if block is editable
        if block.block_type().is_immutable() {
            return Err(CrdtError::ImmutableBlock(id.clone()));
        }

        let crdt = block
            .text_crdt_mut()
            .ok_or_else(|| CrdtError::ImmutableBlock(id.clone()))?;

        // Validate position
        let len = crdt.branch.content().len_chars();
        if pos > len {
            return Err(CrdtError::PositionOutOfBounds { pos, len });
        }
        if pos + delete > len {
            return Err(CrdtError::PositionOutOfBounds {
                pos: pos + delete,
                len,
            });
        }

        // Apply to CRDT
        let agent = crdt.oplog.get_or_create_agent_id(&self.agent_id);

        if delete > 0 {
            crdt.delete_without_content(agent, pos..pos + delete);
        }
        if !insert.is_empty() {
            crdt.insert(agent, pos, insert);
        }

        self.version += 1;

        // Queue operation
        self.pending_ops.push(BlockDocOp::EditBlockText {
            id: id.clone(),
            pos,
            insert: insert.to_string(),
            delete,
            dt_encoded: None, // TODO: encode diamond-types op
        });

        Ok(())
    }

    /// Append text to a block.
    pub fn append_text(&mut self, id: &BlockId, text: &str) -> Result<()> {
        let block = self
            .blocks
            .get(id)
            .ok_or_else(|| CrdtError::BlockNotFound(id.clone()))?;

        let len = block.text_crdt().map(|c| c.branch.content().len_chars()).unwrap_or(0);

        self.edit_text(id, len, text, 0)
    }

    /// Set collapsed state of a thinking block.
    pub fn set_collapsed(&mut self, id: &BlockId, collapsed: bool) -> Result<()> {
        let block = self
            .blocks
            .get_mut(id)
            .ok_or_else(|| CrdtError::BlockNotFound(id.clone()))?;

        if !matches!(block.content, BlockContent::Thinking { .. }) {
            return Err(CrdtError::ImmutableBlock(id.clone()));
        }

        block.content.set_collapsed(collapsed);
        self.version += 1;

        // Queue operation
        self.pending_ops.push(BlockDocOp::SetCollapsed {
            id: id.clone(),
            collapsed,
        });

        Ok(())
    }

    // =========================================================================
    // Sync Operations
    // =========================================================================

    /// Take pending operations for sending to server.
    pub fn take_pending_ops(&mut self) -> Vec<BlockDocOp> {
        std::mem::take(&mut self.pending_ops)
    }

    /// Check if there are pending operations.
    pub fn has_pending_ops(&self) -> bool {
        !self.pending_ops.is_empty()
    }

    /// Apply a remote operation.
    pub fn apply_remote_op(&mut self, op: &BlockDocOp) -> Result<()> {
        match op {
            BlockDocOp::InsertBlock {
                id,
                after,
                content,
                author,
                created_at,
                fugue_meta: _,
            } => {
                self.apply_remote_insert(id.clone(), after.clone(), content.clone(), author.clone(), *created_at)
            }
            BlockDocOp::DeleteBlock { id } => self.apply_remote_delete(id),
            BlockDocOp::EditBlockText {
                id,
                pos,
                insert,
                delete,
                dt_encoded: _,
            } => self.apply_remote_edit(id, *pos, insert, *delete),
            BlockDocOp::SetCollapsed { id, collapsed } => {
                self.apply_remote_collapsed(id, *collapsed)
            }
            BlockDocOp::MoveBlock {
                id,
                after,
                fugue_meta: _,
            } => self.apply_remote_move(id, after.as_ref()),
        }
    }

    /// Apply remote block insertion.
    fn apply_remote_insert(
        &mut self,
        id: BlockId,
        after: Option<BlockId>,
        snapshot: BlockContentSnapshot,
        author: String,
        created_at: u64,
    ) -> Result<()> {
        // Skip if we already have this block (idempotent)
        if self.blocks.contains_key(&id) {
            return Ok(());
        }

        // Find insertion index
        let insert_idx = match &after {
            Some(after_id) => self
                .block_order
                .iter()
                .position(|bid| bid == after_id)
                .map(|i| i + 1)
                .unwrap_or(self.block_order.len()),
            None => 0,
        };

        // Update Fugue
        let _ = self.ordering.inserted(insert_idx, 1);

        // Create block with content from remote agent and explicit timestamp
        let content = snapshot.into_content(&id.agent_id);
        let block = Block::with_timestamp(id.clone(), content, author, created_at);

        // Update state
        self.blocks.insert(id.clone(), block);
        self.block_order.insert(insert_idx, id);
        self.version += 1;

        // Update sequence counter if needed
        if let Some(seq) = self.blocks.keys().filter(|bid| bid.agent_id == self.agent_id).map(|bid| bid.seq).max() {
            self.next_seq = self.next_seq.max(seq + 1);
        }

        Ok(())
    }

    /// Apply remote block deletion.
    fn apply_remote_delete(&mut self, id: &BlockId) -> Result<()> {
        // Skip if block doesn't exist (idempotent)
        if !self.blocks.contains_key(id) {
            return Ok(());
        }

        // Find position
        if let Some(idx) = self.block_order.iter().position(|bid| bid == id) {
            let _ = self.ordering.deleted(idx..idx + 1);
            self.block_order.remove(idx);
        }

        self.blocks.remove(id);
        self.version += 1;

        Ok(())
    }

    /// Apply remote text edit.
    fn apply_remote_edit(
        &mut self,
        id: &BlockId,
        pos: usize,
        insert: &str,
        delete: usize,
    ) -> Result<()> {
        let block = match self.blocks.get_mut(id) {
            Some(b) => b,
            None => return Ok(()), // Block may have been deleted
        };

        // Skip if immutable
        if block.block_type().is_immutable() {
            return Ok(());
        }

        let crdt = match block.text_crdt_mut() {
            Some(c) => c,
            None => return Ok(()),
        };

        // Apply to CRDT using remote agent ID
        let agent = crdt.oplog.get_or_create_agent_id(&id.agent_id);

        // Bounds check with current length
        let len = crdt.branch.content().len_chars();
        let pos = pos.min(len);
        let delete = delete.min(len.saturating_sub(pos));

        if delete > 0 {
            crdt.delete_without_content(agent, pos..pos + delete);
        }
        if !insert.is_empty() {
            crdt.insert(agent, pos, insert);
        }

        self.version += 1;

        Ok(())
    }

    /// Apply remote collapsed state change.
    fn apply_remote_collapsed(&mut self, id: &BlockId, collapsed: bool) -> Result<()> {
        let block = match self.blocks.get_mut(id) {
            Some(b) => b,
            None => return Ok(()), // Block may have been deleted
        };

        block.content.set_collapsed(collapsed);
        self.version += 1;

        Ok(())
    }

    /// Apply remote move operation.
    fn apply_remote_move(&mut self, id: &BlockId, after: Option<&BlockId>) -> Result<()> {
        // Skip if block doesn't exist
        if !self.blocks.contains_key(id) {
            return Ok(());
        }

        // Find current position
        let current_idx = match self.block_order.iter().position(|bid| bid == id) {
            Some(idx) => idx,
            None => return Ok(()),
        };

        // Remove from current position
        let _ = self.ordering.deleted(current_idx..current_idx + 1);
        self.block_order.remove(current_idx);

        // Find new position
        let insert_idx = match after {
            Some(after_id) => self
                .block_order
                .iter()
                .position(|bid| bid == after_id)
                .map(|i| i + 1)
                .unwrap_or(self.block_order.len()),
            None => 0,
        };

        // Insert at new position
        let _ = self.ordering.inserted(insert_idx, 1);
        self.block_order.insert(insert_idx, id.clone());
        self.version += 1;

        Ok(())
    }

    // =========================================================================
    // Serialization
    // =========================================================================

    /// Create a snapshot of the entire document.
    pub fn snapshot(&self) -> DocumentSnapshot {
        DocumentSnapshot {
            cell_id: self.cell_id.clone(),
            blocks: self
                .block_order
                .iter()
                .filter_map(|id| {
                    self.blocks.get(id).map(|b| BlockSnapshot {
                        id: id.clone(),
                        content: b.snapshot(),
                        author: b.author.clone(),
                        created_at: b.created_at,
                    })
                })
                .collect(),
            version: self.version,
        }
    }

    /// Restore from a snapshot.
    pub fn from_snapshot(snapshot: DocumentSnapshot, agent_id: impl Into<String>) -> Self {
        let agent_id = agent_id.into();
        let mut doc = Self::new(&snapshot.cell_id, &agent_id);

        for block_snap in snapshot.blocks {
            let content = block_snap.content.into_content(&block_snap.id.agent_id);
            let block = Block::with_timestamp(
                block_snap.id.clone(),
                content,
                block_snap.author,
                block_snap.created_at,
            );

            doc.block_order.push(block_snap.id.clone());
            let _ = doc.ordering.inserted(doc.block_order.len() - 1, 1);
            doc.blocks.insert(block_snap.id, block);
        }

        doc.version = snapshot.version;

        // Find max seq for our agent
        if let Some(max_seq) = doc.blocks.keys().filter(|id| id.agent_id == agent_id).map(|id| id.seq).max() {
            doc.next_seq = max_seq + 1;
        }

        doc
    }
}

/// Snapshot of a block document (serializable).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DocumentSnapshot {
    /// Cell ID.
    pub cell_id: String,
    /// Blocks in order.
    pub blocks: Vec<BlockSnapshot>,
    /// Version.
    pub version: u64,
}

/// Snapshot of a single block.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BlockSnapshot {
    /// Block ID.
    pub id: BlockId,
    /// Content snapshot.
    pub content: BlockContentSnapshot,
    /// Author who created this block.
    pub author: String,
    /// Timestamp when block was created (Unix millis).
    pub created_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_document() {
        let doc = BlockDocument::new("cell-1", "alice");
        assert_eq!(doc.cell_id(), "cell-1");
        assert_eq!(doc.agent_id(), "alice");
        assert!(doc.is_empty());
        assert_eq!(doc.version(), 0);
    }

    #[test]
    fn test_insert_and_order() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        let id1 = doc.insert_text_block(None, "First").unwrap();
        let id2 = doc.insert_text_block(Some(&id1), "Second").unwrap();
        let id3 = doc.insert_text_block(Some(&id2), "Third").unwrap();

        let order: Vec<_> = doc.blocks_ordered().iter().map(|b| &b.id).collect();
        assert_eq!(order, vec![&id1, &id2, &id3]);
    }

    #[test]
    fn test_insert_at_beginning() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        let id1 = doc.insert_text_block(None, "First").unwrap();
        let id2 = doc.insert_text_block(None, "Before First").unwrap();

        let order: Vec<_> = doc.blocks_ordered().iter().map(|b| &b.id).collect();
        assert_eq!(order, vec![&id2, &id1]);
    }

    #[test]
    fn test_snapshot_roundtrip() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        doc.insert_thinking_block(None, "Thinking...").unwrap();
        doc.insert_text_block(None, "Response").unwrap();

        let snapshot = doc.snapshot();
        let restored = BlockDocument::from_snapshot(snapshot.clone(), "bob");

        assert_eq!(restored.block_count(), doc.block_count());
        assert_eq!(restored.full_text(), doc.full_text());
    }

    #[test]
    fn test_pending_ops() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        assert!(!doc.has_pending_ops());

        doc.insert_text_block(None, "Hello").unwrap();

        assert!(doc.has_pending_ops());

        let ops = doc.take_pending_ops();
        assert_eq!(ops.len(), 1);
        assert!(!doc.has_pending_ops());
    }
}
