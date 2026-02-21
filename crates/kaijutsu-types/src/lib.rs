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
//! | [`Principal`]     | Full identity (id + username + display_name) |
//! | [`PrincipalId`]   | Who (user, model, system)                    |
//! | [`KernelId`]      | Which kernel instance                        |
//! | [`ContextId`]     | Which context (= document)                   |
//! | [`SessionId`]     | Which connection session                     |
//! | [`BlockId`]       | Unique block address (context + agent + seq) |
//! | [`BlockSnapshot`] | Serializable block state                     |
//! |-------------------|----------------------------------------------|

pub mod ids;
pub mod principal;
pub mod block;
pub mod context;
pub mod kernel;

// Re-export primary types at crate root for convenience.
pub use ids::{ContextId, KernelId, PrincipalId, SessionId};
pub use ids::{PrefixError, resolve_context_prefix};
pub use principal::{Principal, Credential, CredentialKind};
pub use block::{
    BlockId, BlockKind, BlockSnapshot, BlockSnapshotBuilder, DriftKind, Role, Status, ToolKind,
};
pub use context::{Context, fork_lineage};
pub use kernel::Kernel;
