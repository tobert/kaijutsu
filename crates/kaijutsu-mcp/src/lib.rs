//! MCP server exposing kaijutsu CRDT kernel.
//!
//! Provides tools for document and block manipulation via Model Context Protocol,
//! enabling agents like Claude Code and opencode to collaborate
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

use kaijutsu_client::{
    ActorHandle, ConnectionStatus, SshConfig, SyncEffect, SyncedDocument, connect_ssh, spawn_actor,
};
use kaijutsu_crdt::{BlockId, ContextId, ConversationDAG, PrincipalId};
use kaijutsu_kernel::{SharedBlockStore, shared_block_store};
use tokio::sync::watch;

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
    /// Reserved for connection-loss detection. The current SyncedDocument poll
    /// degrades a mid-command disconnect to `Timeout` (it waits on `change`
    /// rather than the raw event stream); this variant + its `to_json` arm are
    /// kept for when the poll learns to surface a closed connection directly.
    #[allow(dead_code)]
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
                // exit_code: surface the real persisted value, or `null` when
                // it genuinely hasn't replicated. We deliberately do NOT fall
                // back to a status-derived 0/1 here — a false `0` reads as
                // success and masks a replication gap. `null` is self-
                // announcing; callers detect failure via a non-zero code OR a
                // status of "error"/"timeout"/"stream_closed".
                let exit_code = match snapshot.exit_code {
                    Some(c) => serde_json::Value::from(c),
                    None => serde_json::Value::Null,
                };
                // stdout lives in content; stderr is its own field (split at
                // the source — see shell_execute). Empty string when unset.
                let stdout = snapshot.content.clone();
                let stderr = snapshot.stderr.clone().unwrap_or_default();
                let data = snapshot
                    .output
                    .as_ref()
                    .and_then(|o| serde_json::to_value(o).ok());
                serde_json::json!({
                    "stdout": stdout,
                    "stderr": stderr,
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
    /// The single joined context's synced CRDT document — `None` until
    /// `register_session`. Owns the `SyncManager`. `parking_lot::Mutex`: the
    /// guard never poisons (a panic under lock can't cascade-kill every later
    /// `lock()`) and `lock()` is a cheap non-async critical section — held only
    /// for fast doc reads/applies, never across an `.await`.
    pub synced: Arc<parking_lot::Mutex<Option<SyncedDocument>>>,
    /// Wake signal: a monotonic generation counter the background listener bumps
    /// (`send_modify`) after each applied event. Waiters (the shell completion
    /// poll) `subscribe()` and `await changed()`. Unlike a bare `Notify`, the
    /// watch channel records the version, so a bump between a waiter's state
    /// check and its `changed().await` is not lost.
    pub change: watch::Sender<u64>,
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
    /// Abort handle for the background event listener.
    _bg_task: Arc<AbortOnDrop>,
}

impl JoinedContext {
    /// Testing seam: abort the background event listener without touching
    /// the connection or joined state otherwise. `remote.change` then never
    /// advances again for this context, reproducing the client-visible
    /// symptom of a server-reaped FlowBus bridge (see `SubscriberHealth`)
    /// without needing to actually starve a real one server-side. Exists for
    /// `tests/e2e_shell.rs` to exercise `execute_and_poll_shell`'s stall
    /// fallback end-to-end; production code never calls this.
    pub fn debug_kill_event_listener(&self) {
        self._bg_task.0.abort();
    }
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
    /// Backing router for the served prompts (analyze_document, search_context,
    /// editing_assistant). Wired into rmcp via `#[prompt_handler]`; the field
    /// itself isn't read directly, hence the allow.
    #[allow(dead_code)]
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
    /// Per-session principal for block authorship. Captured at connect; the
    /// authorship path doesn't read it back through this handle yet.
    #[allow(dead_code)]
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
        Self::connect_with_config(config, context_name, cc_session_id).await
    }

    /// Connect using an explicit [`SshConfig`].
    ///
    /// This is the seam `connect` delegates to. It exists so callers (and the
    /// e2e test harness) can point the full MCP machinery — actor, store,
    /// background sync listener, poll path — at a server reachable only with a
    /// non-default config (e.g. an ephemeral test server using
    /// `KeySource::ephemeral()` + `insecure`). Must be called within a
    /// `LocalSet`. Like `connect`, it establishes the connection and spawns the
    /// actor but does NOT join a context — call `register_session` for that.
    pub async fn connect_with_config(
        config: SshConfig,
        context_name: &str,
        cc_session_id: Option<&str>,
    ) -> Result<Self, anyhow::Error> {
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
        // scope_blocks_to_context = true: the MCP is single-context, and its
        // single-threaded RPC LocalSet is starved by kernel-wide foreign-context
        // event volume (the 2026-06-17 shell-timeout stall). Scoping the block
        // subscription to the joined context cuts that volume to zero.
        let actor = spawn_actor(config, None, "mcp-server".to_string(), true);

        tracing::info!("RPC actor spawned, persistent connection ready");

        let shared_context_id = Arc::new(Mutex::new(None));
        let session_principal = PrincipalId::new();

        Ok(Self {
            backend: Backend::Remote(RemoteState {
                kernel_id: kernel_id_typed,
                actor,
                // SyncedDocument is built once the context is known, in
                // register_session. `change` wakes the shell poll on each apply.
                synced: Arc::new(parking_lot::Mutex::new(None)),
                change: watch::channel(0u64).0,
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

    /// Get the remote state if connected to a server.
    fn remote(&self) -> Option<&RemoteState> {
        match &self.backend {
            Backend::Local(_) => None,
            Backend::Remote(remote) => Some(remote),
        }
    }

    /// Run `f` against a context's CRDT document, regardless of backend, and
    /// return its result (or `None` if the context isn't resident). Local reads
    /// the multi-context kernel store; Remote reads the single joined
    /// `SyncedDocument`. The closure runs while the backend's guard/lock is
    /// held — keep it cheap and never `.await` inside it.
    fn with_doc<R>(
        &self,
        ctx: ContextId,
        f: impl FnOnce(&kaijutsu_crdt::block_store::BlockStore) -> R,
    ) -> Option<R> {
        match &self.backend {
            Backend::Local(store) => store.get(ctx).map(|e| f(&e.doc)),
            Backend::Remote(remote) => {
                let guard = remote.synced.lock();
                // The Remote backend holds exactly one context. Honor the
                // requested `ctx`: a query for a different context must miss
                // (return None), not silently read the joined document.
                guard
                    .as_ref()
                    .filter(|d| d.context_id() == ctx)
                    .map(|d| f(d.doc()))
            }
        }
    }

    /// Resident context ids. Remote exposes only the single joined context.
    fn context_ids(&self) -> Vec<ContextId> {
        match &self.backend {
            Backend::Local(store) => store.list_ids(),
            Backend::Remote(remote) => remote
                .synced
                .lock()
                .as_ref()
                .map(|d| vec![d.context_id()])
                .unwrap_or_default(),
        }
    }

    /// Whether `ctx` is resident in this backend.
    fn contains_context(&self, ctx: ContextId) -> bool {
        self.with_doc(ctx, |_| ()).is_some()
    }

    /// Read one block snapshot, regardless of backend.
    fn read_block(&self, ctx: ContextId, id: &BlockId) -> Option<kaijutsu_crdt::BlockSnapshot> {
        self.with_doc(ctx, |doc| doc.get_block_snapshot(id)).flatten()
    }

    /// Resolve a block-id string to `(ContextId, BlockId)` if it's resident,
    /// regardless of backend. Replaces the free `find_block(store, ..)` helper.
    fn locate_block(&self, block_id_str: &str) -> Option<(ContextId, BlockId)> {
        let block_id = parse_block_id(block_id_str)?;
        let ctx = block_id.context_id;
        self.read_block(ctx, &block_id).map(|_| (ctx, block_id))
    }

    /// Get the joined context's context_id and sync state.
    /// Returns an error string if no context has been joined (register_session not called).
    async fn require_joined(&self) -> Result<(ContextId, &ActorHandle), String> {
        match &self.backend {
            Backend::Local(_) => Err("Error: not connected to server".to_string()),
            Backend::Remote(remote) => {
                let guard = remote.joined.read().await;
                match guard.as_ref() {
                    Some(joined) => Ok((joined.context_id, &remote.actor)),
                    None => {
                        Err("Error: no active context — call register_session first".to_string())
                    }
                }
            }
        }
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
        let fallback_interval = tokio::time::Duration::from_millis(500);

        // Completion check — finds the finished ToolResult child of our command
        // block (Done/Error) in the local SyncedDocument.
        let find_terminal = || -> Option<kaijutsu_crdt::BlockSnapshot> {
            let guard = remote.synced.lock();
            let doc = guard.as_ref()?;
            doc.blocks().into_iter().find(|b| {
                b.parent_id.as_ref() == Some(&cmd_block_id)
                    && b.is_shell()
                    && b.kind == kaijutsu_crdt::BlockKind::ToolResult
                    && matches!(
                        b.status,
                        kaijutsu_crdt::Status::Done | kaijutsu_crdt::Status::Error
                    )
            })
        };

        // Phase 1 — wait until the ToolResult reaches a terminal status locally.
        // Subscribe to the change generation BEFORE the first check: the watch
        // channel records the version, so a bump that lands between our check
        // and the `changed().await` below is still observed (no lost wakeup).
        let mut change_rx = remote.change.subscribe();

        // Stall fallback: `change` is bumped only by the background listener,
        // which only has something to bump when the server's FlowBus bridge
        // delivers an event. If that bridge gets reaped mid-command (a burst
        // of callback timeouts from one transient client stall — see
        // `SubscriberHealth` server-side), the client is never told: the
        // broadcast channel stays open, just silent, and without this we'd
        // block the *entire* `timeout_secs` on a channel that may never fire
        // again. So: if the stall window passes with no watch progress while
        // a command is still pending, assume delivery may be dead and force
        // an authoritative catch-up rather than keep waiting on it blind.
        //
        // The window backs off exponentially within a stall episode (5s,
        // 10s, 20s, capped at 30s) rather than firing flat every 5s. Dead-
        // bridge discovery still lands fast — most shell commands finish
        // under the 5s initial window — but a long, healthy, quiet command
        // (a multi-minute build, `kj synth all`) doesn't pay for a full-
        // context `get_context_sync` snapshot every 5s for its whole
        // runtime; that per-call snapshot cost is already flagged as a perf
        // issue (docs/issues.md) without piling a tight poll on top of it.
        const STALL_INITIAL_WINDOW: tokio::time::Duration = tokio::time::Duration::from_secs(5);
        const STALL_MAX_WINDOW: tokio::time::Duration = tokio::time::Duration::from_secs(30);
        let mut stall_window = STALL_INITIAL_WINDOW;
        let mut next_stall_check = std::time::Instant::now() + stall_window;
        // Resubscribing replaces a live bridge too (idempotent, but not
        // free — see `ActorHandle::resubscribe_blocks`), so fire it at most
        // once per stall episode; real watch progress (delivery is alive
        // after all, just slow) clears this — and the backed-off window —
        // for the next one.
        let mut stall_resubscribed = false;

        let local = loop {
            if let Some(snap) = find_terminal() {
                break snap;
            }
            if start.elapsed().as_secs() > timeout_secs {
                // A timeout is the precise signal that block-event delivery may
                // be dead — the server can reap a subscription after a sustained
                // callback stall, and the client is never told (the broadcast
                // channel stays open, just silent). Re-subscribe so the *next*
                // shell call recovers without a full reconnect. Best-effort:
                // the resubscribe replaces any prior subscription by
                // (principal, instance) on the server.
                if let Err(e) = remote.actor.resubscribe_blocks().await {
                    tracing::warn!("{label}: resubscribe after timeout failed: {e}");
                }
                return ShellCompletion::Timeout {
                    cmd_block_id,
                    timeout_secs,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                };
            }

            // Wait for the next applied event; the fallback tick is now just a
            // safety net (the watch channel makes lost wakeups impossible) —
            // UNLESS the bridge is dead, in which case it never fires and this
            // always times out. That's exactly the case the stall check below
            // exists to catch.
            let watch_progressed = matches!(
                tokio::time::timeout(fallback_interval, change_rx.changed()).await,
                Ok(Ok(()))
            );
            if watch_progressed {
                stall_window = STALL_INITIAL_WINDOW;
                next_stall_check = std::time::Instant::now() + stall_window;
                stall_resubscribed = false;
                continue;
            }

            if std::time::Instant::now() < next_stall_check {
                continue;
            }
            tracing::info!(
                command = %command,
                cmd_block = %cmd_block_id.to_key(),
                elapsed_ms = start.elapsed().as_millis() as u64,
                "shell poll stall fallback: no event-feed progress for the current {}s \
                 window, forcing authoritative resync",
                stall_window.as_secs(),
            );
            // Pull the server's authoritative snapshot straight into the local
            // SyncedDocument — the same resync `resync_synced` performs after a
            // reconnect/lag. It flushes any locally-authored ops first, then
            // folds the fetched state into `remote.synced`. If the command
            // finished server-side while our delivery path was dead, the very
            // next `find_terminal()` at the top of this loop picks it up
            // exactly as if it had arrived locally — no separate fetch/decode
            // path to keep in sync with Phase 2 below.
            resync_synced(&remote.actor, &remote.synced, ctx_id).await;
            if !stall_resubscribed {
                if let Err(e) = remote.actor.resubscribe_blocks().await {
                    tracing::warn!("{label}: stall-fallback resubscribe failed: {e}");
                }
                stall_resubscribed = true;
            }
            // Back off within this episode — see the comment above the
            // window consts. Real watch progress (handled above) is the only
            // thing that resets it.
            stall_window = (stall_window * 2).min(STALL_MAX_WINDOW);
            next_stall_check = std::time::Instant::now() + stall_window;
        };

        // Phase 2 — read the AUTHORITATIVE final block from the server. A shell
        // result's content (BlockTextOps), exit_code/stderr (BlockMetadataChanged)
        // and status (BlockStatusChanged) ride three independently-reorderable
        // topics, so a locally-applied terminal `status` does NOT guarantee
        // content/exit_code have replicated — any of the three can arrive last,
        // which would surface empty stdout / null exit_code. But the server writes
        // content+exit_code BEFORE flipping status (program order), so a snapshot
        // taken after we observe Done is guaranteed complete. Decode it into a
        // throwaway document (no write to the shared doc → no race with the
        // sole-writer bg listener) and read just this block.
        // TODO(perf): this pulls the full context snapshot per shell command; a
        // per-block read RPC would avoid that for large contexts (docs/issues.md).
        match remote.actor.get_context_sync(ctx_id).await {
            Ok(state) => match SyncedDocument::from_sync_state(&state, self.session_principal) {
                Ok(doc) => {
                    if let Some(snap) = doc.get_block(&local.id) {
                        let elapsed_ms = start.elapsed().as_millis() as u64;
                        tracing::info!(
                            command = %command,
                            status = %snap.status.as_str(),
                            exit_code = ?snap.exit_code,
                            output_len = snap.content.len(),
                            elapsed_ms,
                            "{label} completed"
                        );
                        return ShellCompletion::Done {
                            snapshot: snap,
                            elapsed_ms,
                        };
                    }
                    tracing::warn!("{label}: authoritative snapshot missing block, using local");
                }
                Err(e) => {
                    tracing::warn!("{label}: authoritative decode failed: {e}, using local")
                }
            },
            Err(e) => tracing::warn!("{label}: authoritative fetch failed: {e}, using local"),
        }
        // Fallback to the local terminal snapshot if the authoritative read failed.
        ShellCompletion::Done {
            snapshot: local,
            elapsed_ms: start.elapsed().as_millis() as u64,
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
        description = "Execute a kaish command in your current kernel context. The shell is context-bound — '.' references this context in kj commands, and durable cwd/env carry across calls. Full kaish: pipes, variables, scripting, plus `kj` for context/drift/fork management (run `kj help`). Returns a JSON object: {stdout, stderr, exit_code, status, block_id, content_type, ephemeral, data, elapsed_ms}. `stdout` and `stderr` are separate (stderr is empty when the command wrote none). Detect failure via exit_code != 0 (or status == 'timeout'/'stream_closed') rather than text-matching; exit_code may be null if it hasn't replicated yet — treat null as unknown, not success. `data` is the kj structured payload when present (arrays for list commands, objects for inspect). Output also lands as CRDT blocks observable in kaijutsu-app. Examples: 'kj context list --tree', 'kj fork --name alt', 'ls /mnt/project | grep rs'. Requires --connect and register_session.",
        annotations(open_world_hint = true)
    )]
    #[tracing::instrument(skip(self, req), name = "mcp.shell")]
    pub async fn shell(&self, Parameters(req): Parameters<ShellRequest>) -> String {
        let (ctx_id, actor) = match self.require_joined().await {
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
    pub async fn register_session(
        &self,
        Parameters(req): Parameters<RegisterSessionRequest>,
    ) -> String {
        self.register_session_auto(req.label, req.context_type).await
    }

    /// Core of `register_session`, callable without the MCP tool call
    /// machinery — `run_serve` uses this to auto-register a session at
    /// startup so hook events land somewhere without requiring a model to
    /// call the tool first. Same idempotency (`already_registered`) and
    /// wire shape as the tool.
    pub async fn register_session_auto(
        &self,
        label: Option<String>,
        context_type: Option<String>,
    ) -> String {
        let req = RegisterSessionRequest { label, context_type };
        self.register_session_impl(req).await
    }

    async fn register_session_impl(&self, req: RegisterSessionRequest) -> String {
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

        // 1. Create context on the server. MCP-attached contexts default to
        // the "mcp" mode bundle so their rc lifecycle + tool policy runs.
        let context_type = req.context_type.unwrap_or_else(|| "mcp".to_string());
        let context_id = match remote
            .actor
            .create_context_typed(&label, &context_type)
            .await
        {
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

        // 4. Build the synced document from the server snapshot. SyncedDocument
        // owns the SyncManager and buffers out-of-order events (text ops /
        // status changes that arrive before their BlockInserted), replaying
        // them on insert — the fix for the dropped-stdout bug.
        let synced_doc = match SyncedDocument::from_sync_state(&sync_state, self.session_principal) {
            Ok(d) => d,
            Err(e) => return format!("Error building synced document: {e}"),
        };
        {
            let mut g = remote.synced.lock();
            *g = Some(synced_doc);
        }

        // 5. Spawn the single background event listener — the ONLY writer of the
        // SyncedDocument. It applies each event, then wakes waiters (the shell
        // completion poll) via `change`. A `NeedsResync` effect, a broadcast
        // `Lagged`, or a reconnect (`Connected`) triggers a full resync from the
        // server snapshot. Single applier + wake-on-change replaces the old
        // two-receiver race (poll read the store before the listener applied).
        let bg_abort = {
            let mut event_rx = remote.actor.subscribe_events();
            let mut status_rx = remote.actor.subscribe_status();
            let synced_bg = Arc::clone(&remote.synced);
            let change_bg = remote.change.clone();
            let actor_bg = remote.actor.clone();
            let ctx_id_bg = context_id;

            let bg_handle = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        ev = event_rx.recv() => match ev {
                            Ok(event) => {
                                // Apply under the lock, drop it before any await.
                                let effect = {
                                    let mut g = synced_bg.lock();
                                    g.as_mut().map(|s| s.apply_event(&event))
                                };
                                if matches!(effect, Some(SyncEffect::NeedsResync)) {
                                    resync_synced(&actor_bg, &synced_bg, ctx_id_bg).await;
                                }
                                change_bg.send_modify(|g| *g = g.wrapping_add(1));
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!("Missed {n} events, forcing full resync");
                                resync_synced(&actor_bg, &synced_bg, ctx_id_bg).await;
                                change_bg.send_modify(|g| *g = g.wrapping_add(1));
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        },
                        st = status_rx.recv() => match st {
                            Ok(ConnectionStatus::Connected { .. }) => {
                                tracing::info!(
                                    context_id = %ctx_id_bg,
                                    "Reconnected — resyncing MCP synced document",
                                );
                                resync_synced(&actor_bg, &synced_bg, ctx_id_bg).await;
                                change_bg.send_modify(|g| *g = g.wrapping_add(1));
                            }
                            // A lagged status stream may have DROPPED a Connected
                            // transition — we can't tell, so resync to be safe.
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!(
                                    context_id = %ctx_id_bg,
                                    "Status stream lagged ({n}) — resyncing in case a reconnect was missed",
                                );
                                resync_synced(&actor_bg, &synced_bg, ctx_id_bg).await;
                                change_bg.send_modify(|g| *g = g.wrapping_add(1));
                            }
                            Ok(_) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                        },
                    }
                }
            });
            let abort = bg_handle.abort_handle();
            // Supervise the listener. It is the sole writer of the
            // SyncedDocument; if it panics or its event stream closes, the
            // document stops updating and every shell poll silently degrades to
            // its fallback timer (then a timeout). Surface that loudly instead
            // of leaving it to look like a hang. The supervisor self-terminates
            // when the listener resolves (including on teardown abort), so it
            // needs no separate cancellation.
            let sup_ctx = context_id;
            tokio::spawn(async move {
                match bg_handle.await {
                    Ok(()) => tracing::warn!(
                        context_id = %sup_ctx,
                        "MCP event listener exited (server event stream closed); \
                         synced document will no longer update — reconnect needed",
                    ),
                    Err(e) if e.is_cancelled() => tracing::debug!(
                        context_id = %sup_ctx,
                        "MCP event listener cancelled (session teardown)",
                    ),
                    Err(e) => tracing::error!(
                        context_id = %sup_ctx,
                        "MCP event listener PANICKED: {e}; synced document frozen — reconnect needed",
                    ),
                }
            });
            abort
        };

        // 6. Write JoinedContext
        {
            let mut guard = remote.joined.write().await;
            *guard = Some(JoinedContext {
                context_id,
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
    pub async fn whoami(&self) -> String {
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

        let params = match serde_json::to_vec(&normalize_peer_params(&req.params)) {
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
            Backend::Remote(remote) => {
                match remote.actor.get_input_state(ctx_id).await {
                    Ok(state) => serde_json::json!({
                        "context_id": ctx_id.short(),
                        "content": state.content,
                        "length": state.content.len(),
                        "version": state.version,
                    })
                    .to_string(),
                    Err(e) => format!("Error: {}", e),
                }
            }
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

        let focus = args.focus.as_deref().unwrap_or("all");

        // Pull blocks, structure tree, and version under one guard (works for
        // both backends), then build the prompt text.
        let want_structure = focus == "all" || focus == "structure";
        let extracted = self.with_doc(context_id, |doc| {
            let blocks = doc.blocks_ordered();
            let tree_lines = if want_structure {
                let dag = ConversationDAG::from_store(doc);
                Some(format_dag_tree(&dag, None, false))
            } else {
                None
            };
            (blocks, tree_lines, doc.version())
        });
        let (blocks, tree_lines, version) = extracted.ok_or_else(|| {
            McpError::invalid_params(format!("Document '{}' not found", args.document_id), None)
        })?;

        let mut content = String::new();

        // Document overview
        content.push_str(&format!("# Document: {}\n\n", args.document_id));
        content.push_str("**Kind:** Conversation\n");
        content.push_str(&format!("**Block count:** {}\n", blocks.len()));
        content.push_str(&format!("**Version:** {}\n\n", version));

        // Structure analysis
        if let Some(tree_lines) = tree_lines {
            content.push_str("## Structure\n\n");
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
            if self.contains_context(id) {
                vec![id]
            } else {
                return Err(McpError::invalid_params(
                    format!("Document '{}' not found", doc_id),
                    None,
                ));
            }
        } else {
            self.context_ids()
        };

        let mut content = String::new();
        content.push_str(&format!("# Search Results for: `{}`\n\n", args.query));

        let mut total_matches = 0;
        let context_lines = 3;

        for context_id in context_ids {
            let snapshots = self
                .with_doc(context_id, |doc| doc.blocks_ordered())
                .unwrap_or_default();

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
        let (context_id, block_id) = self.locate_block(&args.block_id).ok_or_else(|| {
            McpError::invalid_params(format!("Block '{}' not found", args.block_id), None)
        })?;

        let snapshot = self
            .read_block(context_id, &block_id)
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
            && let Some(parent_snap) = self.read_block(context_id, &parent_id)
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

/// Push locally-authored ops (hook-captured blocks) to the server.
///
/// The base is the SyncManager's inbound frontier, so this re-sends every
/// local op each call — wasteful but safe (server-side CRDT merge is
/// idempotent). Eliminating the re-send needs a dedicated "pushed" frontier;
/// that, plus making this the sole writer so a resync can't race local
/// authoring, is the cohesive follow-up tracked in docs/issues.md.
///
/// Computes the payload under the lock, then releases it before the push await.
pub(crate) async fn flush_local_ops(
    actor: &ActorHandle,
    synced: &Arc<parking_lot::Mutex<Option<SyncedDocument>>>,
    context_id: ContextId,
) {
    let ops = {
        let guard = synced.lock();
        let Some(doc) = guard.as_ref() else { return };
        let frontier = doc.sync().frontier().cloned().unwrap_or_default();
        doc.doc().ops_since(&frontier)
    };
    if ops.block_ops.is_empty()
        && ops.new_blocks.is_empty()
        && ops.updated_headers.is_empty()
        && ops.deleted_blocks.is_empty()
    {
        return;
    }
    let bytes = match kaijutsu_types::codec::encode(&ops) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(%context_id, "flush_local_ops encode failed: {e}");
            return;
        }
    };
    if let Err(e) = actor.push_ops(context_id, &bytes).await {
        tracing::warn!(%context_id, "flush_local_ops push failed: {e}");
    }
}

/// Re-fetch the server's full snapshot and realign the SyncedDocument.
///
/// This is the catch-up that a bare re-subscribe skips. After a reconnect, a
/// lag, or a `NeedsResync` effect, the client replica can diverge. Pulling
/// `get_context_sync` and feeding it through `apply_sync_state` rebuilds the
/// document from the server's current state and resets the frontier so future
/// incremental ops merge cleanly. The async fetch happens with NO lock held;
/// the apply re-takes the lock briefly.
///
/// We FLUSH locally-authored ops first: `apply_sync_state` replaces the document
/// wholesale, so any hook-authored blocks the server hasn't seen would be wiped.
/// Pushing them before the fetch means the snapshot reflects them and they
/// survive. (Residual: a block authored in the flush→apply window is still lost
/// — closing that needs the sole-writer restructure noted in docs/issues.md.)
async fn resync_synced(
    actor: &ActorHandle,
    synced: &Arc<parking_lot::Mutex<Option<SyncedDocument>>>,
    context_id: ContextId,
) {
    flush_local_ops(actor, synced, context_id).await;
    let sync_state = match actor.get_context_sync(context_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%context_id, "resync get_context_sync failed: {e}");
            return;
        }
    };
    let mut guard = synced.lock();
    let Some(doc) = guard.as_mut() else {
        tracing::warn!(%context_id, "resync skipped — no synced document");
        return;
    };
    match doc.apply_sync_state(&sync_state) {
        Ok(effect) => {
            tracing::info!(%context_id, ?effect, "MCP synced document resynced from server snapshot");
        }
        Err(e) => tracing::warn!(%context_id, "resync apply_sync_state failed: {e}"),
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
            for doc_id in self.context_ids() {
                let blocks = self.with_doc(doc_id, |doc| doc.blocks_ordered());
                if let Some(blocks) = blocks {
                    let doc_hex = doc_id.to_hex();
                    resources.push(
                        RawResource {
                            uri: format!("kaijutsu://docs/{}", doc_hex),
                            name: doc_hex.clone(),
                            title: Some(format!("Document: {}", doc_hex)),
                            description: Some(format!(
                                "Conversation document with {} blocks",
                                blocks.len()
                            )),
                            mime_type: Some("application/json".to_string()),
                            size: None,
                            icons: None,
                            meta: None,
                        }
                        .no_annotation(),
                    );

                    // Add each block as a resource
                    for snapshot in blocks {
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
                    .context_ids()
                    .iter()
                    .map(|id| {
                        let block_count = self
                            .with_doc(*id, |doc| doc.blocks_ordered().len())
                            .unwrap_or(0);
                        serde_json::json!({
                            "id": id.to_hex(),
                            "kind": "Conversation",
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
                let extracted = self.with_doc(doc_ctx_id, |doc| {
                    let blocks: Vec<serde_json::Value> = doc
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
                    (blocks, doc.version())
                });
                let (blocks, version) = extracted.ok_or_else(|| {
                    McpError::invalid_params(format!("Document '{}' not found", doc_id_str), None)
                })?;

                let result = serde_json::json!({
                    "id": doc_id_str,
                    "kind": "Conversation",
                    "language": serde_json::Value::Null,
                    "version": version,
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
                    self.locate_block(block_key).ok_or_else(|| {
                        McpError::invalid_params(
                            format!(
                                "Block '{}' not found in document '{}'",
                                block_key, doc_id_str
                            ),
                            None,
                        )
                    })?;

                let snapshot = self
                    .read_block(found_ctx_id, &block_id)
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
                                self.context_ids()
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
                                self.context_ids()
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
                        self.context_ids()
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

/// Normalize peer-invocation `params` before serializing to the wire bytes the
/// peer's `dispatch_peer_action` deserializes.
///
/// Peers (e.g. the app's `switch_context`) expect an object/array payload. But
/// an object passed to the `invoke_peer` MCP tool can arrive here double-encoded
/// as a JSON *string* (a `Value::String` holding `"{...}"`) — `serde_json::to_vec`
/// would then emit a quoted string and the peer's `from_slice::<Params>` would
/// see a string, not a struct. So if `params` is a string that itself decodes to
/// a JSON object or array, unwrap that one layer. A genuine scalar/string param
/// (whose text is not JSON object/array) is passed through unchanged — we only
/// undo the specific double-encoding, never reinterpret real string values.
fn normalize_peer_params(params: &serde_json::Value) -> serde_json::Value {
    if let serde_json::Value::String(s) = params
        && let Ok(inner @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) =
            serde_json::from_str::<serde_json::Value>(s)
    {
        tracing::debug!("invoke_peer: unwrapped double-encoded JSON params");
        return inner;
    }
    params.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug: an object param arrives double-encoded as a JSON string. We must
    /// unwrap exactly one layer so the peer receives an object, not a string.
    #[test]
    fn normalize_unwraps_double_encoded_object() {
        let double = serde_json::Value::String(r#"{"context_id":"019ec11b"}"#.to_string());
        let got = normalize_peer_params(&double);
        assert_eq!(got, serde_json::json!({"context_id": "019ec11b"}));
    }

    /// A correctly-passed object is left untouched (idempotent for the good case).
    #[test]
    fn normalize_passes_through_real_object() {
        let obj = serde_json::json!({"context_id": "019ec11b"});
        assert_eq!(normalize_peer_params(&obj), obj);
    }

    /// A double-encoded array unwraps too.
    #[test]
    fn normalize_unwraps_double_encoded_array() {
        let double = serde_json::Value::String(r#"[1,2,3]"#.to_string());
        assert_eq!(normalize_peer_params(&double), serde_json::json!([1, 2, 3]));
    }

    /// A genuine string value (not JSON object/array) is preserved — we only
    /// undo the object/array double-encoding, never reinterpret real strings.
    #[test]
    fn normalize_preserves_genuine_string() {
        let s = serde_json::Value::String("hello".to_string());
        assert_eq!(normalize_peer_params(&s), s);
        // A bare-number string is also a real string here, not an object/array.
        let n = serde_json::Value::String("123".to_string());
        assert_eq!(normalize_peer_params(&n), n);
    }

    use kaijutsu_crdt::ContextId;

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

    /// Like [`make_result_snapshot`] but with a separate stderr stream.
    fn make_result_snapshot_with_stderr(
        content: &str,
        stderr: &str,
        exit_code: Option<i32>,
    ) -> kaijutsu_crdt::BlockSnapshot {
        let mut snap = make_result_snapshot(content, exit_code);
        snap.stderr = Some(stderr.to_string());
        snap
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
        assert_eq!(json["stderr"], "", "no stderr → empty string");
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["status"], "done");
        assert_eq!(json["block_id"], block_key);
        assert_eq!(json["content_type"], "text/plain");
        assert_eq!(json["ephemeral"], false);
        assert!(json["data"].is_null(), "no OutputData → data must be null");
        assert_eq!(json["elapsed_ms"], 42);
    }

    #[test]
    fn test_shell_completion_surfaces_stderr_separately() {
        // A successful-with-warnings command: stdout + stderr + exit 0. The
        // envelope keeps them apart (the old merge would have hidden stderr
        // inside stdout).
        let snap = make_result_snapshot_with_stderr(
            "build ok\n",
            "warning: unused variable\n",
            Some(0),
        );
        let json: serde_json::Value =
            serde_json::from_str(&ShellCompletion::Done { snapshot: snap, elapsed_ms: 3 }.to_json())
                .unwrap();

        assert_eq!(json["stdout"], "build ok\n");
        assert_eq!(json["stderr"], "warning: unused variable\n");
        assert_eq!(json["exit_code"], 0, "stderr present does not imply failure");
        assert_eq!(json["status"], "done");
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
    fn test_shell_completion_missing_exit_code_is_null_not_zero() {
        // A missing exit_code means it hasn't replicated — surface `null`, NOT
        // a false `0`. A `0` reads as success and masks the replication gap
        // that produced the empty-stdout-after-reconnect bug; `null` is self-
        // announcing so callers don't trust a fabricated success.
        let snap = make_result_snapshot("ok\n", None);
        let json: serde_json::Value =
            serde_json::from_str(&ShellCompletion::Done { snapshot: snap, elapsed_ms: 1 }.to_json())
                .unwrap();
        assert!(
            json["exit_code"].is_null(),
            "missing exit_code must be null, got {}",
            json["exit_code"]
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
