//! Block tools for CRDT-native editing.
//!
//! This module provides the tool interface for blocks, bridging the
//! model-friendly line-based editing with the CRDT's character-based operations.
//!
//! # Tools
//!
//! | Tool | Purpose |
//! |------|---------|
//! | `block_create` | Create a new block with role, kind, content |
//! | `block_append` | Append text to a block (streaming-optimized) |
//! | `block_edit` | Line-based editing with atomic operations and CAS |
//! | `block_splice` | Character-based editing for programmatic tools |
//! | `block_read` | Read block content with line numbers and ranges |
//! | `block_search` | Search within a block using regex |
//! | `block_list` | List blocks with filters |
//! | `block_status` | Set block status |
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────┐
//! │           Model / Human / MCP           │
//! └────────────────────┬────────────────────┘
//!                      │ Tool calls
//!                      ▼
//! ┌─────────────────────────────────────────┐
//! │         Block Tool Engines              │
//! │   (ExecutionEngine implementations)     │
//! └────────────────────┬────────────────────┘
//!                      │ Line operations
//!                      ▼
//! ┌─────────────────────────────────────────┐
//! │         Translation Layer               │
//! │   (line ↔ byte offsets, CAS)            │
//! └────────────────────┬────────────────────┘
//!                      │ CRDT operations
//!                      ▼
//! ┌─────────────────────────────────────────┐
//! │         BlockStore (CRDT)               │
//! └─────────────────────────────────────────┘
//! ```

pub mod batch;
pub mod cursor;
pub mod engines;
pub mod error;
pub mod translate;

// Re-export engine types
pub use engines::{
    BlockAppendEngine, BlockCreateEngine, BlockEditEngine, BlockListEngine, BlockReadEngine,
    BlockSearchEngine, BlockSpliceEngine, BlockStatusEngine, KernelSearchEngine,
    // Parameter types
    BlockAppendParams, BlockCreateParams, BlockEditParams, BlockListParams, BlockReadParams,
    BlockSearchParams, BlockSpliceParams, BlockStatusParams, KernelSearchParams,
    EditOp, KernelSearchMatch,
};

// Re-export error types
pub use error::{EditError, Result};

// Re-export translation utilities
pub use translate::{
    content_with_line_numbers, extract_lines_with_numbers, line_count, line_end_byte_offset,
    line_range_to_byte_range, line_to_byte_offset, validate_expected_text,
};

// Re-export batching utilities
pub use batch::{AppendBatcher, BatchConfig, BatcherStats};

// Re-export cursor tracking
pub use cursor::{CursorEvent, CursorPosition, CursorTracker};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_module_exports() {
        // Ensure all exports are accessible
        let _ = std::any::type_name::<BlockEditEngine>();
        let _ = std::any::type_name::<EditError>();
    }
}
