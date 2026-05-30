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
pub mod hook_listener;
pub mod hook_types;
mod models;
mod tree;

use regex::Regex;

/// Wrapper that aborts a tokio task when the last reference is dropped.
#[derive(Clone)]
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{
        router::prompt::PromptRouter, router::tool::ToolRouter, wrapper::Parameters,
    },
    model::{
        // Resource types
        AnnotateAble,
        // Cancellation types
        CancelledNotificationParam,
        // Completion types
        CompleteRequestParams,
        CompleteResult,
        CompletionInfo,
        // Prompt types
        GetPromptRequestParams,
        GetPromptResult,
        ListPromptsResult,
        ListResourcesResult,
        LoggingLevel,
        PaginatedRequestParams,
        PromptMessage,
        PromptMessageRole,
        RawResource,
        ReadResourceRequestParams,
        ReadResourceResult,
        ResourceContents,
        // Server types
        ServerCapabilities,
        ServerInfo,
        // Logging types
        SetLevelRequestParams,
        SubscribeRequestParams,
        UnsubscribeRequestParams,
    },
    prompt, prompt_handler, prompt_router,
    schemars::JsonSchema,
    service::{NotificationContext, RequestContext},
    tool, tool_handler, tool_router,
};

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

use kaijutsu_client::{ActorHandle, ServerEvent, SshConfig, SyncManager, connect_ssh, spawn_actor};
use kaijutsu_crdt::{BlockId, ContextId, ConversationDAG, PrincipalId};
use kaijutsu_kernel::block_store::DocumentKind as DocKind;
use kaijutsu_kernel::{SharedBlockStore, shared_block_flow_bus, shared_block_store};

// Re-export public types
use helpers::*;
pub use models::*;
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

/// Outcome of `execute_and_poll_shell` — passed to per-tool JSON builders.
///
/// `Done` carries the completed ToolResult snapshot. The other variants
/// describe failure modes that don't have a snapshot to read but still
/// have useful diagnostic info (block id, elapsed time).
enum ShellCompletion {
    Done {
        snapshot: kaijutsu_crdt::BlockSnapshot,
        elapsed_ms: u64,
    },
    Timeout {
        cmd_block_id: BlockId,
        timeout_secs: u64,
        elapsed_ms: u64,
    },
    StreamClosed {
        cmd_block_id: BlockId,
        elapsed_ms: u64,
    },
}

impl ShellCompletion {
    /// Render this completion as the JSON envelope returned by `shell` and
    /// `context_shell`. The shape is documented on the tool descriptions —
    /// agents parse this to extract `stdout`, `exit_code`, structured `data`,
    /// and the result block id for follow-up reads.
    fn to_json(&self) -> String {
        match self {
            Self::Done {
                snapshot,
                elapsed_ms,
            } => {
                // Derive exit_code: prefer the persisted value (the real
                // kaish exit code); fall back to status mapping when absent
                // (e.g. ToolResult blocks written by paths that predate
                // exit_code propagation).
                let exit_code = snapshot.exit_code.unwrap_or_else(|| match snapshot.status {
                    kaijutsu_crdt::Status::Done => 0,
                    _ => 1,
                });
                let stdout = if snapshot.content.is_empty() {
                    "".to_string()
                } else {
                    snapshot.content.clone()
                };
                let data = snapshot
                    .output
                    .as_ref()
                    .and_then(|o| serde_json::to_value(o).ok());
                serde_json::json!({
                    "stdout": stdout,
                    "exit_code": exit_code,
                    "status": snapshot.status.as_str(),
                    "block_id": snapshot.id.to_key(),
                    "content_type": snapshot.content_type.as_mime(),
                    "ephemeral": snapshot.ephemeral,
                    "data": data,
                    "elapsed_ms": elapsed_ms,
                })
                .to_string()
            }
            Self::Timeout {
                cmd_block_id,
                timeout_secs,
                elapsed_ms,
            } => serde_json::json!({
                "stdout": "",
                "exit_code": -1,
                "status": "timeout",
                "block_id": cmd_block_id.to_key(),
                "elapsed_ms": elapsed_ms,
                "error": format!("Timeout after {}s waiting for command", timeout_secs),
            })
            .to_string(),
            Self::StreamClosed {
                cmd_block_id,
                elapsed_ms,
            } => serde_json::json!({
                "stdout": "",
                "exit_code": -1,
                "status": "stream_closed",
                "block_id": cmd_block_id.to_key(),
                "elapsed_ms": elapsed_ms,
                "error": "Event stream closed before completion",
            })
            .to_string(),
        }
    }
}

/// Remote backend state — persistent actor connection to kaijutsu-server.
///
/// The `ActorHandle` is `Send+Sync` and wraps the `!Send` Cap'n Proto
/// types in a `spawn_local` task with auto-reconnect.
///
/// Context joining is deferred — `connect()` establishes the SSH connection
/// and spawns the actor, but does not join a context. Call `register_session`
/// to create and join a context, which populates the store.
#[derive(Clone)]
pub struct RemoteState {
    /// Kernel ID we connected to
    pub kernel_id: kaijutsu_crdt::KernelId,
    /// Send+Sync actor handle for RPC operations
    pub actor: ActorHandle,
    /// Local block store — starts empty, populated by register_session.
    pub store: SharedBlockStore,
    /// Joined context state (None until register_session is called)
    pub joined: Arc<tokio::sync::RwLock<Option<JoinedContext>>>,
    /// Shared context_id for hook listener (updated by register_session)
    pub shared_context_id: Arc<Mutex<Option<kaijutsu_crdt::ContextId>>>,
}

/// State for a joined context — created by `register_session`.
#[derive(Clone)]
pub struct JoinedContext {
    /// Context ID we joined
    pub context_id: kaijutsu_crdt::ContextId,
    /// Frontier-tracking sync state machine.
    pub sync: Arc<Mutex<SyncManager>>,
    /// Abort handle for the background event listener.
    _bg_task: Arc<AbortOnDrop>,
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
    /// Handle to abort the background event listener when all clones are dropped.
    _bg_task: Option<Arc<AbortOnDrop>>,
    /// Agent session ID (e.g., Claude Code session UUID).
    session_id: Arc<Mutex<Option<String>>>,
    /// Context label used at connection time.
    context_name: String,
    /// Detected agent name (e.g., "claude-code").
    agent_name: Option<String>,
    /// Per-session principal for block authorship.
    session_principal: PrincipalId,
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
            _bg_task: None,
            session_id: Arc::new(Mutex::new(None)),
            context_name: "local".to_string(),
            agent_name: None,
            session_principal: PrincipalId::new(),
        }
    }

    /// Create a new MCP server with an in-memory store.
    pub fn new() -> Self {
        let principal = PrincipalId::new();
        Self::with_store(shared_block_store(principal))
    }

    /// Connect to a running kaijutsu-server via SSH.
    ///
    /// Uses ssh-agent for authentication. Must be called within a `LocalSet`.
    ///
    /// Establishes the SSH connection and spawns the actor, but does NOT
    /// join a context. Call `register_session` to create and join a context.
    pub async fn connect(
        host: &str,
        port: u16,
        context_name: &str,
        cc_session_id: Option<&str>,
    ) -> Result<Self, anyhow::Error> {
        let config = SshConfig {
            host: host.to_string(),
            port,
            username: whoami::username(),
            ..SshConfig::default()
        };

        tracing::debug!(?config, "Connecting via SSH");

        let client = connect_ssh(config.clone()).await?;
        let (_kernel, kernel_id_typed) = client.bind_kernel().await?;

        tracing::info!(
            kernel = %kernel_id_typed,
            context_label = %context_name,
            "Connected to server (no context joined yet)"
        );

        // Drop the eagerly-built client+kernel; the actor builds its own
        // connection via its FSM. The eager bind_kernel above served as a
        // permission probe (auth, kernel reachable) before we commit to
        // spawning. The 50ms re-handshake by the actor is acceptable.
        drop(client);

        // Spawn actor with no context — it will join via register_session.
        let actor = spawn_actor(config, None, "mcp-server".to_string());

        tracing::info!("RPC actor spawned, persistent connection ready");

        let shared_context_id = Arc::new(Mutex::new(None));
        let session_principal = PrincipalId::new();

        // Create an empty store — populated by register_session
        let store = std::sync::Arc::new(kaijutsu_kernel::BlockStore::with_flows(
            session_principal,
            shared_block_flow_bus(1024),
        ));

        Ok(Self {
            backend: Backend::Remote(RemoteState {
                kernel_id: kernel_id_typed,
                actor,
                store,
                joined: Arc::new(tokio::sync::RwLock::new(None)),
                shared_context_id,
            }),
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
            server_state: McpServerState::default(),
            _bg_task: None,
            session_id: Arc::new(Mutex::new(cc_session_id.map(String::from))),
            context_name: context_name.to_string(),
            agent_name: cc_session_id.map(|_| "claude-code".to_string()),
            session_principal,
        })
    }

    /// Get the backend variant (for hook listener setup, etc.).
    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    /// Get the shared session ID arc (for hook listener to share).
    pub fn session_id_arc(&self) -> &Arc<Mutex<Option<String>>> {
        &self.session_id
    }

    /// Get the underlying store for tool operations.
    ///
    /// For Remote mode, returns the store which starts empty and is populated
    /// by `register_session`.
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

    /// Get the joined context's context_id and sync state.
    /// Returns an error string if no context has been joined (register_session not called).
    async fn require_joined(&self) -> Result<(ContextId, &SharedBlockStore, &ActorHandle), String> {
        match &self.backend {
            Backend::Local(_) => Err("Error: not connected to server".to_string()),
            Backend::Remote(remote) => {
                let guard = remote.joined.read().await;
                match guard.as_ref() {
                    Some(joined) => Ok((joined.context_id, &remote.store, &remote.actor)),
                    None => {
                        Err("Error: no active context — call register_session first".to_string())
                    }
                }
            }
        }
    }

    /// Push local changes to the server via the actor.
    ///
    /// Returns the number of ops pushed and the new ack version.
    pub async fn push_to_server(&self) -> Result<(usize, u64), anyhow::Error> {
        let remote = self
            .remote()
            .ok_or_else(|| anyhow::anyhow!("Not connected to server"))?;

        let guard = remote.joined.read().await;
        let joined = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No active context — call register_session first"))?;

        // Get ops since last sync frontier from SyncManager
        let frontier = {
            let sync = joined
                .sync
                .lock()
                .map_err(|e| anyhow::anyhow!("Lock error: {}", e))?;
            sync.frontier().cloned().unwrap_or_default()
        };

        let ops = remote
            .store
            .ops_since(joined.context_id, &frontier)
            .map_err(|e| anyhow::anyhow!(e))?;

        // Check for empty payload before serializing
        if ops.block_ops.is_empty()
            && ops.new_blocks.is_empty()
            && ops.updated_headers.is_empty()
            && ops.deleted_blocks.is_empty()
        {
            tracing::debug!("No ops to push");
            return Ok((0, 0));
        }

        let ops_bytes =
            postcard::to_allocvec(&ops).map_err(|e| anyhow::anyhow!("Serialize error: {}", e))?;

        tracing::debug!(
            ctx = %joined.context_id,
            ops_bytes = ops_bytes.len(),
            "Pushing ops to server"
        );

        // Push via persistent actor (no reconnect dance)
        let ack_version = remote
            .actor
            .push_ops(joined.context_id, &ops_bytes)
            .await
            .map_err(|e| anyhow::anyhow!("Push ops: {e}"))?;

        tracing::info!(ctx = %joined.context_id, ack_version, "Pushed ops");

        let ops_count = ops_bytes.len() / 50; // Rough estimate
        Ok((ops_count.max(1), ack_version))
    }

    /// Get the actor handle for direct RPC operations.
    fn actor(&self) -> Option<&ActorHandle> {
        match &self.backend {
            Backend::Local(_) => None,
            Backend::Remote(remote) => Some(&remote.actor),
        }
    }

    /// Resolve a user-provided context query (label or hex prefix) to a ContextId.
    async fn resolve_context(
        &self,
        actor: &ActorHandle,
        query: &str,
    ) -> Result<kaijutsu_crdt::ContextId, String> {
        let contexts = actor
            .list_contexts()
            .await
            .map_err(|e| format!("Error listing contexts: {e}"))?;
        let entries = contexts.iter().map(|c| {
            let label: Option<&str> = if c.label.is_empty() {
                None
            } else {
                Some(&c.label)
            };
            (c.id, label)
        });
        kaijutsu_crdt::resolve_context_prefix(entries, query)
            .map_err(|e| format!("Error resolving context '{query}': {e}"))
    }

    /// Resolve a context ID for input document operations.
    ///
    /// If `query` is Some, resolves via label/hex prefix lookup (Remote) or
    /// direct parse (Local). If None, falls back to the current joined
    /// context (Remote) or errors (Local).
    async fn resolve_input_context(
        &self,
        query: Option<&str>,
    ) -> Result<kaijutsu_crdt::ContextId, String> {
        match (&self.backend, query) {
            // Explicit context provided — resolve it
            (Backend::Remote(remote), Some(q)) => self.resolve_context(&remote.actor, q).await,
            (Backend::Local(_), Some(q)) => {
                ContextId::parse(q).map_err(|e| format!("Error: invalid context ID '{}': {}", q, e))
            }
            // No context provided — use current joined context
            (Backend::Remote(remote), None) => {
                let guard = remote.joined.read().await;
                match guard.as_ref() {
                    Some(joined) => Ok(joined.context_id),
                    None => {
                        Err("Error: no active context — call register_session first".to_string())
                    }
                }
            }
            (Backend::Local(_), None) => {
                Err("Error: context_id is required in local mode".to_string())
            }
        }
    }

    /// Shared polling loop for shell command completion.
    ///
    /// Both `shell()` and `context_shell()` dispatch a command via `shell_execute`
    /// then wait for the ToolResult child block to reach Done/Error status.
    /// Returns the completed ToolResult block snapshot (or a synthetic one
    /// describing timeout/event-stream errors). The caller serializes the
    /// JSON envelope so each tool can shape its own response.
    async fn execute_and_poll_shell(
        &self,
        remote: &RemoteState,
        ctx_id: ContextId,
        cmd_block_id: BlockId,
        command: &str,
        timeout_secs: u64,
        label: &str,
    ) -> ShellCompletion {
        let start = std::time::Instant::now();
        let mut event_rx = remote.actor.subscribe_events();
        let fallback_interval = tokio::time::Duration::from_millis(500);

        // Completion check — looks for a finished ToolResult child of our command block.
        // One shell command produces exactly one ToolResult child, so parent_id match is sufficient.
        let check_completion = |source: &str| -> Option<kaijutsu_crdt::BlockSnapshot> {
            let entry = remote.store.get(ctx_id)?;
            let blocks = entry.doc.blocks_ordered();
            let output = blocks.iter().find(|b| {
                b.parent_id.as_ref() == Some(&cmd_block_id)
                    && b.is_shell()
                    && b.kind == kaijutsu_crdt::BlockKind::ToolResult
                    && matches!(
                        b.status,
                        kaijutsu_crdt::Status::Done | kaijutsu_crdt::Status::Error
                    )
            })?;
            tracing::info!(
                command = %command,
                status = %output.status.as_str(),
                exit_code = ?output.exit_code,
                output_len = output.content.len(),
                elapsed_ms = start.elapsed().as_millis() as u64,
                "{label} completed (via {source})"
            );
            Some((*output).clone())
        };

        loop {
            if start.elapsed().as_secs() > timeout_secs {
                return ShellCompletion::Timeout {
                    cmd_block_id,
                    timeout_secs,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                };
            }

            // Wait for either an event or the fallback timeout
            let event = tokio::time::timeout(fallback_interval, event_rx.recv()).await;

            match event {
                Ok(Ok(ServerEvent::BlockStatusChanged {
                    status: kaijutsu_crdt::Status::Done | kaijutsu_crdt::Status::Error,
                    ..
                })) => {
                    if let Some(snap) = check_completion("event") {
                        return ShellCompletion::Done {
                            snapshot: snap,
                            elapsed_ms: start.elapsed().as_millis() as u64,
                        };
                    }
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    return ShellCompletion::StreamClosed {
                        cmd_block_id,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                    };
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                    tracing::warn!(skipped = n, "Event stream lagged, checking store");
                    if let Some(snap) = check_completion("lagged") {
                        return ShellCompletion::Done {
                            snapshot: snap,
                            elapsed_ms: start.elapsed().as_millis() as u64,
                        };
                    }
                }
                Err(_timeout) => {
                    // 500ms fallback — check store state
                    if let Some(snap) = check_completion("fallback") {
                        return ShellCompletion::Done {
                            snapshot: snap,
                            elapsed_ms: start.elapsed().as_millis() as u64,
                        };
                    }
                }
                _ => continue, // Other events — keep waiting
            }
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
    // Removed in MCP slim-down — see docs/kj-cleanup.md + docs/kj-cleanup-parity.md
    //
    // The following 16 tools previously duplicated kernel-side functionality
    // that now lives in `kj` (clap_derive). Agents drive them through
    // `context_shell "kj …"`:
    //
    //   doc_create | doc_list | doc_delete | doc_tree     → kj doc
    //   block_create | block_read | block_append | block_edit |
    //   block_list | block_status | block_exclude |
    //   block_inspect | block_history | block_diff        → kj block
    //   kernel_search                                      → kj search
    //   stage_commit                                       → kj stage commit
    //
    // The narrow MCP surface that remains: shell/context_shell as the rich
    // entry points, register_session/whoami/invoke_peer for peer-and-session
    // concerns, list_kernel_tools/kaish_exec as the escape hatches,
    // {read,write,edit,submit}_input for the shared scratchpad.
    // ========================================================================

    // ========================================================================
    // Kaish Execution (via ActorHandle → broker dispatch)
    // ========================================================================

    #[tool(
        description = "Execute a kernel tool by exact name. Use list_kernel_tools to discover available tool names and their input schemas. Common tools: glob, grep, kernel_search. Requires --connect.",
        annotations(open_world_hint = true)
    )]
    #[tracing::instrument(skip(self, req), name = "mcp.kaish_exec")]
    async fn kaish_exec(&self, Parameters(req): Parameters<KaishExecRequest>) -> String {
        let actor = match self.actor() {
            Some(a) => a,
            None => return "Error: kaish_exec requires --connect to kaijutsu-server".to_string(),
        };

        match actor.execute_tool(&req.tool, &req.params).await {
            Ok(result) => {
                if result.success {
                    result.output
                } else {
                    format!("Tool error: {}", result.output)
                }
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    #[tool(
        description = "List all kernel tools with their names, descriptions, categories, and input schemas. Use this to discover exact tool names for kaish_exec. Requires --connect.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    #[tracing::instrument(skip(self), name = "mcp.list_kernel_tools")]
    async fn list_kernel_tools(&self) -> String {
        let actor = match self.actor() {
            Some(a) => a,
            None => {
                return "Error: list_kernel_tools requires --connect to kaijutsu-server"
                    .to_string();
            }
        };

        match actor.get_tool_schemas().await {
            Ok(schemas) => {
                let tools: Vec<serde_json::Value> = schemas.iter().map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "description": s.description,
                        "category": s.category,
                        "input_schema": serde_json::from_str::<serde_json::Value>(&s.input_schema).unwrap_or(serde_json::Value::Object(Default::default())),
                    })
                }).collect();
                serde_json::to_string_pretty(&tools)
                    .unwrap_or_else(|e| format!("Error serializing: {e}"))
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    #[tool(
        description = "Execute a kaish command through the kernel. Returns a JSON object: {stdout, exit_code, status, block_id, content_type, ephemeral, data, elapsed_ms}. Detect failure via exit_code != 0 (or status == 'timeout'/'stream_closed') rather than text-matching. `data` is the kj structured payload when present (e.g. `kj context list` returns an array of context labels). Output also lands as CRDT blocks observable in kaijutsu-app. Requires --connect and register_session.",
        annotations(open_world_hint = true)
    )]
    #[tracing::instrument(skip(self, req), name = "mcp.shell")]
    async fn shell(&self, Parameters(req): Parameters<ShellRequest>) -> String {
        let (ctx_id, _store, actor) = match self.require_joined().await {
            Ok(v) => v,
            Err(e) => return e,
        };
        let remote = match self.remote() {
            Some(r) => r,
            None => return "Error: shell requires --connect to server".to_string(),
        };

        // Execute command — creates ToolCall + ToolResult blocks in the document.
        // The output block starts as Status::Running and transitions to Done/Error
        // when execution completes.
        let cmd_block_id = match actor.shell_execute(&req.command, ctx_id, false).await {
            Ok(id) => id,
            Err(e) => return format!("Error starting command: {e}"),
        };

        tracing::info!(
            command = %req.command,
            cmd_block = %cmd_block_id.to_key(),
            ctx = %ctx_id,
            "Shell command dispatched"
        );

        let timeout_secs = req.timeout_secs.unwrap_or(300).min(600);
        self.execute_and_poll_shell(
            remote,
            ctx_id,
            cmd_block_id,
            &req.command,
            timeout_secs,
            "Shell command",
        )
        .await
        .to_json()
    }

    #[tool(
        description = "Context-bound kaish shell. Executes commands in your current kernel context — '.' references it in kj commands. Full kaish: pipes, variables, scripting. Returns the same JSON envelope as `shell`: {stdout, exit_code, status, block_id, content_type, ephemeral, data, elapsed_ms}. Detect failure via exit_code != 0; `data` carries kj's structured payload (arrays for list commands, objects for inspect). Run `kj help` for context/drift/fork management. Examples: 'kj context list --tree', 'kj fork --name alt', 'kj drift push impl \"found the bug\"', 'ls /mnt/project | grep rs'. Requires --connect and register_session.",
        annotations(open_world_hint = true)
    )]
    #[tracing::instrument(skip(self, req), name = "mcp.context_shell")]
    async fn context_shell(&self, Parameters(req): Parameters<ContextShellRequest>) -> String {
        // Route through shell_execute — same path as the shell tool.
        // The command is passed verbatim to kaish (no auto-prepending).
        let (ctx_id, _store, actor) = match self.require_joined().await {
            Ok(v) => v,
            Err(e) => return e,
        };
        let remote = match self.remote() {
            Some(r) => r,
            None => return "Error: shell requires --connect to server".to_string(),
        };

        let cmd_block_id = match actor.shell_execute(&req.command, ctx_id, false).await {
            Ok(id) => id,
            Err(e) => return format!("Error starting command: {e}"),
        };

        tracing::info!(
            command = %req.command,
            cmd_block = %cmd_block_id.to_key(),
            ctx = %ctx_id,
            "Context shell command dispatched"
        );

        let timeout_secs = req.timeout_secs.unwrap_or(300).min(600);
        self.execute_and_poll_shell(
            remote,
            ctx_id,
            cmd_block_id,
            &req.command,
            timeout_secs,
            "Context shell command",
        )
        .await
        .to_json()
    }

    // ========================================================================
    // Session Registration
    // ========================================================================

    #[tool(
        description = "Register this agent session and create a context. Must be called before using context-dependent tools (shell, context_shell, read_input/write_input/submit_input). Returns the new context ID and session info.",
        annotations(
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    #[tracing::instrument(skip(self, req), name = "mcp.register_session")]
    async fn register_session(
        &self,
        Parameters(req): Parameters<RegisterSessionRequest>,
    ) -> String {
        let remote = match self.remote() {
            Some(r) => r,
            None => {
                return "Error: register_session requires --connect to kaijutsu-server".to_string();
            }
        };

        // Check if already joined
        {
            let guard = remote.joined.read().await;
            if let Some(joined) = guard.as_ref() {
                return serde_json::json!({
                    "already_registered": true,
                    "context_id": joined.context_id.to_hex(),
                    "context_short": joined.context_id.short(),
                })
                .to_string();
            }
        }

        // Generate label
        let label = req.label.unwrap_or_else(|| {
            let session = self.session_id.lock().ok().and_then(|g| g.clone());
            session.unwrap_or_else(|| format!("mcp-{}", &ContextId::new().short()))
        });

        // 1. Create context on the server
        let context_id = match remote.actor.create_context(&label).await {
            Ok(id) => id,
            Err(e) => return format!("Error creating context: {e}"),
        };

        // 2. Join it via the actor (updates actor's internal state for reconnects).
        // The actor's `instance` was set at spawn_actor time; the join_context
        // RPC now only takes the context id.
        if let Err(e) = remote.actor.join_context(context_id).await {
            return format!("Error joining context: {e}");
        }

        // 3. Sync initial state from server
        let sync_state = match remote.actor.get_context_sync(context_id).await {
            Ok(s) => s,
            Err(e) => return format!("Error syncing context: {e}"),
        };

        // 4. Populate the store
        if !sync_state.ops.is_empty() {
            if let Err(e) = remote.store.create_document_from_snapshot(
                sync_state.context_id,
                DocKind::Conversation,
                None,
                &sync_state.ops,
            ) {
                return format!("Error populating store: {e}");
            }
        } else if let Err(e) =
            remote
                .store
                .create_document(sync_state.context_id, DocKind::Conversation, None)
        {
            return format!("Error creating document: {e}");
        }

        // 5. Init SyncManager
        let frontier = remote
            .store
            .frontier(sync_state.context_id)
            .unwrap_or_default();
        let sync = SyncManager::with_state(Some(sync_state.context_id), Some(frontier));
        let sync_arc = Arc::new(Mutex::new(sync));

        // 6. Spawn background event listener
        let bg_abort = {
            let mut event_rx = remote.actor.subscribe_events();
            let store_bg = Arc::clone(&remote.store);
            let sync_bg = Arc::clone(&sync_arc);
            let ctx_id_bg = context_id;

            let bg_handle = tokio::spawn(async move {
                loop {
                    match event_rx.recv().await {
                        Ok(event) => {
                            apply_server_event(&store_bg, &sync_bg, ctx_id_bg, event);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("Missed {n} events, forcing full resync");
                            sync_bg.lock().expect("sync mutex poisoned").reset();
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
            bg_handle.abort_handle()
        };

        // 7. Write JoinedContext
        {
            let mut guard = remote.joined.write().await;
            *guard = Some(JoinedContext {
                context_id,
                sync: sync_arc,
                _bg_task: Arc::new(AbortOnDrop(bg_abort)),
            });
        }

        // 8. Update shared context_id for hook listener
        if let Ok(mut ctx) = remote.shared_context_id.lock() {
            *ctx = Some(context_id);
        }

        tracing::info!(
            context_id = %context_id,
            label = %label,
            "Session registered with new context"
        );

        serde_json::json!({
            "success": true,
            "context_id": context_id.to_hex(),
            "context_short": context_id.short(),
            "label": label,
        })
        .to_string()
    }

    // ========================================================================
    // Context Identity
    // ========================================================================

    #[tool(
        description = "Get this MCP server's identity: context short ID, context name, authenticated user, agent session info. Useful for understanding your position in the drift network.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    #[tracing::instrument(skip(self), name = "mcp.whoami")]
    async fn whoami(&self) -> String {
        let session_id = self.session_id.lock().ok().and_then(|g| g.clone());

        let actor = match self.actor() {
            Some(a) => a,
            None => {
                // Local mode — return what we have
                return serde_json::json!({
                    "mode": "local",
                    "context_name": self.context_name,
                    "session_id": session_id,
                    "agent_name": self.agent_name,
                })
                .to_string();
            }
        };

        let identity = match actor.whoami().await {
            Ok(id) => id,
            Err(e) => return format!("Error getting identity: {e}"),
        };

        let (context_id, ctx_label) = match actor.get_context_id().await {
            Ok(pair) => pair,
            Err(e) => return format!("Error getting context: {e}"),
        };

        serde_json::json!({
            "username": identity.username,
            "display_name": identity.display_name,
            "context_id": context_id.short(),
            "context_label": ctx_label,
            "context_name": self.context_name,
            "session_id": session_id,
            "agent_name": self.agent_name,
        })
        .to_string()
    }

    // ========================================================================
    // Peer Invocation (drift navigation)
    // ========================================================================

    #[tool(
        description = "Invoke a peer through the kernel. Peers are named RPC participants attached to the kernel (e.g., kaijutsu-app registers as a peer and exposes actions like 'switch_context' and 'active_context' for drift navigation). Params and result are JSON. Requires --connect.",
        annotations(
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    #[tracing::instrument(skip(self, req), name = "mcp.invoke_peer")]
    async fn invoke_peer(&self, Parameters(req): Parameters<InvokePeerRequest>) -> String {
        let actor = match self.actor() {
            Some(a) => a,
            None => return "Error: invoke_peer requires --connect".to_string(),
        };

        let params = match serde_json::to_vec(&req.params) {
            Ok(v) => v,
            Err(e) => return format!("Error: failed to serialize params: {e}"),
        };
        match actor.invoke_peer(&req.nick, &req.action, &params).await {
            Ok(result) => String::from_utf8_lossy(&result).to_string(),
            Err(e) => format!("Error: {e}"),
        }
    }

    // ========================================================================
    // Input Document Tools (CRDT compose scratchpad)
    // ========================================================================

    #[tool(
        description = "Read the current input document text for a context. The input document is a CRDT-backed scratchpad shared across all participants (compose box, agents, MCP tools). Omit context_id to use the current context.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    #[tracing::instrument(skip(self, req), name = "mcp.read_input")]
    async fn read_input(&self, Parameters(req): Parameters<InputReadRequest>) -> String {
        let ctx_id = match self.resolve_input_context(req.context_id.as_deref()).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        match &self.backend {
            Backend::Local(store) => {
                // Ensure input doc exists
                let _ = store.create_input_doc(ctx_id);
                match store.get_input_text(ctx_id) {
                    Ok(text) => serde_json::json!({
                        "context_id": ctx_id.short(),
                        "content": text,
                        "length": text.len(),
                    })
                    .to_string(),
                    Err(e) => format!("Error: {}", e),
                }
            }
            Backend::Remote(remote) => match remote.actor.get_input_state(ctx_id).await {
                Ok(state) => serde_json::json!({
                    "context_id": ctx_id.short(),
                    "content": state.content,
                    "length": state.content.len(),
                    "version": state.version,
                })
                .to_string(),
                Err(e) => format!("Error: {}", e),
            },
        }
    }

    #[tool(
        description = "Replace all text in the input document. Clears existing content and writes the new text. The input document is shared — changes are visible to all participants immediately. Omit context_id to use the current context.",
        annotations(destructive_hint = false, open_world_hint = false)
    )]
    #[tracing::instrument(skip(self, req), name = "mcp.write_input")]
    async fn write_input(&self, Parameters(req): Parameters<InputWriteRequest>) -> String {
        let ctx_id = match self.resolve_input_context(req.context_id.as_deref()).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        match &self.backend {
            Backend::Local(store) => {
                // Ensure input doc exists
                let _ = store.create_input_doc(ctx_id);
                // Clear then write
                let _ = store.clear_input(ctx_id);
                if !req.text.is_empty()
                    && let Err(e) = store.edit_input(ctx_id, 0, &req.text, 0)
                {
                    return format!("Error: {}", e);
                }
                serde_json::json!({
                    "success": true,
                    "context_id": ctx_id.short(),
                    "length": req.text.len(),
                })
                .to_string()
            }
            Backend::Remote(remote) => {
                // Get current state to know how much to delete
                let current_len = match remote.actor.get_input_state(ctx_id).await {
                    Ok(state) => state.content.len() as u64,
                    Err(e) => return format!("Error getting current state: {}", e),
                };
                // Delete all, then insert new text in one operation
                match remote
                    .actor
                    .edit_input(ctx_id, 0, &req.text, current_len)
                    .await
                {
                    Ok(version) => serde_json::json!({
                        "success": true,
                        "context_id": ctx_id.short(),
                        "length": req.text.len(),
                        "version": version,
                    })
                    .to_string(),
                    Err(e) => format!("Error: {}", e),
                }
            }
        }
    }

    #[tool(
        description = "Surgical edit on the input document: insert and/or delete characters at a specific position. More efficient than write_input for small edits to large text. Omit context_id to use the current context.",
        annotations(destructive_hint = false, open_world_hint = false)
    )]
    #[tracing::instrument(skip(self, req), name = "mcp.edit_input")]
    async fn edit_input(&self, Parameters(req): Parameters<InputEditRequest>) -> String {
        let ctx_id = match self.resolve_input_context(req.context_id.as_deref()).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        match &self.backend {
            Backend::Local(store) => {
                // Ensure input doc exists
                let _ = store.create_input_doc(ctx_id);
                match store.edit_input(ctx_id, req.pos as usize, &req.insert, req.delete as usize) {
                    Ok(_ops) => {
                        let text = store.get_input_text(ctx_id).unwrap_or_default();
                        serde_json::json!({
                            "success": true,
                            "context_id": ctx_id.short(),
                            "length": text.len(),
                        })
                        .to_string()
                    }
                    Err(e) => format!("Error: {}", e),
                }
            }
            Backend::Remote(remote) => {
                match remote
                    .actor
                    .edit_input(ctx_id, req.pos, &req.insert, req.delete)
                    .await
                {
                    Ok(version) => serde_json::json!({
                        "success": true,
                        "context_id": ctx_id.short(),
                        "version": version,
                    })
                    .to_string(),
                    Err(e) => format!("Error: {}", e),
                }
            }
        }
    }

    #[tool(
        description = "Submit the input document: snapshot its content into a conversation block and clear it. This is equivalent to pressing Enter in the compose box. Returns the created block ID and whether it was detected as a shell command. Omit context_id to use the current context.",
        annotations(destructive_hint = true, open_world_hint = false)
    )]
    #[tracing::instrument(skip(self, req), name = "mcp.submit_input")]
    async fn submit_input(&self, Parameters(req): Parameters<InputSubmitRequest>) -> String {
        let ctx_id = match self.resolve_input_context(req.context_id.as_deref()).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        match &self.backend {
            Backend::Local(_store) => {
                // Local mode doesn't have submit semantics (no conversation block creation)
                "Error: submit_input requires --connect to kaijutsu-server".to_string()
            }
            Backend::Remote(remote) => {
                let is_shell = req.mode.as_deref() == Some("shell");
                match remote.actor.submit_input(ctx_id, is_shell).await {
                    Ok(result) => serde_json::json!({
                        "success": true,
                        "context_id": ctx_id.short(),
                        "block_id": result.block_id.to_key(),
                    })
                    .to_string(),
                    Err(e) => format!("Error: {}", e),
                }
            }
        }
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
        let context_id = ContextId::parse(&args.document_id).map_err(|e| {
            McpError::invalid_params(
                format!("Invalid document ID '{}': {}", args.document_id, e),
                None,
            )
        })?;

        let entry = self.store().get(context_id).ok_or_else(|| {
            McpError::invalid_params(format!("Document '{}' not found", args.document_id), None)
        })?;

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
            let dag = ConversationDAG::from_store(&entry.doc);
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
            let mut authors: std::collections::HashSet<String> = std::collections::HashSet::new();
            for block in &blocks {
                authors.insert(block.author().to_hex());
            }
            let mut authors_sorted: Vec<_> = authors.into_iter().collect();
            authors_sorted.sort();
            content.push_str(&format!("**Authors:** {}\n", authors_sorted.join(", ")));

            // Count by role
            let mut role_counts: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for block in &blocks {
                *role_counts.entry(block.role.as_str()).or_insert(0) += 1;
            }
            content.push_str("**Blocks by role:**\n");
            for (role, count) in role_counts {
                content.push_str(&format!("  - {}: {}\n", role, count));
            }
        }

        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            content,
        )])
        .with_description(format!("Analysis of document '{}'", args.document_id)))
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
        let regex = Regex::new(&args.query).map_err(|e| {
            McpError::invalid_params(format!("Invalid regex '{}': {}", args.query, e), None)
        })?;

        let context_ids: Vec<ContextId> = if let Some(ref doc_id) = args.document_id {
            let id = ContextId::parse(doc_id).map_err(|e| {
                McpError::invalid_params(format!("Invalid document ID '{}': {}", doc_id, e), None)
            })?;
            if self.store().contains(id) {
                vec![id]
            } else {
                return Err(McpError::invalid_params(
                    format!("Document '{}' not found", doc_id),
                    None,
                ));
            }
        } else {
            self.store().list_ids()
        };

        let mut content = String::new();
        content.push_str(&format!("# Search Results for: `{}`\n\n", args.query));

        let mut total_matches = 0;
        let context_lines = 3;

        for context_id in context_ids {
            let snapshots = match self.store().block_snapshots(context_id) {
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
                            context_id.to_hex(),
                            snapshot.id.to_key(),
                            snapshot.role.as_str(),
                            snapshot.kind.as_str()
                        ));
                        content.push_str("```\n");

                        // Context before
                        let start = line_idx.saturating_sub(context_lines);
                        for (i, ctx_line) in lines[start..line_idx].iter().enumerate() {
                            content.push_str(&format!("{:4} │ {}\n", start + i + 1, ctx_line));
                        }

                        // Matching line (highlighted)
                        content.push_str(&format!("{:4} │ >>> {} <<<\n", line_idx + 1, line));

                        // Context after
                        let end = (line_idx + 1 + context_lines).min(lines.len());
                        for (i, ctx_line) in lines[(line_idx + 1)..end].iter().enumerate() {
                            content.push_str(&format!("{:4} │ {}\n", line_idx + 2 + i, ctx_line));
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

        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            content,
        )])
        .with_description(format!("Search results for '{}'", args.query)))
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
        let (context_id, block_id) = find_block(self.store(), &args.block_id).ok_or_else(|| {
            McpError::invalid_params(format!("Block '{}' not found", args.block_id), None)
        })?;

        let entry = self
            .store()
            .get(context_id)
            .ok_or_else(|| McpError::invalid_params("Document not found", None))?;

        let snapshot = entry
            .doc
            .get_block_snapshot(&block_id)
            .ok_or_else(|| McpError::invalid_params("Block not found", None))?;

        let edit_type = args.edit_type.as_deref().unwrap_or("refine");

        let mut content = String::new();

        content.push_str(&format!("# Editing Assistant: {}\n\n", args.block_id));
        content.push_str(&format!("**Document:** {}\n", context_id.to_hex()));
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
        if let Some(parent_id) = snapshot.parent_id
            && let Some(parent_snap) = entry.doc.get_block_snapshot(&parent_id)
        {
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

        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            content,
        )])
        .with_description(format!("Editing assistant for block '{}'", args.block_id)))
    }
}

// ============================================================================
// Background Event Listener
// ============================================================================

/// Apply a server event to the store's BlockDocument via SyncManager.
///
/// Called by the background event listener spawned in `connect()`.
/// Only handles CRDT-relevant events (BlockInserted, BlockTextOps, BlockStatusChanged).
/// Uses the store as the single source of truth — all MCP tools read from the store.
fn apply_server_event(
    store: &SharedBlockStore,
    sync: &Arc<Mutex<SyncManager>>,
    context_id: ContextId,
    event: ServerEvent,
) {
    match event {
        ServerEvent::BlockInserted {
            context_id: event_ctx_id,
            block,
            ops,
            ..
        } => {
            if event_ctx_id != context_id {
                return;
            }
            let mut s = sync.lock().expect("sync mutex poisoned");
            if let Some(mut entry) = store.get_mut(context_id) {
                match s.apply_block_inserted(&mut entry.doc, event_ctx_id, &block, &ops) {
                    Ok(_) => {
                        entry.touch(PrincipalId::system());
                        tracing::trace!(block = ?block.id, "Applied BlockInserted");
                    }
                    Err(e) => tracing::warn!(block = ?block.id, "BlockInserted sync error: {e}"),
                }
            }
        }
        ServerEvent::BlockTextOps {
            context_id: event_ctx_id,
            ops,
            ..
        } => {
            if event_ctx_id != context_id {
                return;
            }
            let mut s = sync.lock().expect("sync mutex poisoned");
            if let Some(mut entry) = store.get_mut(context_id) {
                match s.apply_text_ops(&mut entry.doc, event_ctx_id, &ops) {
                    Ok(_) => {
                        entry.touch(PrincipalId::system());
                        tracing::trace!("Applied BlockTextOps");
                    }
                    Err(e) => tracing::warn!("BlockTextOps sync error: {e}"),
                }
            }
        }
        ServerEvent::BlockStatusChanged {
            context_id: event_ctx_id,
            block_id,
            status,
            ref output,
        } => {
            if event_ctx_id != context_id {
                return;
            }
            // Apply piggybacked output data (not DTE-tracked)
            if let Some(output_data) = output
                && let Err(e) = store.set_output(context_id, &block_id, Some(output_data))
            {
                tracing::warn!("BlockStatusChanged set_output error: {e}");
            }
            // Use store's set_status — handles version bump and flow events
            if let Err(e) = store.set_status(context_id, &block_id, status) {
                tracing::warn!("BlockStatusChanged error: {e}");
            }
        }
        // Other event variants don't affect CRDT state
        _ => {}
    }
}

#[tool_handler]
#[prompt_handler]
impl ServerHandler for KaijutsuMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_prompts_list_changed()
                .enable_resources()
                .enable_resources_subscribe()
                .enable_logging()
                .enable_completions()
                .build(),
        ).with_instructions("Kaijutsu CRDT kernel MCP server. Provides tools for collaborative document and block editing with CRDT-backed consistency.")
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
            resources.push(
                RawResource {
                    uri: "kaijutsu://docs".to_string(),
                    name: "documents".to_string(),
                    title: Some("All Documents".to_string()),
                    description: Some("List of all documents in the kernel".to_string()),
                    mime_type: Some("application/json".to_string()),
                    size: None,
                    icons: None,
                    meta: None,
                }
                .no_annotation(),
            );

            // Add each document as a resource
            for doc_id in self.store().list_ids() {
                if let Some(entry) = self.store().get(doc_id) {
                    let doc_hex = doc_id.to_hex();
                    resources.push(
                        RawResource {
                            uri: format!("kaijutsu://docs/{}", doc_hex),
                            name: doc_hex.clone(),
                            title: Some(format!("Document: {}", doc_hex)),
                            description: Some(format!(
                                "{} document with {} blocks",
                                entry.kind.as_str(),
                                entry.doc.blocks_ordered().len()
                            )),
                            mime_type: Some("application/json".to_string()),
                            size: None,
                            icons: None,
                            meta: None,
                        }
                        .no_annotation(),
                    );

                    // Add each block as a resource
                    for snapshot in entry.doc.blocks_ordered() {
                        let block_key = snapshot.id.to_key();
                        resources.push(
                            RawResource {
                                uri: format!("kaijutsu://blocks/{}/{}", doc_hex, block_key),
                                name: block_key.clone(),
                                title: Some(format!(
                                    "[{}/{}]",
                                    snapshot.role.as_str(),
                                    snapshot.kind.as_str()
                                )),
                                description: Some(format!(
                                    "{} block, {} bytes",
                                    snapshot.kind.as_str(),
                                    snapshot.content.len()
                                )),
                                mime_type: Some("text/plain".to_string()),
                                size: Some(snapshot.content.len() as u32),
                                icons: None,
                                meta: None,
                            }
                            .no_annotation(),
                        );
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
                let docs: Vec<serde_json::Value> = self
                    .store()
                    .list_ids()
                    .iter()
                    .map(|id| {
                        let (kind, block_count) = self
                            .store()
                            .get(*id)
                            .map(|e| (e.kind.as_str().to_string(), e.doc.blocks_ordered().len()))
                            .unwrap_or(("unknown".to_string(), 0));
                        serde_json::json!({
                            "id": id.to_hex(),
                            "kind": kind,
                            "block_count": block_count
                        })
                    })
                    .collect();

                let content =
                    serde_json::to_string_pretty(&docs).unwrap_or_else(|_| "[]".to_string());

                return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                    content,
                    uri.clone(),
                )]));
            }

            if let Some(doc_id_str) = uri.strip_prefix("kaijutsu://docs/") {
                let doc_ctx_id = ContextId::parse(doc_id_str).map_err(|e| {
                    McpError::invalid_params(
                        format!("Invalid document ID '{}': {}", doc_id_str, e),
                        None,
                    )
                })?;
                // Return document metadata and block list
                let entry = self.store().get(doc_ctx_id).ok_or_else(|| {
                    McpError::invalid_params(format!("Document '{}' not found", doc_id_str), None)
                })?;

                let blocks: Vec<serde_json::Value> = entry
                    .doc
                    .blocks_ordered()
                    .iter()
                    .map(|s| {
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
                    })
                    .collect();

                let result = serde_json::json!({
                    "id": doc_id_str,
                    "kind": entry.kind.as_str(),
                    "language": entry.language,
                    "version": entry.version(),
                    "blocks": blocks
                });

                let content =
                    serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".to_string());

                return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                    content,
                    uri.clone(),
                )]));
            }

            if let Some(rest) = uri.strip_prefix("kaijutsu://blocks/") {
                // Parse doc_id/block_key
                let parts: Vec<&str> = rest.splitn(2, '/').collect();
                if parts.len() != 2 {
                    return Err(McpError::invalid_params(
                        format!("Invalid block URI format: {}", uri),
                        None,
                    ));
                }

                let doc_id_str = parts[0];
                let block_key = parts[1];

                let (found_ctx_id, block_id) =
                    find_block(self.store(), block_key).ok_or_else(|| {
                        McpError::invalid_params(
                            format!(
                                "Block '{}' not found in document '{}'",
                                block_key, doc_id_str
                            ),
                            None,
                        )
                    })?;

                let entry = self.store().get(found_ctx_id).ok_or_else(|| {
                    McpError::invalid_params(format!("Document '{}' not found", doc_id_str), None)
                })?;

                let snapshot = entry
                    .doc
                    .get_block_snapshot(&block_id)
                    .ok_or_else(|| McpError::invalid_params("Block not found", None))?;

                return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                    snapshot.content.clone(),
                    uri.clone(),
                )]));
            }

            Err(McpError::invalid_params(
                format!("Unknown resource URI: {}", uri),
                None,
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
            let mut subs = self
                .server_state
                .subscriptions
                .lock()
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
            let mut subs = self
                .server_state
                .subscriptions
                .lock()
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
                            if request.argument.name == "document_id"
                                || request.argument.name == "block_id"
                            {
                                // Complete document IDs
                                self.store()
                                    .list_ids()
                                    .into_iter()
                                    .map(|id| id.to_hex())
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
                                self.store()
                                    .list_ids()
                                    .into_iter()
                                    .map(|id| id.to_hex())
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
                        self.store()
                            .list_ids()
                            .into_iter()
                            .map(|id| format!("kaijutsu://docs/{}", id.to_hex()))
                            .filter(|uri| uri.contains(&request.argument.value))
                            .take(10)
                            .collect()
                    } else {
                        Vec::new()
                    }
                }
            };

            Ok(CompleteResult::new(
                CompletionInfo::new(values).map_err(|e| McpError::invalid_params(e, None))?,
            ))
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
            let mut level = self
                .server_state
                .log_level
                .lock()
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


    // =========================================================================
    // apply_server_event tests — exercises the store-based sync path
    //
    // Uses kernel BlockStore as both "server" and "client" to generate proper
    // SyncPayload (postcard-serialized) ops, matching the real sync protocol.
    // =========================================================================

    use kaijutsu_crdt::{BlockId, BlockKind, BlockSnapshot, ContentType, ContextId, PrincipalId, Role, Status};
    use std::collections::HashMap;

    /// Helper: create a synced client/server pair with one block ("Hello from server").
    ///
    /// The server store inserts the block, then the client store syncs from it
    /// via `ops_since` + `merge_ops`, so both share CRDT causal history.
    /// Returns (client_store, sync_manager, server_store, context_id).
    fn setup_synced_store() -> (
        SharedBlockStore,
        Arc<Mutex<SyncManager>>,
        SharedBlockStore,
        ContextId,
    ) {
        let context_id = ContextId::new();

        // Server store — the authoritative source
        let server = shared_block_store(PrincipalId::new());
        server
            .create_document(context_id, DocKind::Conversation, None)
            .expect("create server document");
        server
            .insert_block(
                context_id,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Hello from server",
                Status::Done,
                ContentType::Plain,
            )
            .expect("insert block on server");

        // Client store — synced from server via SyncPayload
        let client = shared_block_store(PrincipalId::system());
        client
            .create_document(context_id, DocKind::Conversation, None)
            .expect("create client document");
        let initial_payload = server
            .ops_since(context_id, &HashMap::new())
            .expect("ops_since from empty frontier");
        client
            .merge_ops(context_id, initial_payload)
            .expect("initial sync merge");

        let frontier = client.frontier(context_id).unwrap_or_default();
        let sync = Arc::new(Mutex::new(SyncManager::with_state(
            Some(context_id),
            Some(frontier),
        )));

        (client, sync, server, context_id)
    }

    /// Helper: get SyncPayload from server as postcard bytes.
    fn server_ops_bytes(
        server: &SharedBlockStore,
        ctx_id: ContextId,
        frontier: &HashMap<BlockId, kaijutsu_crdt::Frontier>,
    ) -> Vec<u8> {
        let payload = server.ops_since(ctx_id, frontier).expect("ops_since");
        postcard::to_allocvec(&payload).expect("serialize SyncPayload")
    }

    #[test]
    fn test_apply_block_inserted_updates_store() {
        let (store, sync, server, ctx_id) = setup_synced_store();

        // Server inserts a new block
        let pre_frontier = server.frontier(ctx_id).unwrap();
        let block_id = server
            .insert_block(
                ctx_id,
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "New block from server",
                Status::Done,
                ContentType::Plain,
            )
            .expect("insert");
        let ops_bytes = server_ops_bytes(&server, ctx_id, &pre_frontier);
        let block = server
            .get(ctx_id)
            .unwrap()
            .doc
            .get_block_snapshot(&block_id)
            .unwrap();

        // Before applying: store should have 1 block
        assert_eq!(
            store.get(ctx_id).unwrap().doc.block_count(),
            1,
            "Store should have 1 block before event"
        );

        // Apply the event through our function
        apply_server_event(
            &store,
            &sync,
            ctx_id,
            ServerEvent::BlockInserted {
                context_id: ctx_id,
                block: Box::new(block),
                ops: ops_bytes,
            },
        );

        // After: store should have 2 blocks
        let entry = store.get(ctx_id).expect("doc exists");
        assert_eq!(
            entry.doc.block_count(),
            2,
            "Store should have 2 blocks after BlockInserted"
        );
        assert!(
            entry.doc.full_text().contains("New block from server"),
            "Store should contain the new block's content"
        );
    }

    #[test]
    fn test_apply_text_ops_updates_store() {
        let (store, sync, server, ctx_id) = setup_synced_store();

        // Get the block ID of the existing block on the server
        let block_id = server.block_snapshots(ctx_id).unwrap()[0].id;

        // Server edits the block's text
        let pre_frontier = server.frontier(ctx_id).unwrap();
        server
            .edit_text(ctx_id, &block_id, 17, " — updated!", 0)
            .expect("edit");
        let ops_bytes = server_ops_bytes(&server, ctx_id, &pre_frontier);

        // Before: store has original text
        assert!(
            store
                .get(ctx_id)
                .unwrap()
                .doc
                .full_text()
                .contains("Hello from server"),
            "Store should have original text"
        );

        apply_server_event(
            &store,
            &sync,
            ctx_id,
            ServerEvent::BlockTextOps {
                context_id: ctx_id,
                block_id,
                ops: ops_bytes,
                seq_num: 0,
            },
        );

        // After: store should have updated text
        let entry = store.get(ctx_id).expect("doc exists");
        assert!(
            entry.doc.full_text().contains("— updated!"),
            "Store should contain the edited text, got: {}",
            entry.doc.full_text()
        );
    }

    #[test]
    fn test_apply_status_changed_updates_store() {
        let (store, sync, server, ctx_id) = setup_synced_store();

        let block_id = server.block_snapshots(ctx_id).unwrap()[0].id;

        // The block starts as Done (from BlockSnapshot::text constructor)
        assert_eq!(
            store
                .get(ctx_id)
                .unwrap()
                .doc
                .get_block_snapshot(&block_id)
                .unwrap()
                .status,
            Status::Done,
        );

        // Apply status change to Error
        apply_server_event(
            &store,
            &sync,
            ctx_id,
            ServerEvent::BlockStatusChanged {
                context_id: ctx_id,
                block_id,
                status: Status::Error,
                output: None,
            },
        );

        // Store should reflect the new status
        let entry = store.get(ctx_id).expect("doc exists");
        let snap = entry
            .doc
            .get_block_snapshot(&block_id)
            .expect("block exists");
        assert_eq!(
            snap.status,
            Status::Error,
            "Status should be Error after event"
        );
    }

    #[test]
    fn test_apply_event_wrong_document_ignored() {
        let (store, sync, server, ctx_id) = setup_synced_store();

        // Server inserts a new block
        let pre_frontier = server.frontier(ctx_id).unwrap();
        let block_id = server
            .insert_block(
                ctx_id,
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "Should not appear",
                Status::Done,
                ContentType::Plain,
            )
            .expect("insert");
        let ops_bytes = server_ops_bytes(&server, ctx_id, &pre_frontier);
        let block = server
            .get(ctx_id)
            .unwrap()
            .doc
            .get_block_snapshot(&block_id)
            .unwrap();

        // Apply with WRONG context_id — should be silently ignored
        let wrong_ctx = ContextId::new();
        apply_server_event(
            &store,
            &sync,
            ctx_id,
            ServerEvent::BlockInserted {
                context_id: wrong_ctx,
                block: Box::new(block),
                ops: ops_bytes,
            },
        );

        // Store should still have only 1 block
        assert_eq!(
            store.get(ctx_id).unwrap().doc.block_count(),
            1,
            "Store should not be affected by events for other documents"
        );
    }

    #[test]
    fn test_apply_event_bumps_store_version() {
        let (store, sync, server, ctx_id) = setup_synced_store();

        let version_before = store.get(ctx_id).unwrap().version();

        // Apply a block insert
        let pre_frontier = server.frontier(ctx_id).unwrap();
        let block_id = server
            .insert_block(
                ctx_id,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "Version bump test",
                Status::Done,
                ContentType::Plain,
            )
            .expect("insert");
        let ops_bytes = server_ops_bytes(&server, ctx_id, &pre_frontier);
        let block = server
            .get(ctx_id)
            .unwrap()
            .doc
            .get_block_snapshot(&block_id)
            .unwrap();

        apply_server_event(
            &store,
            &sync,
            ctx_id,
            ServerEvent::BlockInserted {
                context_id: ctx_id,
                block: Box::new(block),
                ops: ops_bytes,
            },
        );

        let version_after = store.get(ctx_id).unwrap().version();
        assert!(
            version_after > version_before,
            "Store version should increase after event: before={version_before}, after={version_after}"
        );
    }

    #[test]
    fn test_store_reads_consistent_after_multiple_events() {
        let (store, sync, server, ctx_id) = setup_synced_store();

        // Simulate a burst of events: 3 block inserts + a text edit + a status change
        let mut inserted_ids = Vec::new();
        for i in 0..3 {
            let pre = server.frontier(ctx_id).unwrap();
            let bid = server
                .insert_block(
                    ctx_id,
                    None,
                    None,
                    Role::Model,
                    BlockKind::Text,
                    &format!("Block {i}"),
                    Status::Done,
                    ContentType::Plain,
                )
                .expect("insert");
            let ops = server_ops_bytes(&server, ctx_id, &pre);
            let block = server
                .get(ctx_id)
                .unwrap()
                .doc
                .get_block_snapshot(&bid)
                .unwrap();
            inserted_ids.push(bid);

            apply_server_event(
                &store,
                &sync,
                ctx_id,
                ServerEvent::BlockInserted {
                    context_id: ctx_id,
                    block: Box::new(block),
                    ops,
                },
            );
        }

        // Now edit the last-inserted block's text ("Block 2" → "Block 2 — edited")
        let last_block_id = inserted_ids[2];
        let last_content_len = server
            .get(ctx_id)
            .unwrap()
            .doc
            .get_block_snapshot(&last_block_id)
            .unwrap()
            .content
            .len();
        let pre = server.frontier(ctx_id).unwrap();
        server
            .edit_text(ctx_id, &last_block_id, last_content_len, " — edited", 0)
            .expect("edit");
        let ops = server_ops_bytes(&server, ctx_id, &pre);

        apply_server_event(
            &store,
            &sync,
            ctx_id,
            ServerEvent::BlockTextOps {
                context_id: ctx_id,
                block_id: last_block_id,
                ops,
                seq_num: 0,
            },
        );

        // Status change on the first inserted block
        let first_new_block = inserted_ids[0];
        apply_server_event(
            &store,
            &sync,
            ctx_id,
            ServerEvent::BlockStatusChanged {
                context_id: ctx_id,
                block_id: first_new_block,
                status: Status::Running,
                output: None,
            },
        );

        // Verify everything is visible through store.get() — the path MCP tools use
        let entry = store.get(ctx_id).expect("doc exists");
        let text = entry.doc.full_text();
        let block_count = entry.doc.block_count();

        assert_eq!(block_count, 4, "1 original + 3 inserted");
        assert!(
            text.contains("Hello from server"),
            "Original content preserved, got: {text}"
        );
        assert!(
            text.contains("Block 0"),
            "First inserted block, got: {text}"
        );
        assert!(
            text.contains("Block 1"),
            "Second inserted block, got: {text}"
        );
        assert!(text.contains("— edited"), "Text edit applied, got: {text}");

        let first_snap = entry
            .doc
            .get_block_snapshot(&first_new_block)
            .expect("block exists");
        assert_eq!(first_snap.status, Status::Running, "Status change applied");
    }

    #[test]
    fn test_corrupted_ops_dont_corrupt_store() {
        let (store, sync, server, ctx_id) = setup_synced_store();

        let original_block_count = store.get(ctx_id).unwrap().doc.block_count();
        let original_version = store.get(ctx_id).unwrap().version();
        let block_id = server.block_snapshots(ctx_id).unwrap()[0].id;

        // Apply BlockInserted with garbage ops
        let evil_agent = PrincipalId::new();
        apply_server_event(
            &store,
            &sync,
            ctx_id,
            ServerEvent::BlockInserted {
                context_id: ctx_id,
                // This block already exists (idempotent skip), but let's also test garbage ops
                // with a "new" block that doesn't exist yet
                block: Box::new(BlockSnapshot::text(
                    BlockId::new(ctx_id, evil_agent, 99),
                    None,
                    Role::User,
                    "corrupted block",
                )),
                ops: vec![0xFF, 0xDE, 0xAD, 0xBE, 0xEF],
            },
        );

        // Store should be unchanged — no corruption
        {
            let entry = store.get(ctx_id).unwrap();
            assert_eq!(
                entry.doc.block_count(),
                original_block_count,
                "Block count unchanged after corrupt ops"
            );
            assert_eq!(
                entry.version(),
                original_version,
                "Version unchanged — touch() not called on error"
            );
        } // Drop DashMap Ref before next event needs get_mut()

        // Apply BlockTextOps with garbage ops
        apply_server_event(
            &store,
            &sync,
            ctx_id,
            ServerEvent::BlockTextOps {
                context_id: ctx_id,
                block_id,
                ops: vec![0xBA, 0xAD, 0xF0, 0x0D],
                seq_num: 0,
            },
        );

        // Store content should be unchanged
        let entry = store.get(ctx_id).unwrap();
        assert!(
            entry.doc.full_text().contains("Hello from server"),
            "Original content preserved after corrupt text ops"
        );
    }

    #[test]
    fn test_status_change_for_nonexistent_block() {
        let (store, sync, _server, ctx_id) = setup_synced_store();

        // Send status change for a block that doesn't exist
        let ghost_agent = PrincipalId::new();
        apply_server_event(
            &store,
            &sync,
            ctx_id,
            ServerEvent::BlockStatusChanged {
                context_id: ctx_id,
                block_id: BlockId::new(ctx_id, ghost_agent, 999),
                status: Status::Done,
                output: None,
            },
        );

        // Store should not crash, version might or might not change depending on
        // whether set_status errors before or after touch. The important thing
        // is no panic and no corruption.
        let entry = store.get(ctx_id).unwrap();
        assert_eq!(entry.doc.block_count(), 1, "Block count unchanged");
        assert!(
            entry.doc.full_text().contains("Hello from server"),
            "Content unchanged"
        );
    }

    #[test]
    fn test_reset_then_incremental_event_preserves_blocks() {
        let (store, sync, server, ctx_id) = setup_synced_store();

        // Verify initial state
        assert_eq!(store.get(ctx_id).unwrap().doc.block_count(), 1);

        // Simulate Lagged — reset SyncManager (sets needs_full_sync = true)
        sync.lock().unwrap().reset();

        // Server adds a new block. Ops are incremental (not full oplog) —
        // this is what would arrive after missing some events.
        let pre_frontier = server.frontier(ctx_id).unwrap();
        let block_id = server
            .insert_block(
                ctx_id,
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "Post-reset block",
                Status::Done,
                ContentType::Plain,
            )
            .expect("insert");
        let ops_bytes = server_ops_bytes(&server, ctx_id, &pre_frontier);
        let block = server
            .get(ctx_id)
            .unwrap()
            .doc
            .get_block_snapshot(&block_id)
            .unwrap();

        apply_server_event(
            &store,
            &sync,
            ctx_id,
            ServerEvent::BlockInserted {
                context_id: ctx_id,
                block: Box::new(block),
                ops: ops_bytes,
            },
        );

        // Critical check: do we still have BOTH blocks?
        // SyncManager should try incremental merge first (doc is not empty).
        // If incremental succeeds, both blocks are preserved.
        // If it falls through to do_full_sync with incremental ops, we lose block 1.
        let entry = store.get(ctx_id).unwrap();
        assert_eq!(
            entry.doc.block_count(),
            2,
            "Both original and new block should be preserved after reset + incremental event"
        );
        assert!(
            entry.doc.full_text().contains("Hello from server"),
            "Original block content preserved after reset recovery"
        );
        assert!(
            entry.doc.full_text().contains("Post-reset block"),
            "New block content present after reset recovery"
        );
    }

    // =========================================================================
    // Input Document Tools (Local mode)
    // =========================================================================

    #[tokio::test]
    async fn test_read_input_local_requires_context() {
        let mcp = KaijutsuMcp::new();
        let result = mcp
            .read_input(Parameters(InputReadRequest { context_id: None }))
            .await;
        assert!(
            result.contains("Error"),
            "Should error without context_id in local mode: {result}"
        );
    }

    #[tokio::test]
    async fn test_read_input_local_empty() {
        let mcp = KaijutsuMcp::new();
        let ctx_id = ContextId::new();
        let result = mcp
            .read_input(Parameters(InputReadRequest {
                context_id: Some(ctx_id.to_hex()),
            }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["content"].as_str().unwrap(), "");
        assert_eq!(parsed["length"].as_u64().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_write_and_read_input_local() {
        let mcp = KaijutsuMcp::new();
        let ctx_id = ContextId::new();
        let hex = ctx_id.to_hex();

        // Write some text
        let result = mcp
            .write_input(Parameters(InputWriteRequest {
                context_id: Some(hex.clone()),
                text: "hello from MCP".to_string(),
            }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(
            parsed["success"].as_bool().unwrap(),
            "write_input failed: {result}"
        );
        assert_eq!(parsed["length"].as_u64().unwrap(), 14);

        // Read it back
        let result = mcp
            .read_input(Parameters(InputReadRequest {
                context_id: Some(hex.clone()),
            }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["content"].as_str().unwrap(), "hello from MCP");
    }

    #[tokio::test]
    async fn test_write_input_overwrite() {
        let mcp = KaijutsuMcp::new();
        let ctx_id = ContextId::new();
        let hex = ctx_id.to_hex();

        mcp.write_input(Parameters(InputWriteRequest {
            context_id: Some(hex.clone()),
            text: "first".to_string(),
        }))
        .await;

        mcp.write_input(Parameters(InputWriteRequest {
            context_id: Some(hex.clone()),
            text: "second".to_string(),
        }))
        .await;

        let result = mcp
            .read_input(Parameters(InputReadRequest {
                context_id: Some(hex.clone()),
            }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["content"].as_str().unwrap(), "second");
    }

    #[tokio::test]
    async fn test_edit_input_insert() {
        let mcp = KaijutsuMcp::new();
        let ctx_id = ContextId::new();
        let hex = ctx_id.to_hex();

        // Write initial text
        mcp.write_input(Parameters(InputWriteRequest {
            context_id: Some(hex.clone()),
            text: "hello world".to_string(),
        }))
        .await;

        // Insert " beautiful" at position 5
        let result = mcp
            .edit_input(Parameters(InputEditRequest {
                context_id: Some(hex.clone()),
                pos: 5,
                insert: " beautiful".to_string(),
                delete: 0,
            }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(
            parsed["success"].as_bool().unwrap(),
            "edit_input failed: {result}"
        );

        // Read back
        let result = mcp
            .read_input(Parameters(InputReadRequest {
                context_id: Some(hex.clone()),
            }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["content"].as_str().unwrap(), "hello beautiful world");
    }

    #[tokio::test]
    async fn test_edit_input_delete() {
        let mcp = KaijutsuMcp::new();
        let ctx_id = ContextId::new();
        let hex = ctx_id.to_hex();

        mcp.write_input(Parameters(InputWriteRequest {
            context_id: Some(hex.clone()),
            text: "hello world".to_string(),
        }))
        .await;

        // Delete "world" (5 chars starting at position 6)
        mcp.edit_input(Parameters(InputEditRequest {
            context_id: Some(hex.clone()),
            pos: 6,
            insert: String::new(),
            delete: 5,
        }))
        .await;

        let result = mcp
            .read_input(Parameters(InputReadRequest {
                context_id: Some(hex.clone()),
            }))
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["content"].as_str().unwrap(), "hello ");
    }

    #[tokio::test]
    async fn test_submit_input_local_errors() {
        let mcp = KaijutsuMcp::new();
        let ctx_id = ContextId::new();
        let result = mcp
            .submit_input(Parameters(InputSubmitRequest {
                context_id: Some(ctx_id.to_hex()),
                mode: None,
            }))
            .await;
        assert!(
            result.contains("Error"),
            "submit_input should error in local mode: {result}"
        );
    }

    // ========================================================================
    // ShellCompletion JSON envelope
    //
    // Locks in the wire contract returned by `shell` and `context_shell`.
    // Agents parse this JSON to extract `stdout`, `exit_code`, structured
    // `data`, and `block_id` for follow-up reads. Changing the shape is
    // an agent-visible break — start here when you do.
    // ========================================================================

    fn make_result_snapshot(content: &str, exit_code: Option<i32>) -> kaijutsu_crdt::BlockSnapshot {
        let ctx_id = ContextId::new();
        let call_id = kaijutsu_crdt::BlockId {
            context_id: ctx_id,
            principal_id: PrincipalId::new(),
            seq: 1,
        };
        let result_id = kaijutsu_crdt::BlockId {
            context_id: ctx_id,
            principal_id: PrincipalId::new(),
            seq: 2,
        };
        kaijutsu_crdt::BlockSnapshot::tool_result(
            result_id,
            call_id,
            kaijutsu_crdt::ToolKind::Shell,
            content,
            exit_code.is_some_and(|c| c != 0),
            exit_code,
            None,
        )
    }

    #[test]
    fn test_shell_completion_done_success_envelope() {
        let snap = make_result_snapshot("hello world\n", Some(0));
        let block_key = snap.id.to_key();
        let completion = ShellCompletion::Done {
            snapshot: snap,
            elapsed_ms: 42,
        };
        let json: serde_json::Value = serde_json::from_str(&completion.to_json()).unwrap();

        assert_eq!(json["stdout"], "hello world\n");
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["status"], "done");
        assert_eq!(json["block_id"], block_key);
        assert_eq!(json["content_type"], "text/plain");
        assert_eq!(json["ephemeral"], false);
        assert!(json["data"].is_null(), "no OutputData → data must be null");
        assert_eq!(json["elapsed_ms"], 42);
    }

    #[test]
    fn test_shell_completion_done_failure_propagates_exit_code() {
        // `false` builtin or `kj` Err — non-zero exit_code persisted on block.
        let snap = make_result_snapshot("error: something broke\n", Some(7));
        let completion = ShellCompletion::Done {
            snapshot: snap,
            elapsed_ms: 5,
        };
        let json: serde_json::Value = serde_json::from_str(&completion.to_json()).unwrap();

        assert_eq!(json["exit_code"], 7);
        assert_eq!(json["status"], "error");
        assert_eq!(json["stdout"], "error: something broke\n");
    }

    #[test]
    fn test_shell_completion_done_derives_exit_code_when_absent() {
        // Backward-compat: blocks written before exit_code propagation have
        // `exit_code = None`. Fall back to Status: Done → 0, anything else → 1.
        let snap = make_result_snapshot("ok\n", None);
        let json: serde_json::Value =
            serde_json::from_str(&ShellCompletion::Done { snapshot: snap, elapsed_ms: 1 }.to_json())
                .unwrap();
        assert_eq!(
            json["exit_code"], 0,
            "missing exit_code + Status::Done → 0"
        );
    }

    #[test]
    fn test_shell_completion_timeout_envelope() {
        let ctx_id = ContextId::new();
        let cmd_block_id = kaijutsu_crdt::BlockId {
            context_id: ctx_id,
            principal_id: PrincipalId::new(),
            seq: 99,
        };
        let block_key = cmd_block_id.to_key();
        let completion = ShellCompletion::Timeout {
            cmd_block_id,
            timeout_secs: 300,
            elapsed_ms: 300_000,
        };
        let json: serde_json::Value = serde_json::from_str(&completion.to_json()).unwrap();

        assert_eq!(json["status"], "timeout");
        assert_eq!(json["exit_code"], -1);
        assert_eq!(json["block_id"], block_key);
        assert_eq!(json["elapsed_ms"], 300_000);
        assert!(
            json["error"].as_str().unwrap().contains("300s"),
            "timeout error string should mention duration: {json}"
        );
    }

    #[test]
    fn test_shell_completion_stream_closed_envelope() {
        let ctx_id = ContextId::new();
        let cmd_block_id = kaijutsu_crdt::BlockId {
            context_id: ctx_id,
            principal_id: PrincipalId::new(),
            seq: 99,
        };
        let completion = ShellCompletion::StreamClosed {
            cmd_block_id,
            elapsed_ms: 50,
        };
        let json: serde_json::Value = serde_json::from_str(&completion.to_json()).unwrap();

        assert_eq!(json["status"], "stream_closed");
        assert_eq!(json["exit_code"], -1);
        assert!(json["error"].is_string());
    }
}
