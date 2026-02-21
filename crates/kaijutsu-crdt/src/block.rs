//! Block types — re-exported from kaijutsu-types.
//!
//! All block identity, snapshot, and enum types are defined in kaijutsu-types
//! and re-exported here for backward compatibility.

pub use kaijutsu_types::{
    BlockId, BlockKind, BlockSnapshot, DriftKind, Role, Status, ToolKind,
    // Also available but not re-exported by default:
    // BlockHeader, BlockSnapshotBuilder, MAX_DAG_DEPTH
};
