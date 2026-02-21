//! Shared identity and block types for Kaijutsu.
//!
//! This crate is the relational foundation: typed IDs, principals, credentials,
//! blocks, and context metadata. It has **no internal kaijutsu dependencies** —
//! a pure leaf crate that other crates build on.
//!
//! # Entity-Relationship Overview
//!
//! ```text
//! Principal (PrincipalId)
//!     └── authenticates via Credential (fingerprint → PrincipalId)
//!     └── owns Kernel (KernelId)
//!     └── creates Context (ContextId, within a kernel)
//!     └── authors Block (BlockId = ContextId + PrincipalId + seq)
//!     └── opens Session (SessionId)
//! ```
//!
//! # Key Types
//!
//! | Type | Purpose |
//! |------|---------|
//! | [`PrincipalId`] | Who (user, model, system) |
//! | [`KernelId`] | Which kernel instance |
//! | [`ContextId`] | Which context (= document) |
//! | [`SessionId`] | Which connection session |
//! | [`Principal`] | Full identity (id + username + display_name) |
//! | [`BlockId`] | Unique block address (context + agent + seq) |
//! | [`BlockSnapshot`] | Serializable block state |
//! | [`ContextInfo`] | Context metadata for listing/display |

pub mod ids;
pub mod principal;
pub mod block;
pub mod context;

// Re-export primary types at crate root for convenience.
pub use ids::{ContextId, KernelId, PrincipalId, SessionId};
pub use ids::{PrefixError, resolve_context_prefix};
pub use principal::{Principal, Credential, CredentialKind};
pub use block::{BlockId, BlockKind, BlockSnapshot, DriftKind, Role, Status};
pub use context::{ContextInfo, fork_lineage};
