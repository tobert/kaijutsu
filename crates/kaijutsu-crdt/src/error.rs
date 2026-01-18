//! Error types for CRDT operations.

use thiserror::Error;

use crate::BlockId;

/// Errors that can occur during CRDT operations.
#[derive(Error, Debug)]
pub enum CrdtError {
    /// Block not found in document.
    #[error("block not found: {0:?}")]
    BlockNotFound(BlockId),

    /// Attempted to edit an immutable block type.
    #[error("block {0:?} is immutable (ToolUse or ToolResult)")]
    ImmutableBlock(BlockId),

    /// Edit position out of bounds.
    #[error("edit position {pos} out of bounds for block with length {len}")]
    PositionOutOfBounds { pos: usize, len: usize },

    /// Invalid reference block for insertion.
    #[error("reference block not found: {0:?}")]
    InvalidReference(BlockId),

    /// Duplicate block ID.
    #[error("block already exists: {0:?}")]
    DuplicateBlock(BlockId),

    /// Agent ID mismatch.
    #[error("agent ID mismatch: expected {expected}, got {got}")]
    AgentMismatch { expected: String, got: String },

    /// Serialization error.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// Internal CRDT consistency error.
    #[error("internal CRDT error: {0}")]
    Internal(String),
}
