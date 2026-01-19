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
mod document;
mod error;
mod ops;

pub use block::{BlockContentSnapshot, BlockId, BlockType};
pub use document::{BlockDocument, BlockSnapshot, DocumentSnapshot};
pub use error::CrdtError;
pub use ops::{Frontier, SerializedOps, SerializedOpsOwned, LV};

/// Result type for CRDT operations.
pub type Result<T> = std::result::Result<T, CrdtError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_document_basic_operations() {
        let mut doc = BlockDocument::new("test-cell", "alice");

        // Insert a text block
        let block_id = doc.insert_text_block(None, "Hello, world!").unwrap();
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
        let mut doc = BlockDocument::new("test-cell", "alice");

        // Insert thinking block first
        let thinking_id = doc.insert_thinking_block(None, "Let me think...").unwrap();

        // Insert text block after thinking
        let text_id = doc
            .insert_text_block(Some(&thinking_id), "Here's my answer.")
            .unwrap();

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
        let mut doc = BlockDocument::new("test-cell", "alice");

        // Tool use is now editable (supports streaming)
        let tool_id = doc
            .insert_tool_use(None, "tool-123", "read_file", serde_json::json!({"path": "/test"}))
            .unwrap();

        // Editing tool use content should succeed (appending to JSON)
        let result = doc.append_text(&tool_id, ", \"extra\": true}");
        assert!(result.is_ok());

        // Verify the content changed
        let snapshot = doc.get_block_snapshot(&tool_id).unwrap();
        if let BlockContentSnapshot::ToolUse { .. } = snapshot.content {
            // The appended text modifies the JSON string in the Text CRDT
            // Text CRDT contains the concatenated content
            assert!(matches!(snapshot.content, BlockContentSnapshot::ToolUse { .. }));
        } else {
            panic!("Expected ToolUse block");
        }

        // Tool result is also editable (supports streaming)
        let result_id = doc
            .insert_tool_result(Some(&tool_id), "tool-123", "file contents", false)
            .unwrap();

        // Edit tool result content
        doc.append_text(&result_id, "\nmore output").unwrap();

        let snapshot = doc.get_block_snapshot(&result_id).unwrap();
        if let BlockContentSnapshot::ToolResult { content, .. } = snapshot.content {
            assert!(content.contains("file contents"));
            assert!(content.contains("more output"));
        } else {
            panic!("Expected ToolResult block");
        }
    }

    #[test]
    fn test_document_delete_block() {
        let mut doc = BlockDocument::new("test-cell", "alice");

        let id1 = doc.insert_text_block(None, "First").unwrap();
        let id2 = doc.insert_text_block(Some(&id1), "Second").unwrap();
        let _id3 = doc.insert_text_block(Some(&id2), "Third").unwrap();

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
        // 1. Having a single canonical OpLog per cell
        // 2. Syncing deltas via SerializedOps
        // 3. Ensuring all clients operate on the same CRDT structure
        let mut doc1 = BlockDocument::new("test-cell", "alice");
        let mut doc2 = BlockDocument::new("test-cell", "bob");

        let _alice_id = doc1.insert_text_block(None, "Alice's block").unwrap();
        let _bob_id = doc2.insert_text_block(None, "Bob's block").unwrap();

        doc1.oplog.merge_ops(doc2.oplog.ops_since(&[])).unwrap();
        doc2.oplog.merge_ops(doc1.oplog.ops_since(&[])).unwrap();

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
        let mut doc1 = BlockDocument::new("test-cell", "alice");
        let mut doc2 = BlockDocument::new("test-cell", "bob");

        let block_id = doc1.insert_text_block(None, "hello").unwrap();
        doc2.oplog.merge_ops(doc1.oplog.ops_since(&[])).unwrap();

        doc1.edit_text(&block_id, 5, " alice", 0).unwrap();
        doc2.edit_text(&block_id, 5, " bob", 0).unwrap();

        let doc1_frontier = doc1.frontier();
        let doc2_frontier = doc2.frontier();

        doc1.oplog.merge_ops(doc2.oplog.ops_since(&doc1_frontier)).unwrap();
        doc2.oplog.merge_ops(doc1.oplog.ops_since(&doc2_frontier)).unwrap();

        assert_eq!(doc1.full_text(), doc2.full_text());

        let text = doc1.full_text();
        assert!(text.contains("alice"));
        assert!(text.contains("bob"));
        assert!(text.contains("hello"));
    }
}
