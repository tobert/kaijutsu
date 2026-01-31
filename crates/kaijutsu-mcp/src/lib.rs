//! MCP server exposing kaijutsu CRDT kernel.
//!
//! Provides tools for document and block manipulation via Model Context Protocol,
//! enabling agents like Claude Code, Gemini CLI, and opencode to collaborate
//! on shared CRDT state.
//!
//! ## Backends
//!
//! - **Local**: In-memory ephemeral store for testing
//! - **Remote**: SSH + Cap'n Proto RPC to kaijutsu-server (shared state)

use regex::Regex;
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};

use kaijutsu_client::{SshConfig, connect_ssh};
use kaijutsu_crdt::{BlockId, BlockKind, Role, Status};
use kaijutsu_kernel::{DocumentKind, SharedBlockStore, shared_block_store};

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

// ============================================================================
// Response Types
// ============================================================================

/// Document info for listing.
#[derive(Debug, Serialize)]
struct DocumentInfo {
    id: String,
    kind: String,
    language: Option<String>,
    block_count: usize,
}

/// Block summary for listing.
#[derive(Debug, Serialize)]
struct BlockSummary {
    block_id: String,
    parent_id: Option<String>,
    role: String,
    kind: String,
    status: String,
    summary: String,
}

/// Search match result.
#[derive(Debug, Serialize)]
struct SearchMatch {
    document_id: String,
    block_id: String,
    line: u32,
    content: String,
    before: Vec<String>,
    after: Vec<String>,
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Parse document kind from string.
fn parse_document_kind(s: &str) -> Option<DocumentKind> {
    match s.to_lowercase().as_str() {
        "conversation" => Some(DocumentKind::Conversation),
        "code" => Some(DocumentKind::Code),
        "text" => Some(DocumentKind::Text),
        "git" => Some(DocumentKind::Git),
        _ => None,
    }
}

/// Parse role from string.
fn parse_role(s: &str) -> Option<Role> {
    match s.to_lowercase().as_str() {
        "user" | "human" => Some(Role::User),
        "model" | "assistant" | "agent" => Some(Role::Model),
        "system" => Some(Role::System),
        "tool" => Some(Role::Tool),
        _ => None,
    }
}

/// Parse block kind from string.
fn parse_block_kind(s: &str) -> Option<BlockKind> {
    match s.to_lowercase().as_str() {
        "text" => Some(BlockKind::Text),
        "thinking" => Some(BlockKind::Thinking),
        "tool_call" | "toolcall" => Some(BlockKind::ToolCall),
        "tool_result" | "toolresult" => Some(BlockKind::ToolResult),
        _ => None,
    }
}

/// Parse status from string.
fn parse_status(s: &str) -> Option<Status> {
    match s.to_lowercase().as_str() {
        "pending" => Some(Status::Pending),
        "running" | "active" => Some(Status::Running),
        "done" | "complete" | "completed" => Some(Status::Done),
        "error" => Some(Status::Error),
        _ => None,
    }
}

/// Parse block ID from key string.
fn parse_block_id(s: &str) -> Option<BlockId> {
    BlockId::from_key(s)
}

/// Find a block across all documents, returning (document_id, BlockId).
fn find_block(store: &SharedBlockStore, block_id_str: &str) -> Option<(String, BlockId)> {
    let block_id = parse_block_id(block_id_str)?;

    for document_id in store.list_ids() {
        if let Some(entry) = store.get(&document_id) {
            for snapshot in entry.doc.blocks_ordered() {
                if snapshot.id == block_id {
                    return Some((document_id.clone(), block_id));
                }
            }
        }
    }
    None
}

/// Add line numbers to content.
fn content_with_line_numbers(content: &str) -> String {
    content
        .lines()
        .enumerate()
        .map(|(i, line)| format!("{:4}→{}", i + 1, line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract lines with numbers for a range.
fn extract_lines_with_numbers(content: &str, start: u32, end: u32) -> String {
    content
        .lines()
        .enumerate()
        .skip(start as usize)
        .take((end.saturating_sub(start)) as usize)
        .map(|(i, line)| format!("{:4}→{}", i + 1, line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Count lines in content.
fn line_count(content: &str) -> usize {
    if content.is_empty() {
        0
    } else {
        content.lines().count()
    }
}

/// Convert line number to byte offset.
fn line_to_byte_offset(content: &str, line: u32) -> Option<usize> {
    let mut offset = 0;
    for (i, l) in content.lines().enumerate() {
        if i == line as usize {
            return Some(offset);
        }
        offset += l.len() + 1; // +1 for newline
    }
    // Line at end
    if line as usize == content.lines().count() {
        return Some(content.len());
    }
    None
}

/// Convert line range to byte range.
fn line_range_to_byte_range(content: &str, start_line: u32, end_line: u32) -> Option<(usize, usize)> {
    let start = line_to_byte_offset(content, start_line)?;
    let end = line_to_byte_offset(content, end_line)?;
    Some((start, end))
}

// ============================================================================
// Backend Abstraction
// ============================================================================

/// Backend for block operations - either local or remote via RPC.
///
/// The Remote backend syncs state from kaijutsu-server at connection time,
/// then operates on a local cache. Full bidirectional sync is a future enhancement.
#[derive(Clone)]
pub enum Backend {
    /// In-memory local store (ephemeral)
    Local(SharedBlockStore),
    /// Connected to kaijutsu-server - state synced into local store
    Remote(RemoteState),
}

/// Remote backend state - cached from initial connection.
///
/// Note: Cap'n Proto RPC types are !Send, so we sync state at connection
/// and store it locally. The RpcClient/KernelHandle stay in the LocalSet.
#[derive(Clone)]
pub struct RemoteState {
    /// The document ID from our seat
    pub document_id: String,
    /// Kernel ID we connected to
    pub kernel_id: String,
    /// Local cache of synced state
    pub store: SharedBlockStore,
}

// ============================================================================
// KaijutsuMcp Server
// ============================================================================

/// MCP server exposing kaijutsu CRDT kernel.
#[derive(Clone)]
pub struct KaijutsuMcp {
    backend: Backend,
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for KaijutsuMcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let backend_name = match &self.backend {
            Backend::Local(_) => "Local",
            Backend::Remote(_) => "Remote",
        };
        f.debug_struct("KaijutsuMcp")
            .field("backend", &backend_name)
            .field("tool_router", &self.tool_router)
            .finish()
    }
}

impl KaijutsuMcp {
    /// Create a new MCP server with the given block store.
    pub fn with_store(store: SharedBlockStore) -> Self {
        Self {
            backend: Backend::Local(store),
            tool_router: Self::tool_router(),
        }
    }

    /// Create a new MCP server with an in-memory store.
    pub fn new() -> Self {
        Self::with_store(shared_block_store("mcp-server"))
    }

    /// Connect to a running kaijutsu-server via SSH.
    ///
    /// Uses ssh-agent for authentication. Must be called within a `LocalSet`.
    ///
    /// Syncs initial state from the server into a local cache, then returns
    /// a KaijutsuMcp that operates on that cache. The RPC connection is closed
    /// after initial sync (phase 1 - read-only sync).
    pub async fn connect(
        host: &str,
        port: u16,
        kernel_id: &str,
    ) -> Result<Self, anyhow::Error> {
        let config = SshConfig {
            host: host.to_string(),
            port,
            username: whoami::username(),
        };

        tracing::debug!(?config, "Connecting via SSH");

        let client = connect_ssh(config).await?;
        let kernel = client.attach_kernel(kernel_id).await?;
        let seat = kernel.join_context("default", "mcp-server").await?;

        tracing::info!(
            kernel = %kernel_id,
            document_id = %seat.document_id,
            "Connected to server"
        );

        // Sync document state into local store
        let store = shared_block_store("mcp-remote");
        let doc_state = kernel.get_document_state(&seat.document_id).await?;

        // Create the document from the server's oplog
        if !doc_state.ops.is_empty() {
            store.create_document_from_oplog(
                doc_state.document_id.clone(),
                DocumentKind::Conversation,
                None,
                &doc_state.ops,
            ).map_err(|e| anyhow::anyhow!(e))?;
            tracing::debug!(
                doc = %doc_state.document_id,
                blocks = %doc_state.blocks.len(),
                ops_bytes = %doc_state.ops.len(),
                "Synced document state"
            );
        } else {
            // Empty document - just create fresh
            store.create_document(
                doc_state.document_id.clone(),
                DocumentKind::Conversation,
                None,
            ).map_err(|e| anyhow::anyhow!(e))?;
        }

        // Detach cleanly (the kernel handle drops, but we do it explicitly)
        // Note: In phase 2, we'd keep the connection for writes
        kernel.detach().await?;

        Ok(Self {
            backend: Backend::Remote(RemoteState {
                document_id: seat.document_id,
                kernel_id: kernel_id.to_string(),
                store,
            }),
            tool_router: Self::tool_router(),
        })
    }

    /// Get the underlying store for tool operations.
    fn store(&self) -> &SharedBlockStore {
        match &self.backend {
            Backend::Local(store) => store,
            Backend::Remote(remote) => &remote.store,
        }
    }
}

impl Default for KaijutsuMcp {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router]
impl KaijutsuMcp {
    // ========================================================================
    // Document Tools
    // ========================================================================

    #[tool(description = "Create a new document for collaborative editing. Documents contain blocks of content organized in a DAG structure.")]
    fn doc_create(&self, Parameters(req): Parameters<DocCreateRequest>) -> String {
        let kind = match parse_document_kind(&req.kind) {
            Some(k) => k,
            None => return format!("Error: invalid document kind '{}'. Use: conversation, code, text, or git", req.kind),
        };

        match self.store().create_document(req.id.clone(), kind, req.language) {
            Ok(()) => serde_json::json!({
                "success": true,
                "document_id": req.id,
                "kind": req.kind
            }).to_string(),
            Err(e) => format!("Error: {}", e),
        }
    }

    #[tool(description = "List all documents in the kernel with their metadata and block counts.")]
    fn doc_list(&self) -> String {
        let docs: Vec<DocumentInfo> = self.store().list_ids().iter().map(|id| {
            let (kind, language, block_count) = self.store().get(id)
                .map(|entry| {
                    let kind = entry.kind.as_str().to_string();
                    let lang = entry.language.clone();
                    let count = entry.doc.blocks_ordered().len();
                    (kind, lang, count)
                })
                .unwrap_or_else(|| ("unknown".to_string(), None, 0));

            DocumentInfo {
                id: id.clone(),
                kind,
                language,
                block_count,
            }
        }).collect();

        serde_json::json!({
            "documents": docs,
            "count": docs.len()
        }).to_string()
    }

    #[tool(description = "Delete a document and all its blocks.")]
    fn doc_delete(&self, Parameters(req): Parameters<DocDeleteRequest>) -> String {
        match self.store().delete_document(&req.id) {
            Ok(()) => serde_json::json!({
                "success": true,
                "deleted": req.id
            }).to_string(),
            Err(e) => format!("Error: {}", e),
        }
    }

    // ========================================================================
    // Block Tools
    // ========================================================================

    #[tool(description = "Create a new block with role, kind, and optional content. Blocks are the atomic units of content in documents.")]
    fn block_create(&self, Parameters(req): Parameters<BlockCreateRequest>) -> String {
        let role = match parse_role(&req.role) {
            Some(r) => r,
            None => return format!("Error: invalid role '{}'. Use: user, model, system, or tool", req.role),
        };

        let kind = match parse_block_kind(&req.kind) {
            Some(k) => k,
            None => return format!("Error: invalid kind '{}'. Use: text, thinking, tool_call, or tool_result", req.kind),
        };

        // Parse parent_id if provided
        let parent_id = req.parent_id.as_ref().and_then(|s| parse_block_id(s));
        let after_id = req.after_id.as_ref().and_then(|s| parse_block_id(s));
        let content = req.content.unwrap_or_default();

        match self.store().insert_block(
            &req.document_id,
            parent_id.as_ref(),
            after_id.as_ref(),
            role,
            kind,
            &content,
        ) {
            Ok(block_id) => {
                let version = self.store().get(&req.document_id)
                    .map(|e| e.version())
                    .unwrap_or(0);

                serde_json::json!({
                    "success": true,
                    "block_id": block_id.to_key(),
                    "version": version
                }).to_string()
            }
            Err(e) => format!("Error: {}", e),
        }
    }

    #[tool(description = "Read block content with optional line numbers and range filtering. Returns formatted content suitable for editing.")]
    fn block_read(&self, Parameters(req): Parameters<BlockReadRequest>) -> String {
        let (document_id, block_id) = match find_block(self.store(), &req.block_id) {
            Some(r) => r,
            None => return format!("Error: block '{}' not found", req.block_id),
        };

        let entry = match self.store().get(&document_id) {
            Some(e) => e,
            None => return format!("Error: document not found"),
        };

        let snapshot = match entry.doc.get_block_snapshot(&block_id) {
            Some(s) => s,
            None => return format!("Error: block not found"),
        };

        let content = &snapshot.content;
        let total_lines = line_count(content);

        let formatted_content = if let Some(ref range) = req.range {
            if range.len() >= 2 {
                let (start, end) = (range[0], range[1]);
                if req.line_numbers {
                    extract_lines_with_numbers(content, start, end)
                } else {
                    content
                        .lines()
                        .skip(start as usize)
                        .take((end.saturating_sub(start)) as usize)
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            } else {
                content.clone()
            }
        } else if req.line_numbers {
            content_with_line_numbers(content)
        } else {
            content.clone()
        };

        serde_json::json!({
            "content": formatted_content,
            "role": snapshot.role.as_str(),
            "kind": snapshot.kind.as_str(),
            "status": snapshot.status.as_str(),
            "version": entry.version(),
            "line_count": total_lines,
            "metadata": {
                "tool_name": snapshot.tool_name,
                "tool_call_id": snapshot.tool_call_id.map(|id| id.to_key()),
                "is_error": snapshot.is_error,
            }
        }).to_string()
    }

    #[tool(description = "Append text to a block. Optimized for streaming output - use this for incremental content updates.")]
    fn block_append(&self, Parameters(req): Parameters<BlockAppendRequest>) -> String {
        let (document_id, block_id) = match find_block(self.store(), &req.block_id) {
            Some(r) => r,
            None => return format!("Error: block '{}' not found", req.block_id),
        };

        match self.store().append_text(&document_id, &block_id, &req.text) {
            Ok(()) => {
                let version = self.store().get(&document_id)
                    .map(|e| e.version())
                    .unwrap_or(0);

                serde_json::json!({
                    "success": true,
                    "version": version
                }).to_string()
            }
            Err(e) => format!("Error: {}", e),
        }
    }

    #[tool(description = "Edit block content with line-based operations. Supports insert, delete, and replace with optional CAS validation.")]
    fn block_edit(&self, Parameters(req): Parameters<BlockEditRequest>) -> String {
        let (document_id, block_id) = match find_block(self.store(), &req.block_id) {
            Some(r) => r,
            None => return format!("Error: block '{}' not found", req.block_id),
        };

        for (idx, op) in req.operations.into_iter().enumerate() {
            // Get current content
            let content = match self.store().get(&document_id) {
                Some(entry) => {
                    entry.doc.get_block_snapshot(&block_id)
                        .map(|s| s.content.clone())
                        .unwrap_or_default()
                }
                None => return format!("Error: document not found"),
            };

            let result = match op {
                EditOp::Insert { line, content: text } => {
                    if let Some(pos) = line_to_byte_offset(&content, line) {
                        let text_with_newline = if text.ends_with('\n') || content.is_empty() {
                            text
                        } else {
                            format!("{}\n", text)
                        };
                        self.store().edit_text(&document_id, &block_id, pos, &text_with_newline, 0)
                    } else {
                        Err(format!("Invalid line number: {}", line))
                    }
                }
                EditOp::Delete { start_line, end_line } => {
                    if let Some((start, end)) = line_range_to_byte_range(&content, start_line, end_line) {
                        if start < end {
                            self.store().edit_text(&document_id, &block_id, start, "", end - start)
                        } else {
                            Ok(())
                        }
                    } else {
                        Err(format!("Invalid line range: {}-{}", start_line, end_line))
                    }
                }
                EditOp::Replace { start_line, end_line, content: text, expected_text } => {
                    // CAS validation
                    if let Some(ref expected) = expected_text {
                        let actual: String = content
                            .lines()
                            .skip(start_line as usize)
                            .take((end_line.saturating_sub(start_line)) as usize)
                            .collect::<Vec<_>>()
                            .join("\n");
                        if actual.trim() != expected.trim() {
                            return format!("Error: CAS validation failed at operation {}. Expected '{}' but found '{}'", idx, expected, actual);
                        }
                    }

                    if let Some((start, end)) = line_range_to_byte_range(&content, start_line, end_line) {
                        let text_with_newline = if text.ends_with('\n') || text.is_empty() {
                            text
                        } else {
                            format!("{}\n", text)
                        };
                        self.store().edit_text(&document_id, &block_id, start, &text_with_newline, end - start)
                    } else {
                        Err(format!("Invalid line range: {}-{}", start_line, end_line))
                    }
                }
            };

            if let Err(e) = result {
                return format!("Error at operation {}: {}", idx, e);
            }
        }

        let version = self.store().get(&document_id)
            .map(|e| e.version())
            .unwrap_or(0);

        serde_json::json!({
            "success": true,
            "version": version
        }).to_string()
    }

    #[tool(description = "List blocks with optional filters for document, kind, status, and role.")]
    fn block_list(&self, Parameters(req): Parameters<BlockListRequest>) -> String {
        let kind_filter = req.kind.as_ref().and_then(|k| parse_block_kind(k));
        let status_filter = req.status.as_ref().and_then(|s| parse_status(s));
        let role_filter = req.role.as_ref().and_then(|r| parse_role(r));

        let mut blocks = Vec::new();

        let document_ids: Vec<String> = if let Some(ref doc_id) = req.document_id {
            if self.store().contains(doc_id) {
                vec![doc_id.clone()]
            } else {
                vec![]
            }
        } else {
            self.store().list_ids()
        };

        for document_id in document_ids {
            if let Some(entry) = self.store().get(&document_id) {
                for snapshot in entry.doc.blocks_ordered() {
                    // Apply filters
                    if let Some(kind) = kind_filter {
                        if snapshot.kind != kind {
                            continue;
                        }
                    }
                    if let Some(status) = status_filter {
                        if snapshot.status != status {
                            continue;
                        }
                    }
                    if let Some(role) = role_filter {
                        if snapshot.role != role {
                            continue;
                        }
                    }

                    // Create summary (first 100 chars)
                    let summary = if snapshot.content.chars().count() > 100 {
                        let truncated: String = snapshot.content.chars().take(100).collect();
                        format!("{}... ({} lines)", truncated, line_count(&snapshot.content))
                    } else {
                        snapshot.content.clone()
                    };

                    blocks.push(BlockSummary {
                        block_id: snapshot.id.to_key(),
                        parent_id: snapshot.parent_id.map(|id| id.to_key()),
                        role: snapshot.role.as_str().to_string(),
                        kind: snapshot.kind.as_str().to_string(),
                        status: snapshot.status.as_str().to_string(),
                        summary,
                    });
                }
            }
        }

        serde_json::json!({
            "blocks": blocks,
            "count": blocks.len()
        }).to_string()
    }

    #[tool(description = "Set the status of a block: pending, running, done, or error.")]
    fn block_status(&self, Parameters(req): Parameters<BlockStatusRequest>) -> String {
        let (document_id, block_id) = match find_block(self.store(), &req.block_id) {
            Some(r) => r,
            None => return format!("Error: block '{}' not found", req.block_id),
        };

        let status = match parse_status(&req.status) {
            Some(s) => s,
            None => return format!("Error: invalid status '{}'. Use: pending, running, done, or error", req.status),
        };

        match self.store().set_status(&document_id, &block_id, status) {
            Ok(()) => {
                let version = self.store().get(&document_id)
                    .map(|e| e.version())
                    .unwrap_or(0);

                serde_json::json!({
                    "success": true,
                    "version": version
                }).to_string()
            }
            Err(e) => format!("Error: {}", e),
        }
    }

    // ========================================================================
    // Search Tools
    // ========================================================================

    #[tool(description = "Search across all blocks using regex patterns. Returns matches with context lines.")]
    fn kernel_search(&self, Parameters(req): Parameters<KernelSearchRequest>) -> String {
        let regex = match Regex::new(&req.query) {
            Ok(r) => r,
            Err(e) => return format!("Error: invalid regex '{}': {}", req.query, e),
        };

        let kind_filter = req.kind.as_ref().and_then(|k| parse_block_kind(k));
        let role_filter = req.role.as_ref().and_then(|r| parse_role(r));

        let mut matches = Vec::new();

        let document_ids: Vec<String> = if let Some(ref doc_id) = req.document_id {
            if self.store().contains(doc_id) {
                vec![doc_id.clone()]
            } else {
                vec![]
            }
        } else {
            self.store().list_ids()
        };

        'outer: for document_id in document_ids {
            let snapshots = match self.store().block_snapshots(&document_id) {
                Ok(s) => s,
                Err(_) => continue,
            };

            for snapshot in snapshots {
                // Apply filters
                if let Some(kind) = kind_filter {
                    if snapshot.kind != kind {
                        continue;
                    }
                }
                if let Some(role) = role_filter {
                    if snapshot.role != role {
                        continue;
                    }
                }

                // Search content
                let lines: Vec<&str> = snapshot.content.lines().collect();
                for (line_idx, line) in lines.iter().enumerate() {
                    if regex.is_match(line) {
                        // Collect context
                        let before: Vec<String> = (0..req.context_lines as usize)
                            .filter_map(|i| {
                                if line_idx >= i + 1 {
                                    Some(lines[line_idx - i - 1].to_string())
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect();

                        let after: Vec<String> = (1..=req.context_lines as usize)
                            .filter_map(|i| {
                                lines.get(line_idx + i).map(|s| s.to_string())
                            })
                            .collect();

                        matches.push(SearchMatch {
                            document_id: document_id.clone(),
                            block_id: snapshot.id.to_key(),
                            line: line_idx as u32,
                            content: line.to_string(),
                            before,
                            after,
                        });

                        if matches.len() >= req.max_matches {
                            break 'outer;
                        }
                    }
                }
            }
        }

        serde_json::json!({
            "matches": matches,
            "total": matches.len(),
            "truncated": matches.len() >= req.max_matches
        }).to_string()
    }
}

#[tool_handler]
impl ServerHandler for KaijutsuMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Kaijutsu CRDT kernel MCP server. Provides tools for collaborative document and block editing with CRDT-backed consistency.".into()
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_doc_create_and_list() {
        let mcp = KaijutsuMcp::new();

        // Create a document
        let result = mcp.doc_create(Parameters(DocCreateRequest {
            id: "test-doc".to_string(),
            kind: "conversation".to_string(),
            language: None,
        }));
        assert!(result.contains("success"));
        assert!(result.contains("test-doc"));

        // List documents
        let result = mcp.doc_list();
        assert!(result.contains("test-doc"));
        assert!(result.contains("conversation"));
    }

    #[test]
    fn test_block_create_and_read() {
        let mcp = KaijutsuMcp::new();

        // Create document first
        mcp.doc_create(Parameters(DocCreateRequest {
            id: "test-doc".to_string(),
            kind: "code".to_string(),
            language: Some("rust".to_string()),
        }));

        // Create a block
        let result = mcp.block_create(Parameters(BlockCreateRequest {
            document_id: "test-doc".to_string(),
            parent_id: None,
            after_id: None,
            role: "user".to_string(),
            kind: "text".to_string(),
            content: Some("Hello, world!".to_string()),
        }));
        assert!(result.contains("success"));
        assert!(result.contains("block_id"));

        // Extract block_id from result
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let block_id = parsed["block_id"].as_str().unwrap();

        // Read the block
        let result = mcp.block_read(Parameters(BlockReadRequest {
            block_id: block_id.to_string(),
            line_numbers: true,
            range: None,
        }));
        assert!(result.contains("Hello, world!"));
        assert!(result.contains("user"));
        assert!(result.contains("text"));
    }

    #[test]
    fn test_block_append() {
        let mcp = KaijutsuMcp::new();

        mcp.doc_create(Parameters(DocCreateRequest {
            id: "test-doc".to_string(),
            kind: "conversation".to_string(),
            language: None,
        }));

        let result = mcp.block_create(Parameters(BlockCreateRequest {
            document_id: "test-doc".to_string(),
            parent_id: None,
            after_id: None,
            role: "model".to_string(),
            kind: "text".to_string(),
            content: Some("Hello".to_string()),
        }));

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let block_id = parsed["block_id"].as_str().unwrap();

        // Append text
        let result = mcp.block_append(Parameters(BlockAppendRequest {
            block_id: block_id.to_string(),
            text: ", world!".to_string(),
        }));
        assert!(result.contains("success"));

        // Verify content
        let result = mcp.block_read(Parameters(BlockReadRequest {
            block_id: block_id.to_string(),
            line_numbers: false,
            range: None,
        }));
        assert!(result.contains("Hello, world!"));
    }

    #[test]
    fn test_kernel_search() {
        let mcp = KaijutsuMcp::new();

        mcp.doc_create(Parameters(DocCreateRequest {
            id: "doc1".to_string(),
            kind: "conversation".to_string(),
            language: None,
        }));

        mcp.block_create(Parameters(BlockCreateRequest {
            document_id: "doc1".to_string(),
            parent_id: None,
            after_id: None,
            role: "user".to_string(),
            kind: "text".to_string(),
            content: Some("The quick brown fox".to_string()),
        }));

        mcp.block_create(Parameters(BlockCreateRequest {
            document_id: "doc1".to_string(),
            parent_id: None,
            after_id: None,
            role: "model".to_string(),
            kind: "text".to_string(),
            content: Some("The lazy dog".to_string()),
        }));

        // Search for "The"
        let result = mcp.kernel_search(Parameters(KernelSearchRequest {
            query: "The".to_string(),
            document_id: None,
            kind: None,
            role: None,
            context_lines: 0,
            max_matches: 100,
        }));

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["total"], 2);
    }
}
