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
//!
//! ## Module Structure
//!
//! - `models`: Request and response types for MCP tools
//! - `helpers`: Parsing and utility functions
//! - `tree`: DAG visualization as ASCII tree

mod helpers;
mod models;
mod tree;

use regex::Regex;
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};

use kaijutsu_client::{SshConfig, connect_ssh};
use kaijutsu_crdt::ConversationDAG;
use kaijutsu_kernel::{DocumentKind, SharedBlockStore, shared_block_store};

// Re-export public types
pub use models::*;
use helpers::*;
use tree::format_dag_tree;

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
            ..SshConfig::default()
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
                    let total_lines = line_count(&content);
                    if let Some(pos) = line_to_byte_offset(&content, line) {
                        let text_with_newline = if text.ends_with('\n') || content.is_empty() {
                            text
                        } else {
                            format!("{}\n", text)
                        };
                        self.store().edit_text(&document_id, &block_id, pos, &text_with_newline, 0)
                    } else {
                        Err(format!(
                            "Invalid line number {}: block has {} line{} (valid range: 0-{})",
                            line, total_lines, if total_lines == 1 { "" } else { "s" }, total_lines
                        ))
                    }
                }
                EditOp::Delete { start_line, end_line } => {
                    let total_lines = line_count(&content);
                    if let Some((start, end)) = line_range_to_byte_range(&content, start_line, end_line) {
                        if start < end {
                            self.store().edit_text(&document_id, &block_id, start, "", end - start)
                        } else {
                            Ok(())
                        }
                    } else {
                        Err(format!(
                            "Invalid line range {}-{}: block has {} line{} (valid range: 0-{})",
                            start_line, end_line, total_lines,
                            if total_lines == 1 { "" } else { "s" }, total_lines
                        ))
                    }
                }
                EditOp::Replace { start_line, end_line, content: text, expected_text } => {
                    let total_lines = line_count(&content);

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
                        Err(format!(
                            "Invalid line range {}-{}: block has {} line{} (valid range: 0-{})",
                            start_line, end_line, total_lines,
                            if total_lines == 1 { "" } else { "s" }, total_lines
                        ))
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

    // ========================================================================
    // Debug/Visualization Tools
    // ========================================================================

    #[tool(description = "Display a document's conversation DAG as a compact ASCII tree. Useful for understanding conversation structure and debugging.")]
    fn doc_tree(&self, Parameters(req): Parameters<DocTreeRequest>) -> String {
        let entry = match self.store().get(&req.document_id) {
            Some(e) => e,
            None => return format!("Error: document '{}' not found", req.document_id),
        };

        let dag = ConversationDAG::from_document(&entry.doc);
        let mut output = String::new();

        // Header: document_id (kind, N blocks)
        let kind = entry.kind.as_str();
        output.push_str(&format!("{} ({}, {} block{})\n",
            req.document_id, kind, dag.len(),
            if dag.len() == 1 { "" } else { "s" }
        ));

        // Build tree lines
        let lines = format_dag_tree(&dag, req.max_depth, req.expand_tools);
        for line in lines {
            output.push_str(&line);
            output.push('\n');
        }

        output
    }

    #[tool(description = "Inspect CRDT internals of a block for debugging. Returns version, frontier, operation counts, and metadata.")]
    fn block_inspect(&self, Parameters(req): Parameters<BlockInspectRequest>) -> String {
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

        // Get CRDT internals from the oplog
        let frontier = entry.doc.frontier();
        let version = entry.version();

        // Count content characters/lines
        let content_length = snapshot.content.len();
        let content_lines = line_count(&snapshot.content);

        serde_json::json!({
            "block_id": req.block_id,
            "document_id": document_id,
            "version": version,
            "frontier": frontier,
            "content_length": content_length,
            "content_lines": content_lines,
            "metadata": {
                "role": snapshot.role.as_str(),
                "kind": snapshot.kind.as_str(),
                "status": snapshot.status.as_str(),
                "parent_id": snapshot.parent_id.map(|id| id.to_key()),
                "created_at": snapshot.created_at,
                "author": snapshot.author,
                "collapsed": snapshot.collapsed,
                "tool_name": snapshot.tool_name,
                "tool_call_id": snapshot.tool_call_id.map(|id| id.to_key()),
                "is_error": snapshot.is_error,
                "exit_code": snapshot.exit_code,
            }
        }).to_string()
    }

    #[tool(description = "Get version history information for a block. Shows creation time and current version details.")]
    fn block_history(&self, Parameters(req): Parameters<BlockHistoryRequest>) -> String {
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

        let content_lines = line_count(&snapshot.content);
        let version = entry.version();

        // Format as human-readable output
        let mut output = String::new();
        output.push_str(&format!("block: {}\n", req.block_id));
        output.push_str(&format!("{}\n", "─".repeat(40)));

        // Creation info - simple timestamp display
        let created_time = if snapshot.created_at > 0 {
            format!("{}ms (unix epoch)", snapshot.created_at)
        } else {
            "unknown".to_string()
        };

        output.push_str(&format!("created: {} by {}\n", created_time, snapshot.author));
        output.push_str(&format!("version: {} (document version)\n", version));
        output.push_str(&format!("content: {} line{}, {} byte{}\n",
            content_lines, if content_lines == 1 { "" } else { "s" },
            snapshot.content.len(), if snapshot.content.len() == 1 { "" } else { "s" }
        ));
        output.push_str(&format!("status: {}\n", snapshot.status.as_str()));

        output
    }

    #[tool(description = "Compare block content against original text, showing a unified diff with +/- prefixes.")]
    fn block_diff(&self, Parameters(req): Parameters<BlockDiffRequest>) -> String {
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

        let current = &snapshot.content;

        // If no original provided, just show current content summary
        let original = match req.original {
            Some(ref orig) => orig,
            None => {
                let mut output = String::new();
                output.push_str(&format!("block: {}\n", req.block_id));
                output.push_str(&format!("{}\n", "─".repeat(40)));
                output.push_str(&format!("No original text provided for comparison.\n"));
                output.push_str(&format!("Current content ({} lines, {} bytes):\n\n",
                    line_count(current), current.len()));
                output.push_str(current);
                return output;
            }
        };

        // Generate unified diff
        let mut output = String::new();
        output.push_str(&format!("diff {}\n", req.block_id));
        output.push_str(&format!("{}\n", "─".repeat(40)));

        let orig_lines: Vec<&str> = original.lines().collect();
        let curr_lines: Vec<&str> = current.lines().collect();

        // Simple line-by-line diff
        let max_lines = orig_lines.len().max(curr_lines.len());
        let mut has_changes = false;

        for i in 0..max_lines {
            let orig_line = orig_lines.get(i).copied();
            let curr_line = curr_lines.get(i).copied();

            match (orig_line, curr_line) {
                (Some(o), Some(c)) if o == c => {
                    output.push_str(&format!("  {}\n", o));
                }
                (Some(o), Some(c)) => {
                    output.push_str(&format!("- {}\n", o));
                    output.push_str(&format!("+ {}\n", c));
                    has_changes = true;
                }
                (Some(o), None) => {
                    output.push_str(&format!("- {}\n", o));
                    has_changes = true;
                }
                (None, Some(c)) => {
                    output.push_str(&format!("+ {}\n", c));
                    has_changes = true;
                }
                (None, None) => {}
            }
        }

        if !has_changes {
            output.push_str("\n(no changes)\n");
        }

        output
    }

    #[tool(description = "Preview recent operations on a document (dry-run only). Shows what blocks were recently added, useful for understanding document history.")]
    fn doc_undo(&self, Parameters(req): Parameters<DocUndoRequest>) -> String {
        let entry = match self.store().get(&req.document_id) {
            Some(e) => e,
            None => return format!("Error: document '{}' not found", req.document_id),
        };

        let blocks = entry.doc.blocks_ordered();
        let steps = req.steps.min(blocks.len() as u32) as usize;

        let mut output = String::new();
        output.push_str(&format!("doc_undo {} --dry-run\n", req.document_id));
        output.push_str(&format!("{}\n", "─".repeat(40)));
        output.push_str(&format!("Preview of {} most recent block{}:\n\n",
            steps, if steps == 1 { "" } else { "s" }));

        if blocks.is_empty() {
            output.push_str("(no blocks in document)\n");
            return output;
        }

        // Show most recent blocks (in reverse order - newest first)
        for (idx, snapshot) in blocks.iter().rev().take(steps).enumerate() {
            let content_preview = if snapshot.content.len() > 50 {
                let truncated: String = snapshot.content.chars().take(50).collect();
                format!("{}...", truncated.replace('\n', "\\n"))
            } else {
                snapshot.content.replace('\n', "\\n")
            };

            output.push_str(&format!("  {}. [{}] {} \"{}\" at pos {}\n",
                idx + 1,
                snapshot.author,
                snapshot.kind.as_str(),
                content_preview,
                snapshot.id.seq
            ));
        }

        output.push_str(&format!("\n⚠️  Undo is not yet implemented. This is a dry-run preview only.\n"));
        output.push_str(&format!("    Full undo would require storing undo stack or computing inverse operations.\n"));

        output
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

    #[test]
    fn test_doc_tree() {
        let mcp = KaijutsuMcp::new();

        mcp.doc_create(Parameters(DocCreateRequest {
            id: "tree-test".to_string(),
            kind: "conversation".to_string(),
            language: None,
        }));

        // Create a simple conversation structure
        let result = mcp.block_create(Parameters(BlockCreateRequest {
            document_id: "tree-test".to_string(),
            parent_id: None,
            after_id: None,
            role: "user".to_string(),
            kind: "text".to_string(),
            content: Some("Hello!".to_string()),
        }));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let user_block_id = parsed["block_id"].as_str().unwrap().to_string();

        mcp.block_create(Parameters(BlockCreateRequest {
            document_id: "tree-test".to_string(),
            parent_id: Some(user_block_id.clone()),
            after_id: Some(user_block_id),
            role: "model".to_string(),
            kind: "text".to_string(),
            content: Some("Hi there!".to_string()),
        }));

        // Test doc_tree output
        let result = mcp.doc_tree(Parameters(DocTreeRequest {
            document_id: "tree-test".to_string(),
            max_depth: None,
            expand_tools: false,
        }));

        assert!(result.contains("tree-test"));
        assert!(result.contains("conversation"));
        assert!(result.contains("2 blocks"));
        assert!(result.contains("[user/text]"));
        assert!(result.contains("[model/text]"));
    }

    #[test]
    fn test_doc_tree_with_tools() {
        let mcp = KaijutsuMcp::new();

        mcp.doc_create(Parameters(DocCreateRequest {
            id: "tool-tree-test".to_string(),
            kind: "conversation".to_string(),
            language: None,
        }));

        // Create a tool call
        let result = mcp.block_create(Parameters(BlockCreateRequest {
            document_id: "tool-tree-test".to_string(),
            parent_id: None,
            after_id: None,
            role: "model".to_string(),
            kind: "tool_call".to_string(),
            content: Some("{\"path\": \"/test\"}".to_string()),
        }));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let tool_call_id = parsed["block_id"].as_str().unwrap().to_string();

        // Create a tool result as child
        mcp.block_create(Parameters(BlockCreateRequest {
            document_id: "tool-tree-test".to_string(),
            parent_id: Some(tool_call_id.clone()),
            after_id: Some(tool_call_id),
            role: "tool".to_string(),
            kind: "tool_result".to_string(),
            content: Some("File contents".to_string()),
        }));

        // Test collapsed format (default)
        let result = mcp.doc_tree(Parameters(DocTreeRequest {
            document_id: "tool-tree-test".to_string(),
            max_depth: None,
            expand_tools: false,
        }));

        // Collapsed format shows "→ ✓" or "→ ✗"
        assert!(result.contains("→ ✓") || result.contains("tool("));

        // Test expanded format
        let result = mcp.doc_tree(Parameters(DocTreeRequest {
            document_id: "tool-tree-test".to_string(),
            max_depth: None,
            expand_tools: true,
        }));

        // Expanded format shows both nodes separately
        assert!(result.contains("[model/tool_call]"));
        assert!(result.contains("[tool/tool_result]"));
    }

    #[test]
    fn test_block_inspect() {
        let mcp = KaijutsuMcp::new();

        mcp.doc_create(Parameters(DocCreateRequest {
            id: "inspect-test".to_string(),
            kind: "conversation".to_string(),
            language: None,
        }));

        let result = mcp.block_create(Parameters(BlockCreateRequest {
            document_id: "inspect-test".to_string(),
            parent_id: None,
            after_id: None,
            role: "user".to_string(),
            kind: "text".to_string(),
            content: Some("Test content".to_string()),
        }));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let block_id = parsed["block_id"].as_str().unwrap();

        // Test block_inspect
        let result = mcp.block_inspect(Parameters(BlockInspectRequest {
            block_id: block_id.to_string(),
        }));

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["block_id"].is_string());
        assert!(parsed["version"].is_number());
        assert!(parsed["frontier"].is_array());
        assert_eq!(parsed["content_lines"], 1);
        assert_eq!(parsed["metadata"]["role"], "user");
        assert_eq!(parsed["metadata"]["kind"], "text");
    }

    #[test]
    fn test_block_history() {
        let mcp = KaijutsuMcp::new();

        mcp.doc_create(Parameters(DocCreateRequest {
            id: "history-test".to_string(),
            kind: "conversation".to_string(),
            language: None,
        }));

        let result = mcp.block_create(Parameters(BlockCreateRequest {
            document_id: "history-test".to_string(),
            parent_id: None,
            after_id: None,
            role: "model".to_string(),
            kind: "text".to_string(),
            content: Some("Initial content".to_string()),
        }));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let block_id = parsed["block_id"].as_str().unwrap();

        // Test block_history
        let result = mcp.block_history(Parameters(BlockHistoryRequest {
            block_id: block_id.to_string(),
            limit: None,
        }));

        assert!(result.contains("block:"));
        assert!(result.contains("created:"));
        assert!(result.contains("version:"));
        assert!(result.contains("content:"));
        assert!(result.contains("status:"));
    }

    #[test]
    fn test_improved_error_messages() {
        let mcp = KaijutsuMcp::new();

        mcp.doc_create(Parameters(DocCreateRequest {
            id: "error-test".to_string(),
            kind: "conversation".to_string(),
            language: None,
        }));

        let result = mcp.block_create(Parameters(BlockCreateRequest {
            document_id: "error-test".to_string(),
            parent_id: None,
            after_id: None,
            role: "user".to_string(),
            kind: "text".to_string(),
            content: Some("Single line".to_string()),
        }));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let block_id = parsed["block_id"].as_str().unwrap();

        // Try to delete a line range that doesn't exist
        let result = mcp.block_edit(Parameters(BlockEditRequest {
            block_id: block_id.to_string(),
            operations: vec![EditOp::Delete {
                start_line: 5,
                end_line: 10,
            }],
        }));

        // Should have improved error message
        assert!(result.contains("Invalid line range 5-10"));
        assert!(result.contains("block has 1 line"));
        assert!(result.contains("valid range: 0-1"));
    }

    #[test]
    fn test_block_diff() {
        let mcp = KaijutsuMcp::new();

        mcp.doc_create(Parameters(DocCreateRequest {
            id: "diff-test".to_string(),
            kind: "conversation".to_string(),
            language: None,
        }));

        let result = mcp.block_create(Parameters(BlockCreateRequest {
            document_id: "diff-test".to_string(),
            parent_id: None,
            after_id: None,
            role: "user".to_string(),
            kind: "text".to_string(),
            content: Some("Hello\nWorld\nFoo".to_string()),
        }));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let block_id = parsed["block_id"].as_str().unwrap();

        // Test diff with original
        let result = mcp.block_diff(Parameters(BlockDiffRequest {
            block_id: block_id.to_string(),
            original: Some("Hello\nOld\nFoo".to_string()),
        }));

        assert!(result.contains("diff"));
        assert!(result.contains("- Old"));
        assert!(result.contains("+ World"));
        assert!(result.contains("  Hello")); // Unchanged line

        // Test diff without original (shows summary)
        let result = mcp.block_diff(Parameters(BlockDiffRequest {
            block_id: block_id.to_string(),
            original: None,
        }));

        assert!(result.contains("No original text provided"));
        assert!(result.contains("3 lines"));
    }

    #[test]
    fn test_doc_undo() {
        let mcp = KaijutsuMcp::new();

        mcp.doc_create(Parameters(DocCreateRequest {
            id: "undo-test".to_string(),
            kind: "conversation".to_string(),
            language: None,
        }));

        // Create a few blocks
        mcp.block_create(Parameters(BlockCreateRequest {
            document_id: "undo-test".to_string(),
            parent_id: None,
            after_id: None,
            role: "user".to_string(),
            kind: "text".to_string(),
            content: Some("First block".to_string()),
        }));

        mcp.block_create(Parameters(BlockCreateRequest {
            document_id: "undo-test".to_string(),
            parent_id: None,
            after_id: None,
            role: "model".to_string(),
            kind: "text".to_string(),
            content: Some("Second block".to_string()),
        }));

        // Test doc_undo dry-run
        let result = mcp.doc_undo(Parameters(DocUndoRequest {
            document_id: "undo-test".to_string(),
            steps: 2,
        }));

        assert!(result.contains("doc_undo"));
        assert!(result.contains("--dry-run"));
        assert!(result.contains("2 most recent blocks"));
        assert!(result.contains("text"));
        assert!(result.contains("not yet implemented"));
    }

    #[test]
    fn test_block_diff_no_changes() {
        let mcp = KaijutsuMcp::new();

        mcp.doc_create(Parameters(DocCreateRequest {
            id: "diff-same".to_string(),
            kind: "conversation".to_string(),
            language: None,
        }));

        let result = mcp.block_create(Parameters(BlockCreateRequest {
            document_id: "diff-same".to_string(),
            parent_id: None,
            after_id: None,
            role: "user".to_string(),
            kind: "text".to_string(),
            content: Some("Same content".to_string()),
        }));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let block_id = parsed["block_id"].as_str().unwrap();

        // Test diff with identical original
        let result = mcp.block_diff(Parameters(BlockDiffRequest {
            block_id: block_id.to_string(),
            original: Some("Same content".to_string()),
        }));

        assert!(result.contains("(no changes)"));
    }
}
