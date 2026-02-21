//! Compaction vocabulary for summarizing conversation history.
//!
//! When a context grows large, older blocks can be summarized into a single
//! summary block. `CompactionBoundary` records this transition point.

use serde::{Deserialize, Serialize};

use crate::block::BlockId;

/// Marks a boundary between summarized history and live blocks.
///
/// When conversation history is compacted, older blocks are summarized into
/// a single summary block. This struct records where that happened so the
/// system knows which blocks are live and which are behind the boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionBoundary {
    /// Block ID at which compaction ends (exclusive — this and later are live).
    pub cutoff_block: BlockId,
    /// Summary block that replaced the compacted range.
    pub summary_block: BlockId,
    /// How many original blocks were summarized.
    pub original_count: u32,
    /// When this compaction happened (Unix millis).
    pub compacted_at: u64,
}

impl CompactionBoundary {
    /// Create a new compaction boundary, auto-timestamped.
    pub fn new(cutoff_block: BlockId, summary_block: BlockId, original_count: u32) -> Self {
        Self {
            cutoff_block,
            summary_block,
            original_count,
            compacted_at: crate::now_millis(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ContextId, PrincipalId};

    fn test_boundary() -> CompactionBoundary {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        CompactionBoundary::new(
            BlockId::new(ctx, agent, 100),
            BlockId::new(ctx, agent, 101),
            99,
        )
    }

    #[test]
    fn test_construction() {
        let b = test_boundary();
        assert_eq!(b.original_count, 99);
        assert!(b.compacted_at > 0);
    }

    #[test]
    fn test_json_roundtrip() {
        let b = test_boundary();
        let json = serde_json::to_string(&b).unwrap();
        let parsed: CompactionBoundary = serde_json::from_str(&json).unwrap();
        assert_eq!(b, parsed);
    }

    #[test]
    fn test_postcard_roundtrip() {
        let b = test_boundary();
        let bytes = postcard::to_stdvec(&b).unwrap();
        let parsed: CompactionBoundary = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(b, parsed);
    }
}
