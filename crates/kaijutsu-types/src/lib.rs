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
pub mod codec;
pub mod compaction;
pub mod context;
pub mod enums;
pub mod error_block;
pub mod ids;
pub mod kernel;
pub mod paths;
pub mod principal;
pub mod session;
pub mod share;
pub mod theme;
pub mod tick;
pub mod timeout;
pub mod track;

// Re-export kaish output types for structured tool results.
pub use kaish_types::output::{EntryType as OutputEntryType, OutputData, OutputNode};

/// SSH subsystem name the client requests to bind a session channel to the
/// Cap'n Proto RPC handler. One source of truth for both `kaijutsu-client`
/// (which requests it) and `kaijutsu-server` (which dispatches on it). Siblings
/// like SFTP and a debug shell will join this namespace.
pub const SSH_RPC_SUBSYSTEM: &str = "kaijutsu-rpc";

/// SSH subsystem name for the SFTP file-transfer channel — the sibling of
/// [`SSH_RPC_SUBSYSTEM`]. The server binds it to the `russh_sftp` adapter over
/// the kernel VFS (`kaijutsu-server/src/sftp.rs`); the client requests it on a
/// second session channel to read objects (`/v/cas/<hash>`) and browse `/v`.
/// Standard SSH name so off-the-shelf clients (`sftp`, `sshfs`) work too.
pub const SSH_SFTP_SUBSYSTEM: &str = "sftp";

/// SSH subsystem name for a client-offered share session — the **reverse**
/// of [`SSH_SFTP_SUBSYSTEM`] (`docs/slash-r.md`). SSH subsystem requests only
/// travel client→server, so the client must still open the channel — but
/// once open, the **roles swap**: the client speaks the SFTP *server* role
/// (serving its local directories) and the kernel speaks the *client* role,
/// reading them. Kept as its own name (not reusing `"sftp"`) because that
/// name is already taken with the opposite meaning by the kernel's forward
/// adapter (`kaijutsu-server/src/sftp.rs`) on the same dispatch scaffold.
pub const SSH_SHARE_SUBSYSTEM: &str = "kaijutsu-share";

// Re-export primary types at crate root for convenience.
pub use block::{
    BlockEventFilter, BlockFilter, BlockFlowKind, BlockHeader, BlockId, BlockKind, BlockMetadata,
    BlockQuery, BlockSnapshot, BlockSnapshotBuilder, ContentType, DriftKind, ErrorCategory,
    ErrorPayload,
    ErrorSeverity, ErrorSpan, LogLevel, MAX_DAG_DEPTH, NotificationKind, NotificationPayload,
    ResourcePayload, Role, Status, ToolKind, ERROR_DETAIL_HYDRATION_BUDGET,
    NOTIFICATION_DETAIL_HYDRATION_BUDGET, RESOURCE_CONTENT_HYDRATION_BUDGET,
    TOOL_CONTENT_HYDRATION_BUDGET, format_error_for_llm, format_notification_for_llm,
    format_resource_for_llm, format_tool_content_for_llm,
};
pub use error_block::IntoErrorPayload;
pub use compaction::CompactionBoundary;
pub use context::{Context, RING_SLOTS, fork_lineage};
pub use enums::{ConsentMode, ContextState, DocKind, EdgeKind, ForkKind};
pub use ids::{ContextId, KernelId, PresetId, PrincipalId, SessionId, WorkspaceId};
pub use ids::{PrefixError, PrefixResolvable, resolve_context_prefix, resolve_prefix};
pub use kernel::Kernel;
pub use principal::{Credential, CredentialKind, Principal};
pub use session::Session;
pub use tick::{Span, Tick, TickDelta};
pub use timeout::TimeoutPolicy;
pub use track::{TrackId, TrackIdError};

/// Current time as Unix milliseconds. Canonical source — used by constructors
/// throughout the crate and by downstream crates (drift, kernel_db, rpc).
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
