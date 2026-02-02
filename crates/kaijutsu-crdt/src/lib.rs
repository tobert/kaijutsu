//! Block-based CRDT document model for Kaijutsu.
//!
//! Uses the unified diamond-types fork with Map, Set, Register, and Text CRDTs.
//! All CRDT operations go through a single OpLog for guaranteed convergence.
//!
//! # Design Philosophy
//!
//! Content is structured as blocks, not flat text. This enables:
//! - Structured streaming (thinking → text → tool_use as separate blocks)
//! - Collapsible UI elements (thinking blocks collapse when complete)
//! - Streamable tool blocks (ToolUse/ToolResult content via Text CRDT)
//! - Clean paragraph-level collaboration
//!
//! # Block Types
//!
//! All block types support Text CRDT for their primary content, enabling streaming:
//!
//! - **Thinking**: Extended reasoning with text CRDT, collapsible
//! - **Text**: Main response text with text CRDT
//! - **ToolUse**: Tool invocation with streamable JSON input
//! - **ToolResult**: Tool result with streamable content
//!
//! # CRDT Semantics
//!
//! - **Maps**: Last-Write-Wins (LWW) - most recent write wins by Lamport timestamp
//! - **Sets**: OR-Set (add-wins) - concurrent add and remove both succeed, add wins
//! - **Text**: Sequence CRDT - character-level merging with proper interleaving

mod block;
mod dag;
mod document;
mod error;
mod ops;

pub use block::{BlockId, BlockKind, BlockSnapshot, Role, Status};
pub use dag::ConversationDAG;
pub use document::{BlockDocument, DocumentSnapshot};
pub use error::CrdtError;
pub use ops::{Frontier, SerializedOps, SerializedOpsOwned, LV};

/// Result type for CRDT operations.
pub type Result<T> = std::result::Result<T, CrdtError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_document_basic_operations() {
        let mut doc = BlockDocument::new("test-doc", "alice");

        // Insert a text block using new API
        let block_id = doc.insert_block(None, None, Role::User, BlockKind::Text, "Hello, world!", "alice").unwrap();
        assert_eq!(doc.full_text(), "Hello, world!");

        // Append to the text block
        doc.append_text(&block_id, " How are you?").unwrap();
        assert_eq!(doc.full_text(), "Hello, world! How are you?");

        // Edit within the block
        doc.edit_text(&block_id, 7, "CRDT", 5).unwrap(); // Replace "world" with "CRDT"
        assert_eq!(doc.full_text(), "Hello, CRDT! How are you?");
    }

    #[test]
    fn test_document_multiple_blocks() {
        let mut doc = BlockDocument::new("test-doc", "alice");

        // Insert thinking block first
        let thinking_id = doc.insert_block(None, None, Role::Model, BlockKind::Thinking, "Let me think...", "alice").unwrap();

        // Insert text block after thinking
        let text_id = doc.insert_block(None, Some(&thinking_id), Role::Model, BlockKind::Text, "Here's my answer.", "alice").unwrap();

        // Full text concatenates blocks
        let text = doc.full_text();
        assert!(text.contains("Let me think..."));
        assert!(text.contains("Here's my answer."));

        // Collapse thinking
        doc.set_collapsed(&thinking_id, true).unwrap();

        // Verify blocks are in order
        let blocks = doc.blocks_ordered();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].id, thinking_id);
        assert_eq!(blocks[1].id, text_id);
    }

    #[test]
    fn test_tool_blocks_are_editable() {
        let mut doc = BlockDocument::new("test-doc", "alice");

        // Tool call using new API
        let tool_id = doc.insert_tool_call(None, None, "read_file", serde_json::json!({"path": "/test"}), "alice").unwrap();

        // Editing tool use content should succeed (appending to JSON)
        let result = doc.append_text(&tool_id, ", \"extra\": true}");
        assert!(result.is_ok());

        // Verify the content changed
        let snapshot = doc.get_block_snapshot(&tool_id).unwrap();
        assert_eq!(snapshot.kind, BlockKind::ToolCall);
        assert_eq!(snapshot.tool_name, Some("read_file".to_string()));

        // Tool result using new API
        let result_id = doc.insert_tool_result_block(&tool_id, Some(&tool_id), "file contents", false, None, "system").unwrap();

        // Edit tool result content
        doc.append_text(&result_id, "\nmore output").unwrap();

        let snapshot = doc.get_block_snapshot(&result_id).unwrap();
        assert_eq!(snapshot.kind, BlockKind::ToolResult);
        assert!(snapshot.content.contains("file contents"));
        assert!(snapshot.content.contains("more output"));
    }

    #[test]
    fn test_document_delete_block() {
        let mut doc = BlockDocument::new("test-doc", "alice");

        let id1 = doc.insert_block(None, None, Role::User, BlockKind::Text, "First", "alice").unwrap();
        let id2 = doc.insert_block(None, Some(&id1), Role::User, BlockKind::Text, "Second", "alice").unwrap();
        let _id3 = doc.insert_block(None, Some(&id2), Role::User, BlockKind::Text, "Third", "alice").unwrap();

        assert_eq!(doc.block_count(), 3);

        doc.delete_block(&id2).unwrap();

        assert_eq!(doc.block_count(), 2);
        assert!(!doc.full_text().contains("Second"));
    }

    #[test]
    #[ignore = "Multi-client sync requires shared initial state - handled by BlockStore in Phase 2"]
    fn test_concurrent_block_insertion() {
        // NOTE: This test requires documents to share a common initial OpLog state.
        // Independent documents create different CRDT IDs for their "blocks" Sets.
        // The BlockStore in Phase 2 will handle proper multi-client sync by:
        // 1. Having a single canonical OpLog per document
        // 2. Syncing deltas via SerializedOps
        // 3. Ensuring all clients operate on the same CRDT structure
        let mut doc1 = BlockDocument::new("test-doc", "alice");
        let mut doc2 = BlockDocument::new("test-doc", "bob");

        let _alice_id = doc1.insert_block(None, None, Role::User, BlockKind::Text, "Alice's block", "alice").unwrap();
        let _bob_id = doc2.insert_block(None, None, Role::User, BlockKind::Text, "Bob's block", "bob").unwrap();

        doc1.merge_ops_owned(doc2.ops_since(&[])).unwrap();
        doc2.merge_ops_owned(doc1.ops_since(&[])).unwrap();

        assert_eq!(doc1.block_count(), 2);
        assert_eq!(doc2.block_count(), 2);

        let doc1_order: Vec<_> = doc1.blocks_ordered().iter().map(|b| b.id.clone()).collect();
        let doc2_order: Vec<_> = doc2.blocks_ordered().iter().map(|b| b.id.clone()).collect();
        assert_eq!(doc1_order, doc2_order);
    }

    #[test]
    #[ignore = "Multi-client sync requires shared initial state - handled by BlockStore in Phase 2"]
    fn test_concurrent_text_editing() {
        // NOTE: Same issue as above - independent documents have different CRDT structures.
        // For proper convergence, clients must start from the same OpLog state.
        let mut doc1 = BlockDocument::new("test-doc", "alice");
        let mut doc2 = BlockDocument::new("test-doc", "bob");

        let block_id = doc1.insert_block(None, None, Role::User, BlockKind::Text, "hello", "alice").unwrap();
        doc2.merge_ops_owned(doc1.ops_since(&[])).unwrap();

        doc1.edit_text(&block_id, 5, " alice", 0).unwrap();
        doc2.edit_text(&block_id, 5, " bob", 0).unwrap();

        let doc1_frontier = doc1.frontier();
        let doc2_frontier = doc2.frontier();

        doc1.merge_ops_owned(doc2.ops_since(&doc1_frontier)).unwrap();
        doc2.merge_ops_owned(doc1.ops_since(&doc2_frontier)).unwrap();

        assert_eq!(doc1.full_text(), doc2.full_text());

        let text = doc1.full_text();
        assert!(text.contains("alice"));
        assert!(text.contains("bob"));
        assert!(text.contains("hello"));
    }
}
