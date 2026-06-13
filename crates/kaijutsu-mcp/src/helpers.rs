//! Block lookup helpers used by the remaining MCP tools.
//!
//! Most of the parsing/formatting helpers were retired with the
//! MCP slim-down (block_*, doc_*, kernel_search moved to `kj`).
//! Block resolution now lives on `KaijutsuMcp` (`locate_block`/`read_block`),
//! which is backend-agnostic; what stays here is the key parser they use.

use kaijutsu_crdt::BlockId;

/// Parse block ID from key string.
pub fn parse_block_id(s: &str) -> Option<BlockId> {
    BlockId::from_key(s)
}
