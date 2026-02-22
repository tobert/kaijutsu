//! Helper functions for parsing and text manipulation.
//!
//! Parsing functions delegate to strum-derived FromStr implementations on the
//! enums in kaijutsu-crdt and kaijutsu-kernel.

use kaijutsu_crdt::{BlockId, BlockKind, ContextId, Role, Status};
use kaijutsu_kernel::{DocumentKind, SharedBlockStore};

// ============================================================================
// Parsing Helpers
// ============================================================================

/// Parse document kind from string.
pub fn parse_document_kind(s: &str) -> Option<DocumentKind> {
    DocumentKind::from_str(s)
}

/// Parse role from string.
pub fn parse_role(s: &str) -> Option<Role> {
    Role::from_str(s)
}

/// Parse block kind from string.
pub fn parse_block_kind(s: &str) -> Option<BlockKind> {
    BlockKind::from_str(s)
}

/// Parse status from string.
pub fn parse_status(s: &str) -> Option<Status> {
    Status::from_str(s)
}

/// Parse block ID from key string.
pub fn parse_block_id(s: &str) -> Option<BlockId> {
    BlockId::from_key(s)
}

// ============================================================================
// Block Lookup
// ============================================================================

/// Find a block across all documents, returning (ContextId, BlockId).
///
/// Since BlockId contains context_id, we first check that document directly.
/// Falls back to scanning all documents if the context isn't found.
pub fn find_block(store: &SharedBlockStore, block_id_str: &str) -> Option<(ContextId, BlockId)> {
    let block_id = parse_block_id(block_id_str)?;

    // Fast path: BlockId contains context_id, check directly
    let ctx = block_id.context_id;
    if let Some(entry) = store.get(ctx)
        && entry.doc.get_block_snapshot(&block_id).is_some()
    {
        return Some((ctx, block_id));
    }

    // Slow fallback: scan all documents
    for context_id in store.list_ids() {
        if let Some(entry) = store.get(context_id)
            && entry.doc.get_block_snapshot(&block_id).is_some()
        {
            return Some((context_id, block_id));
        }
    }
    None
}

// ============================================================================
// Line Number Utilities
// ============================================================================

/// Add line numbers to content.
pub fn content_with_line_numbers(content: &str) -> String {
    content
        .lines()
        .enumerate()
        .map(|(i, line)| format!("{:4}→{}", i + 1, line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract lines with numbers for a range.
pub fn extract_lines_with_numbers(content: &str, start: u32, end: u32) -> String {
    content
        .lines()
        .enumerate()
        .skip(start as usize)
        .take((end.saturating_sub(start)) as usize)
        .map(|(i, line)| format!("{:4}→{}", i + 1, line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Count lines in content.
pub fn line_count(content: &str) -> usize {
    if content.is_empty() {
        0
    } else {
        content.lines().count()
    }
}

/// Convert line number to byte offset.
pub fn line_to_byte_offset(content: &str, line: u32) -> Option<usize> {
    let mut offset = 0;
    for (i, l) in content.lines().enumerate() {
        if i == line as usize {
            return Some(offset);
        }
        offset += l.len() + 1; // +1 for newline
    }
    // Line at end
    if line as usize == content.lines().count() {
        return Some(content.len());
    }
    None
}

/// Convert line range to byte range.
pub fn line_range_to_byte_range(content: &str, start_line: u32, end_line: u32) -> Option<(usize, usize)> {
    let start = line_to_byte_offset(content, start_line)?;
    let end = line_to_byte_offset(content, end_line)?;
    Some((start, end))
}
