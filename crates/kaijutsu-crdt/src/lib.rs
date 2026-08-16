//! Block-based CRDT document model for Kaijutsu.
//!
//! # Architecture
//!
//! `BlockStore` is the storage model: per-block DTE instances. Each block
//! owns its own diamond-types-extended Document for content. Metadata lives
//! in `BlockHeader` (plain data).
//!
//! The legacy `BlockDocument` model (a single shared DTE Document with all
//! blocks addressed as paths within it) was removed 2026-08-09 — see
//! `docs/crdt-position-2026-08.md` for the position review that found it
//! dead weight, with its only outside consumer down to a single test.
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

pub mod block_store;
pub(crate) mod content;
mod error;
mod ops;
pub mod selection;

// Re-export types from kaijutsu-types
pub use kaijutsu_types::{
    BlockFilter, BlockHeader, BlockId, BlockKind, BlockMetadata, BlockQuery, BlockSnapshot,
    BlockSnapshotBuilder, ContentType, ContextId, ConversationDAG, DriftKind, ErrorCategory,
    ErrorPayload, ErrorSeverity, ErrorSpan, KernelId, LogLevel, MAX_DAG_DEPTH, NotificationKind,
    NotificationPayload, OutputData, OutputEntryType, OutputNode, PrefixError, PrefixResolvable,
    PrincipalId, ResourcePayload, Role, SessionId, Span, Status, TaskStatus, Tick, TickDelta,
    ToolKind, resolve_context_prefix,
};

// New architecture
pub use block_store::{BlockStore, ForkBlockFilter, MergeStats, StoreSnapshot, SyncPayload};
pub use selection::{
    IntervalSet, RangeError, SelectionError, parse_range, resolve_keep_set, window_base,
};
pub use content::BlockContent;

pub use error::CrdtError;
pub use ops::{Frontier, LV, SerializedOps, SerializedOpsOwned};

/// Result type for CRDT operations.
pub type Result<T> = std::result::Result<T, CrdtError>;

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> BlockStore {
        BlockStore::new(ContextId::new(), PrincipalId::new())
    }

    #[test]
    fn test_store_basic_operations() {
        let mut store = test_store();

        let block_id = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Hello, world!",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        assert_eq!(store.full_text(), "Hello, world!");

        store.append_text(&block_id, " How are you?").unwrap();
        assert_eq!(store.full_text(), "Hello, world! How are you?");

        store.edit_text(&block_id, 7, "CRDT", 5).unwrap();
        assert_eq!(store.full_text(), "Hello, CRDT! How are you?");
    }

    #[test]
    fn test_store_delete_block() {
        let mut store = test_store();

        let id1 = store
            .insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                "First",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let id2 = store
            .insert_block(
                None,
                Some(&id1),
                Role::User,
                BlockKind::Text,
                "Second",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let _id3 = store
            .insert_block(
                None,
                Some(&id2),
                Role::User,
                BlockKind::Text,
                "Third",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();

        assert_eq!(store.block_count(), 3);

        store.delete_block(&id2).unwrap();

        assert_eq!(store.block_count(), 2);
        assert!(!store.full_text().contains("Second"));
    }
}
