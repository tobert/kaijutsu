//! Error types for `BlockDocument` operations.

use thiserror::Error;

use kaijutsu_types::BlockId;

/// Errors that can occur during `BlockDocument` operations.
#[derive(Error, Debug)]
pub enum BlockDocumentError {
    /// Block not found in document.
    #[error("block not found: {0:?}")]
    BlockNotFound(BlockId),

    /// Operation not supported on this block type.
    ///
    /// For example, collapse is only supported on Thinking blocks.
    #[error("operation not supported on block {0:?}")]
    UnsupportedOperation(BlockId),

    /// Edit position out of bounds.
    #[error("edit position {pos} out of bounds for block with length {len}")]
    PositionOutOfBounds { pos: usize, len: usize },

    /// Invalid reference block for insertion.
    #[error("reference block not found: {0:?}")]
    InvalidReference(BlockId),

    /// Duplicate block ID.
    #[error("block already exists: {0:?}")]
    DuplicateBlock(BlockId),

    /// Serialization error.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// Internal block-document consistency error.
    #[error("internal block-document error: {0}")]
    Internal(String),

    /// Schema corruption detected (missing required fields).
    #[error("schema corruption: {0}")]
    SchemaCorruption(String),
}
