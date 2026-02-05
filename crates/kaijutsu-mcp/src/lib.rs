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
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, router::prompt::PromptRouter, wrapper::Parameters},
    model::{
        // Prompt types
        GetPromptRequestParams, GetPromptResult, ListPromptsResult,
        PaginatedRequestParams, PromptMessage, PromptMessageRole,
        // Resource types
        AnnotateAble, RawResource, ReadResourceRequestParams, ReadResourceResult, ResourceContents,
        ListResourcesResult, SubscribeRequestParams, UnsubscribeRequestParams,
        // Completion types
        CompleteRequestParams, CompleteResult, CompletionInfo,
        // Logging types
        SetLevelRequestParams, LoggingLevel,
        // Cancellation types
        CancelledNotificationParam,
        // Server types
        ServerCapabilities, ServerInfo,
    },
    prompt, prompt_handler, prompt_router, tool, tool_handler, tool_router,
    schemars::JsonSchema,
    service::{NotificationContext, RequestContext},
};

use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};

use kaijutsu_client::{SshConfig, connect_ssh};
use kaijutsu_crdt::{ConversationDAG, LV};
use kaijutsu_kernel::{DocumentKind, SharedBlockStore, shared_block_store, shared_block_flow_bus};

// Re-export public types
pub use models::*;
use helpers::*;
use tree::format_dag_tree;

// ============================================================================
// Prompt Argument Types
// ============================================================================

/// Arguments for the document analysis prompt
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Document analysis parameters")]
pub struct AnalyzeDocumentArgs {
    #[schemars(description = "Document ID to analyze")]
    pub document_id: String,
    #[schemars(description = "Focus area: 'structure', 'content', 'activity', or 'all'")]
    pub focus: Option<String>,
}

/// Arguments for the search context prompt
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Search context parameters")]
pub struct SearchContextArgs {
    #[schemars(description = "Search query (regex pattern)")]
    pub query: String,
    #[schemars(description = "Optional document ID to limit search")]
    pub document_id: Option<String>,
}

/// Arguments for the editing assistant prompt
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[schemars(description = "Editing assistant parameters")]
pub struct EditingAssistantArgs {
    #[schemars(description = "Block ID to edit")]
    pub block_id: String,
    #[schemars(description = "Edit type: 'refine', 'expand', 'summarize', or 'fix'")]
    pub edit_type: Option<String>,
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
/// Note: Cap'n Proto RPC types are !Send, which conflicts with rmcp's
/// Send+Sync requirement. We store connection info and reconnect when
/// pushing ops to the server.
#[derive(Clone)]
pub struct RemoteState {
    /// The document ID from our seat
    pub document_id: String,
    /// Kernel ID we connected to
    pub kernel_id: String,
    /// Local cache of synced state (with FlowBus for local event tracking)
    pub store: SharedBlockStore,
    /// Connection info for reconnecting
    pub host: String,
    pub port: u16,
    /// Server's frontier at last sync (used to calculate ops to push)
    /// Protected by Arc<Mutex<>> for thread-safe updates
    pub last_sync_frontier: Arc<Mutex<Vec<LV>>>,
}

// ============================================================================
// KaijutsuMcp Server
// ============================================================================

/// Shared state for server-side MCP features.
#[derive(Clone)]
pub struct McpServerState {
    /// Current logging level (default: info)
    pub log_level: Arc<Mutex<LoggingLevel>>,
    /// Resource subscriptions (URI -> subscription active)
    pub subscriptions: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl Default for McpServerState {
    fn default() -> Self {
        Self {
            log_level: Arc::new(Mutex::new(LoggingLevel::Info)),
            subscriptions: Arc::new(Mutex::new(std::collections::HashSet::new())),
        }
    }
}

/// MCP server exposing kaijutsu CRDT kernel.
#[derive(Clone)]
pub struct KaijutsuMcp {
    backend: Backend,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
    server_state: McpServerState,
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
            prompt_router: Self::prompt_router(),
            server_state: McpServerState::default(),
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
    /// Syncs initial state from the server into a local cache. The connection
    /// is closed after sync due to rmcp's Send+Sync requirements conflicting
    /// with Cap'n Proto's !Send types. Push operations reconnect as needed.
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
        let seat_handle = kernel.join_context("default", "mcp-server").await?;
        let seat_info = seat_handle.get_state().await?;

        // Derive document_id from kernel and context
        let document_id = format!("{}@{}", seat_info.id.kernel, seat_info.id.context);

        tracing::info!(
            kernel = %kernel_id,
            document_id = %document_id,
            "Connected to server"
        );

        // Create store with FlowBus for local event tracking
        let block_flows = shared_block_flow_bus(1024);
        let store = std::sync::Arc::new(
            kaijutsu_kernel::BlockStore::with_flows("mcp-remote", block_flows.clone())
        );

        // Sync document state from server
        let doc_state = kernel.get_document_state(&document_id).await?;

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

        // Get the local frontier after syncing (this represents what we received from server)
        let frontier = store.frontier(&doc_state.document_id)
            .unwrap_or_default();

        tracing::debug!(
            doc = %doc_state.document_id,
            frontier = ?frontier,
            "Initial sync complete, recorded frontier"
        );

        // Detach cleanly - we'll reconnect for push operations
        // Note: Real-time subscription requires a different architecture
        // that can maintain a persistent connection outside the MCP handler
        kernel.detach().await?;

        Ok(Self {
            backend: Backend::Remote(RemoteState {
                document_id,
                kernel_id: kernel_id.to_string(),
                store,
                host: host.to_string(),
                port,
                last_sync_frontier: Arc::new(Mutex::new(frontier)),
            }),
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
            server_state: McpServerState::default(),
        })
    }

    /// Get the underlying store for tool operations.
    fn store(&self) -> &SharedBlockStore {
        match &self.backend {
            Backend::Local(store) => store,
            Backend::Remote(remote) => &remote.store,
        }
    }

    /// Get the remote state if connected to a server.
    fn remote(&self) -> Option<&RemoteState> {
        match &self.backend {
            Backend::Local(_) => None,
            Backend::Remote(remote) => Some(remote),
        }
    }

    /// Push local changes to the server (async, requires LocalSet context).
    ///
    /// Returns the number of ops pushed and the new ack version.
    pub async fn push_to_server(&self) -> Result<(usize, u64), anyhow::Error> {
        let remote = self.remote()
            .ok_or_else(|| anyhow::anyhow!("Not connected to server"))?;

        // Get ops since last sync
        let frontier = remote.last_sync_frontier.lock()
            .map_err(|e| anyhow::anyhow!("Lock error: {}", e))?
            .clone();

        let ops = remote.store.ops_since(&remote.document_id, &frontier)
            .map_err(|e| anyhow::anyhow!(e))?;

        // Serialize ops for transmission
        let ops_bytes = serde_json::to_vec(&ops)
            .map_err(|e| anyhow::anyhow!("Serialize error: {}", e))?;

        if ops_bytes.len() <= 2 {
            // Empty ops (just "[]" or "{}")
            tracing::debug!("No ops to push");
            return Ok((0, 0));
        }

        tracing::debug!(
            doc = %remote.document_id,
            ops_bytes = ops_bytes.len(),
            "Pushing ops to server"
        );

        // Reconnect to server
        let config = SshConfig {
            host: remote.host.clone(),
            port: remote.port,
            username: whoami::username(),
            ..SshConfig::default()
        };

        let client = connect_ssh(config).await?;
        let kernel = client.attach_kernel(&remote.kernel_id).await?;

        // Push ops
        let ack_version = kernel.push_ops(&remote.document_id, &ops_bytes).await?;

        // Update last sync frontier to current local frontier
        let new_frontier = remote.store.frontier(&remote.document_id)
            .unwrap_or_default();

        *remote.last_sync_frontier.lock()
            .map_err(|e| anyhow::anyhow!("Lock error: {}", e))? = new_frontier;

        // Detach
        kernel.detach().await?;

        tracing::info!(
            doc = %remote.document_id,
            ack_version,
            "Pushed ops successfully"
        );

        // Estimate ops count (rough, based on serialized size)
        let ops_count = ops_bytes.len() / 50; // Rough estimate

        Ok((ops_count.max(1), ack_version))
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

        let mut metadata = serde_json::json!({
            "tool_name": snapshot.tool_name,
            "tool_call_id": snapshot.tool_call_id.map(|id| id.to_key()),
            "is_error": snapshot.is_error,
        });

        // Include drift provenance when present
        if let Some(ref ctx) = snapshot.source_context {
            metadata["source_context"] = serde_json::json!(ctx);
        }
        if let Some(ref model) = snapshot.source_model {
            metadata["source_model"] = serde_json::json!(model);
        }
        if let Some(ref dk) = snapshot.drift_kind {
            metadata["drift_kind"] = serde_json::json!(dk.to_string());
        }

        serde_json::json!({
            "content": formatted_content,
            "role": snapshot.role.as_str(),
            "kind": snapshot.kind.as_str(),
            "status": snapshot.status.as_str(),
            "version": entry.version(),
            "line_count": total_lines,
            "metadata": metadata,
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

                    let mut block_sum = BlockSummary {
                        block_id: snapshot.id.to_key(),
                        parent_id: snapshot.parent_id.map(|id| id.to_key()),
                        role: snapshot.role.as_str().to_string(),
                        kind: snapshot.kind.as_str().to_string(),
                        status: snapshot.status.as_str().to_string(),
                        summary,
                    };

                    // Prepend drift source to summary for drift blocks
                    if snapshot.kind == kaijutsu_crdt::BlockKind::Drift {
                        if let Some(ref ctx) = snapshot.source_context {
                            let model = snapshot.source_model.as_deref().unwrap_or("?");
                            block_sum.summary = format!(
                                "[drift from {} via {}] {}",
                                ctx, model, block_sum.summary
                            );
                        }
                    }

                    blocks.push(block_sum);
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

    // ========================================================================
    // Drift Tools (Cross-Context Communication)
    // ========================================================================

    #[tool(description = "List all registered drift contexts. Shows short IDs, names, providers, and lineage. Requires remote connection.")]
    fn drift_ls(&self) -> String {
        let remote = match self.remote() {
            Some(r) => r,
            None => return "Error: drift_ls requires a remote connection to kaijutsu-server".to_string(),
        };

        // Use tokio current thread to reconnect and call RPC
        let host = remote.host.clone();
        let port = remote.port;
        let kernel_id = remote.kernel_id.clone();

        // Since we can't do async in a sync tool handler, return connection info
        // The actual RPC call would need the async push_to_server pattern
        serde_json::json!({
            "note": "drift_ls requires async RPC — use kaish 'drift ls' or direct RPC for now",
            "server": format!("{}:{}", host, port),
            "kernel": kernel_id,
        }).to_string()
    }

    #[tool(description = "Stage a drift push to transfer content to another context. Requires remote connection.")]
    fn drift_push(&self, Parameters(req): Parameters<DriftPushRequest>) -> String {
        let _remote = match self.remote() {
            Some(r) => r,
            None => return "Error: drift_push requires a remote connection to kaijutsu-server".to_string(),
        };

        serde_json::json!({
            "note": "drift_push requires async RPC — use kaish 'drift push' or direct RPC for now",
            "target_ctx": req.target_ctx,
            "content_length": req.content.len(),
            "summarize": req.summarize,
        }).to_string()
    }

    #[tool(description = "View the drift staging queue. Shows pending transfers awaiting flush. Requires remote connection.")]
    fn drift_queue(&self) -> String {
        match self.remote() {
            Some(_) => serde_json::json!({
                "note": "drift_queue requires async RPC — use kaish 'drift queue' or direct RPC for now",
            }).to_string(),
            None => "Error: drift_queue requires a remote connection to kaijutsu-server".to_string(),
        }
    }

    #[tool(description = "Cancel a staged drift by its ID. Requires remote connection.")]
    fn drift_cancel(&self, Parameters(req): Parameters<DriftCancelRequest>) -> String {
        match self.remote() {
            Some(_) => serde_json::json!({
                "note": "drift_cancel requires async RPC — use kaish 'drift cancel' or direct RPC for now",
                "staged_id": req.staged_id,
            }).to_string(),
            None => "Error: drift_cancel requires a remote connection to kaijutsu-server".to_string(),
        }
    }

    #[tool(description = "Flush all staged drifts, injecting content into target contexts. Requires remote connection.")]
    fn drift_flush(&self) -> String {
        match self.remote() {
            Some(_) => serde_json::json!({
                "note": "drift_flush requires async RPC — use kaish 'drift flush' or direct RPC for now",
            }).to_string(),
            None => "Error: drift_flush requires a remote connection to kaijutsu-server".to_string(),
        }
    }

    #[tool(description = "Pull summarized content from another context. Reads the source context's conversation, distills it via LLM, and injects the summary as a Drift block in the current context. Use 'prompt' to direct the summary focus.")]
    fn drift_pull(&self, Parameters(req): Parameters<DriftPullRequest>) -> String {
        match self.remote() {
            Some(_) => serde_json::json!({
                "note": "drift_pull requires async RPC — use kaish 'drift pull' or direct RPC for now",
                "source_ctx": req.source_ctx,
                "prompt": req.prompt,
            }).to_string(),
            None => "Error: drift_pull requires a remote connection to kaijutsu-server".to_string(),
        }
    }

    #[tool(description = "Merge a forked context back into its parent. Distills the fork's conversation via LLM and injects the summary into the parent context as a Drift block.")]
    fn drift_merge(&self, Parameters(req): Parameters<DriftMergeRequest>) -> String {
        match self.remote() {
            Some(_) => serde_json::json!({
                "note": "drift_merge requires async RPC — use kaish 'drift merge' or direct RPC for now",
                "source_ctx": req.source_ctx,
            }).to_string(),
            None => "Error: drift_merge requires a remote connection to kaijutsu-server".to_string(),
        }
    }

    // ========================================================================
    // Undo
    // ========================================================================

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

// ============================================================================
// Prompt Router
// ============================================================================

#[prompt_router]
impl KaijutsuMcp {
    /// Analyze a document's structure, content, and activity.
    ///
    /// Provides comprehensive context about a document for LLM consumption,
    /// including block relationships, content summaries, and metadata.
    #[prompt(
        name = "analyze_document",
        description = "Analyze a document's structure, content, and activity for comprehensive understanding"
    )]
    fn analyze_document(
        &self,
        Parameters(args): Parameters<AnalyzeDocumentArgs>,
    ) -> Result<GetPromptResult, McpError> {
        let entry = self.store().get(&args.document_id)
            .ok_or_else(|| McpError::invalid_params(
                format!("Document '{}' not found", args.document_id),
                None
            ))?;

        let focus = args.focus.as_deref().unwrap_or("all");
        let blocks = entry.doc.blocks_ordered();

        let mut content = String::new();

        // Document overview
        content.push_str(&format!("# Document: {}\n\n", args.document_id));
        content.push_str(&format!("**Kind:** {}\n", entry.kind.as_str()));
        if let Some(ref lang) = entry.language {
            content.push_str(&format!("**Language:** {}\n", lang));
        }
        content.push_str(&format!("**Block count:** {}\n", blocks.len()));
        content.push_str(&format!("**Version:** {}\n\n", entry.version()));

        // Structure analysis
        if focus == "all" || focus == "structure" {
            content.push_str("## Structure\n\n");
            let dag = ConversationDAG::from_document(&entry.doc);
            let tree_lines = format_dag_tree(&dag, None, false);
            for line in tree_lines {
                content.push_str(&line);
                content.push('\n');
            }
            content.push('\n');
        }

        // Content summaries
        if focus == "all" || focus == "content" {
            content.push_str("## Content Summary\n\n");
            for (i, block) in blocks.iter().enumerate() {
                let preview = if block.content.len() > 200 {
                    format!("{}...", &block.content[..200])
                } else {
                    block.content.clone()
                };
                content.push_str(&format!(
                    "### Block {} [{}/{}]\n{}\n\n",
                    i + 1,
                    block.role.as_str(),
                    block.kind.as_str(),
                    preview
                ));
            }
        }

        // Activity/metadata
        if focus == "all" || focus == "activity" {
            content.push_str("## Activity\n\n");
            let mut authors: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for block in &blocks {
                authors.insert(&block.author);
            }
            content.push_str(&format!("**Authors:** {}\n", authors.into_iter().collect::<Vec<_>>().join(", ")));

            // Count by role
            let mut role_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
            for block in &blocks {
                *role_counts.entry(block.role.as_str()).or_insert(0) += 1;
            }
            content.push_str("**Blocks by role:**\n");
            for (role, count) in role_counts {
                content.push_str(&format!("  - {}: {}\n", role, count));
            }
        }

        Ok(GetPromptResult {
            description: Some(format!("Analysis of document '{}'", args.document_id)),
            messages: vec![PromptMessage {
                role: PromptMessageRole::User,
                content: rmcp::model::PromptMessageContent::Text {
                    text: content,
                },
            }],
        })
    }

    /// Search across documents and provide context around matches.
    ///
    /// Finds matching content using regex and returns results with surrounding
    /// context, ideal for understanding code or conversation patterns.
    #[prompt(
        name = "search_context",
        description = "Search documents and return matches with surrounding context"
    )]
    fn search_context(
        &self,
        Parameters(args): Parameters<SearchContextArgs>,
    ) -> Result<GetPromptResult, McpError> {
        let regex = Regex::new(&args.query)
            .map_err(|e| McpError::invalid_params(
                format!("Invalid regex '{}': {}", args.query, e),
                None
            ))?;

        let document_ids: Vec<String> = if let Some(ref doc_id) = args.document_id {
            if self.store().contains(doc_id) {
                vec![doc_id.clone()]
            } else {
                return Err(McpError::invalid_params(
                    format!("Document '{}' not found", doc_id),
                    None
                ));
            }
        } else {
            self.store().list_ids()
        };

        let mut content = String::new();
        content.push_str(&format!("# Search Results for: `{}`\n\n", args.query));

        let mut total_matches = 0;
        let context_lines = 3;

        for document_id in document_ids {
            let snapshots = match self.store().block_snapshots(&document_id) {
                Ok(s) => s,
                Err(_) => continue,
            };

            for snapshot in snapshots {
                let lines: Vec<&str> = snapshot.content.lines().collect();
                for (line_idx, line) in lines.iter().enumerate() {
                    if regex.is_match(line) {
                        total_matches += 1;

                        content.push_str(&format!(
                            "## Match in `{}`:{} [{}/{}]\n",
                            document_id,
                            snapshot.id.to_key(),
                            snapshot.role.as_str(),
                            snapshot.kind.as_str()
                        ));
                        content.push_str("```\n");

                        // Context before
                        let start = line_idx.saturating_sub(context_lines);
                        for i in start..line_idx {
                            content.push_str(&format!("{:4} │ {}\n", i + 1, lines[i]));
                        }

                        // Matching line (highlighted)
                        content.push_str(&format!("{:4} │ >>> {} <<<\n", line_idx + 1, line));

                        // Context after
                        let end = (line_idx + 1 + context_lines).min(lines.len());
                        for i in (line_idx + 1)..end {
                            content.push_str(&format!("{:4} │ {}\n", i + 1, lines[i]));
                        }

                        content.push_str("```\n\n");

                        if total_matches >= 20 {
                            content.push_str("*... (truncated, showing first 20 matches)*\n");
                            break;
                        }
                    }
                }
            }
        }

        if total_matches == 0 {
            content.push_str("*No matches found.*\n");
        } else {
            content.push_str(&format!("\n**Total matches:** {}\n", total_matches));
        }

        Ok(GetPromptResult {
            description: Some(format!("Search results for '{}'", args.query)),
            messages: vec![PromptMessage {
                role: PromptMessageRole::User,
                content: rmcp::model::PromptMessageContent::Text {
                    text: content,
                },
            }],
        })
    }

    /// Assist with editing a block by providing context and suggestions.
    ///
    /// Reads a block's content and metadata, then provides editing context
    /// based on the requested edit type (refine, expand, summarize, fix).
    #[prompt(
        name = "editing_assistant",
        description = "Get editing context and suggestions for a specific block"
    )]
    fn editing_assistant(
        &self,
        Parameters(args): Parameters<EditingAssistantArgs>,
    ) -> Result<GetPromptResult, McpError> {
        let (document_id, block_id) = find_block(self.store(), &args.block_id)
            .ok_or_else(|| McpError::invalid_params(
                format!("Block '{}' not found", args.block_id),
                None
            ))?;

        let entry = self.store().get(&document_id)
            .ok_or_else(|| McpError::invalid_params("Document not found", None))?;

        let snapshot = entry.doc.get_block_snapshot(&block_id)
            .ok_or_else(|| McpError::invalid_params("Block not found", None))?;

        let edit_type = args.edit_type.as_deref().unwrap_or("refine");

        let mut content = String::new();

        content.push_str(&format!("# Editing Assistant: {}\n\n", args.block_id));
        content.push_str(&format!("**Document:** {}\n", document_id));
        content.push_str(&format!("**Role:** {}\n", snapshot.role.as_str()));
        content.push_str(&format!("**Kind:** {}\n", snapshot.kind.as_str()));
        content.push_str(&format!("**Edit type:** {}\n\n", edit_type));

        content.push_str("## Current Content\n\n");
        content.push_str("```\n");
        content.push_str(&snapshot.content);
        if !snapshot.content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str("```\n\n");

        // Add parent context if available
        if let Some(parent_id) = snapshot.parent_id {
            if let Some(parent_snap) = entry.doc.get_block_snapshot(&parent_id) {
                content.push_str("## Parent Context\n\n");
                let preview = if parent_snap.content.len() > 500 {
                    format!("{}...", &parent_snap.content[..500])
                } else {
                    parent_snap.content.clone()
                };
                content.push_str(&format!(
                    "[{}/{}]\n```\n{}\n```\n\n",
                    parent_snap.role.as_str(),
                    parent_snap.kind.as_str(),
                    preview
                ));
            }
        }

        // Add edit-type specific instructions
        content.push_str("## Instructions\n\n");
        match edit_type {
            "refine" => {
                content.push_str("Please refine this content by:\n");
                content.push_str("- Improving clarity and conciseness\n");
                content.push_str("- Fixing any grammatical or spelling errors\n");
                content.push_str("- Maintaining the original meaning and intent\n");
            }
            "expand" => {
                content.push_str("Please expand this content by:\n");
                content.push_str("- Adding more detail and explanation\n");
                content.push_str("- Including relevant examples\n");
                content.push_str("- Elaborating on key points\n");
            }
            "summarize" => {
                content.push_str("Please summarize this content by:\n");
                content.push_str("- Extracting the key points\n");
                content.push_str("- Reducing length while preserving meaning\n");
                content.push_str("- Creating a concise overview\n");
            }
            "fix" => {
                content.push_str("Please fix any issues in this content:\n");
                content.push_str("- Correct errors or bugs (if code)\n");
                content.push_str("- Fix logical inconsistencies\n");
                content.push_str("- Address any incomplete sections\n");
            }
            _ => {
                content.push_str(&format!(
                    "Edit type '{}' not recognized. Please describe what changes you'd like.\n",
                    edit_type
                ));
            }
        }

        Ok(GetPromptResult {
            description: Some(format!("Editing assistant for block '{}'", args.block_id)),
            messages: vec![PromptMessage {
                role: PromptMessageRole::User,
                content: rmcp::model::PromptMessageContent::Text {
                    text: content,
                },
            }],
        })
    }
}

#[tool_handler]
#[prompt_handler]
impl ServerHandler for KaijutsuMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Kaijutsu CRDT kernel MCP server. Provides tools for collaborative document and block editing with CRDT-backed consistency.".into()
            ),
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_prompts_list_changed()
                .enable_resources()
                .enable_resources_subscribe()
                .enable_logging()
                .enable_completions()
                .build(),
            ..Default::default()
        }
    }

    // ========================================================================
    // Resources
    // ========================================================================

    /// List available resources.
    ///
    /// Resources exposed:
    /// - `kaijutsu://docs` - List all documents
    /// - `kaijutsu://docs/{doc_id}` - Document metadata and block list
    /// - `kaijutsu://blocks/{doc_id}/{block_key}` - Block content
    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListResourcesResult, McpError>> + Send + '_ {
        async move {
            let mut resources = Vec::new();

            // Add root docs resource
            resources.push(RawResource {
                uri: "kaijutsu://docs".to_string(),
                name: "documents".to_string(),
                title: Some("All Documents".to_string()),
                description: Some("List of all documents in the kernel".to_string()),
                mime_type: Some("application/json".to_string()),
                size: None,
                icons: None,
                meta: None,
            }.no_annotation());

            // Add each document as a resource
            for doc_id in self.store().list_ids() {
                if let Some(entry) = self.store().get(&doc_id) {
                    resources.push(RawResource {
                        uri: format!("kaijutsu://docs/{}", doc_id),
                        name: doc_id.clone(),
                        title: Some(format!("Document: {}", doc_id)),
                        description: Some(format!(
                            "{} document with {} blocks",
                            entry.kind.as_str(),
                            entry.doc.blocks_ordered().len()
                        )),
                        mime_type: Some("application/json".to_string()),
                        size: None,
                        icons: None,
                        meta: None,
                    }.no_annotation());

                    // Add each block as a resource
                    for snapshot in entry.doc.blocks_ordered() {
                        let block_key = snapshot.id.to_key();
                        resources.push(RawResource {
                            uri: format!("kaijutsu://blocks/{}/{}", doc_id, block_key),
                            name: block_key.clone(),
                            title: Some(format!("[{}/{}]", snapshot.role.as_str(), snapshot.kind.as_str())),
                            description: Some(format!(
                                "{} block, {} bytes",
                                snapshot.kind.as_str(),
                                snapshot.content.len()
                            )),
                            mime_type: Some("text/plain".to_string()),
                            size: Some(snapshot.content.len() as u32),
                            icons: None,
                            meta: None,
                        }.no_annotation());
                    }
                }
            }

            Ok(ListResourcesResult {
                meta: None,
                next_cursor: None,
                resources,
            })
        }
    }

    /// Read a specific resource.
    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ReadResourceResult, McpError>> + Send + '_ {
        async move {
            let uri = &request.uri;

            // Parse URI: kaijutsu://docs, kaijutsu://docs/{id}, kaijutsu://blocks/{id}/{key}
            if uri == "kaijutsu://docs" {
                // Return list of all documents
                let docs: Vec<serde_json::Value> = self.store().list_ids().iter().map(|id| {
                    let (kind, block_count) = self.store().get(id)
                        .map(|e| (e.kind.as_str().to_string(), e.doc.blocks_ordered().len()))
                        .unwrap_or(("unknown".to_string(), 0));
                    serde_json::json!({
                        "id": id,
                        "kind": kind,
                        "block_count": block_count
                    })
                }).collect();

                let content = serde_json::to_string_pretty(&docs)
                    .unwrap_or_else(|_| "[]".to_string());

                return Ok(ReadResourceResult {
                    contents: vec![ResourceContents::text(content, uri.clone())],
                });
            }

            if let Some(doc_id) = uri.strip_prefix("kaijutsu://docs/") {
                // Return document metadata and block list
                let entry = self.store().get(doc_id)
                    .ok_or_else(|| McpError::invalid_params(
                        format!("Document '{}' not found", doc_id),
                        None
                    ))?;

                let blocks: Vec<serde_json::Value> = entry.doc.blocks_ordered().iter().map(|s| {
                    serde_json::json!({
                        "id": s.id.to_key(),
                        "role": s.role.as_str(),
                        "kind": s.kind.as_str(),
                        "status": s.status.as_str(),
                        "content_preview": if s.content.len() > 100 {
                            format!("{}...", &s.content[..100])
                        } else {
                            s.content.clone()
                        }
                    })
                }).collect();

                let result = serde_json::json!({
                    "id": doc_id,
                    "kind": entry.kind.as_str(),
                    "language": entry.language,
                    "version": entry.version(),
                    "blocks": blocks
                });

                let content = serde_json::to_string_pretty(&result)
                    .unwrap_or_else(|_| "{}".to_string());

                return Ok(ReadResourceResult {
                    contents: vec![ResourceContents::text(content, uri.clone())],
                });
            }

            if let Some(rest) = uri.strip_prefix("kaijutsu://blocks/") {
                // Parse doc_id/block_key
                let parts: Vec<&str> = rest.splitn(2, '/').collect();
                if parts.len() != 2 {
                    return Err(McpError::invalid_params(
                        format!("Invalid block URI format: {}", uri),
                        None
                    ));
                }

                let doc_id = parts[0];
                let block_key = parts[1];

                let (_, block_id) = find_block(self.store(), block_key)
                    .ok_or_else(|| McpError::invalid_params(
                        format!("Block '{}' not found in document '{}'", block_key, doc_id),
                        None
                    ))?;

                let entry = self.store().get(doc_id)
                    .ok_or_else(|| McpError::invalid_params(
                        format!("Document '{}' not found", doc_id),
                        None
                    ))?;

                let snapshot = entry.doc.get_block_snapshot(&block_id)
                    .ok_or_else(|| McpError::invalid_params("Block not found", None))?;

                return Ok(ReadResourceResult {
                    contents: vec![ResourceContents::text(snapshot.content.clone(), uri.clone())],
                });
            }

            Err(McpError::invalid_params(
                format!("Unknown resource URI: {}", uri),
                None
            ))
        }
    }

    /// Subscribe to resource updates.
    fn subscribe(
        &self,
        request: SubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), McpError>> + Send + '_ {
        async move {
            let mut subs = self.server_state.subscriptions.lock()
                .map_err(|_| McpError::internal_error("Lock error", None))?;
            subs.insert(request.uri);
            Ok(())
        }
    }

    /// Unsubscribe from resource updates.
    fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), McpError>> + Send + '_ {
        async move {
            let mut subs = self.server_state.subscriptions.lock()
                .map_err(|_| McpError::internal_error("Lock error", None))?;
            subs.remove(&request.uri);
            Ok(())
        }
    }

    // ========================================================================
    // Completion
    // ========================================================================

    /// Provide completions for prompts and resources.
    fn complete(
        &self,
        request: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CompleteResult, McpError>> + Send + '_ {
        async move {
            let values = match &request.r#ref {
                rmcp::model::Reference::Prompt(prompt_ref) => {
                    // Complete prompt arguments
                    match prompt_ref.name.as_str() {
                        "analyze_document" | "editing_assistant" => {
                            if request.argument.name == "document_id" || request.argument.name == "block_id" {
                                // Complete document IDs
                                self.store().list_ids().into_iter()
                                    .filter(|id| id.contains(&request.argument.value))
                                    .take(10)
                                    .collect()
                            } else if request.argument.name == "focus" {
                                // Complete focus values
                                vec!["all", "structure", "content", "activity"]
                                    .into_iter()
                                    .filter(|v| v.contains(&request.argument.value))
                                    .map(String::from)
                                    .collect()
                            } else if request.argument.name == "edit_type" {
                                // Complete edit types
                                vec!["refine", "expand", "summarize", "fix"]
                                    .into_iter()
                                    .filter(|v| v.contains(&request.argument.value))
                                    .map(String::from)
                                    .collect()
                            } else {
                                Vec::new()
                            }
                        }
                        "search_context" => {
                            if request.argument.name == "document_id" {
                                self.store().list_ids().into_iter()
                                    .filter(|id| id.contains(&request.argument.value))
                                    .take(10)
                                    .collect()
                            } else {
                                Vec::new()
                            }
                        }
                        _ => Vec::new(),
                    }
                }
                rmcp::model::Reference::Resource(resource_ref) => {
                    // Complete resource URIs
                    let prefix = &resource_ref.uri;
                    if prefix.starts_with("kaijutsu://docs") {
                        self.store().list_ids().into_iter()
                            .map(|id| format!("kaijutsu://docs/{}", id))
                            .filter(|uri| uri.contains(&request.argument.value))
                            .take(10)
                            .collect()
                    } else {
                        Vec::new()
                    }
                }
            };

            Ok(CompleteResult {
                completion: CompletionInfo {
                    values,
                    total: None,
                    has_more: Some(false),
                },
            })
        }
    }

    // ========================================================================
    // Logging
    // ========================================================================

    /// Set the logging level.
    fn set_level(
        &self,
        request: SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), McpError>> + Send + '_ {
        async move {
            let mut level = self.server_state.log_level.lock()
                .map_err(|_| McpError::internal_error("Lock error", None))?;
            *level = request.level;
            tracing::info!("Log level set to {:?}", request.level);
            Ok(())
        }
    }

    // ========================================================================
    // Cancellation
    // ========================================================================

    /// Handle cancellation notifications.
    fn on_cancelled(
        &self,
        notification: CancelledNotificationParam,
        _context: NotificationContext<RoleServer>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        async move {
            tracing::info!(
                request_id = ?notification.request_id,
                reason = ?notification.reason,
                "Request cancelled"
            );
            // Future: track in-flight operations and cancel them
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
