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
//! │   (inherent methods invoked by the      │
//! │    mcp::servers::BlockToolsServer)      │
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

pub mod error;
pub mod translate;

// Re-export error types
pub use error::{EditError, Result};

// Re-export translation utilities. The `*_char_*` twins are the ones to feed
// `edit_text`/`edit_text_as` (the CRDT text layer is char-indexed); the byte
// variants are for byte-oriented consumers (string slicing, replace_range).
pub use translate::{
    byte_to_char_offset, content_with_line_numbers, extract_lines_with_numbers, line_count,
    line_range_to_byte_range, line_range_to_char_range, line_to_byte_offset, line_to_char_offset,
    validate_expected_text,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_module_exports() {
        // Ensure all exports are accessible
        let _ = std::any::type_name::<EditError>();
    }
}
