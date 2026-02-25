//! Block types — re-exported from kaijutsu-types.
//!
//! All block identity, snapshot, and enum types are defined in kaijutsu-types
//! and re-exported here for backward compatibility.

pub use kaijutsu_types::{
    BlockFilter, BlockHeader, BlockId, BlockKind, BlockQuery, BlockSnapshot,
    BlockSnapshotBuilder, DriftKind, MAX_DAG_DEPTH, OutputData, OutputEntryType,
    OutputNode, Role, Status, ToolKind,
};
