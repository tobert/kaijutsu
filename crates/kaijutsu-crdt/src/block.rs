//! Block types and identifiers.
//!
//! Simplified version that works with the unified diamond-types CRDT.
//! Text CRDTs are now managed internally by diamond-types OpLog.

use serde::{Deserialize, Serialize};

/// Globally unique block identifier.
///
/// Composed of:
/// - `cell_id`: The cell this block belongs to
/// - `agent_id`: The agent that created this block
/// - `seq`: Agent-local sequence number (monotonically increasing)
///
/// This ensures global uniqueness without coordination.
#[derive(Clone, Eq, Hash, PartialEq, Debug, Serialize, Deserialize)]
pub struct BlockId {
    /// Cell this block belongs to.
    pub cell_id: String,
    /// Agent that created this block.
    pub agent_id: String,
    /// Agent-local sequence number.
    pub seq: u64,
}

impl BlockId {
    /// Create a new block ID.
    pub fn new(cell_id: impl Into<String>, agent_id: impl Into<String>, seq: u64) -> Self {
        Self {
            cell_id: cell_id.into(),
            agent_id: agent_id.into(),
            seq,
        }
    }

    /// Convert to a compact string representation.
    pub fn to_key(&self) -> String {
        format!("{}/{}/{}", self.cell_id, self.agent_id, self.seq)
    }

    /// Parse from key string.
    pub fn from_key(key: &str) -> Option<Self> {
        let parts: Vec<&str> = key.splitn(3, '/').collect();
        if parts.len() != 3 {
            return None;
        }
        Some(Self {
            cell_id: parts[0].to_string(),
            agent_id: parts[1].to_string(),
            seq: parts[2].parse().ok()?,
        })
    }
}

impl std::fmt::Display for BlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}#{}", self.cell_id, self.agent_id, self.seq)
    }
}

/// Type of block content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockType {
    /// Extended thinking/reasoning - collapsible.
    Thinking,
    /// Main text response.
    Text,
    /// Tool invocation - immutable.
    ToolUse,
    /// Tool result - immutable.
    ToolResult,
}

impl BlockType {
    /// Check if this block type has editable text.
    pub fn has_text_crdt(&self) -> bool {
        matches!(self, BlockType::Thinking | BlockType::Text)
    }

    /// Check if this block type is immutable after creation.
    pub fn is_immutable(&self) -> bool {
        matches!(self, BlockType::ToolUse | BlockType::ToolResult)
    }
}

/// Serializable snapshot of block content (no CRDT state).
///
/// Used for:
/// - Initial block creation
/// - Full state sync
/// - Wire protocol serialization
/// - Reading current block state
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum BlockContentSnapshot {
    /// Thinking block snapshot.
    Thinking { text: String, collapsed: bool },

    /// Text block snapshot.
    Text { text: String },

    /// Tool use snapshot.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    /// Tool result snapshot.
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

impl BlockContentSnapshot {
    /// Get the block type.
    pub fn block_type(&self) -> BlockType {
        match self {
            BlockContentSnapshot::Thinking { .. } => BlockType::Thinking,
            BlockContentSnapshot::Text { .. } => BlockType::Text,
            BlockContentSnapshot::ToolUse { .. } => BlockType::ToolUse,
            BlockContentSnapshot::ToolResult { .. } => BlockType::ToolResult,
        }
    }

    /// Get text content from snapshot.
    pub fn text(&self) -> &str {
        match self {
            BlockContentSnapshot::Thinking { text, .. } => text,
            BlockContentSnapshot::Text { text } => text,
            BlockContentSnapshot::ToolResult { content, .. } => content,
            BlockContentSnapshot::ToolUse { .. } => "",
        }
    }

    /// Check if this content is collapsed (only for Thinking blocks).
    pub fn is_collapsed(&self) -> bool {
        match self {
            BlockContentSnapshot::Thinking { collapsed, .. } => *collapsed,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_id() {
        let id = BlockId::new("cell-1", "alice", 42);
        assert_eq!(id.to_key(), "cell-1/alice/42");
        assert_eq!(BlockId::from_key("cell-1/alice/42"), Some(id.clone()));
        assert_eq!(id.to_string(), "cell-1@alice#42");
    }

    #[test]
    fn test_block_type() {
        assert!(BlockType::Thinking.has_text_crdt());
        assert!(BlockType::Text.has_text_crdt());
        assert!(!BlockType::ToolUse.has_text_crdt());
        assert!(!BlockType::ToolResult.has_text_crdt());

        assert!(!BlockType::Thinking.is_immutable());
        assert!(!BlockType::Text.is_immutable());
        assert!(BlockType::ToolUse.is_immutable());
        assert!(BlockType::ToolResult.is_immutable());
    }

    #[test]
    fn test_content_snapshot() {
        let thinking = BlockContentSnapshot::Thinking {
            text: "thinking...".to_string(),
            collapsed: true,
        };
        assert_eq!(thinking.block_type(), BlockType::Thinking);
        assert_eq!(thinking.text(), "thinking...");
        assert!(thinking.is_collapsed());

        let text = BlockContentSnapshot::Text {
            text: "hello".to_string(),
        };
        assert_eq!(text.block_type(), BlockType::Text);
        assert_eq!(text.text(), "hello");
        assert!(!text.is_collapsed());
    }
}
