//! Block document operations.
//!
//! All mutations to a BlockDocument are expressed as operations.
//! Operations are:
//! - Serializable for network transmission
//! - Composable for CRDT merge
//! - Sufficient for replay/undo

use serde::{Deserialize, Serialize};

use crate::{BlockContentSnapshot, BlockId};

/// Operations on block documents.
///
/// These operations are designed to be:
/// - **Deterministic**: Same ops applied in any order yield same result (with CRDT)
/// - **Serializable**: Can be sent over the wire
/// - **Reversible**: For undo support (future)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum BlockDocOp {
    /// Insert a new block after a reference block.
    ///
    /// If `after` is None, insert at the beginning of the document.
    InsertBlock {
        /// ID of the new block.
        id: BlockId,
        /// Block to insert after (None = start of document).
        after: Option<BlockId>,
        /// Initial content.
        content: BlockContentSnapshot,
        /// Author who created this block (participant id like "user:amy" or "model:claude").
        author: String,
        /// Timestamp when block was created (Unix millis).
        created_at: u64,
        /// Fugue insertion metadata for ordering.
        #[serde(default)]
        fugue_meta: Option<FugueMeta>,
    },

    /// Delete a block.
    DeleteBlock {
        /// ID of the block to delete.
        id: BlockId,
    },

    /// Edit text within a block.
    ///
    /// Only valid for Thinking and Text blocks.
    /// Operations are applied at the CRDT level for merge.
    EditBlockText {
        /// Block to edit.
        id: BlockId,
        /// Position in the text (char offset).
        pos: usize,
        /// Text to insert at position.
        insert: String,
        /// Number of chars to delete starting at position.
        delete: usize,
        /// Diamond-types encoded operation for merge.
        #[serde(default)]
        dt_encoded: Option<Vec<u8>>,
    },

    /// Toggle collapsed state of a block.
    ///
    /// Only meaningful for Thinking blocks.
    SetCollapsed {
        /// Block ID.
        id: BlockId,
        /// New collapsed state.
        collapsed: bool,
    },

    /// Move a block to a new position.
    ///
    /// In Fugue, this is implemented as delete + insert.
    MoveBlock {
        /// Block to move.
        id: BlockId,
        /// New position (after this block, None = start).
        after: Option<BlockId>,
        /// Fugue metadata for the new position.
        #[serde(default)]
        fugue_meta: Option<FugueMeta>,
    },
}

impl BlockDocOp {
    /// Get the block ID this operation targets.
    pub fn target_block(&self) -> &BlockId {
        match self {
            BlockDocOp::InsertBlock { id, .. } => id,
            BlockDocOp::DeleteBlock { id } => id,
            BlockDocOp::EditBlockText { id, .. } => id,
            BlockDocOp::SetCollapsed { id, .. } => id,
            BlockDocOp::MoveBlock { id, .. } => id,
        }
    }

    /// Check if this is a structural operation (affects block ordering).
    pub fn is_structural(&self) -> bool {
        matches!(
            self,
            BlockDocOp::InsertBlock { .. }
                | BlockDocOp::DeleteBlock { .. }
                | BlockDocOp::MoveBlock { .. }
        )
    }

    /// Check if this is a text edit operation.
    pub fn is_text_edit(&self) -> bool {
        matches!(self, BlockDocOp::EditBlockText { .. })
    }
}

/// Fugue metadata for block ordering.
///
/// This captures the causal context needed for Fugue's
/// interleaving-free concurrent insertion.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FugueMeta {
    /// Origin agent ID.
    pub origin: String,
    /// Left origin (for Fugue's leftOrigin).
    pub left_origin: Option<String>,
    /// Right origin (for Fugue's rightOrigin).
    pub right_origin: Option<String>,
    /// Sequence number for tie-breaking.
    pub seq: u64,
}

/// Batch of operations for atomic application.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpBatch {
    /// Cell ID these ops belong to.
    pub cell_id: String,
    /// Agent that created these ops.
    pub agent_id: String,
    /// The operations.
    pub ops: Vec<BlockDocOp>,
    /// Version after applying these ops.
    pub version: u64,
}

#[allow(dead_code)]
impl OpBatch {
    /// Create a new batch.
    pub fn new(cell_id: impl Into<String>, agent_id: impl Into<String>) -> Self {
        Self {
            cell_id: cell_id.into(),
            agent_id: agent_id.into(),
            ops: Vec::new(),
            version: 0,
        }
    }

    /// Add an operation to the batch.
    pub fn push(&mut self, op: BlockDocOp) {
        self.ops.push(op);
    }

    /// Check if the batch is empty.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Number of operations.
    pub fn len(&self) -> usize {
        self.ops.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BlockContentSnapshot;

    #[test]
    fn test_op_target_block() {
        let id = BlockId::new("cell", "agent", 1);

        let insert = BlockDocOp::InsertBlock {
            id: id.clone(),
            after: None,
            content: BlockContentSnapshot::Text {
                text: "".to_string(),
            },
            author: "user:alice".to_string(),
            created_at: 1234567890,
            fugue_meta: None,
        };
        assert_eq!(insert.target_block(), &id);

        let edit = BlockDocOp::EditBlockText {
            id: id.clone(),
            pos: 0,
            insert: "hello".to_string(),
            delete: 0,
            dt_encoded: None,
        };
        assert_eq!(edit.target_block(), &id);
    }

    #[test]
    fn test_op_categories() {
        let id = BlockId::new("cell", "agent", 1);

        let insert = BlockDocOp::InsertBlock {
            id: id.clone(),
            after: None,
            content: BlockContentSnapshot::Text {
                text: "".to_string(),
            },
            author: "user:alice".to_string(),
            created_at: 1234567890,
            fugue_meta: None,
        };
        assert!(insert.is_structural());
        assert!(!insert.is_text_edit());

        let edit = BlockDocOp::EditBlockText {
            id: id.clone(),
            pos: 0,
            insert: "hello".to_string(),
            delete: 0,
            dt_encoded: None,
        };
        assert!(!edit.is_structural());
        assert!(edit.is_text_edit());
    }
}
