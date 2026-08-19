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
//!     └── owns BlockStore
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
pub mod context;
pub mod dag;
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

/// Wire protocol version, exchanged in `bindKernel` (`kaijutsu.capnp`) — the
/// handshake seam every client hits before anything else. Client and kernel
/// must agree exactly; a mismatch is refused loudly rather than tolerated.
///
/// **Bump this on any change an older client cannot correctly ignore**:
/// retiring a method, changing a method's meaning, or changing the semantics
/// of an existing field. Appending a new field or a new method is additive
/// and does NOT require a bump — capnp already handles that compatibly.
///
/// Concrete receipt for why this exists: retiring `subscribePermissionEvents
/// @93` (→ `retired93 @93 ()`) was exactly the kind of change that needed a
/// bump. It shipped without one — a stale `target/debug/kaijutsu-acp` build
/// artifact kept running against a rebuilt kernel, silently missing the
/// permission-approval feature instead of failing, and cost a morning to
/// diagnose. See `docs/issues.md`, "The ACP binary can silently outlive a
/// wire change (2026-08-18)".
///
/// Starts at 1. 0 is reserved as the "old client that predates this field"
/// sentinel (capnp's struct default for an unset `UInt32`) and must never be
/// a real version.
pub const WIRE_VERSION: u32 = 1;

/// Build the human-facing diagnosis for a `bindKernel` [`WIRE_VERSION`]
/// mismatch — names both sides and points the remedy at whichever one is
/// actually stale (the lower version), never the side that's already
/// correct. Shared by the kernel's refusal (`kaijutsu-server::rpc::bind_kernel`)
/// and the client's symmetric check (`kaijutsu-client::rpc::RpcClient::bind_kernel`)
/// so both directions of mismatch use identical wording and neither call
/// site can independently drift into blaming the wrong side.
///
/// Callers must only invoke this when `client_version != kernel_version`;
/// equal versions are not a mismatch and have nothing to diagnose.
pub fn wire_version_mismatch_message(client_version: u32, kernel_version: u32) -> String {
    debug_assert_ne!(
        client_version, kernel_version,
        "wire_version_mismatch_message called on matching versions — nothing to diagnose"
    );
    if client_version < kernel_version {
        format!(
            "client wire version {client_version}, kernel wire version {kernel_version} — the \
             client is stale: rebuild it. A build artifact (e.g. `target/debug/kaijutsu-acp`) \
             can silently outlive a wire change without a rebuild to catch it."
        )
    } else {
        format!(
            "client wire version {client_version}, kernel wire version {kernel_version} — this \
             kernel is stale: rebuild and restart it (`cargo build -p kaijutsu-server && \
             systemctl --user restart kaijutsu-server`). The unit runs the DEBUG binary \
             deliberately for now, so a `--release` build will not change what is deployed."
        )
    }
}

// Re-export primary types at crate root for convenience.
pub use block::{
    BlockEventFilter, BlockFilter, BlockFlowKind, BlockHeader, BlockId, BlockKind, BlockMetadata,
    BlockQuery, BlockSnapshot, BlockSnapshotBuilder, ContentType, DriftKind, ErrorCategory,
    ErrorPayload,
    ErrorSeverity, ErrorSpan, LogLevel, MAX_DAG_DEPTH, NotificationKind, NotificationPayload,
    ProvenanceTag, ResourcePayload, Role, Status, StyleAttrs, StyleColor, StyleSpan, TaskStatus,
    ToolKind, ERROR_DETAIL_HYDRATION_BUDGET,
    NOTIFICATION_DETAIL_HYDRATION_BUDGET, RESOURCE_CONTENT_HYDRATION_BUDGET,
    TOOL_CONTENT_HYDRATION_BUDGET, format_error_for_llm, format_notification_for_llm,
    format_resource_for_llm, format_task_for_llm, format_tool_content_envelope,
    format_tool_content_for_llm,
};
pub use error_block::IntoErrorPayload;
pub use context::{Context, RING_SLOTS, fork_lineage};
pub use dag::ConversationDAG;
pub use enums::{ConsentMode, ContextState, DocKind, EdgeKind, ForkKind};
pub use ids::{
    BackendId, CastId, ContextId, KernelId, PresetId, PrincipalId, SessionId, WorkspaceId,
};
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 0 is the "old client that predates the `wireVersion` field" sentinel
    /// (capnp's UInt32 struct default) — it must never collide with a real
    /// version, or the mismatch check in `bind_kernel` loses its footing.
    #[test]
    fn wire_version_is_non_zero() {
        assert_ne!(WIRE_VERSION, 0);
    }

    /// A stale client (lower version) must be told to rebuild ITSELF —
    /// never advised to touch the kernel, which is already correct.
    #[test]
    fn wire_version_mismatch_message_blames_stale_client() {
        let msg = wire_version_mismatch_message(0, 1);
        assert!(msg.contains('0'), "must name the client's version: {msg}");
        assert!(msg.contains('1'), "must name the kernel's version: {msg}");
        assert!(
            msg.contains("client is stale"),
            "must diagnose the client as stale: {msg}"
        );
        assert!(
            !msg.contains("kernel is stale"),
            "must not also blame the kernel, which is correct: {msg}"
        );
        assert!(
            !msg.contains("systemctl"),
            "must not tell the operator to restart a kernel that isn't the problem: {msg}"
        );
    }

    /// A stale kernel (lower version) must be told to rebuild+restart
    /// ITSELF — never advised that the client (already correct) needs
    /// rebuilding.
    #[test]
    fn wire_version_mismatch_message_blames_stale_kernel() {
        let msg = wire_version_mismatch_message(2, 1);
        assert!(msg.contains('2'), "must name the client's version: {msg}");
        assert!(msg.contains('1'), "must name the kernel's version: {msg}");
        assert!(
            msg.contains("kernel is stale"),
            "must diagnose the kernel as stale: {msg}"
        );
        assert!(
            !msg.contains("client is stale"),
            "must not also blame the client, which is correct: {msg}"
        );
    }
}
