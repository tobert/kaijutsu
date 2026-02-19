//! MCP request and response types.
//!
//! These types define the API for the Kaijutsu MCP server tools.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

// ============================================================================
// Request Types
// ============================================================================

/// Create a new document for collaborative editing.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocCreateRequest {
    /// Unique document identifier
    #[schemars(description = "Unique document identifier")]
    pub id: String,
    /// Document type: conversation, code, text, or git
    #[schemars(description = "Document type: conversation, code, text, or git")]
    pub kind: String,
    /// Programming language (for code documents)
    #[schemars(description = "Programming language (for code documents)")]
    pub language: Option<String>,
}

/// Delete a document.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocDeleteRequest {
    /// Document ID to delete
    #[schemars(description = "Document ID to delete")]
    pub id: String,
}

/// Create a new block within a document.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlockCreateRequest {
    /// Document ID to create block in
    #[schemars(description = "Document ID to create block in")]
    pub document_id: String,
    /// Parent block ID for DAG relationship (omit for root)
    #[schemars(description = "Parent block ID for DAG relationship (omit for root)")]
    pub parent_id: Option<String>,
    /// Block to insert after (for ordering)
    #[schemars(description = "Block ID to insert after (for ordering)")]
    pub after_id: Option<String>,
    /// Role: user, model, system, or tool
    #[schemars(description = "Role: user, model, system, or tool")]
    pub role: String,
    /// Block kind: text, thinking, tool_call, or tool_result
    #[schemars(description = "Block kind: text, thinking, tool_call, or tool_result")]
    pub kind: String,
    /// Initial content
    #[schemars(description = "Initial text content")]
    pub content: Option<String>,
}

/// Read block content.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlockReadRequest {
    /// Block ID to read (format: document_id/agent_id/seq)
    #[schemars(description = "Block ID to read (format: document_id/agent_id/seq)")]
    pub block_id: String,
    /// Include line numbers in output
    #[schemars(description = "Include line numbers in output (default: true)")]
    #[serde(default = "default_true")]
    pub line_numbers: bool,
    /// Line range [start, end) to read (0-indexed)
    #[schemars(description = "Line range [start, end) to read (0-indexed, exclusive end)")]
    pub range: Option<Vec<u32>>,
}

fn default_true() -> bool {
    true
}

/// Append text to a block (streaming-friendly).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlockAppendRequest {
    /// Block ID to append to
    #[schemars(description = "Block ID to append to (format: document_id/agent_id/seq)")]
    pub block_id: String,
    /// Text to append
    #[schemars(description = "Text to append")]
    pub text: String,
}

/// Edit operation within a block.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum EditOp {
    /// Insert text before a line
    Insert {
        #[schemars(description = "Line number to insert before (0-indexed)")]
        line: u32,
        #[schemars(description = "Content to insert")]
        content: String,
    },
    /// Delete lines [start, end)
    Delete {
        #[schemars(description = "Start line (0-indexed)")]
        start_line: u32,
        #[schemars(description = "End line (exclusive)")]
        end_line: u32,
    },
    /// Replace lines with new content
    Replace {
        #[schemars(description = "Start line (0-indexed)")]
        start_line: u32,
        #[schemars(description = "End line (exclusive)")]
        end_line: u32,
        #[schemars(description = "Replacement content")]
        content: String,
        /// Expected text for compare-and-set validation
        #[schemars(description = "Expected text for CAS validation (fails if mismatch)")]
        #[serde(default)]
        expected_text: Option<String>,
    },
}

/// Edit a block with line-based operations.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlockEditRequest {
    /// Block ID to edit
    #[schemars(description = "Block ID to edit (format: document_id/agent_id/seq)")]
    pub block_id: String,
    /// Edit operations to apply atomically
    #[schemars(description = "Edit operations to apply atomically")]
    pub operations: Vec<EditOp>,
}

/// List blocks with optional filters.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlockListRequest {
    /// Filter to specific document
    #[schemars(description = "Filter to specific document ID")]
    pub document_id: Option<String>,
    /// Filter by block kind
    #[schemars(description = "Filter by block kind: text, thinking, tool_call, tool_result")]
    pub kind: Option<String>,
    /// Filter by status
    #[schemars(description = "Filter by status: pending, running, done, error")]
    pub status: Option<String>,
    /// Filter by role
    #[schemars(description = "Filter by role: user, model, system, tool")]
    pub role: Option<String>,
}

/// Set block status.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlockStatusRequest {
    /// Block ID to update
    #[schemars(description = "Block ID to update (format: document_id/agent_id/seq)")]
    pub block_id: String,
    /// New status: pending, running, done, or error
    #[schemars(description = "New status: pending, running, done, or error")]
    pub status: String,
}

/// Search across blocks.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KernelSearchRequest {
    /// Regex pattern to search for
    #[schemars(description = "Regex pattern to search for")]
    pub query: String,
    /// Limit search to specific document
    #[schemars(description = "Limit search to specific document ID")]
    pub document_id: Option<String>,
    /// Filter by block kind
    #[schemars(description = "Filter by block kind: text, thinking, tool_call, tool_result")]
    pub kind: Option<String>,
    /// Filter by role
    #[schemars(description = "Filter by role: user, model, system, tool")]
    pub role: Option<String>,
    /// Context lines around matches
    #[schemars(description = "Lines of context before/after each match (default: 2)")]
    #[serde(default = "default_context")]
    pub context_lines: u32,
    /// Maximum matches to return
    #[schemars(description = "Maximum matches to return (default: 100)")]
    #[serde(default = "default_max_matches")]
    pub max_matches: usize,
}

fn default_context() -> u32 {
    2
}

fn default_max_matches() -> usize {
    100
}

/// Display a document's conversation DAG as an ASCII tree.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocTreeRequest {
    /// Document ID to visualize
    #[schemars(description = "Document ID to visualize")]
    pub document_id: String,
    /// Maximum tree depth to display
    #[schemars(description = "Maximum tree depth to display (omit for full tree)")]
    pub max_depth: Option<u32>,
    /// Show tool_call and tool_result as separate expanded nodes
    #[schemars(description = "Show tool_call and tool_result as separate nodes (default: false, collapsed shows as single line)")]
    #[serde(default)]
    pub expand_tools: bool,
}

/// Inspect CRDT internals of a block for debugging.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlockInspectRequest {
    /// Block ID to inspect
    #[schemars(description = "Block ID to inspect (format: document_id/agent_id/seq)")]
    pub block_id: String,
}

/// Get version history of a block.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlockHistoryRequest {
    /// Block ID to get history for
    #[schemars(description = "Block ID to get history for (format: document_id/agent_id/seq)")]
    pub block_id: String,
    /// Maximum number of versions to show
    #[schemars(description = "Maximum number of versions to show (default: all)")]
    pub limit: Option<u32>,
}

/// Compare block content showing a unified diff.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlockDiffRequest {
    /// Block ID to compare
    #[schemars(description = "Block ID to compare (format: document_id/agent_id/seq)")]
    pub block_id: String,
    /// Text to compare against current content
    #[schemars(description = "Original text to diff against (if not provided, shows current content summary)")]
    pub original: Option<String>,
}

/// Preview recent operations that could be undone (dry-run only).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocUndoRequest {
    /// Document ID to inspect
    #[schemars(description = "Document ID to inspect for recent operations")]
    pub document_id: String,
    /// Number of recent blocks to show
    #[schemars(description = "Number of recent blocks to preview (default: 3)")]
    #[serde(default = "default_undo_steps")]
    pub steps: u32,
}

fn default_undo_steps() -> u32 {
    3
}

// ============================================================================
// Kaish Execution Types
// ============================================================================

/// Execute a tool through the kernel's tool registry (git, drift, etc.).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KaishExecRequest {
    /// Tool name to execute (e.g., "git", "drift", "search")
    #[schemars(description = "Tool name to execute (e.g., 'git', 'drift', 'search')")]
    pub tool: String,
    /// JSON parameters for the tool
    #[schemars(description = "JSON parameters for the tool (tool-specific)")]
    #[serde(default = "default_empty_params")]
    pub params: String,
}

fn default_empty_params() -> String {
    "{}".to_string()
}

/// Execute a kaish command through the kernel's embedded shell.
/// Output is written to CRDT blocks and observable in kaijutsu-app.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ShellRequest {
    /// The kaish command to execute (e.g., "cargo check", "git status", "ls -la")
    #[schemars(description = "The kaish command to execute (e.g., 'cargo check', 'git status')")]
    pub command: String,
    /// Timeout in seconds (default: 300)
    #[schemars(description = "Timeout in seconds (default: 300, max: 600)")]
    pub timeout_secs: Option<u64>,
}

// ============================================================================
// Drift Types
// ============================================================================

/// Push content to another context's staging queue.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DriftPushRequest {
    /// Target context (short hex ID or label)
    #[schemars(description = "Target context — short hex ID (e.g., 'a1b2c3') or label (e.g., 'default')")]
    pub target_ctx: String,
    /// Content to transfer
    #[schemars(description = "Content to push to the target context")]
    pub content: String,
    /// Whether to LLM-summarize before transfer
    #[schemars(description = "Summarize via LLM before transfer (default: false)")]
    #[serde(default)]
    pub summarize: bool,
}

/// Cancel a staged drift by ID.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DriftCancelRequest {
    /// Staged drift ID to cancel
    #[schemars(description = "ID of the staged drift to cancel")]
    pub staged_id: u64,
}

/// Pull summarized content from another context.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DriftPullRequest {
    /// Source context (short hex ID or label)
    #[schemars(description = "Source context — short hex ID (e.g., 'a1b2c3') or label (e.g., 'default')")]
    pub source_ctx: String,
    /// Optional directed prompt to focus the summary
    #[schemars(description = "Optional prompt to focus the summary (e.g., 'what decisions were made about auth?')")]
    pub prompt: Option<String>,
}

/// Merge a forked context back into its parent.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DriftMergeRequest {
    /// Source context to merge (short hex ID or label)
    #[schemars(description = "Forked context to merge — short hex ID or label")]
    pub source_ctx: String,
}

// ============================================================================
// Response Types
// ============================================================================

/// Document info for listing.
#[derive(Debug, Serialize)]
pub struct DocumentInfo {
    pub id: String,
    pub kind: String,
    pub language: Option<String>,
    pub block_count: usize,
}

/// Block summary for listing.
#[derive(Debug, Serialize)]
pub struct BlockSummary {
    pub block_id: String,
    pub parent_id: Option<String>,
    pub role: String,
    pub kind: String,
    pub status: String,
    pub summary: String,
}

/// Search match result.
#[derive(Debug, Serialize)]
pub struct SearchMatch {
    pub document_id: String,
    pub block_id: String,
    pub line: u32,
    pub content: String,
    pub before: Vec<String>,
    pub after: Vec<String>,
}
