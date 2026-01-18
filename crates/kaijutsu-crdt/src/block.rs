//! Block types and identifiers.

use diamond_types::list::ListCRDT;
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

/// Block content - either with CRDT text or immutable data.
pub enum BlockContent {
    /// Extended thinking with CRDT text.
    Thinking {
        /// Text CRDT for concurrent editing.
        crdt: ListCRDT,
        /// Whether the block is collapsed in UI.
        collapsed: bool,
    },

    /// Main text response with CRDT.
    Text {
        /// Text CRDT for concurrent editing.
        crdt: ListCRDT,
    },

    /// Tool invocation - immutable after creation.
    ToolUse {
        /// Unique tool use ID.
        id: String,
        /// Tool name.
        name: String,
        /// Tool input as JSON.
        input: serde_json::Value,
    },

    /// Tool result - immutable after creation.
    ToolResult {
        /// ID of the tool_use this is a result for.
        tool_use_id: String,
        /// Result content.
        content: String,
        /// Whether this result represents an error.
        is_error: bool,
    },
}

impl BlockContent {
    /// Get the block type.
    pub fn block_type(&self) -> BlockType {
        match self {
            BlockContent::Thinking { .. } => BlockType::Thinking,
            BlockContent::Text { .. } => BlockType::Text,
            BlockContent::ToolUse { .. } => BlockType::ToolUse,
            BlockContent::ToolResult { .. } => BlockType::ToolResult,
        }
    }

    /// Get the text content of this block.
    pub fn text(&self) -> String {
        match self {
            BlockContent::Thinking { crdt, .. } => crdt.branch.content().to_string(),
            BlockContent::Text { crdt } => crdt.branch.content().to_string(),
            BlockContent::ToolResult { content, .. } => content.clone(),
            BlockContent::ToolUse { name, input, .. } => {
                format!("{}({})", name, input)
            }
        }
    }

    /// Get mutable reference to text CRDT if this block has one.
    pub fn text_crdt_mut(&mut self) -> Option<&mut ListCRDT> {
        match self {
            BlockContent::Thinking { crdt, .. } => Some(crdt),
            BlockContent::Text { crdt } => Some(crdt),
            _ => None,
        }
    }

    /// Get reference to text CRDT if this block has one.
    pub fn text_crdt(&self) -> Option<&ListCRDT> {
        match self {
            BlockContent::Thinking { crdt, .. } => Some(crdt),
            BlockContent::Text { crdt } => Some(crdt),
            _ => None,
        }
    }

    /// Check if this content is collapsed (only for Thinking blocks).
    pub fn is_collapsed(&self) -> bool {
        match self {
            BlockContent::Thinking { collapsed, .. } => *collapsed,
            _ => false,
        }
    }

    /// Set collapsed state (only for Thinking blocks).
    pub fn set_collapsed(&mut self, value: bool) -> bool {
        if let BlockContent::Thinking { collapsed, .. } = self {
            *collapsed = value;
            true
        } else {
            false
        }
    }

    /// Create a snapshot of this content (serializable, no CRDT state).
    pub fn snapshot(&self) -> BlockContentSnapshot {
        match self {
            BlockContent::Thinking { crdt, collapsed } => BlockContentSnapshot::Thinking {
                text: crdt.branch.content().to_string(),
                collapsed: *collapsed,
            },
            BlockContent::Text { crdt } => BlockContentSnapshot::Text {
                text: crdt.branch.content().to_string(),
            },
            BlockContent::ToolUse { id, name, input } => BlockContentSnapshot::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            },
            BlockContent::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => BlockContentSnapshot::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            },
        }
    }
}

/// Serializable snapshot of block content (no CRDT state).
///
/// Used for:
/// - Initial block creation
/// - Full state sync
/// - Wire protocol serialization
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

    /// Convert snapshot to full content with CRDT.
    pub fn into_content(self, agent_id: &str) -> BlockContent {
        match self {
            BlockContentSnapshot::Thinking { text, collapsed } => {
                let mut crdt = ListCRDT::new();
                if !text.is_empty() {
                    let agent = crdt.oplog.get_or_create_agent_id(agent_id);
                    crdt.insert(agent, 0, &text);
                }
                BlockContent::Thinking { crdt, collapsed }
            }
            BlockContentSnapshot::Text { text } => {
                let mut crdt = ListCRDT::new();
                if !text.is_empty() {
                    let agent = crdt.oplog.get_or_create_agent_id(agent_id);
                    crdt.insert(agent, 0, &text);
                }
                BlockContent::Text { crdt }
            }
            BlockContentSnapshot::ToolUse { id, name, input } => {
                BlockContent::ToolUse { id, name, input }
            }
            BlockContentSnapshot::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => BlockContent::ToolResult {
                tool_use_id,
                content,
                is_error,
            },
        }
    }
}

/// A block in the document.
pub struct Block {
    /// Globally unique identifier.
    pub id: BlockId,
    /// Block content.
    pub content: BlockContent,
    /// Author who created this block (participant id like "user:amy" or "model:claude").
    pub author: String,
    /// Timestamp when block was created (Unix millis).
    pub created_at: u64,
}

impl Block {
    /// Create a new block with author and timestamp.
    pub fn new(id: BlockId, content: BlockContent, author: impl Into<String>) -> Self {
        Self {
            id,
            content,
            author: author.into(),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }
    }

    /// Create a new block with explicit timestamp (for restoring from snapshot).
    pub fn with_timestamp(
        id: BlockId,
        content: BlockContent,
        author: impl Into<String>,
        created_at: u64,
    ) -> Self {
        Self {
            id,
            content,
            author: author.into(),
            created_at,
        }
    }

    /// Get the block type.
    pub fn block_type(&self) -> BlockType {
        self.content.block_type()
    }

    /// Get text content.
    pub fn text(&self) -> String {
        self.content.text()
    }

    /// Get mutable reference to text CRDT.
    pub fn text_crdt_mut(&mut self) -> Option<&mut ListCRDT> {
        self.content.text_crdt_mut()
    }

    /// Get reference to text CRDT.
    pub fn text_crdt(&self) -> Option<&ListCRDT> {
        self.content.text_crdt()
    }

    /// Create a snapshot of this block.
    pub fn snapshot(&self) -> BlockContentSnapshot {
        self.content.snapshot()
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
    fn test_block_content_text() {
        let mut content = BlockContent::Text {
            crdt: ListCRDT::new(),
        };

        if let Some(crdt) = content.text_crdt_mut() {
            let agent = crdt.oplog.get_or_create_agent_id("alice");
            crdt.insert(agent, 0, "hello");
        }

        assert_eq!(content.text(), "hello");
        assert!(!content.is_collapsed());
    }

    #[test]
    fn test_block_content_thinking_collapsed() {
        let mut content = BlockContent::Thinking {
            crdt: ListCRDT::new(),
            collapsed: false,
        };

        assert!(!content.is_collapsed());
        content.set_collapsed(true);
        assert!(content.is_collapsed());
    }

    #[test]
    fn test_snapshot_roundtrip() {
        let snapshot = BlockContentSnapshot::Text {
            text: "hello world".to_string(),
        };

        let content = snapshot.clone().into_content("alice");
        assert_eq!(content.text(), "hello world");

        let snapshot2 = content.snapshot();
        match (snapshot, snapshot2) {
            (BlockContentSnapshot::Text { text: t1 }, BlockContentSnapshot::Text { text: t2 }) => {
                assert_eq!(t1, t2);
            }
            _ => panic!("type mismatch"),
        }
    }
}
