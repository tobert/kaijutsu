//! Block-based CRDT document model for Kaijutsu.
//!
//! Two-level CRDT architecture:
//! - **Block ordering**: Fugue (via `cola`) for concurrent insert/delete/move
//! - **Block content**: `diamond-types` ListCRDT for text within editable blocks
//!
//! # Design Philosophy
//!
//! Content is structured as blocks, not flat text. This enables:
//! - Structured streaming (thinking → text → tool_use as separate blocks)
//! - Collapsible UI elements (thinking blocks collapse when complete)
//! - Immutable blocks (ToolUse/ToolResult never edited after creation)
//! - Clean paragraph-level collaboration
//!
//! # Block Types
//!
//! - **Thinking**: Extended reasoning with text CRDT, collapsible
//! - **Text**: Main response text with text CRDT
//! - **ToolUse**: Immutable tool invocation
//! - **ToolResult**: Immutable tool result

mod block;
mod document;
mod error;
mod ops;

pub use block::{Block, BlockContent, BlockContentSnapshot, BlockId, BlockType};
pub use document::{BlockDocument, BlockSnapshot, DocumentSnapshot};
pub use error::CrdtError;
pub use ops::BlockDocOp;

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
    fn test_document_immutable_blocks() {
        let mut doc = BlockDocument::new("test-cell", "alice");

        // Tool use is immutable
        let tool_id = doc
            .insert_tool_use(None, "tool-123", "read_file", serde_json::json!({"path": "/test"}))
            .unwrap();

        // Editing tool use should fail
        let result = doc.edit_text(&tool_id, 0, "new text", 0);
        assert!(result.is_err());

        // Tool result is also immutable
        let result_id = doc
            .insert_tool_result(Some(&tool_id), "tool-123", "file contents", false)
            .unwrap();

        let result = doc.edit_text(&result_id, 0, "new text", 0);
        assert!(result.is_err());
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
    #[ignore = "Requires full Fugue state sharing between replicas - tracked for Phase 2"]
    fn test_concurrent_block_insertion() {
        // Simulate two agents inserting blocks concurrently
        // NOTE: Full convergence requires sharing Fugue replica state, not just ops.
        // Current implementation tracks block order locally and merges ops optimistically.
        let mut doc1 = BlockDocument::new("test-cell", "alice");
        let mut doc2 = BlockDocument::new("test-cell", "bob");

        // Both insert at the beginning
        let _alice_id = doc1.insert_text_block(None, "Alice's block").unwrap();
        let _bob_id = doc2.insert_text_block(None, "Bob's block").unwrap();

        // Get ops from each
        let alice_ops = doc1.take_pending_ops();
        let bob_ops = doc2.take_pending_ops();

        // Apply ops to each other's document
        for op in alice_ops {
            doc2.apply_remote_op(&op).unwrap();
        }
        for op in bob_ops {
            doc1.apply_remote_op(&op).unwrap();
        }

        // Both should have 2 blocks and converge to same order
        assert_eq!(doc1.block_count(), 2);
        assert_eq!(doc2.block_count(), 2);

        // CRDT guarantee: both documents converge to same order
        // The specific order depends on agent IDs and insertion context
        let doc1_order: Vec<_> = doc1.blocks_ordered().iter().map(|b| b.id.clone()).collect();
        let doc2_order: Vec<_> = doc2.blocks_ordered().iter().map(|b| b.id.clone()).collect();
        assert_eq!(doc1_order, doc2_order, "Documents must converge to same block order");

        // Full text must also match
        assert_eq!(doc1.full_text(), doc2.full_text());
    }

    #[test]
    #[ignore = "Requires diamond-types oplog sharing between replicas - tracked for Phase 2"]
    fn test_concurrent_text_editing() {
        // NOTE: Full text convergence requires merging diamond-types oplogs, not just
        // replaying insert/delete ops. Current implementation applies ops optimistically.
        let mut doc1 = BlockDocument::new("test-cell", "alice");
        let mut doc2 = BlockDocument::new("test-cell", "bob");

        // Alice creates a block
        let block_id = doc1.insert_text_block(None, "hello").unwrap();

        // Sync to bob
        let ops = doc1.take_pending_ops();
        for op in ops {
            doc2.apply_remote_op(&op).unwrap();
        }

        // Both edit concurrently at the same position
        doc1.edit_text(&block_id, 5, " alice", 0).unwrap(); // "hello alice"
        doc2.edit_text(&block_id, 5, " bob", 0).unwrap(); // "hello bob"

        // Exchange ops
        let alice_ops = doc1.take_pending_ops();
        let bob_ops = doc2.take_pending_ops();

        for op in alice_ops {
            doc2.apply_remote_op(&op).unwrap();
        }
        for op in bob_ops {
            doc1.apply_remote_op(&op).unwrap();
        }

        // CRDT guarantee: both documents converge to same text
        assert_eq!(
            doc1.full_text(),
            doc2.full_text(),
            "Documents must converge to same text"
        );

        // Should contain both additions (order determined by CRDT)
        let text = doc1.full_text();
        assert!(text.contains("alice"), "Text should contain 'alice'");
        assert!(text.contains("bob"), "Text should contain 'bob'");
        assert!(text.contains("hello"), "Text should contain 'hello'");
    }
}
