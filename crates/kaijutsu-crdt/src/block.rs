//! Block types and identifiers.
//!
//! DAG-native block model with parent/child relationships, role tracking, and status management.
//! All blocks are CRDT-tracked via the unified diamond-types OpLog.

use serde::{Deserialize, Serialize};
use std::str::FromStr;
use strum::EnumString;

/// Globally unique block identifier.
///
/// Composed of:
/// - `document_id`: The document this block belongs to
/// - `agent_id`: The agent that created this block
/// - `seq`: Agent-local sequence number (monotonically increasing)
///
/// This ensures global uniqueness without coordination.
#[derive(Clone, Eq, Hash, PartialEq, Debug, Serialize, Deserialize)]
pub struct BlockId {
    /// Document this block belongs to.
    pub document_id: String,
    /// Agent that created this block.
    pub agent_id: String,
    /// Agent-local sequence number.
    pub seq: u64,
}

impl BlockId {
    /// Create a new block ID.
    ///
    /// # Panics
    /// Panics if `document_id` or `agent_id` contain `/` (used as key separator).
    pub fn new(document_id: impl Into<String>, agent_id: impl Into<String>, seq: u64) -> Self {
        let document_id = document_id.into();
        let agent_id = agent_id.into();
        assert!(!document_id.contains('/'), "document_id must not contain '/'");
        assert!(!agent_id.contains('/'), "agent_id must not contain '/'");
        Self {
            document_id,
            agent_id,
            seq,
        }
    }

    /// Convert to a compact string representation.
    pub fn to_key(&self) -> String {
        format!("{}/{}/{}", self.document_id, self.agent_id, self.seq)
    }

    /// Parse from key string.
    pub fn from_key(key: &str) -> Option<Self> {
        let parts: Vec<&str> = key.splitn(3, '/').collect();
        if parts.len() != 3 {
            return None;
        }
        Some(Self {
            document_id: parts[0].to_string(),
            agent_id: parts[1].to_string(),
            seq: parts[2].parse().ok()?,
        })
    }
}

impl std::fmt::Display for BlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}#{}", self.document_id, self.agent_id, self.seq)
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
    /// Model (AI model - Claude, GPT, etc.).
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
    /// Supports aliases: "human" → User, "assistant"/"agent" → Model.
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
    ///
    /// Supports aliases: "active" → Running, "complete"/"completed" → Done.
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

/// Block content type (what kind of content this block holds).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, EnumString)]
#[serde(rename_all = "lowercase")]
#[strum(ascii_case_insensitive)]
pub enum BlockKind {
    /// Main text response.
    #[default]
    Text,
    /// Extended thinking/reasoning - collapsible.
    Thinking,
    /// Tool invocation - content (input JSON) is streamable via Text CRDT.
    #[serde(rename = "tool_call")]
    #[strum(serialize = "tool_call", serialize = "toolcall")]
    ToolCall,
    /// Tool result - content is streamable via Text CRDT.
    #[serde(rename = "tool_result")]
    #[strum(serialize = "tool_result", serialize = "toolresult")]
    ToolResult,
    /// Shell command entered by user (kaish REPL).
    #[serde(rename = "shell_command")]
    #[strum(serialize = "shell_command", serialize = "shellcommand")]
    ShellCommand,
    /// Shell command output/result (stdout, exit code).
    #[serde(rename = "shell_output")]
    #[strum(serialize = "shell_output", serialize = "shelloutput")]
    ShellOutput,
    /// Drifted content from another context (cross-context transfer).
    #[serde(rename = "drift")]
    #[strum(serialize = "drift")]
    Drift,
}

impl BlockKind {
    /// Parse from string (case-insensitive).
    ///
    /// Supports aliases: "toolcall" → ToolCall, "toolresult" → ToolResult, etc.
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
            BlockKind::ShellCommand => "shell_command",
            BlockKind::ShellOutput => "shell_output",
            BlockKind::Drift => "drift",
        }
    }

    /// Check if this block type has editable text content via Text CRDT.
    ///
    /// All block types support Text CRDT for their primary content field, enabling streaming.
    pub fn has_text_crdt(&self) -> bool {
        true
    }

    /// Check if this is a tool-related block.
    pub fn is_tool(&self) -> bool {
        matches!(self, BlockKind::ToolCall | BlockKind::ToolResult)
    }

    /// Check if this is a shell-related block.
    pub fn is_shell(&self) -> bool {
        matches!(self, BlockKind::ShellCommand | BlockKind::ShellOutput)
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

/// How a drift block arrived from another context.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, EnumString)]
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
/// This is a flat struct with all fields. Tool-specific fields are `Option` types -
/// only populated for relevant block kinds.
///
/// Used for:
/// - Initial block creation
/// - Full state sync
/// - Wire protocol serialization
/// - Reading current block state
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockSnapshot {
    /// Block ID.
    pub id: BlockId,
    /// Parent block ID (DAG edge - None for root blocks).
    pub parent_id: Option<BlockId>,
    /// Role of the block author (human, agent, system, tool).
    pub role: Role,
    /// Execution status (pending, running, done, error).
    pub status: Status,
    /// Content type (text, thinking, tool_call, tool_result).
    pub kind: BlockKind,
    /// Primary text content.
    pub content: String,
    /// Whether this block is collapsed (only meaningful for Thinking).
    pub collapsed: bool,
    /// Author who created this block (e.g., "user:amy", "model:claude").
    pub author: String,
    /// Timestamp when block was created (Unix millis).
    pub created_at: u64,

    // Tool-specific dedicated fields

    /// Tool name (for ToolCall blocks).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Tool input as JSON (for ToolCall blocks).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<serde_json::Value>,
    /// Reference to parent ToolCall block (for ToolResult blocks).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<BlockId>,
    /// Exit code from tool execution (for ToolResult blocks).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Whether this is an error result (for ToolResult blocks).
    #[serde(default)]
    pub is_error: bool,
    /// Display hint for richer output formatting (JSON-serialized).
    /// Used for shell output blocks to enable per-viewer rendering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_hint: Option<String>,

    // Drift-specific fields (for Drift blocks)

    /// Short ID of the originating context (for Drift blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_context: Option<String>,
    /// Model that produced this content (for Drift blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_model: Option<String>,
    /// How this block arrived from another context (for Drift blocks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drift_kind: Option<DriftKind>,
}

impl BlockSnapshot {
    /// Create a new text block snapshot.
    pub fn text(
        id: BlockId,
        parent_id: Option<BlockId>,
        role: Role,
        content: impl Into<String>,
        author: impl Into<String>,
    ) -> Self {
        Self {
            id,
            parent_id,
            role,
            status: Status::Done,
            kind: BlockKind::Text,
            content: content.into(),
            collapsed: false,
            author: author.into(),
            created_at: Self::now_millis(),
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
        author: impl Into<String>,
    ) -> Self {
        Self {
            id,
            parent_id,
            role: Role::Model,
            status: Status::Done,
            kind: BlockKind::Thinking,
            content: content.into(),
            collapsed: false,
            author: author.into(),
            created_at: Self::now_millis(),
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

    /// Create a new tool call block snapshot.
    pub fn tool_call(
        id: BlockId,
        parent_id: Option<BlockId>,
        tool_name: impl Into<String>,
        tool_input: serde_json::Value,
        author: impl Into<String>,
    ) -> Self {
        let input_json = serde_json::to_string_pretty(&tool_input).unwrap_or_default();
        Self {
            id,
            parent_id,
            role: Role::Model,
            status: Status::Running,
            kind: BlockKind::ToolCall,
            content: input_json,
            collapsed: false,
            author: author.into(),
            created_at: Self::now_millis(),
            tool_name: Some(tool_name.into()),
            tool_input: Some(tool_input),
            tool_call_id: None,
            exit_code: None,
            is_error: false,
            display_hint: None,
            source_context: None,
            source_model: None,
            drift_kind: None,
        }
    }

    /// Create a new tool result block snapshot.
    pub fn tool_result(
        id: BlockId,
        tool_call_id: BlockId,
        content: impl Into<String>,
        is_error: bool,
        exit_code: Option<i32>,
        author: impl Into<String>,
    ) -> Self {
        Self {
            id,
            parent_id: Some(tool_call_id.clone()),
            role: Role::Tool,
            status: if is_error { Status::Error } else { Status::Done },
            kind: BlockKind::ToolResult,
            content: content.into(),
            collapsed: false,
            author: author.into(),
            created_at: Self::now_millis(),
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

    /// Create a new shell command block (user input in shell mode).
    pub fn shell_command(
        id: BlockId,
        parent_id: Option<BlockId>,
        content: impl Into<String>,
        author: impl Into<String>,
    ) -> Self {
        Self {
            id,
            parent_id,
            role: Role::User,
            status: Status::Done,
            kind: BlockKind::ShellCommand,
            content: content.into(),
            collapsed: false,
            author: author.into(),
            created_at: Self::now_millis(),
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

    /// Create a new shell output block (kaish execution result).
    pub fn shell_output(
        id: BlockId,
        command_block_id: BlockId,
        content: impl Into<String>,
        is_error: bool,
        exit_code: Option<i32>,
        author: impl Into<String>,
    ) -> Self {
        Self {
            id,
            parent_id: Some(command_block_id.clone()),
            role: Role::System,
            status: if is_error { Status::Error } else { Status::Done },
            kind: BlockKind::ShellOutput,
            content: content.into(),
            collapsed: false,
            author: author.into(),
            created_at: Self::now_millis(),
            tool_name: None,
            tool_input: None,
            tool_call_id: None,
            exit_code,
            is_error,
            display_hint: None,
            source_context: None,
            source_model: None,
            drift_kind: None,
        }
    }

    /// Create a new shell output block with a display hint.
    pub fn shell_output_with_hint(
        id: BlockId,
        command_block_id: BlockId,
        content: impl Into<String>,
        is_error: bool,
        exit_code: Option<i32>,
        author: impl Into<String>,
        display_hint: Option<String>,
    ) -> Self {
        Self {
            id,
            parent_id: Some(command_block_id.clone()),
            role: Role::System,
            status: if is_error { Status::Error } else { Status::Done },
            kind: BlockKind::ShellOutput,
            content: content.into(),
            collapsed: false,
            author: author.into(),
            created_at: Self::now_millis(),
            tool_name: None,
            tool_input: None,
            tool_call_id: None,
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
        author: impl Into<String>,
        source_context: impl Into<String>,
        source_model: Option<String>,
        drift_kind: DriftKind,
    ) -> Self {
        Self {
            id,
            parent_id,
            role: Role::System,
            status: Status::Done,
            kind: BlockKind::Drift,
            content: content.into(),
            collapsed: false,
            author: author.into(),
            created_at: Self::now_millis(),
            tool_name: None,
            tool_input: None,
            tool_call_id: None,
            exit_code: None,
            is_error: false,
            display_hint: None,
            source_context: Some(source_context.into()),
            source_model,
            drift_kind: Some(drift_kind),
        }
    }

    /// Get current timestamp in milliseconds.
    fn now_millis() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Get text content (always available via content field).
    pub fn text_content(&self) -> &str {
        &self.content
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_id() {
        let id = BlockId::new("doc-1", "alice", 42);
        assert_eq!(id.to_key(), "doc-1/alice/42");
        assert_eq!(BlockId::from_key("doc-1/alice/42"), Some(id.clone()));
        assert_eq!(id.to_string(), "doc-1@alice#42");
    }

    #[test]
    fn test_role_parsing() {
        // Test basic parsing
        assert_eq!(Role::from_str("user"), Some(Role::User));
        assert_eq!(Role::from_str("MODEL"), Some(Role::Model));
        assert_eq!(Role::from_str("System"), Some(Role::System));
        assert_eq!(Role::from_str("tool"), Some(Role::Tool));
        assert_eq!(Role::from_str("invalid"), None);
        // Test aliases
        assert_eq!(Role::from_str("human"), Some(Role::User));
        assert_eq!(Role::from_str("assistant"), Some(Role::Model));
        assert_eq!(Role::from_str("agent"), Some(Role::Model));
    }

    #[test]
    fn test_status_parsing() {
        // Test basic parsing
        assert_eq!(Status::from_str("pending"), Some(Status::Pending));
        assert_eq!(Status::from_str("RUNNING"), Some(Status::Running));
        assert_eq!(Status::from_str("Done"), Some(Status::Done));
        assert_eq!(Status::from_str("error"), Some(Status::Error));
        assert!(Status::Done.is_terminal());
        assert!(Status::Error.is_terminal());
        assert!(!Status::Pending.is_terminal());
        assert!(Status::Running.is_active());
        // Test aliases
        assert_eq!(Status::from_str("active"), Some(Status::Running));
        assert_eq!(Status::from_str("complete"), Some(Status::Done));
        assert_eq!(Status::from_str("completed"), Some(Status::Done));
    }

    #[test]
    fn test_block_kind_parsing() {
        assert_eq!(BlockKind::from_str("text"), Some(BlockKind::Text));
        assert_eq!(BlockKind::from_str("THINKING"), Some(BlockKind::Thinking));
        assert_eq!(BlockKind::from_str("tool_call"), Some(BlockKind::ToolCall));
        assert_eq!(BlockKind::from_str("toolresult"), Some(BlockKind::ToolResult));
        assert!(BlockKind::Text.has_text_crdt());
        assert!(BlockKind::ToolCall.is_tool());
        assert!(BlockKind::ToolResult.is_tool());
        assert!(!BlockKind::Text.is_tool());
    }

    #[test]
    fn test_block_snapshot_text() {
        let id = BlockId::new("doc-1", "alice", 1);
        let snap = BlockSnapshot::text(id.clone(), None, Role::User, "Hello!", "user:amy");

        assert_eq!(snap.id, id);
        assert!(snap.parent_id.is_none());
        assert_eq!(snap.role, Role::User);
        assert_eq!(snap.status, Status::Done);
        assert_eq!(snap.kind, BlockKind::Text);
        assert_eq!(snap.text_content(), "Hello!");
        assert!(!snap.is_collapsed());
        assert!(snap.is_root());
    }

    #[test]
    fn test_block_snapshot_thinking() {
        let id = BlockId::new("doc-1", "alice", 2);
        let parent = BlockId::new("doc-1", "alice", 1);
        let snap = BlockSnapshot::thinking(id.clone(), Some(parent.clone()), "Let me think...", "model:claude");

        assert_eq!(snap.kind, BlockKind::Thinking);
        assert_eq!(snap.role, Role::Model);
        assert!(!snap.is_collapsed());
        assert!(snap.has_parent());
        assert!(!snap.is_root());
    }

    #[test]
    fn test_block_snapshot_tool_call() {
        let id = BlockId::new("doc-1", "alice", 3);
        let input = serde_json::json!({"path": "/etc/hosts"});
        let snap = BlockSnapshot::tool_call(id.clone(), None, "read_file", input.clone(), "model:claude");

        assert_eq!(snap.kind, BlockKind::ToolCall);
        assert_eq!(snap.status, Status::Running);
        assert_eq!(snap.tool_name, Some("read_file".to_string()));
        assert_eq!(snap.tool_input, Some(input));
    }

    #[test]
    fn test_block_snapshot_tool_result() {
        let tool_call_id = BlockId::new("doc-1", "alice", 3);
        let id = BlockId::new("doc-1", "alice", 4);
        let snap = BlockSnapshot::tool_result(
            id.clone(),
            tool_call_id.clone(),
            "file contents here",
            false,
            Some(0),
            "system",
        );

        assert_eq!(snap.kind, BlockKind::ToolResult);
        assert_eq!(snap.status, Status::Done);
        assert_eq!(snap.tool_call_id, Some(tool_call_id.clone()));
        assert_eq!(snap.parent_id, Some(tool_call_id));
        assert!(!snap.is_error);
        assert_eq!(snap.exit_code, Some(0));
    }

    #[test]
    fn test_block_kind_drift() {
        assert_eq!(BlockKind::from_str("drift"), Some(BlockKind::Drift));
        assert!(BlockKind::Drift.is_drift());
        assert!(!BlockKind::Text.is_drift());
        assert!(BlockKind::Drift.has_text_crdt());
        assert!(!BlockKind::Drift.is_tool());
        assert!(!BlockKind::Drift.is_shell());
    }

    #[test]
    fn test_drift_kind_parsing() {
        assert_eq!(DriftKind::from_str("push"), Some(DriftKind::Push));
        assert_eq!(DriftKind::from_str("pull"), Some(DriftKind::Pull));
        assert_eq!(DriftKind::from_str("merge"), Some(DriftKind::Merge));
        assert_eq!(DriftKind::from_str("distill"), Some(DriftKind::Distill));
        assert_eq!(DriftKind::from_str("PUSH"), Some(DriftKind::Push));
        assert_eq!(DriftKind::from_str("invalid"), None);
        assert_eq!(DriftKind::Push.as_str(), "push");
        assert_eq!(DriftKind::Distill.as_str(), "distill");
    }

    #[test]
    fn test_block_snapshot_drift() {
        let id = BlockId::new("doc-1", "drift", 1);
        let snap = BlockSnapshot::drift(
            id.clone(),
            None,
            "CAS has a race condition in the merge path",
            "drift:a1b2c3",
            "a1b2c3",
            Some("claude-opus-4-6".to_string()),
            DriftKind::Push,
        );

        assert_eq!(snap.id, id);
        assert_eq!(snap.kind, BlockKind::Drift);
        assert_eq!(snap.role, Role::System);
        assert_eq!(snap.status, Status::Done);
        assert_eq!(snap.source_context, Some("a1b2c3".to_string()));
        assert_eq!(snap.source_model, Some("claude-opus-4-6".to_string()));
        assert_eq!(snap.drift_kind, Some(DriftKind::Push));
        assert_eq!(snap.content, "CAS has a race condition in the merge path");
        assert!(snap.tool_name.is_none());
    }

    #[test]
    fn test_drift_kind_serde_roundtrip() {
        let dk = DriftKind::Distill;
        let json = serde_json::to_string(&dk).unwrap();
        assert_eq!(json, "\"distill\"");
        let parsed: DriftKind = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, DriftKind::Distill);
    }

    // Part 4a: BlockId path security tests

    #[test]
    #[should_panic(expected = "document_id must not contain '/'")]
    fn test_block_id_rejects_slash_in_document_id() {
        BlockId::new("doc/evil", "agent", 1);
    }

    #[test]
    #[should_panic(expected = "agent_id must not contain '/'")]
    fn test_block_id_rejects_slash_in_agent_id() {
        BlockId::new("doc", "agent/evil", 1);
    }

    #[test]
    fn test_block_id_accepts_valid_ids() {
        let id = BlockId::new("doc", "agent", 1);
        assert_eq!(id.document_id, "doc");
        assert_eq!(id.agent_id, "agent");
        assert_eq!(id.seq, 1);
    }

    #[test]
    fn test_block_id_from_key_rejects_extra_slashes() {
        // from_key uses splitn(3, '/'), so "a/b/c/d" splits into ["a", "b", "c/d"]
        // The third part "c/d" will fail to parse as u64, returning None
        assert_eq!(BlockId::from_key("a/b/c/d"), None);
    }
}
