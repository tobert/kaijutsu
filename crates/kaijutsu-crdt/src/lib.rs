//! Block-based CRDT document model for Kaijutsu.
//!
//! # Architecture
//!
//! Two storage models are available:
//!
//! - **`BlockStore`** (new): Per-block DTE instances. Each block owns its own
//!   diamond-types-extended Document for content. Metadata lives in `BlockHeader`
//!   (plain data). This is the target architecture.
//!
//! - **`BlockDocument`** (legacy): Single shared DTE Document with all blocks as
//!   paths within it. Still used by downstream crates during migration.
//!
//! # Block Types
//!
//! All block types support Text CRDT for their primary content, enabling streaming:
//!
//! - **Text**: Main response text
//! - **Thinking**: Extended reasoning, collapsible
//! - **ToolCall**: Tool invocation with streamable JSON input
//! - **ToolResult**: Tool result with streamable content
//! - **Drift**: Cross-context content transfer
//! - **File**: File content tracked in a context

mod block;
mod block_store;
pub(crate) mod content;
mod dag;
mod document;
mod error;
pub mod ids;
mod ops;

// Re-export types from kaijutsu-types
pub use block::{
    BlockId, BlockKind, BlockSnapshot, BlockSnapshotBuilder, BlockHeader,
    DriftKind, MAX_DAG_DEPTH, Role, Status, ToolKind,
};

// New architecture
pub use block_store::{BlockStore, StoreSnapshot, SyncPayload};
pub use content::BlockContent;
pub use dag::ConversationDAG;

// Legacy (still used by downstream crates)
pub use document::{BlockDocument, DocumentSnapshot};

pub use error::CrdtError;
pub use ids::{
    ContextId, KernelId, PrefixError, PrefixResolvable, PrincipalId, SessionId,
    resolve_context_prefix,
};
pub use ops::{Frontier, SerializedOps, SerializedOpsOwned, LV};

/// Result type for CRDT operations.
pub type Result<T> = std::result::Result<T, CrdtError>;

#[cfg(test)]
mod tests {
    use super::*;

    fn test_doc() -> BlockDocument {
        BlockDocument::new(ContextId::new(), PrincipalId::new())
    }

    fn test_store() -> BlockStore {
        BlockStore::new(ContextId::new(), PrincipalId::new())
    }

    // ── Legacy BlockDocument tests ──────────────────────────────────────

    #[test]
    fn test_document_basic_operations() {
        let mut doc = test_doc();

        let block_id = doc.insert_block(None, None, Role::User, BlockKind::Text, "Hello, world!").unwrap();
        assert_eq!(doc.full_text(), "Hello, world!");

        doc.append_text(&block_id, " How are you?").unwrap();
        assert_eq!(doc.full_text(), "Hello, world! How are you?");

        doc.edit_text(&block_id, 7, "CRDT", 5).unwrap();
        assert_eq!(doc.full_text(), "Hello, CRDT! How are you?");
    }

    #[test]
    fn test_document_multiple_blocks() {
        let mut doc = test_doc();

        let thinking_id = doc.insert_block(None, None, Role::Model, BlockKind::Thinking, "Let me think...").unwrap();
        let text_id = doc.insert_block(None, Some(&thinking_id), Role::Model, BlockKind::Text, "Here's my answer.").unwrap();

        let text = doc.full_text();
        assert!(text.contains("Let me think..."));
        assert!(text.contains("Here's my answer."));

        doc.set_collapsed(&thinking_id, true).unwrap();

        let blocks = doc.blocks_ordered();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].id, thinking_id);
        assert_eq!(blocks[1].id, text_id);
    }

    #[test]
    fn test_tool_blocks_are_editable() {
        let mut doc = test_doc();

        let tool_id = doc.insert_tool_call(None, None, "read_file", serde_json::json!({"path": "/test"})).unwrap();

        let result = doc.append_text(&tool_id, ", \"extra\": true}");
        assert!(result.is_ok());

        let snapshot = doc.get_block_snapshot(&tool_id).unwrap();
        assert_eq!(snapshot.kind, BlockKind::ToolCall);
        assert_eq!(snapshot.tool_name, Some("read_file".to_string()));

        let result_id = doc.insert_tool_result_block(&tool_id, Some(&tool_id), "file contents", false, None).unwrap();

        doc.append_text(&result_id, "\nmore output").unwrap();

        let snapshot = doc.get_block_snapshot(&result_id).unwrap();
        assert_eq!(snapshot.kind, BlockKind::ToolResult);
        assert!(snapshot.content.contains("file contents"));
        assert!(snapshot.content.contains("more output"));
    }

    #[test]
    fn test_document_delete_block() {
        let mut doc = test_doc();

        let id1 = doc.insert_block(None, None, Role::User, BlockKind::Text, "First").unwrap();
        let id2 = doc.insert_block(None, Some(&id1), Role::User, BlockKind::Text, "Second").unwrap();
        let _id3 = doc.insert_block(None, Some(&id2), Role::User, BlockKind::Text, "Third").unwrap();

        assert_eq!(doc.block_count(), 3);

        doc.delete_block(&id2).unwrap();

        assert_eq!(doc.block_count(), 2);
        assert!(!doc.full_text().contains("Second"));
    }

    #[test]
    fn test_concurrent_block_insertion() {
        let ctx = ContextId::new();
        let mut doc1 = BlockDocument::new(ctx, PrincipalId::new());
        let mut doc2 = BlockDocument::new(ctx, PrincipalId::new());

        doc2.merge_ops_owned(doc1.ops_since(&Frontier::root())).unwrap();

        let _alice_id = doc1.insert_block(None, None, Role::User, BlockKind::Text, "Alice's block").unwrap();
        let _bob_id = doc2.insert_block(None, None, Role::User, BlockKind::Text, "Bob's block").unwrap();

        let doc1_frontier = doc1.frontier();
        let doc2_frontier = doc2.frontier();

        let r1 = doc1.merge_ops_owned(doc2.ops_since(&doc1_frontier));
        let r2 = doc2.merge_ops_owned(doc1.ops_since(&doc2_frontier));

        if r1.is_ok() && r2.is_ok() {
            assert_eq!(doc1.block_count(), 2);
            assert_eq!(doc2.block_count(), 2);
        }
    }

    #[test]
    fn test_concurrent_text_editing() {
        let ctx = ContextId::new();
        let mut doc1 = BlockDocument::new(ctx, PrincipalId::new());
        let mut doc2 = BlockDocument::new(ctx, PrincipalId::new());

        let block_id = doc1.insert_block(None, None, Role::User, BlockKind::Text, "hello").unwrap();
        doc2.merge_ops_owned(doc1.ops_since(&Frontier::root())).unwrap();

        doc1.edit_text(&block_id, 5, " alice", 0).unwrap();
        doc2.edit_text(&block_id, 5, " bob", 0).unwrap();

        let doc1_frontier = doc1.frontier();
        let doc2_frontier = doc2.frontier();

        let r1 = doc1.merge_ops_owned(doc2.ops_since(&doc1_frontier));
        let r2 = doc2.merge_ops_owned(doc1.ops_since(&doc2_frontier));

        if r1.is_ok() && r2.is_ok() {
            assert_eq!(doc1.full_text(), doc2.full_text());
            let text = doc1.full_text();
            assert!(text.contains("alice"));
            assert!(text.contains("bob"));
            assert!(text.contains("hello"));
        }
    }

    // ── New BlockStore tests ────────────────────────────────────────────

    #[test]
    fn test_store_basic_operations() {
        let mut store = test_store();

        let block_id = store.insert_block(None, None, Role::User, BlockKind::Text, "Hello, world!").unwrap();
        assert_eq!(store.full_text(), "Hello, world!");

        store.append_text(&block_id, " How are you?").unwrap();
        assert_eq!(store.full_text(), "Hello, world! How are you?");

        store.edit_text(&block_id, 7, "CRDT", 5).unwrap();
        assert_eq!(store.full_text(), "Hello, CRDT! How are you?");
    }

    #[test]
    fn test_store_delete_block() {
        let mut store = test_store();

        let id1 = store.insert_block(None, None, Role::User, BlockKind::Text, "First").unwrap();
        let id2 = store.insert_block(None, Some(&id1), Role::User, BlockKind::Text, "Second").unwrap();
        let _id3 = store.insert_block(None, Some(&id2), Role::User, BlockKind::Text, "Third").unwrap();

        assert_eq!(store.block_count(), 3);

        store.delete_block(&id2).unwrap();

        assert_eq!(store.block_count(), 2);
        assert!(!store.full_text().contains("Second"));
    }
}
