//! Block/document model — one context's blocks, their content, order keys,
//! and tombstones.
//!
//! `BlockDocument` is the storage model: each block owns its content as a
//! plain `String`. Metadata lives in `BlockHeader` (plain data).
//!
//! Text used to run through diamond-types-extended, a character-level CRDT —
//! retired 2026-08-16 because no block is ever concurrently edited (the
//! kernel is the sole sequencer for every mutation; see CLAUDE.md "Durable
//! state and the wire") and the CRDT never earned its keep past what it
//! materialized down to anyway. See `docs/crdt-position-2026-08.md`.
//!
//! This module used to be a standalone crate. It moved in-tree 2026-08-16:
//! once the CRDT was gone, the crate held no dependency the kernel didn't
//! already have, and its exported abstractions (blocks, forks, selection)
//! are meaningful only over the wire the kernel serves — not at a
//! `Cargo.toml` boundary. See CLAUDE.md "Durable state and the wire" and
//! `docs/crdt-position-2026-08.md`.
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
pub mod selection;

pub use block_store::{BlockDocument, ForkBlockFilter, StoreSnapshot, SyncPayload, TextEdit};
pub use content::BlockContent;
pub use error::BlockDocumentError;
pub use selection::{
    IntervalSet, RangeError, SelectionError, parse_range, resolve_keep_set, window_base,
};

/// Result type for block-document operations.
pub type Result<T> = std::result::Result<T, BlockDocumentError>;

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockKind, ContentType, ContextId, PrincipalId, Role, Status};

    fn test_store() -> BlockDocument {
        BlockDocument::new(ContextId::new(), PrincipalId::new())
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
