//! Shared identity and block types for Kaijutsu.
//!
//! This crate is the relational foundation: typed IDs, principals, credentials,
//! blocks, kernels, and context metadata. It has **no internal kaijutsu
//! dependencies** — a pure leaf crate that other crates build on.
//!
//! # Entity-Relationship Overview
//!
//! ```text
//! Kernel (KernelId) ← 会場, the meeting place
//!     └── founded by Principal (PrincipalId)
//!     └── contains Context (ContextId, forks/threads/drifts)
//!
//! Principal (PrincipalId) ← user, model, or system
//!     └── authenticates via Credential (fingerprint → PrincipalId)
//!     └── founds Kernel
//!     └── joins Kernel as peer
//!     └── creates Context (within a kernel)
//!     └── authors Block (BlockId = ContextId + PrincipalId + seq)
//!     └── opens Session (SessionId)
//!
//! Context (ContextId) ← conversation/workspace within a kernel
//!     └── parent_id forms fork/thread lineage
//!     └── drifts to/from sibling contexts
//!     └── owns BlockDocument (CRDT)
//! ```
//!
//! # Key Types
//!
//! |-------------------|----------------------------------------------|
//! | Type              | Purpose                                      |
//! |-------------------|----------------------------------------------|
//! | [`Kernel`]        | Kernel birth certificate (founder + label)   |
//! | [`Context`]       | Context metadata (lineage + creator)         |
//! | [`Session`]       | Session birth certificate (who + where)      |
//! | [`Principal`]     | Full identity (id + username + display_name) |
//! | [`PrincipalId`]   | Who (user, model, system)                    |
//! | [`KernelId`]      | Which kernel instance                        |
//! | [`ContextId`]     | Which context (= document)                   |
//! | [`SessionId`]     | Which connection session                     |
//! | [`BlockId`]       | Unique block address (context + agent + seq) |
//! | [`BlockHeader`]   | Lightweight Copy-able subset for DAG indexing |
//! | [`BlockSnapshot`] | Serializable block state                     |
//! |-------------------|----------------------------------------------|

pub mod block;
pub mod compaction;
pub mod context;
pub mod enums;
pub mod error_block;
pub mod ids;
pub mod kernel;
pub mod principal;
pub mod session;

// Re-export kaish output types for structured tool results.
pub use kaish_types::output::{EntryType as OutputEntryType, OutputData, OutputNode};

// Re-export primary types at crate root for convenience.
pub use block::{
    BlockEventFilter, BlockFilter, BlockFlowKind, BlockHeader, BlockId, BlockKind, BlockQuery,
    BlockSnapshot, BlockSnapshotBuilder, ContentType, DriftKind, ErrorCategory, ErrorPayload,
    ErrorSeverity, ErrorSpan, MAX_DAG_DEPTH, Role, Status, ToolKind, ERROR_DETAIL_HYDRATION_BUDGET,
    format_error_for_llm,
};
pub use error_block::IntoErrorPayload;
pub use compaction::CompactionBoundary;
pub use context::{Context, fork_lineage};
pub use enums::{ConsentMode, ContextState, DocKind, EdgeKind, ForkKind, ToolFilter};
pub use ids::{ContextId, KernelId, PresetId, PrincipalId, SessionId, WorkspaceId};
pub use ids::{PrefixError, PrefixResolvable, resolve_context_prefix, resolve_prefix};
pub use kernel::Kernel;
pub use principal::{Credential, CredentialKind, Principal};
pub use session::Session;

/// Current time as Unix milliseconds. Canonical source — used by constructors
/// throughout the crate and by downstream crates (drift, kernel_db, rpc).
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
