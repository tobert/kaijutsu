//! Block lookup helpers used by the remaining MCP tools.
//!
//! Most of the parsing/formatting helpers were retired with the
//! MCP slim-down (block_*, doc_*, kernel_search moved to `kj`).
//! What stays is the block-by-key resolver still wired through the
//! `analyze_document` / `editing_assistant` paths.

use kaijutsu_crdt::{BlockId, ContextId};
use kaijutsu_kernel::SharedBlockStore;

/// Parse block ID from key string.
pub fn parse_block_id(s: &str) -> Option<BlockId> {
    BlockId::from_key(s)
}

/// Find a block by its key string, returning (ContextId, BlockId).
///
/// BlockId contains context_id, so we look up the document directly.
pub fn find_block(store: &SharedBlockStore, block_id_str: &str) -> Option<(ContextId, BlockId)> {
    let block_id = parse_block_id(block_id_str)?;
    let ctx = block_id.context_id;

    let entry = store.get(ctx)?;
    if entry.doc.get_block_snapshot(&block_id).is_some() {
        Some((ctx, block_id))
    } else {
        None
    }
}
