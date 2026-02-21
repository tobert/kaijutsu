//! Block types and identifiers with typed IDs.
//!
//! DAG-native block model with parent/child relationships, role tracking, and
//! status management. This module provides the *target* types that replace
//! string-based identifiers in the current `kaijutsu-crdt::block`.
//!
//! ## Design: BlockKind + ToolKind + DriftKind
//!
//! `BlockKind` is deliberately small — 5 variants covering what a block *is*.
//! Mechanism metadata lives in companion enums:
//!
//! - `ToolKind` on ToolCall/ToolResult: which execution engine (Shell, Mcp, Builtin)
//! - `DriftKind` on Drift: how content transferred (Push, Pull, Merge, Distill, Commit)
//!
//! Shell commands are tool calls where `tool_kind = Shell`. The principal on the
//! block tells you who initiated it (user typed `ls` vs model requested shell exec).

use std::str::FromStr;

use serde::{Deserialize, Serialize};
use strum::EnumString;

use crate::ids::{ContextId, PrincipalId};

/// Globally unique block identifier with typed IDs.
///
/// Composed of:
/// - `context_id`: The context (= document) this block belongs to
/// - `agent_id`: The principal that created this block
/// - `seq`: Agent-local sequence number (monotonically increasing)
///
/// This ensures global uniqueness without coordination.
/// UUIDs are hex-only, so `to_key()` / `from_key()` need no slash-escaping.
#[derive(Clone, Copy, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct BlockId {
    /// Context (= document) this block belongs to.
    pub context_id: ContextId,
    /// Principal that created this block.
    pub agent_id: PrincipalId,
    /// Agent-local sequence number.
    ///
    /// Callers must ensure monotonically increasing values per (context_id, agent_id)
    /// pair. The types crate does not enforce this — that's BlockDocument's job.
    /// Violations won't cause incorrect equality, but will confuse debugging
    /// and may break compaction heuristics.
    pub seq: u64,
}

impl BlockId {
    /// Create a new block ID from typed components.
    pub fn new(context_id: ContextId, agent_id: PrincipalId, seq: u64) -> Self {
        Self {
            context_id,
            agent_id,
            seq,
        }
    }

    /// Convert to a compact string key: `"{context_hex}:{principal_hex}:{seq}"`.
    ///
    /// Uses `:` as delimiter — safe because UUIDs are hex-only, and avoids
    /// collision with `/` used as path separator in CRDT engines.
    pub fn to_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.context_id.to_hex(),
            self.agent_id.to_hex(),
            self.seq
        )
    }

    /// Parse from key string: `"{context_hex}:{principal_hex}:{seq}"`.
    pub fn from_key(key: &str) -> Option<Self> {
        let parts: Vec<&str> = key.splitn(3, ':').collect();
        if parts.len() != 3 {
            return None;
        }
        let context_id = ContextId::parse(parts[0]).ok()?;
        let agent_id = PrincipalId::parse(parts[1]).ok()?;
        let seq: u64 = parts[2].parse().ok()?;
        Some(Self {
            context_id,
            agent_id,
            seq,
        })
    }
}

impl std::fmt::Display for BlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}@{}#{}",
            self.context_id.short(),
            self.agent_id.short(),
            self.seq
        )
    }
}

impl std::fmt::Debug for BlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BlockId({}@{}#{})",
            self.context_id.short(),
            self.agent_id.short(),
            self.seq
        )
    }
}

/// Maximum expected DAG depth. Traversal code should use this as a circuit breaker.
///
/// Real conversations rarely exceed depth 50 (user -> thinking -> text -> tool_call ->
/// tool_result, repeated). Depth 512 is generous; exceeding it likely indicates a
/// cycle or corruption.
pub const MAX_DAG_DEPTH: usize = 512;

/// Lightweight Copy-able subset of [`BlockSnapshot`] for DAG indexing.
///
/// Contains only the fields needed for traversal, ordering, and filtering —
/// ~99 bytes vs ~300+ for the full snapshot. Use this when building indexes
/// or walking the DAG without needing content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockHeader {
    pub id: BlockId,
    pub parent_id: Option<BlockId>,
    pub role: Role,
    pub kind: BlockKind,
    pub status: Status,
    pub compacted: bool,
    pub created_at: u64,
}

impl BlockHeader {
    /// Extract a header from a full [`BlockSnapshot`].
    pub fn from_snapshot(snap: &BlockSnapshot) -> Self {
        Self {
            id: snap.id,
            parent_id: snap.parent_id,
            role: snap.role,
            kind: snap.kind,
            status: snap.status,
            compacted: snap.compacted,
            created_at: snap.created_at,
        }
    }

    /// Check if this is a root block (no parent).
    pub fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }

    /// Check if this block has a parent.
    pub fn has_parent(&self) -> bool {
        self.parent_id.is_some()
    }

    /// Check if this is a tool-related block (call or result).
    pub fn is_tool(&self) -> bool {
        self.kind.is_tool()
    }
}

/// Role in conversation (participant type).
///
/// Uses User/Model terminology to reflect collaborative peer model
/// rather than hierarchical Human/Agent relationship.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, EnumString)]
#[serde(rename_all = "lowercase")]
#[strum(ascii_case_insensitive)]
pub enum Role {
    /// User (person at keyboard).
    #[default]
    #[strum(serialize = "user", serialize = "human")]
    User,
    /// Model (AI model — Claude, GPT, etc.).
    #[strum(serialize = "model", serialize = "assistant", serialize = "agent")]
    Model,
    /// System message (errors, notifications).
    System,
    /// Tool execution context (results from tool calls).
    Tool,
}

impl Role {
    /// Parse from string (case-insensitive).
    ///
    /// Supports aliases: "human" -> User, "assistant"/"agent" -> Model.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        <Self as FromStr>::from_str(s).ok()
    }

    /// Convert to string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Model => "model",
            Role::System => "system",
            Role::Tool => "tool",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Execution status for blocks (CRDT-synced).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, EnumString)]
#[serde(rename_all = "lowercase")]
#[strum(ascii_case_insensitive)]
pub enum Status {
    /// Queued, not started.
    #[default]
    Pending,
    /// In progress (streaming, executing).
    #[strum(serialize = "running", serialize = "active")]
    Running,
    /// Completed successfully.
    #[strum(serialize = "done", serialize = "complete", serialize = "completed")]
    Done,
    /// Failed with error.
    Error,
}

impl Status {
    /// Parse from string (case-insensitive).
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        <Self as FromStr>::from_str(s).ok()
    }

    /// Convert to string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Running => "running",
            Status::Done => "done",
            Status::Error => "error",
        }
    }

    /// Check if this status indicates completion (Done or Error).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Status::Done | Status::Error)
    }

    /// Check if this status indicates active work.
    pub fn is_active(&self) -> bool {
        matches!(self, Status::Running)
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// What a block *is* (content type).
///
/// Deliberately small. Mechanism metadata lives in companion enums:
/// - `ToolKind` on ToolCall/ToolResult — which execution engine
/// - `DriftKind` on Drift — how content transferred
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, EnumString)]
#[serde(rename_all = "lowercase")]
#[strum(ascii_case_insensitive)]
pub enum BlockKind {
    /// Main text response.
    #[default]
    Text,
    /// Extended thinking/reasoning — collapsible.
    Thinking,
    /// Tool invocation — content is streamable via Text CRDT.
    /// See `tool_kind` for which engine (Shell, Mcp, Builtin).
    #[serde(rename = "tool_call")]
    #[strum(serialize = "tool_call", serialize = "toolcall")]
    ToolCall,
    /// Tool result — content is streamable via Text CRDT.
    /// See `tool_kind` for which engine.
    #[serde(rename = "tool_result")]
    #[strum(serialize = "tool_result", serialize = "toolresult")]
    ToolResult,
    /// Drifted content from another context (cross-context transfer).
    /// See `drift_kind` for how it arrived.
    #[serde(rename = "drift")]
    #[strum(serialize = "drift")]
    Drift,
}

impl BlockKind {
    /// Parse from string (case-insensitive).
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        <Self as FromStr>::from_str(s).ok()
    }

    /// Convert to string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            BlockKind::Text => "text",
            BlockKind::Thinking => "thinking",
            BlockKind::ToolCall => "tool_call",
            BlockKind::ToolResult => "tool_result",
            BlockKind::Drift => "drift",
        }
    }

    /// Check if this block type has editable text content via Text CRDT.
    pub fn has_text_crdt(&self) -> bool {
        true
    }

    /// Check if this is a tool-related block (call or result).
    pub fn is_tool(&self) -> bool {
        matches!(self, BlockKind::ToolCall | BlockKind::ToolResult)
    }

    /// Check if this is a drift block (cross-context transfer).
    pub fn is_drift(&self) -> bool {
        matches!(self, BlockKind::Drift)
    }
}

impl std::fmt::Display for BlockKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Which execution engine handled a tool call/result.
///
/// Parallel to `DriftKind` — mechanism metadata on ToolCall/ToolResult blocks.
/// The `Role` on the block tells you *who* (user typed a command vs model
/// requested execution). `ToolKind` tells you *how* (kaish, MCP server, builtin).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(ascii_case_insensitive)]
pub enum ToolKind {
    /// kaish shell execution (the default — `shell_execute` RPC).
    #[default]
    Shell,
    /// MCP tool invocation (via registered MCP server).
    Mcp,
    /// Kernel builtin tool (no external process).
    Builtin,
}

impl ToolKind {
    /// Parse from string (case-insensitive).
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        <Self as FromStr>::from_str(s).ok()
    }

    /// Convert to string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolKind::Shell => "shell",
            ToolKind::Mcp => "mcp",
            ToolKind::Builtin => "builtin",
        }
    }
}

impl std::fmt::Display for ToolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// How a drift block arrived from another context.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(ascii_case_insensitive)]
pub enum DriftKind {
    /// User manually pushed content to another context.
    #[default]
    Push,
    /// User pulled/requested content from another context.
    Pull,
    /// Context merge (fork coming home).
    Merge,
    /// LLM-summarized before transfer.
    Distill,
    /// Git commit recorded as conversation provenance.
    Commit,
}

impl DriftKind {
    /// Parse from string (case-insensitive).
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        <Self as FromStr>::from_str(s).ok()
    }

    /// Convert to string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            DriftKind::Push => "push",
            DriftKind::Pull => "pull",
            DriftKind::Merge => "merge",
            DriftKind::Distill => "distill",
            DriftKind::Commit => "commit",
        }
    }
}

impl std::fmt::Display for DriftKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Serializable snapshot of a block (no CRDT state).
///
/// All identity fields use typed IDs: `PrincipalId` for author/agent,
/// `ContextId` for context references. Mechanism-specific fields are
/// `Option` types — only populated for relevant block kinds.
///
/// ## Field groups
///
/// - **Core**: id, parent_id, role, status, kind, content, created_at
/// - **Tool** (ToolCall/ToolResult): tool_kind, tool_name, tool_input, tool_call_id, exit_code, is_error, display_hint
/// - **Drift** (Drift): drift_kind, source_context, source_model
///
/// The block's author is always `id.agent_id` — no separate field to diverge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockSnapshot {
    /// Block ID (includes context_id, agent_id/author, and seq).
    pub id: BlockId,
    /// Parent block ID (DAG edge — None for root blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<BlockId>,
    /// Role of the block author (user, model, system, tool).
    pub role: Role,
    /// Execution status (pending, running, done, error).
    pub status: Status,
    /// Content type (text, thinking, tool_call, tool_result, drift).
    pub kind: BlockKind,
    /// Primary text content.
    pub content: String,
    /// Whether this block is collapsed (only meaningful for Thinking).
    #[serde(default, skip_serializing_if = "is_false")]
    pub collapsed: bool,
    /// Whether this block has been superseded by a compaction summary.
    /// Compacted blocks are retained for history but excluded from active views.
    #[serde(default, skip_serializing_if = "is_false")]
    pub compacted: bool,
    /// Timestamp when block was created (Unix millis).
    pub created_at: u64,

    // Tool-specific fields (ToolCall / ToolResult)

    /// Which execution engine (Shell, Mcp, Builtin). Present on ToolCall/ToolResult.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_kind: Option<ToolKind>,
    /// Tool name (for ToolCall blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Tool input as JSON string (for ToolCall blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<String>,
    /// Reference to parent ToolCall block (for ToolResult blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<BlockId>,
    /// Exit code from tool execution (for ToolResult blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Whether this is an error result (for ToolResult blocks).
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
    /// Display hint for richer output formatting (JSON-serialized).
    /// Used for shell output blocks to enable per-viewer rendering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_hint: Option<String>,

    // Drift-specific fields (Drift)

    /// Originating context (for Drift blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_context: Option<ContextId>,
    /// Model that produced this content (for Drift blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_model: Option<String>,
    /// How this block arrived from another context (for Drift blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drift_kind: Option<DriftKind>,
}

/// Helper for `#[serde(skip_serializing_if)]` on bool fields.
fn is_false(v: &bool) -> bool {
    !v
}

impl BlockSnapshot {
    /// The principal that authored this block (always `id.agent_id`).
    pub fn author(&self) -> PrincipalId {
        self.id.agent_id
    }

    /// Create a new text block snapshot.
    pub fn text(
        id: BlockId,
        parent_id: Option<BlockId>,
        role: Role,
        content: impl Into<String>,
    ) -> Self {
        debug_assert!(
            parent_id.map_or(true, |p| p.context_id == id.context_id),
            "parent_id must be in same context as block (use source_context for drift)"
        );
        Self {
            id,
            parent_id,
            role,
            status: Status::Done,
            kind: BlockKind::Text,
            content: content.into(),
            collapsed: false,
            compacted: false,
            created_at: crate::now_millis(),
            tool_kind: None,
            tool_name: None,
            tool_input: None,
            tool_call_id: None,
            exit_code: None,
            is_error: false,
            display_hint: None,
            source_context: None,
            source_model: None,
            drift_kind: None,
        }
    }

    /// Create a new thinking block snapshot.
    pub fn thinking(
        id: BlockId,
        parent_id: Option<BlockId>,
        content: impl Into<String>,
    ) -> Self {
        debug_assert!(
            parent_id.map_or(true, |p| p.context_id == id.context_id),
            "parent_id must be in same context as block (use source_context for drift)"
        );
        Self {
            id,
            parent_id,
            role: Role::Model,
            status: Status::Done,
            kind: BlockKind::Thinking,
            content: content.into(),
            collapsed: false,
            compacted: false,
            created_at: crate::now_millis(),
            tool_kind: None,
            tool_name: None,
            tool_input: None,
            tool_call_id: None,
            exit_code: None,
            is_error: false,
            display_hint: None,
            source_context: None,
            source_model: None,
            drift_kind: None,
        }
    }

    /// Create a tool call block snapshot.
    ///
    /// `role` determines who initiated: `Role::User` for user-typed shell commands,
    /// `Role::Model` for LLM-issued tool calls.
    pub fn tool_call(
        id: BlockId,
        parent_id: Option<BlockId>,
        tool_kind: ToolKind,
        tool_name: impl Into<String>,
        tool_input: serde_json::Value,
        role: Role,
    ) -> Self {
        debug_assert!(
            parent_id.map_or(true, |p| p.context_id == id.context_id),
            "parent_id must be in same context as block (use source_context for drift)"
        );
        let input_json = serde_json::to_string_pretty(&tool_input).unwrap_or_default();
        Self {
            id,
            parent_id,
            role,
            status: Status::Running,
            kind: BlockKind::ToolCall,
            content: input_json.clone(),
            collapsed: false,
            compacted: false,
            created_at: crate::now_millis(),
            tool_kind: Some(tool_kind),
            tool_name: Some(tool_name.into()),
            tool_input: Some(input_json),
            tool_call_id: None,
            exit_code: None,
            is_error: false,
            display_hint: None,
            source_context: None,
            source_model: None,
            drift_kind: None,
        }
    }

    /// Create a tool result block snapshot.
    pub fn tool_result(
        id: BlockId,
        tool_call_id: BlockId,
        tool_kind: ToolKind,
        content: impl Into<String>,
        is_error: bool,
        exit_code: Option<i32>,
    ) -> Self {
        debug_assert!(
            tool_call_id.context_id == id.context_id,
            "tool_call_id must be in same context as result block"
        );
        Self {
            id,
            parent_id: Some(tool_call_id),
            role: Role::Tool,
            status: if is_error { Status::Error } else { Status::Done },
            kind: BlockKind::ToolResult,
            content: content.into(),
            collapsed: false,
            compacted: false,
            created_at: crate::now_millis(),
            tool_kind: Some(tool_kind),
            tool_name: None,
            tool_input: None,
            tool_call_id: Some(tool_call_id),
            exit_code,
            is_error,
            display_hint: None,
            source_context: None,
            source_model: None,
            drift_kind: None,
        }
    }

    /// Create a tool result block with a display hint.
    ///
    /// Display hints enable per-viewer rendering (e.g., ANSI terminal output,
    /// table formatting) without baking presentation into content.
    pub fn tool_result_with_hint(
        id: BlockId,
        tool_call_id: BlockId,
        tool_kind: ToolKind,
        content: impl Into<String>,
        is_error: bool,
        exit_code: Option<i32>,
        display_hint: Option<String>,
    ) -> Self {
        debug_assert!(
            tool_call_id.context_id == id.context_id,
            "tool_call_id must be in same context as result block"
        );
        Self {
            id,
            parent_id: Some(tool_call_id),
            role: Role::Tool,
            status: if is_error { Status::Error } else { Status::Done },
            kind: BlockKind::ToolResult,
            content: content.into(),
            collapsed: false,
            compacted: false,
            created_at: crate::now_millis(),
            tool_kind: Some(tool_kind),
            tool_name: None,
            tool_input: None,
            tool_call_id: Some(tool_call_id),
            exit_code,
            is_error,
            display_hint,
            source_context: None,
            source_model: None,
            drift_kind: None,
        }
    }

    /// Create a new drift block snapshot (cross-context transfer).
    pub fn drift(
        id: BlockId,
        parent_id: Option<BlockId>,
        content: impl Into<String>,
        source_context: ContextId,
        source_model: Option<String>,
        drift_kind: DriftKind,
    ) -> Self {
        debug_assert!(
            parent_id.map_or(true, |p| p.context_id == id.context_id),
            "parent_id must be in same context as block (use source_context for drift)"
        );
        Self {
            id,
            parent_id,
            role: Role::System,
            status: Status::Done,
            kind: BlockKind::Drift,
            content: content.into(),
            collapsed: false,
            compacted: false,
            created_at: crate::now_millis(),
            tool_kind: None,
            tool_name: None,
            tool_input: None,
            tool_call_id: None,
            exit_code: None,
            is_error: false,
            display_hint: None,
            source_context: Some(source_context),
            source_model,
            drift_kind: Some(drift_kind),
        }
    }

    /// Check if this block is collapsed (only meaningful for Thinking).
    pub fn is_collapsed(&self) -> bool {
        self.collapsed && self.kind == BlockKind::Thinking
    }

    /// Check if this is a root block (no parent).
    pub fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }

    /// Check if this block has a parent.
    pub fn has_parent(&self) -> bool {
        self.parent_id.is_some()
    }

    /// Check if this is a shell tool block (call or result with ToolKind::Shell).
    pub fn is_shell(&self) -> bool {
        self.tool_kind == Some(ToolKind::Shell) && self.kind.is_tool()
    }

    /// Compare two snapshots ignoring timestamp (`created_at`).
    ///
    /// Useful in tests where two independently-constructed snapshots
    /// should be "the same" but have different creation times.
    pub fn content_eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.parent_id == other.parent_id
            && self.role == other.role
            && self.status == other.status
            && self.kind == other.kind
            && self.content == other.content
            && self.collapsed == other.collapsed
            && self.compacted == other.compacted
            && self.tool_kind == other.tool_kind
            && self.tool_name == other.tool_name
            && self.tool_input == other.tool_input
            && self.tool_call_id == other.tool_call_id
            && self.exit_code == other.exit_code
            && self.is_error == other.is_error
            && self.display_hint == other.display_hint
            && self.source_context == other.source_context
            && self.source_model == other.source_model
            && self.drift_kind == other.drift_kind
    }
}

/// Builder for `BlockSnapshot` — reduces boilerplate for the many-field struct.
///
/// Starts with required fields (id, kind) and sane defaults for the rest.
/// The author is always `id.agent_id` — no separate parameter.
///
/// ```
/// # use kaijutsu_types::*;
/// let ctx = ContextId::new();
/// let author = PrincipalId::new();
/// let snap = BlockSnapshotBuilder::new(
///     BlockId::new(ctx, author, 1),
///     BlockKind::ToolCall,
/// )
///     .role(Role::User)
///     .tool_kind(ToolKind::Shell)
///     .tool_name("shell")
///     .content("ls -la")
///     .status(Status::Running)
///     .build();
/// ```
pub struct BlockSnapshotBuilder {
    snap: BlockSnapshot,
}

impl BlockSnapshotBuilder {
    /// Start building a snapshot with the two required fields.
    pub fn new(id: BlockId, kind: BlockKind) -> Self {
        Self {
            snap: BlockSnapshot {
                id,
                parent_id: None,
                role: Role::User,
                status: Status::Done,
                kind,
                content: String::new(),
                collapsed: false,
                compacted: false,
                created_at: crate::now_millis(),
                tool_kind: None,
                tool_name: None,
                tool_input: None,
                tool_call_id: None,
                exit_code: None,
                is_error: false,
                display_hint: None,
                source_context: None,
                source_model: None,
                drift_kind: None,
            },
        }
    }

    pub fn parent_id(mut self, parent_id: BlockId) -> Self {
        self.snap.parent_id = Some(parent_id);
        self
    }

    pub fn role(mut self, role: Role) -> Self {
        self.snap.role = role;
        self
    }

    pub fn status(mut self, status: Status) -> Self {
        self.snap.status = status;
        self
    }

    pub fn content(mut self, content: impl Into<String>) -> Self {
        self.snap.content = content.into();
        self
    }

    pub fn collapsed(mut self, collapsed: bool) -> Self {
        self.snap.collapsed = collapsed;
        self
    }

    pub fn compacted(mut self, compacted: bool) -> Self {
        self.snap.compacted = compacted;
        self
    }

    pub fn tool_kind(mut self, tool_kind: ToolKind) -> Self {
        self.snap.tool_kind = Some(tool_kind);
        self
    }

    pub fn tool_name(mut self, name: impl Into<String>) -> Self {
        self.snap.tool_name = Some(name.into());
        self
    }

    pub fn tool_input(mut self, input: impl Into<String>) -> Self {
        self.snap.tool_input = Some(input.into());
        self
    }

    pub fn tool_call_id(mut self, id: BlockId) -> Self {
        self.snap.tool_call_id = Some(id);
        self
    }

    pub fn exit_code(mut self, code: i32) -> Self {
        self.snap.exit_code = Some(code);
        self
    }

    pub fn is_error(mut self, is_error: bool) -> Self {
        self.snap.is_error = is_error;
        if is_error {
            self.snap.status = Status::Error;
        }
        self
    }

    pub fn display_hint(mut self, hint: impl Into<String>) -> Self {
        self.snap.display_hint = Some(hint.into());
        self
    }

    pub fn source_context(mut self, ctx: ContextId) -> Self {
        self.snap.source_context = Some(ctx);
        self
    }

    pub fn source_model(mut self, model: impl Into<String>) -> Self {
        self.snap.source_model = Some(model.into());
        self
    }

    pub fn drift_kind(mut self, kind: DriftKind) -> Self {
        self.snap.drift_kind = Some(kind);
        self
    }

    /// Consume the builder and return the snapshot.
    pub fn build(self) -> BlockSnapshot {
        self.snap
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context() -> ContextId {
        ContextId::new()
    }

    fn test_agent() -> PrincipalId {
        PrincipalId::new()
    }

    // ── BlockId ─────────────────────────────────────────────────────────

    #[test]
    fn test_block_id_construction() {
        let ctx = test_context();
        let agent = test_agent();
        let id = BlockId::new(ctx, agent, 42);
        assert_eq!(id.context_id, ctx);
        assert_eq!(id.agent_id, agent);
        assert_eq!(id.seq, 42);
    }

    #[test]
    fn test_block_id_to_key_from_key_roundtrip() {
        let id = BlockId::new(test_context(), test_agent(), 7);
        let key = id.to_key();
        let parsed = BlockId::from_key(&key).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_block_id_key_format() {
        let ctx = test_context();
        let agent = test_agent();
        let id = BlockId::new(ctx, agent, 99);
        let key = id.to_key();
        // Format: "{32-hex}:{32-hex}:{seq}"
        let parts: Vec<&str> = key.splitn(3, ':').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].len(), 32); // context hex
        assert_eq!(parts[1].len(), 32); // agent hex
        assert_eq!(parts[2], "99");
    }

    #[test]
    fn test_block_id_from_key_rejects_bad_input() {
        assert!(BlockId::from_key("").is_none());
        assert!(BlockId::from_key("a:b").is_none());
        assert!(BlockId::from_key("not-a-uuid:not-a-uuid:1").is_none());
    }

    #[test]
    fn test_block_id_equality() {
        let ctx = test_context();
        let agent = test_agent();
        let a = BlockId::new(ctx, agent, 1);
        let b = BlockId::new(ctx, agent, 1);
        assert_eq!(a, b);
    }

    #[test]
    fn test_block_id_hash_usable_as_map_key() {
        use std::collections::HashMap;
        let id = BlockId::new(test_context(), test_agent(), 1);
        let mut map = HashMap::new();
        map.insert(id, "hello");
        assert_eq!(map.get(&id), Some(&"hello"));
    }

    #[test]
    fn test_block_id_is_copy() {
        let id = BlockId::new(test_context(), test_agent(), 1);
        let a = id; // move
        let b = id; // copy — would fail without Copy
        assert_eq!(a, b);
    }

    #[test]
    fn test_block_id_display() {
        let id = BlockId::new(test_context(), test_agent(), 5);
        let display = id.to_string();
        assert!(display.contains('@'));
        assert!(display.contains('#'));
        assert!(display.ends_with("#5"));
    }

    #[test]
    fn test_block_id_serde_json_roundtrip() {
        let id = BlockId::new(test_context(), test_agent(), 42);
        let json = serde_json::to_string(&id).unwrap();
        let parsed: BlockId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_block_id_postcard_roundtrip() {
        let id = BlockId::new(test_context(), test_agent(), 42);
        let bytes = postcard::to_stdvec(&id).unwrap();
        let parsed: BlockId = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_system_authored_block() {
        let id = BlockId::new(test_context(), PrincipalId::system(), 1);
        assert_eq!(id.agent_id, PrincipalId::system());
    }

    // ── Role ────────────────────────────────────────────────────────────

    #[test]
    fn test_role_parsing() {
        assert_eq!(Role::from_str("user"), Some(Role::User));
        assert_eq!(Role::from_str("MODEL"), Some(Role::Model));
        assert_eq!(Role::from_str("System"), Some(Role::System));
        assert_eq!(Role::from_str("tool"), Some(Role::Tool));
        assert_eq!(Role::from_str("invalid"), None);
        assert_eq!(Role::from_str("human"), Some(Role::User));
        assert_eq!(Role::from_str("assistant"), Some(Role::Model));
        assert_eq!(Role::from_str("agent"), Some(Role::Model));
    }

    // ── Status ──────────────────────────────────────────────────────────

    #[test]
    fn test_status_parsing() {
        assert_eq!(Status::from_str("pending"), Some(Status::Pending));
        assert_eq!(Status::from_str("RUNNING"), Some(Status::Running));
        assert_eq!(Status::from_str("Done"), Some(Status::Done));
        assert_eq!(Status::from_str("error"), Some(Status::Error));
        assert!(Status::Done.is_terminal());
        assert!(Status::Error.is_terminal());
        assert!(!Status::Pending.is_terminal());
        assert!(Status::Running.is_active());
        assert_eq!(Status::from_str("active"), Some(Status::Running));
        assert_eq!(Status::from_str("complete"), Some(Status::Done));
        assert_eq!(Status::from_str("completed"), Some(Status::Done));
    }

    // ── BlockKind ───────────────────────────────────────────────────────

    #[test]
    fn test_block_kind_parsing() {
        assert_eq!(BlockKind::from_str("text"), Some(BlockKind::Text));
        assert_eq!(BlockKind::from_str("THINKING"), Some(BlockKind::Thinking));
        assert_eq!(BlockKind::from_str("tool_call"), Some(BlockKind::ToolCall));
        assert_eq!(BlockKind::from_str("toolresult"), Some(BlockKind::ToolResult));
        assert_eq!(BlockKind::from_str("drift"), Some(BlockKind::Drift));
        assert!(BlockKind::Text.has_text_crdt());
        assert!(BlockKind::ToolCall.is_tool());
        assert!(BlockKind::ToolResult.is_tool());
        assert!(!BlockKind::Text.is_tool());
        assert!(BlockKind::Drift.is_drift());
        assert!(!BlockKind::Text.is_drift());
    }

    #[test]
    fn test_block_kind_no_shell_variants() {
        // Shell is a ToolKind, not a BlockKind
        assert_eq!(BlockKind::from_str("shell_command"), None);
        assert_eq!(BlockKind::from_str("shell_output"), None);
    }

    // ── ToolKind ────────────────────────────────────────────────────────

    #[test]
    fn test_tool_kind_parsing() {
        assert_eq!(ToolKind::from_str("shell"), Some(ToolKind::Shell));
        assert_eq!(ToolKind::from_str("mcp"), Some(ToolKind::Mcp));
        assert_eq!(ToolKind::from_str("builtin"), Some(ToolKind::Builtin));
        assert_eq!(ToolKind::from_str("SHELL"), Some(ToolKind::Shell));
        assert_eq!(ToolKind::from_str("invalid"), None);
    }

    #[test]
    fn test_tool_kind_serde_roundtrip() {
        let tk = ToolKind::Shell;
        let json = serde_json::to_string(&tk).unwrap();
        assert_eq!(json, "\"shell\"");
        let parsed: ToolKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ToolKind::Shell);
    }

    #[test]
    fn test_tool_kind_default_is_shell() {
        assert_eq!(ToolKind::default(), ToolKind::Shell);
    }

    // ── DriftKind ───────────────────────────────────────────────────────

    #[test]
    fn test_drift_kind_parsing() {
        assert_eq!(DriftKind::from_str("push"), Some(DriftKind::Push));
        assert_eq!(DriftKind::from_str("pull"), Some(DriftKind::Pull));
        assert_eq!(DriftKind::from_str("merge"), Some(DriftKind::Merge));
        assert_eq!(DriftKind::from_str("distill"), Some(DriftKind::Distill));
        assert_eq!(DriftKind::from_str("commit"), Some(DriftKind::Commit));
        assert_eq!(DriftKind::from_str("PUSH"), Some(DriftKind::Push));
        assert_eq!(DriftKind::from_str("invalid"), None);
    }

    #[test]
    fn test_drift_kind_serde_roundtrip() {
        let dk = DriftKind::Distill;
        let json = serde_json::to_string(&dk).unwrap();
        assert_eq!(json, "\"distill\"");
        let parsed: DriftKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, DriftKind::Distill);
    }

    // ── BlockSnapshot ───────────────────────────────────────────────────

    #[test]
    fn test_block_snapshot_text() {
        let ctx = test_context();
        let author = test_agent();
        let id = BlockId::new(ctx, author, 1);
        let snap = BlockSnapshot::text(id, None, Role::User, "Hello!");

        assert_eq!(snap.id, id);
        assert!(snap.parent_id.is_none());
        assert_eq!(snap.role, Role::User);
        assert_eq!(snap.status, Status::Done);
        assert_eq!(snap.kind, BlockKind::Text);
        assert_eq!(snap.content, "Hello!");
        assert_eq!(snap.author(), author);
        assert!(!snap.is_collapsed());
        assert!(snap.is_root());
        assert!(!snap.is_shell());
        assert!(snap.tool_kind.is_none());
    }

    #[test]
    fn test_block_snapshot_thinking() {
        let ctx = test_context();
        let author = test_agent();
        let id = BlockId::new(ctx, author, 2);
        let parent = BlockId::new(ctx, author, 1);
        let snap = BlockSnapshot::thinking(id, Some(parent), "Let me think...");

        assert_eq!(snap.kind, BlockKind::Thinking);
        assert_eq!(snap.role, Role::Model);
        assert_eq!(snap.author(), author);
        assert!(snap.has_parent());
        assert!(!snap.is_root());
    }

    #[test]
    fn test_block_snapshot_mcp_tool_call() {
        let ctx = test_context();
        let author = test_agent();
        let id = BlockId::new(ctx, author, 3);
        let input = serde_json::json!({"path": "/etc/hosts"});
        let snap = BlockSnapshot::tool_call(
            id,
            None,
            ToolKind::Mcp,
            "read_file",
            input,
            Role::Model,
        );

        assert_eq!(snap.kind, BlockKind::ToolCall);
        assert_eq!(snap.status, Status::Running);
        assert_eq!(snap.tool_kind, Some(ToolKind::Mcp));
        assert_eq!(snap.tool_name, Some("read_file".to_string()));
        assert_eq!(snap.role, Role::Model);
        assert_eq!(snap.author(), author);
        assert!(!snap.is_shell());
    }

    #[test]
    fn test_block_snapshot_shell_call_by_user() {
        let ctx = test_context();
        let user = test_agent();
        let id = BlockId::new(ctx, user, 1);
        let input = serde_json::json!({"command": "ls -la"});
        let snap = BlockSnapshot::tool_call(
            id,
            None,
            ToolKind::Shell,
            "shell",
            input,
            Role::User,
        );

        assert_eq!(snap.kind, BlockKind::ToolCall);
        assert_eq!(snap.tool_kind, Some(ToolKind::Shell));
        assert_eq!(snap.role, Role::User);
        assert!(snap.is_shell());
    }

    #[test]
    fn test_block_snapshot_shell_call_by_model() {
        let ctx = test_context();
        let model = test_agent();
        let id = BlockId::new(ctx, model, 1);
        let input = serde_json::json!({"command": "cargo build"});
        let snap = BlockSnapshot::tool_call(
            id,
            None,
            ToolKind::Shell,
            "shell",
            input,
            Role::Model,
        );

        assert_eq!(snap.tool_kind, Some(ToolKind::Shell));
        assert_eq!(snap.role, Role::Model);
        assert!(snap.is_shell());
    }

    #[test]
    fn test_block_snapshot_tool_result() {
        let ctx = test_context();
        let author = test_agent();
        let tool_call_id = BlockId::new(ctx, author, 3);
        let id = BlockId::new(ctx, PrincipalId::system(), 1);
        let snap = BlockSnapshot::tool_result(
            id,
            tool_call_id,
            ToolKind::Mcp,
            "file contents here",
            false,
            Some(0),
        );

        assert_eq!(snap.kind, BlockKind::ToolResult);
        assert_eq!(snap.status, Status::Done);
        assert_eq!(snap.tool_kind, Some(ToolKind::Mcp));
        assert_eq!(snap.tool_call_id, Some(tool_call_id));
        assert_eq!(snap.parent_id, Some(tool_call_id));
        assert_eq!(snap.author(), PrincipalId::system());
        assert!(!snap.is_error);
        assert_eq!(snap.exit_code, Some(0));
    }

    #[test]
    fn test_block_snapshot_shell_result() {
        let ctx = test_context();
        let cmd_id = BlockId::new(ctx, test_agent(), 1);
        let id = BlockId::new(ctx, PrincipalId::system(), 1);
        let snap = BlockSnapshot::tool_result(
            id,
            cmd_id,
            ToolKind::Shell,
            "total 42\ndrwxr-xr-x ...",
            false,
            Some(0),
        );

        assert_eq!(snap.kind, BlockKind::ToolResult);
        assert_eq!(snap.tool_kind, Some(ToolKind::Shell));
        assert!(snap.is_shell());
        assert_eq!(snap.parent_id, Some(cmd_id));
    }

    #[test]
    fn test_block_snapshot_shell_result_with_hint() {
        let ctx = test_context();
        let cmd_id = BlockId::new(ctx, test_agent(), 1);
        let id = BlockId::new(ctx, PrincipalId::system(), 1);
        let snap = BlockSnapshot::tool_result_with_hint(
            id,
            cmd_id,
            ToolKind::Shell,
            "output",
            true,
            Some(1),
            Some("ansi".to_string()),
        );

        assert!(snap.is_shell());
        assert!(snap.is_error);
        assert_eq!(snap.status, Status::Error);
        assert_eq!(snap.display_hint, Some("ansi".to_string()));
    }

    #[test]
    fn test_block_snapshot_drift() {
        let ctx = test_context();
        let source_ctx = ContextId::new();
        let author = test_agent();
        let id = BlockId::new(ctx, author, 1);
        let snap = BlockSnapshot::drift(
            id,
            None,
            "CAS has a race condition",
            source_ctx,
            Some("claude-opus-4-6".to_string()),
            DriftKind::Push,
        );

        assert_eq!(snap.kind, BlockKind::Drift);
        assert_eq!(snap.role, Role::System);
        assert_eq!(snap.author(), author);
        assert_eq!(snap.source_context, Some(source_ctx));
        assert_eq!(snap.source_model, Some("claude-opus-4-6".to_string()));
        assert_eq!(snap.drift_kind, Some(DriftKind::Push));
        assert!(!snap.is_shell());
    }

    #[test]
    fn test_block_snapshot_serde_roundtrip() {
        let ctx = test_context();
        let author = test_agent();
        let id = BlockId::new(ctx, author, 1);
        let snap = BlockSnapshot::text(id, None, Role::User, "hello");
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: BlockSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap.id, parsed.id);
        assert_eq!(snap.content, parsed.content);
        assert_eq!(snap.author(), parsed.author());
    }

    #[test]
    fn test_block_snapshot_skip_serializing_none_fields() {
        let ctx = test_context();
        let author = test_agent();
        let id = BlockId::new(ctx, author, 1);
        let snap = BlockSnapshot::text(id, None, Role::User, "hello");
        let json = serde_json::to_string(&snap).unwrap();
        // None fields should be absent, not "null"
        assert!(!json.contains("tool_kind"));
        assert!(!json.contains("tool_name"));
        assert!(!json.contains("drift_kind"));
        assert!(!json.contains("parent_id"));
        // false bools should be absent too
        assert!(!json.contains("collapsed"));
        assert!(!json.contains("is_error"));
        // Deserialize back — defaults fill in
        let parsed: BlockSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tool_kind, None);
        assert!(!parsed.collapsed);
        assert!(!parsed.is_error);
    }

    #[test]
    fn test_block_snapshot_tool_kind_serde_roundtrip() {
        let ctx = test_context();
        let author = test_agent();
        let id = BlockId::new(ctx, author, 1);
        let input = serde_json::json!({"cmd": "ls"});
        let snap = BlockSnapshot::tool_call(
            id,
            None,
            ToolKind::Shell,
            "shell",
            input,
            Role::User,
        );
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: BlockSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tool_kind, Some(ToolKind::Shell));
    }

    // ── Builder ─────────────────────────────────────────────────────────

    #[test]
    fn test_builder_minimal() {
        let ctx = test_context();
        let author = test_agent();
        let id = BlockId::new(ctx, author, 1);
        let snap = BlockSnapshotBuilder::new(id, BlockKind::Text).build();

        assert_eq!(snap.id, id);
        assert_eq!(snap.kind, BlockKind::Text);
        assert_eq!(snap.author(), author);
        assert_eq!(snap.status, Status::Done);
        assert_eq!(snap.content, "");
    }

    #[test]
    fn test_builder_shell_tool_call() {
        let ctx = test_context();
        let author = test_agent();
        let id = BlockId::new(ctx, author, 1);
        let snap = BlockSnapshotBuilder::new(id, BlockKind::ToolCall)
            .role(Role::User)
            .tool_kind(ToolKind::Shell)
            .tool_name("shell")
            .content("ls -la")
            .status(Status::Running)
            .build();

        assert_eq!(snap.kind, BlockKind::ToolCall);
        assert_eq!(snap.role, Role::User);
        assert_eq!(snap.tool_kind, Some(ToolKind::Shell));
        assert_eq!(snap.tool_name, Some("shell".to_string()));
        assert_eq!(snap.content, "ls -la");
        assert_eq!(snap.status, Status::Running);
        assert!(snap.is_shell());
    }

    #[test]
    fn test_builder_drift() {
        let ctx = test_context();
        let source = ContextId::new();
        let author = test_agent();
        let id = BlockId::new(ctx, author, 1);
        let snap = BlockSnapshotBuilder::new(id, BlockKind::Drift)
            .role(Role::System)
            .content("finding from other context")
            .source_context(source)
            .drift_kind(DriftKind::Push)
            .source_model("claude-opus-4-6")
            .build();

        assert_eq!(snap.kind, BlockKind::Drift);
        assert_eq!(snap.source_context, Some(source));
        assert_eq!(snap.drift_kind, Some(DriftKind::Push));
    }

    #[test]
    fn test_builder_is_error_sets_status() {
        let ctx = test_context();
        let author = test_agent();
        let id = BlockId::new(ctx, author, 1);
        let snap = BlockSnapshotBuilder::new(id, BlockKind::ToolResult)
            .is_error(true)
            .exit_code(1)
            .build();

        assert!(snap.is_error);
        assert_eq!(snap.status, Status::Error);
        assert_eq!(snap.exit_code, Some(1));
    }

    #[test]
    fn test_builder_exhaustive() {
        // Exercises every builder method to catch any that forget to map their field.
        let ctx = test_context();
        let author = test_agent();
        let id = BlockId::new(ctx, author, 1);
        let parent_id = BlockId::new(ctx, author, 0);
        let call_id = BlockId::new(ctx, author, 2);
        let source = ContextId::new();

        let snap = BlockSnapshotBuilder::new(id, BlockKind::ToolResult)
            .parent_id(parent_id)
            .role(Role::Tool)
            .status(Status::Error)
            .content("error output")
            .collapsed(true)
            .compacted(true)
            .tool_kind(ToolKind::Shell)
            .tool_name("shell")
            .tool_input("{\"cmd\":\"ls\"}")
            .tool_call_id(call_id)
            .exit_code(127)
            .is_error(true)
            .display_hint("ansi")
            .source_context(source)
            .source_model("gemini-pro")
            .drift_kind(DriftKind::Distill)
            .build();

        assert_eq!(snap.id, id);
        assert_eq!(snap.parent_id, Some(parent_id));
        assert_eq!(snap.role, Role::Tool);
        assert_eq!(snap.status, Status::Error);
        assert_eq!(snap.content, "error output");
        assert!(snap.collapsed);
        assert!(snap.compacted);
        assert_eq!(snap.tool_kind, Some(ToolKind::Shell));
        assert_eq!(snap.tool_name, Some("shell".to_string()));
        assert_eq!(snap.tool_input, Some("{\"cmd\":\"ls\"}".to_string()));
        assert_eq!(snap.tool_call_id, Some(call_id));
        assert_eq!(snap.exit_code, Some(127));
        assert!(snap.is_error);
        assert_eq!(snap.display_hint, Some("ansi".to_string()));
        assert_eq!(snap.source_context, Some(source));
        assert_eq!(snap.source_model, Some("gemini-pro".to_string()));
        assert_eq!(snap.drift_kind, Some(DriftKind::Distill));
    }

    // ── content_eq ──────────────────────────────────────────────────────

    #[test]
    fn test_content_eq_ignores_timestamp() {
        let ctx = test_context();
        let author = test_agent();
        let id = BlockId::new(ctx, author, 1);
        let a = BlockSnapshot::text(id, None, Role::User, "hello");
        // Simulate a different timestamp
        let mut b = a.clone();
        b.created_at = a.created_at + 1000;
        assert_ne!(a, b); // PartialEq sees the difference
        assert!(a.content_eq(&b)); // content_eq ignores it
    }

    // ── compacted ──────────────────────────────────────────────────────

    #[test]
    fn test_compacted_defaults_to_false() {
        let id = BlockId::new(test_context(), test_agent(), 1);
        let snap = BlockSnapshot::text(id, None, Role::User, "hello");
        assert!(!snap.compacted);
    }

    #[test]
    fn test_compacted_skipped_in_json_when_false() {
        let id = BlockId::new(test_context(), test_agent(), 1);
        let snap = BlockSnapshot::text(id, None, Role::User, "hello");
        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains("compacted"));
    }

    #[test]
    fn test_compacted_roundtrip_when_true() {
        let id = BlockId::new(test_context(), test_agent(), 1);
        let mut snap = BlockSnapshot::text(id, None, Role::User, "hello");
        snap.compacted = true;
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"compacted\":true"));
        let parsed: BlockSnapshot = serde_json::from_str(&json).unwrap();
        assert!(parsed.compacted);
    }

    // ── BlockHeader ────────────────────────────────────────────────────

    #[test]
    fn test_block_header_from_snapshot() {
        let ctx = test_context();
        let author = test_agent();
        let parent = BlockId::new(ctx, author, 0);
        let id = BlockId::new(ctx, author, 1);
        let snap = BlockSnapshot::text(id, Some(parent), Role::User, "hello");
        let header = BlockHeader::from_snapshot(&snap);

        assert_eq!(header.id, snap.id);
        assert_eq!(header.parent_id, snap.parent_id);
        assert_eq!(header.role, snap.role);
        assert_eq!(header.kind, snap.kind);
        assert_eq!(header.status, snap.status);
        assert_eq!(header.compacted, snap.compacted);
        assert_eq!(header.created_at, snap.created_at);
    }

    #[test]
    fn test_block_header_is_root() {
        let id = BlockId::new(test_context(), test_agent(), 1);
        let snap = BlockSnapshot::text(id, None, Role::User, "root");
        let header = BlockHeader::from_snapshot(&snap);
        assert!(header.is_root());
        assert!(!header.has_parent());
    }

    #[test]
    fn test_block_header_is_tool() {
        let ctx = test_context();
        let author = test_agent();
        let id = BlockId::new(ctx, author, 1);
        let snap = BlockSnapshot::tool_call(
            id,
            None,
            ToolKind::Shell,
            "shell",
            serde_json::json!({"cmd": "ls"}),
            Role::User,
        );
        let header = BlockHeader::from_snapshot(&snap);
        assert!(header.is_tool());
    }

    #[test]
    fn test_block_header_is_copy() {
        let id = BlockId::new(test_context(), test_agent(), 1);
        let snap = BlockSnapshot::text(id, None, Role::User, "hello");
        let header = BlockHeader::from_snapshot(&snap);
        let a = header;
        let b = header; // copy — would fail without Copy
        assert_eq!(a, b);
    }

    #[test]
    fn test_block_header_serde_roundtrip() {
        let id = BlockId::new(test_context(), test_agent(), 1);
        let snap = BlockSnapshot::text(id, None, Role::User, "hello");
        let header = BlockHeader::from_snapshot(&snap);
        let json = serde_json::to_string(&header).unwrap();
        let parsed: BlockHeader = serde_json::from_str(&json).unwrap();
        assert_eq!(header, parsed);
    }

    // ── MAX_DAG_DEPTH ──────────────────────────────────────────────────

    #[test]
    fn test_max_dag_depth_is_512() {
        assert_eq!(MAX_DAG_DEPTH, 512);
    }
}
