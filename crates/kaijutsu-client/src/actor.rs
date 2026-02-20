//! Actor-based RPC client with concurrent dispatch and backpressure.
//!
//! Provides a `Send+Sync` [`ActorHandle`] that wraps Cap'n Proto's `!Send`
//! types. The actor runs in a `spawn_local` task, dispatching commands
//! concurrently via child tasks while maintaining a persistent SSH
//! connection with auto-reconnect.
//!
//! ```text
//!   ActorHandle (Send+Sync)    bounded(32)   RpcActor (spawn_local, !Send)
//!   ┌─────────────────────┐  ────────────▶  ┌──────────────────────────────┐
//!   │ .drift_push()       │                 │ ensure_connected() [serial]  │
//!   │ .execute_tool()     │  ◀────────────  │ dispatch_command [concurrent]│
//!   │ .push_ops()         │    oneshot      │ auto-reconnect on error      │
//!   └─────────────────────┘                 └──────────────────────────────┘
//! ```
//!
//! Backpressure: The bounded channel (capacity 32) naturally throttles callers
//! when the actor is saturated — `.send().await` blocks until commands complete
//! and slots free up. The system recovers as in-flight RPCs finish.

use kaijutsu_crdt::{BlockId, ContextId};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::Instrument;

use crate::rpc::{
    ClientToolFilter, Completion, ContextInfo, DocumentState, HistoryEntry, Identity,
    KernelInfo, LlmConfigInfo, McpResource, McpResourceContents, McpToolResult, ShellValue,
    StagedDriftInfo, ToolResult, VersionSnapshot,
};
use crate::subscriptions::{
    BlockEventsForwarder, ConnectionStatus, ResourceEventsForwarder, ServerEvent,
};
use crate::{connect_ssh, KernelHandle, RpcClient, SshConfig};

/// Channel capacity — when 32 commands are in-flight, callers block on send.
const CHANNEL_CAPACITY: usize = 32;

/// Broadcast capacity for server events.
const EVENT_BROADCAST_CAPACITY: usize = 256;

/// Broadcast capacity for connection status events.
const STATUS_BROADCAST_CAPACITY: usize = 16;

// ============================================================================
// Error Type
// ============================================================================

/// Errors from the actor system.
#[derive(Debug, thiserror::Error)]
pub enum ActorError {
    #[error("not connected to server")]
    NotConnected,
    #[error("connection lost: {0}")]
    ConnectionLost(String),
    #[error("RPC error: {0}")]
    Rpc(String),
    #[error("actor shut down")]
    Shutdown,
}

// ============================================================================
// RPC Commands
// ============================================================================

/// Internal command sent from ActorHandle → RpcActor via mpsc.
///
/// Each variant carries its arguments and a oneshot reply channel.
/// World-level commands (Whoami, ListKernels) are handled inline in the
/// run loop; kernel commands are dispatched concurrently via spawn_local.
#[allow(clippy::large_enum_variant)]
enum RpcCommand {
    // ── Drift ────────────────────────────────────────────────────────────
    DriftPush { target_ctx: ContextId, content: String, summarize: bool, reply: oneshot::Sender<Result<u64, ActorError>> },
    DriftFlush { reply: oneshot::Sender<Result<u32, ActorError>> },
    DriftQueue { reply: oneshot::Sender<Result<Vec<StagedDriftInfo>, ActorError>> },
    DriftCancel { staged_id: u64, reply: oneshot::Sender<Result<bool, ActorError>> },
    DriftPull { source_ctx: ContextId, prompt: Option<String>, reply: oneshot::Sender<Result<BlockId, ActorError>> },
    DriftMerge { source_ctx: ContextId, reply: oneshot::Sender<Result<BlockId, ActorError>> },

    // ── Context ──────────────────────────────────────────────────────────
    GetDocumentId { reply: oneshot::Sender<Result<Option<String>, ActorError>> },
    GetContextId { reply: oneshot::Sender<Result<(ContextId, String), ActorError>> },
    ListContexts { reply: oneshot::Sender<Result<Vec<ContextInfo>, ActorError>> },
    CreateContext { label: String, reply: oneshot::Sender<Result<ContextId, ActorError>> },
    AttachDocument { context_id: ContextId, document_id: String, reply: oneshot::Sender<Result<(), ActorError>> },
    DetachDocument { context_id: ContextId, document_id: String, reply: oneshot::Sender<Result<(), ActorError>> },

    // ── CRDT Sync ────────────────────────────────────────────────────────
    PushOps { document_id: String, ops: Vec<u8>, reply: oneshot::Sender<Result<u64, ActorError>> },
    GetDocumentState { document_id: String, reply: oneshot::Sender<Result<DocumentState, ActorError>> },
    CompactDocument { document_id: String, reply: oneshot::Sender<Result<(u64, u64), ActorError>> },

    // ── Shell / Execution ────────────────────────────────────────────────
    Execute { code: String, reply: oneshot::Sender<Result<u64, ActorError>> },
    ShellExecute { code: String, cell_id: String, reply: oneshot::Sender<Result<BlockId, ActorError>> },
    Interrupt { exec_id: u64, reply: oneshot::Sender<Result<(), ActorError>> },
    Complete { partial: String, cursor: u32, reply: oneshot::Sender<Result<Vec<Completion>, ActorError>> },
    GetCommandHistory { limit: u32, reply: oneshot::Sender<Result<Vec<HistoryEntry>, ActorError>> },

    // ── Shell Variables ──────────────────────────────────────────────────
    GetShellVar { name: String, reply: oneshot::Sender<Result<(Option<ShellValue>, bool), ActorError>> },
    SetShellVar { name: String, value: ShellValue, reply: oneshot::Sender<Result<(), ActorError>> },
    ListShellVars { reply: oneshot::Sender<Result<Vec<(String, ShellValue)>, ActorError>> },

    // ── Tool Execution ───────────────────────────────────────────────────
    ExecuteTool { tool: String, params: String, reply: oneshot::Sender<Result<ToolResult, ActorError>> },
    CallMcpTool { server: String, tool: String, arguments: serde_json::Value, reply: oneshot::Sender<Result<McpToolResult, ActorError>> },

    // ── MCP Resources ────────────────────────────────────────────────────
    ListMcpResources { server: String, reply: oneshot::Sender<Result<Vec<McpResource>, ActorError>> },
    ReadMcpResource { server: String, uri: String, reply: oneshot::Sender<Result<Option<McpResourceContents>, ActorError>> },

    // ── LLM ──────────────────────────────────────────────────────────────
    Prompt { content: String, model: Option<String>, cell_id: String, reply: oneshot::Sender<Result<String, ActorError>> },
    ConfigureLlm { provider: String, model: String, reply: oneshot::Sender<Result<bool, ActorError>> },
    GetLlmConfig { reply: oneshot::Sender<Result<LlmConfigInfo, ActorError>> },
    SetDefaultProvider { provider: String, reply: oneshot::Sender<Result<bool, ActorError>> },
    SetDefaultModel { provider: String, model: String, reply: oneshot::Sender<Result<bool, ActorError>> },

    // ── Tool Filter ──────────────────────────────────────────────────────
    GetToolFilter { reply: oneshot::Sender<Result<ClientToolFilter, ActorError>> },
    SetToolFilter { filter: ClientToolFilter, reply: oneshot::Sender<Result<bool, ActorError>> },

    // ── Timeline / Fork ──────────────────────────────────────────────────
    ForkFromVersion { document_id: String, version: u64, label: String, reply: oneshot::Sender<Result<ContextId, ActorError>> },
    CherryPickBlock { block_id: BlockId, target_context: ContextId, reply: oneshot::Sender<Result<BlockId, ActorError>> },
    GetDocumentHistory { document_id: String, limit: u32, reply: oneshot::Sender<Result<Vec<VersionSnapshot>, ActorError>> },

    // ── Kernel Info ──────────────────────────────────────────────────────
    GetInfo { reply: oneshot::Sender<Result<KernelInfo, ActorError>> },

    // ── World-level (handled inline, not dispatched to child tasks) ──────
    Whoami { reply: oneshot::Sender<Result<Identity, ActorError>> },
    ListKernels { reply: oneshot::Sender<Result<Vec<KernelInfo>, ActorError>> },
}

impl RpcCommand {
    /// Send an error reply without matching all variant fields.
    fn reply_err(self, err: ActorError) {
        match self {
            Self::DriftPush { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::DriftFlush { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::DriftQueue { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::DriftCancel { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::DriftPull { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::DriftMerge { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetDocumentId { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetContextId { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ListContexts { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::CreateContext { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::AttachDocument { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::DetachDocument { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::PushOps { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetDocumentState { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::CompactDocument { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::Execute { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ShellExecute { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::Interrupt { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::Complete { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetCommandHistory { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetShellVar { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::SetShellVar { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ListShellVars { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ExecuteTool { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::CallMcpTool { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ListMcpResources { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ReadMcpResource { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::Prompt { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ConfigureLlm { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetLlmConfig { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::SetDefaultProvider { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::SetDefaultModel { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetToolFilter { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::SetToolFilter { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ForkFromVersion { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::CherryPickBlock { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetDocumentHistory { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::GetInfo { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::Whoami { reply, .. } => { let _ = reply.send(Err(err)); }
            Self::ListKernels { reply, .. } => { let _ = reply.send(Err(err)); }
        }
    }
}

// ============================================================================
// Channel wrapper (carries caller span across mpsc boundary)
// ============================================================================

/// Wraps an [`RpcCommand`] with the caller's tracing span so that
/// actor-side dispatch inherits the correct parent context.
///
/// Without this, the mpsc channel severs the span hierarchy — the actor's
/// `spawn_local` tasks would start new root spans instead of being children
/// of the `ActorHandle` method that initiated the call.
struct ChannelCmd {
    command: RpcCommand,
    span: tracing::Span,
}

// ============================================================================
// ActorHandle (Send + Sync public API)
// ============================================================================

/// Send+Sync handle to an RPC actor running in a LocalSet.
///
/// Each method sends a command via a bounded mpsc channel and awaits the
/// oneshot reply. When the channel is full (32 in-flight commands), callers
/// naturally block on `.send().await` until slots free up — providing
/// backpressure without explicit semaphores.
///
/// The handle can be cloned and shared across threads.
#[derive(Clone)]
pub struct ActorHandle {
    tx: mpsc::Sender<ChannelCmd>,
    event_tx: broadcast::Sender<ServerEvent>,
    status_tx: broadcast::Sender<ConnectionStatus>,
}

impl ActorHandle {
    /// Generic send helper — creates a oneshot, sends the command, awaits reply.
    ///
    /// Captures `tracing::Span::current()` so the actor-side dispatch inherits
    /// the caller's span context across the mpsc channel boundary.
    async fn send<T: Send + 'static>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, ActorError>>) -> RpcCommand,
    ) -> Result<T, ActorError> {
        let (reply, rx) = oneshot::channel();
        let cmd = ChannelCmd {
            command: build(reply),
            span: tracing::Span::current(),
        };
        self.tx
            .send(cmd)
            .await
            .map_err(|_| ActorError::Shutdown)?;
        rx.await.map_err(|_| ActorError::Shutdown)?
    }

    // ── Subscriptions ────────────────────────────────────────────────────

    /// Subscribe to server-push events (block changes, resource updates).
    ///
    /// Returns a broadcast receiver. Dropping it is fine — no backpressure
    /// on the sender side; lagged receivers get `RecvError::Lagged`.
    pub fn subscribe_events(&self) -> broadcast::Receiver<ServerEvent> {
        self.event_tx.subscribe()
    }

    /// Subscribe to connection lifecycle events.
    pub fn subscribe_status(&self) -> broadcast::Receiver<ConnectionStatus> {
        self.status_tx.subscribe()
    }

    // ── Drift ────────────────────────────────────────────────────────────

    /// Stage a drift push to another context.
    #[tracing::instrument(skip(self, content))]
    pub async fn drift_push(
        &self,
        target_ctx: ContextId,
        content: &str,
        summarize: bool,
    ) -> Result<u64, ActorError> {
        self.send(|reply| RpcCommand::DriftPush {
            target_ctx, content: content.into(), summarize, reply,
        }).await
    }

    /// Flush all staged drifts.
    #[tracing::instrument(skip(self))]
    pub async fn drift_flush(&self) -> Result<u32, ActorError> {
        self.send(|reply| RpcCommand::DriftFlush { reply }).await
    }

    /// View the drift staging queue.
    #[tracing::instrument(skip(self))]
    pub async fn drift_queue(&self) -> Result<Vec<StagedDriftInfo>, ActorError> {
        self.send(|reply| RpcCommand::DriftQueue { reply }).await
    }

    /// Cancel a staged drift.
    #[tracing::instrument(skip(self))]
    pub async fn drift_cancel(&self, staged_id: u64) -> Result<bool, ActorError> {
        self.send(|reply| RpcCommand::DriftCancel { staged_id, reply }).await
    }

    /// Pull summarized content from another context.
    #[tracing::instrument(skip(self, prompt))]
    pub async fn drift_pull(
        &self,
        source_ctx: ContextId,
        prompt: Option<&str>,
    ) -> Result<BlockId, ActorError> {
        self.send(|reply| RpcCommand::DriftPull {
            source_ctx, prompt: prompt.map(String::from), reply,
        }).await
    }

    /// Merge a forked context back into its parent.
    #[tracing::instrument(skip(self))]
    pub async fn drift_merge(&self, source_ctx: ContextId) -> Result<BlockId, ActorError> {
        self.send(|reply| RpcCommand::DriftMerge { source_ctx, reply }).await
    }

    // ── Context ──────────────────────────────────────────────────────────

    /// Get this kernel's context ID and label.
    #[tracing::instrument(skip(self))]
    pub async fn get_context_id(&self) -> Result<(ContextId, String), ActorError> {
        self.send(|reply| RpcCommand::GetContextId { reply }).await
    }

    /// List all contexts in this kernel (includes drift info).
    #[tracing::instrument(skip(self))]
    pub async fn list_contexts(&self) -> Result<Vec<ContextInfo>, ActorError> {
        self.send(|reply| RpcCommand::ListContexts { reply }).await
    }

    /// Create a new context with an optional label.
    #[tracing::instrument(skip(self))]
    pub async fn create_context(&self, label: &str) -> Result<ContextId, ActorError> {
        self.send(|reply| RpcCommand::CreateContext { label: label.into(), reply }).await
    }

    /// Attach a document to a context.
    #[tracing::instrument(skip(self))]
    pub async fn attach_document(
        &self,
        context_id: ContextId,
        document_id: &str,
    ) -> Result<(), ActorError> {
        self.send(|reply| RpcCommand::AttachDocument {
            context_id, document_id: document_id.into(), reply,
        }).await
    }

    /// Detach a document from a context.
    #[tracing::instrument(skip(self))]
    pub async fn detach_document(
        &self,
        context_id: ContextId,
        document_id: &str,
    ) -> Result<(), ActorError> {
        self.send(|reply| RpcCommand::DetachDocument {
            context_id, document_id: document_id.into(), reply,
        }).await
    }

    // ── CRDT Sync ────────────────────────────────────────────────────────

    /// Push CRDT operations to the server.
    #[tracing::instrument(skip(self, ops))]
    pub async fn push_ops(&self, document_id: &str, ops: &[u8]) -> Result<u64, ActorError> {
        self.send(|reply| RpcCommand::PushOps {
            document_id: document_id.into(), ops: ops.to_vec(), reply,
        }).await
    }

    /// Get full document state from the server.
    #[tracing::instrument(skip(self))]
    pub async fn get_document_state(&self, document_id: &str) -> Result<DocumentState, ActorError> {
        self.send(|reply| RpcCommand::GetDocumentState { document_id: document_id.into(), reply }).await
    }

    /// Compact a document's oplog. Returns (new_size, generation).
    #[tracing::instrument(skip(self))]
    pub async fn compact_document(&self, document_id: &str) -> Result<(u64, u64), ActorError> {
        self.send(|reply| RpcCommand::CompactDocument { document_id: document_id.into(), reply }).await
    }

    // ── Shell / Execution ────────────────────────────────────────────────

    /// Execute code in the kernel's embedded kaish.
    #[tracing::instrument(skip(self, code))]
    pub async fn execute(&self, code: &str) -> Result<u64, ActorError> {
        self.send(|reply| RpcCommand::Execute { code: code.into(), reply }).await
    }

    /// Execute shell command with block output (kaish REPL mode).
    #[tracing::instrument(skip(self, code))]
    pub async fn shell_execute(&self, code: &str, cell_id: &str) -> Result<BlockId, ActorError> {
        self.send(|reply| RpcCommand::ShellExecute {
            code: code.into(), cell_id: cell_id.into(), reply,
        }).await
    }

    /// Interrupt an execution.
    #[tracing::instrument(skip(self))]
    pub async fn interrupt(&self, exec_id: u64) -> Result<(), ActorError> {
        self.send(|reply| RpcCommand::Interrupt { exec_id, reply }).await
    }

    /// Get completions for partial input.
    #[tracing::instrument(skip(self, partial))]
    pub async fn complete(&self, partial: &str, cursor: u32) -> Result<Vec<Completion>, ActorError> {
        self.send(|reply| RpcCommand::Complete { partial: partial.into(), cursor, reply }).await
    }

    /// Get command history.
    #[tracing::instrument(skip(self))]
    pub async fn get_command_history(&self, limit: u32) -> Result<Vec<HistoryEntry>, ActorError> {
        self.send(|reply| RpcCommand::GetCommandHistory { limit, reply }).await
    }

    // ── Shell Variables ─────────────────────────────────────────────────

    /// Get a shell variable by name.
    #[tracing::instrument(skip(self))]
    pub async fn get_shell_var(&self, name: &str) -> Result<(Option<ShellValue>, bool), ActorError> {
        self.send(|reply| RpcCommand::GetShellVar { name: name.into(), reply }).await
    }

    /// Set a shell variable.
    #[tracing::instrument(skip(self, value))]
    pub async fn set_shell_var(&self, name: &str, value: ShellValue) -> Result<(), ActorError> {
        self.send(|reply| RpcCommand::SetShellVar { name: name.into(), value, reply }).await
    }

    /// List all shell variables with their values.
    #[tracing::instrument(skip(self))]
    pub async fn list_shell_vars(&self) -> Result<Vec<(String, ShellValue)>, ActorError> {
        self.send(|reply| RpcCommand::ListShellVars { reply }).await
    }

    // ── Tool Execution ───────────────────────────────────────────────────

    /// Execute a tool on the server (git, etc).
    #[tracing::instrument(skip(self, params))]
    pub async fn execute_tool(&self, tool: &str, params: &str) -> Result<ToolResult, ActorError> {
        self.send(|reply| RpcCommand::ExecuteTool {
            tool: tool.into(), params: params.into(), reply,
        }).await
    }

    /// Call an MCP tool.
    #[tracing::instrument(skip(self, arguments))]
    pub async fn call_mcp_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: &serde_json::Value,
    ) -> Result<McpToolResult, ActorError> {
        self.send(|reply| RpcCommand::CallMcpTool {
            server: server.into(), tool: tool.into(), arguments: arguments.clone(), reply,
        }).await
    }

    // ── MCP Resources ────────────────────────────────────────────────────

    /// List resources from an MCP server.
    #[tracing::instrument(skip(self))]
    pub async fn list_mcp_resources(&self, server: &str) -> Result<Vec<McpResource>, ActorError> {
        self.send(|reply| RpcCommand::ListMcpResources { server: server.into(), reply }).await
    }

    /// Read a resource from an MCP server.
    #[tracing::instrument(skip(self))]
    pub async fn read_mcp_resource(
        &self,
        server: &str,
        uri: &str,
    ) -> Result<Option<McpResourceContents>, ActorError> {
        self.send(|reply| RpcCommand::ReadMcpResource {
            server: server.into(), uri: uri.into(), reply,
        }).await
    }

    // ── LLM ──────────────────────────────────────────────────────────────

    /// Send a prompt to the server-side LLM.
    #[tracing::instrument(skip(self, content))]
    pub async fn prompt(
        &self,
        content: &str,
        model: Option<&str>,
        cell_id: &str,
    ) -> Result<String, ActorError> {
        self.send(|reply| RpcCommand::Prompt {
            content: content.into(), model: model.map(String::from), cell_id: cell_id.into(), reply,
        }).await
    }

    /// Configure the LLM provider and model for this kernel.
    #[tracing::instrument(skip(self))]
    pub async fn configure_llm(&self, provider: &str, model: &str) -> Result<bool, ActorError> {
        self.send(|reply| RpcCommand::ConfigureLlm {
            provider: provider.into(), model: model.into(), reply,
        }).await
    }

    /// Get current LLM configuration.
    #[tracing::instrument(skip(self))]
    pub async fn get_llm_config(&self) -> Result<LlmConfigInfo, ActorError> {
        self.send(|reply| RpcCommand::GetLlmConfig { reply }).await
    }

    /// Set the default LLM provider.
    #[tracing::instrument(skip(self))]
    pub async fn set_default_provider(&self, provider: &str) -> Result<bool, ActorError> {
        self.send(|reply| RpcCommand::SetDefaultProvider { provider: provider.into(), reply }).await
    }

    /// Set the default model for a provider.
    #[tracing::instrument(skip(self))]
    pub async fn set_default_model(&self, provider: &str, model: &str) -> Result<bool, ActorError> {
        self.send(|reply| RpcCommand::SetDefaultModel {
            provider: provider.into(), model: model.into(), reply,
        }).await
    }

    // ── Tool Filter ──────────────────────────────────────────────────────

    /// Get current tool filter configuration.
    #[tracing::instrument(skip(self))]
    pub async fn get_tool_filter(&self) -> Result<ClientToolFilter, ActorError> {
        self.send(|reply| RpcCommand::GetToolFilter { reply }).await
    }

    /// Set tool filter configuration.
    #[tracing::instrument(skip(self, filter))]
    pub async fn set_tool_filter(&self, filter: ClientToolFilter) -> Result<bool, ActorError> {
        self.send(|reply| RpcCommand::SetToolFilter { filter, reply }).await
    }

    // ── Timeline / Fork ──────────────────────────────────────────────────

    /// Fork a document at a specific version, creating a new context.
    ///
    /// Returns the server-assigned ContextId for the new fork.
    #[tracing::instrument(skip(self))]
    pub async fn fork_from_version(
        &self,
        document_id: &str,
        version: u64,
        label: &str,
    ) -> Result<ContextId, ActorError> {
        self.send(|reply| RpcCommand::ForkFromVersion {
            document_id: document_id.into(), version, label: label.into(), reply,
        }).await
    }

    /// Cherry-pick a block from one context into another.
    #[tracing::instrument(skip(self))]
    pub async fn cherry_pick_block(
        &self,
        block_id: &BlockId,
        target_context: ContextId,
    ) -> Result<BlockId, ActorError> {
        self.send(|reply| RpcCommand::CherryPickBlock {
            block_id: block_id.clone(), target_context, reply,
        }).await
    }

    /// Get document history (version snapshots).
    #[tracing::instrument(skip(self))]
    pub async fn get_document_history(
        &self,
        document_id: &str,
        limit: u32,
    ) -> Result<Vec<VersionSnapshot>, ActorError> {
        self.send(|reply| RpcCommand::GetDocumentHistory {
            document_id: document_id.into(), limit, reply,
        }).await
    }

    // ── Kernel Info ──────────────────────────────────────────────────────

    /// Get kernel info.
    #[tracing::instrument(skip(self))]
    pub async fn get_info(&self) -> Result<KernelInfo, ActorError> {
        self.send(|reply| RpcCommand::GetInfo { reply }).await
    }

    // ── World-level Methods ──────────────────────────────────────────────

    /// Get the current user's identity.
    #[tracing::instrument(skip(self))]
    pub async fn whoami(&self) -> Result<Identity, ActorError> {
        self.send(|reply| RpcCommand::Whoami { reply }).await
    }

    /// Get the document ID returned by join_context (server-authoritative).
    #[tracing::instrument(skip(self))]
    pub async fn document_id(&self) -> Result<Option<String>, ActorError> {
        self.send(|reply| RpcCommand::GetDocumentId { reply }).await
    }

    /// List available kernels.
    #[tracing::instrument(skip(self))]
    pub async fn list_kernels(&self) -> Result<Vec<KernelInfo>, ActorError> {
        self.send(|reply| RpcCommand::ListKernels { reply }).await
    }
}

// ============================================================================
// RpcActor (internal, !Send, runs in spawn_local)
// ============================================================================

/// Call an RPC method on a cloned KernelHandle, signaling errors to the actor.
///
/// On success, sends `Ok(val)` to the reply channel. On failure, logs a warning,
/// signals the actor to disconnect via `err_tx`, and sends `Err(ActorError::Rpc)`
/// to the reply channel.
macro_rules! rpc_call {
    ($kernel:ident, $reply:ident, $err_tx:ident, $k:ident, $call:expr) => {{
        let rpc_result = {
            let $k = &$kernel;
            $call.await.map_err(|e| e.to_string())
        };
        let result = match rpc_result {
            Ok(val) => Ok(val),
            Err(msg) => {
                log::warn!("RPC error, will reconnect on next call: {msg}");
                let _ = $err_tx.send(());
                Err(ActorError::Rpc(msg))
            }
        };
        let _ = $reply.send(result);
    }};
}

/// Maximum backoff between reconnect attempts (30 seconds).
const MAX_BACKOFF_SECS: f64 = 30.0;

/// Base backoff duration (1 second), doubles each attempt.
const BASE_BACKOFF_SECS: f64 = 1.0;

/// The actual actor that holds !Send Cap'n Proto types.
struct RpcActor {
    config: SshConfig,
    #[allow(dead_code)] // Phase 5: used for logging/display
    kernel_id: String,
    context_id: Option<ContextId>,
    instance: String,
    /// Live connection state (None = disconnected, will reconnect)
    connection: Option<ConnectionState>,
    /// Document ID returned by join_context (server-authoritative)
    document_id: Option<String>,
    /// Broadcast sender for server events
    event_tx: broadcast::Sender<ServerEvent>,
    /// Broadcast sender for connection status
    status_tx: broadcast::Sender<ConnectionStatus>,
    /// Reconnection attempt counter (reset on success)
    reconnect_attempts: u32,
    /// When the next reconnect attempt is allowed (exponential backoff).
    /// None = no cooldown, reconnect immediately.
    next_reconnect_at: Option<tokio::time::Instant>,
}

/// Held by the actor when connected.
struct ConnectionState {
    client: RpcClient,
    kernel: KernelHandle,
}

impl RpcActor {
    fn new(
        config: SshConfig,
        kernel_id: String,
        context_id: Option<ContextId>,
        instance: String,
        existing: Option<(RpcClient, KernelHandle)>,
        event_tx: broadcast::Sender<ServerEvent>,
        status_tx: broadcast::Sender<ConnectionStatus>,
    ) -> Self {
        let connection = existing.map(|(client, kernel)| ConnectionState { client, kernel });
        Self {
            config,
            kernel_id,
            context_id,
            instance,
            connection,
            document_id: None,
            event_tx,
            status_tx,
            reconnect_attempts: 0,
            next_reconnect_at: None,
        }
    }

    /// Ensure we have a live connection, reconnecting if needed.
    ///
    /// Uses exponential backoff (1s, 2s, 4s, ... up to 30s) between reconnect
    /// attempts. Commands arriving during the cooldown period are rejected with
    /// `ConnectionLost` so callers aren't blocked waiting for the backoff timer.
    async fn ensure_connected(&mut self) -> Result<(), ActorError> {
        if self.connection.is_some() {
            return Ok(());
        }

        // Check backoff cooldown — reject immediately if we're in a cooldown period
        if let Some(next_at) = self.next_reconnect_at {
            let now = tokio::time::Instant::now();
            if now < next_at {
                let remaining = (next_at - now).as_secs_f64();
                let msg = format!(
                    "reconnect backoff (attempt {}, {remaining:.1}s remaining)",
                    self.reconnect_attempts,
                );
                log::warn!("{msg}");
                let _ = self.status_tx.send(ConnectionStatus::Error(msg.clone()));
                return Err(ActorError::ConnectionLost(msg));
            }
        }

        self.reconnect_attempts += 1;
        let _ = self.status_tx.send(ConnectionStatus::Reconnecting {
            attempt: self.reconnect_attempts,
        });

        log::info!(
            "Actor reconnecting to {}:{} kernel={} context={:?} (attempt {})",
            self.config.host, self.config.port, self.kernel_id, self.context_id,
            self.reconnect_attempts,
        );

        let result = self.try_connect().await;

        match result {
            Ok(()) => {
                self.reconnect_attempts = 0;
                self.next_reconnect_at = None;
                let _ = self.status_tx.send(ConnectionStatus::Connected {
                    document_id: self.document_id.clone(),
                });
                Ok(())
            }
            Err(e) => {
                // Set backoff: 1s, 2s, 4s, 8s, 16s, capped at 30s
                let backoff_secs = (BASE_BACKOFF_SECS * 2.0_f64.powi(self.reconnect_attempts.saturating_sub(1) as i32))
                    .min(MAX_BACKOFF_SECS);
                self.next_reconnect_at = Some(
                    tokio::time::Instant::now() + tokio::time::Duration::from_secs_f64(backoff_secs),
                );
                log::debug!(
                    "Reconnect failed, next attempt in {backoff_secs:.1}s: {e}",
                );
                Err(e)
            }
        }
    }

    /// Attempt SSH connect → attach_kernel → join_context → subscriptions.
    ///
    /// The entire sequence is wrapped in a timeout to prevent hanging on
    /// SYN blackholes or stalled servers.
    async fn try_connect(&mut self) -> Result<(), ActorError> {
        use crate::constants::CONNECT_TIMEOUT;

        match tokio::time::timeout(CONNECT_TIMEOUT, self.try_connect_inner()).await {
            Ok(result) => result,
            Err(_) => {
                let msg = format!("connect timeout ({}s)", CONNECT_TIMEOUT.as_secs());
                let _ = self.status_tx.send(ConnectionStatus::Error(msg.clone()));
                Err(ActorError::ConnectionLost(msg))
            }
        }
    }

    /// Inner connect logic (separated so try_connect can wrap with timeout).
    async fn try_connect_inner(&mut self) -> Result<(), ActorError> {
        let client = connect_ssh(self.config.clone())
            .await
            .map_err(|e| {
                let msg = format!("SSH: {e}");
                let _ = self.status_tx.send(ConnectionStatus::Error(msg.clone()));
                ActorError::ConnectionLost(msg)
            })?;

        let (kernel, _kernel_id) = client.attach_kernel()
            .await
            .map_err(|e| {
                let msg = format!("attach_kernel: {e}");
                let _ = self.status_tx.send(ConnectionStatus::Error(msg.clone()));
                ActorError::Rpc(msg)
            })?;

        // Join context if one was specified
        let document_id = if let Some(ctx_id) = &self.context_id {
            let doc_id = kernel.join_context(*ctx_id, &self.instance)
                .await
                .map_err(|e| {
                    let msg = format!("join_context: {e}");
                    let _ = self.status_tx.send(ConnectionStatus::Error(msg.clone()));
                    ActorError::Rpc(msg)
                })?;
            Some(doc_id)
        } else {
            None
        };

        self.document_id = document_id;
        self.connection = Some(ConnectionState { client, kernel });

        // Set up subscriptions on the new connection
        self.setup_subscriptions().await?;
        Ok(())
    }

    /// Register block and resource event subscriptions on the current connection.
    async fn setup_subscriptions(&self) -> Result<(), ActorError> {
        let conn = self.connection.as_ref().ok_or(ActorError::NotConnected)?;

        // Block events
        let block_fwd = BlockEventsForwarder { event_tx: self.event_tx.clone() };
        let block_client: crate::kaijutsu_capnp::block_events::Client =
            capnp_rpc::new_client(block_fwd);
        conn.kernel.subscribe_blocks(block_client).await
            .map_err(|e| ActorError::Rpc(format!("subscribe_blocks: {e}")))?;

        // Resource events
        let resource_fwd = ResourceEventsForwarder { event_tx: self.event_tx.clone() };
        let resource_client: crate::kaijutsu_capnp::resource_events::Client =
            capnp_rpc::new_client(resource_fwd);
        conn.kernel.subscribe_mcp_resources(resource_client).await
            .map_err(|e| ActorError::Rpc(format!("subscribe_resources: {e}")))?;

        log::debug!("Subscriptions registered for block and resource events");
        Ok(())
    }

    /// Drop the connection so next call triggers reconnect.
    fn disconnect(&mut self) {
        self.connection = None;
        let _ = self.status_tx.send(ConnectionStatus::Disconnected);
    }

    /// Process commands concurrently until the channel closes.
    ///
    /// Connection management (ensure_connected) is serial — one at a time.
    /// RPC calls are dispatched concurrently via spawn_local child tasks.
    /// The err_tx channel lets child tasks signal connection failures back
    /// to the main loop, which disconnects so the next command reconnects.
    ///
    /// Each [`ChannelCmd`] carries the caller's tracing span so that
    /// dispatched tasks inherit the correct parent context.
    async fn run(mut self, mut rx: mpsc::Receiver<ChannelCmd>) {
        // If created with an existing connection, register subscriptions now.
        // (try_connect would do this, but ensure_connected skips when already connected)
        if self.connection.is_some() {
            if let Err(e) = self.setup_subscriptions().await {
                log::warn!("Failed to setup subscriptions on existing connection: {e}");
                self.disconnect();
            }
        }

        let (err_tx, mut err_rx) = mpsc::unbounded_channel::<()>();

        loop {
            tokio::select! {
                // Prioritize error signals so we disconnect before spawning
                // new tasks on a dead connection.
                biased;

                // Any child task signaled an RPC error → disconnect
                _ = err_rx.recv() => {
                    log::warn!("Child task reported RPC error, disconnecting");
                    self.disconnect();
                }
                envelope = rx.recv() => {
                    let Some(ChannelCmd { command: cmd, span }) = envelope else { break };

                    // Serial: ensure connection
                    if let Err(e) = self.ensure_connected().await {
                        cmd.reply_err(e);
                        continue;
                    }
                    let conn = self.connection.as_ref().unwrap();

                    // Local queries and world-level commands — handle inline
                    match cmd {
                        RpcCommand::GetDocumentId { reply } => {
                            let _ = reply.send(Ok(self.document_id.clone()));
                            continue;
                        }
                        RpcCommand::Whoami { reply } => {
                            let result = conn.client.whoami().await
                                .map_err(|e| ActorError::Rpc(e.to_string()));
                            let _ = reply.send(result);
                            continue;
                        }
                        RpcCommand::ListKernels { reply } => {
                            let result = conn.client.list_kernels().await
                                .map_err(|e| ActorError::Rpc(e.to_string()));
                            let _ = reply.send(result);
                            continue;
                        }
                        _ => {}
                    }

                    // Kernel-level commands: clone handles and dispatch concurrently.
                    // The caller's span is used as parent so rpc_client spans are
                    // children of the ActorHandle method that initiated the call.
                    let kernel = conn.kernel.clone();
                    let err_tx = err_tx.clone();
                    tokio::task::spawn_local(
                        dispatch_command(cmd, kernel, err_tx).instrument(span),
                    );
                }
            }
        }
        log::debug!("Actor shutting down: channel closed");
    }
}

/// Dispatch a single kernel-level RPC command on a cloned KernelHandle.
///
/// Runs in a spawn_local child task. On RPC error, signals the actor
/// via err_tx so it disconnects and reconnects on the next command.
/// World-level commands (Whoami, ListKernels) are handled inline in the
/// run loop and never reach this function.
async fn dispatch_command(
    cmd: RpcCommand,
    kernel: KernelHandle,
    err_tx: mpsc::UnboundedSender<()>,
) {
    match cmd {
        // ── Drift ────────────────────────────────────────────────
        RpcCommand::DriftPush { target_ctx, content, summarize, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.drift_push(target_ctx, &content, summarize));
        }
        RpcCommand::DriftFlush { reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.drift_flush());
        }
        RpcCommand::DriftQueue { reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.drift_queue());
        }
        RpcCommand::DriftCancel { staged_id, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.drift_cancel(staged_id));
        }
        RpcCommand::DriftPull { source_ctx, prompt, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.drift_pull(source_ctx, prompt.as_deref()));
        }
        RpcCommand::DriftMerge { source_ctx, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.drift_merge(source_ctx));
        }

        // ── Context ──────────────────────────────────────────────
        RpcCommand::GetDocumentId { reply } => {
            let _ = reply.send(Err(ActorError::Rpc("local command in kernel dispatch".into())));
        }
        RpcCommand::GetContextId { reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.get_context_id());
        }
        RpcCommand::ListContexts { reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.list_contexts());
        }
        RpcCommand::CreateContext { label, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.create_context(&label));
        }
        RpcCommand::AttachDocument { context_id, document_id, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.attach_document(context_id, &document_id));
        }
        RpcCommand::DetachDocument { context_id, document_id, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.detach_document(context_id, &document_id));
        }

        // ── CRDT Sync ────────────────────────────────────────────
        RpcCommand::PushOps { document_id, ops, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.push_ops(&document_id, &ops));
        }
        RpcCommand::GetDocumentState { document_id, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.get_document_state(&document_id));
        }
        RpcCommand::CompactDocument { document_id, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.compact_document(&document_id));
        }

        // ── Shell / Execution ────────────────────────────────────
        RpcCommand::Execute { code, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.execute(&code));
        }
        RpcCommand::ShellExecute { code, cell_id, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.shell_execute(&code, &cell_id));
        }
        RpcCommand::Interrupt { exec_id, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.interrupt(exec_id));
        }
        RpcCommand::Complete { partial, cursor, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.complete(&partial, cursor));
        }
        RpcCommand::GetCommandHistory { limit, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.get_command_history(limit));
        }

        // ── Shell Variables ─────────────────────────────────────
        RpcCommand::GetShellVar { name, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.get_shell_var(&name));
        }
        RpcCommand::SetShellVar { name, value, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.set_shell_var(&name, &value));
        }
        RpcCommand::ListShellVars { reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.list_shell_vars());
        }

        // ── Tool Execution ───────────────────────────────────────
        RpcCommand::ExecuteTool { tool, params, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.execute_tool(&tool, &params));
        }
        RpcCommand::CallMcpTool { server, tool, arguments, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.call_mcp_tool(&server, &tool, &arguments));
        }

        // ── MCP Resources ────────────────────────────────────────
        RpcCommand::ListMcpResources { server, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.list_mcp_resources(&server));
        }
        RpcCommand::ReadMcpResource { server, uri, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.read_mcp_resource(&server, &uri));
        }

        // ── LLM ─────────────────────────────────────────────────
        RpcCommand::Prompt { content, model, cell_id, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.prompt(&content, model.as_deref(), &cell_id));
        }
        RpcCommand::ConfigureLlm { provider, model, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.configure_llm(&provider, &model));
        }
        RpcCommand::GetLlmConfig { reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.get_llm_config());
        }
        RpcCommand::SetDefaultProvider { provider, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.set_default_provider(&provider));
        }
        RpcCommand::SetDefaultModel { provider, model, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.set_default_model(&provider, &model));
        }

        // ── Tool Filter ──────────────────────────────────────────
        RpcCommand::GetToolFilter { reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.get_tool_filter());
        }
        RpcCommand::SetToolFilter { filter, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.set_tool_filter(&filter));
        }

        // ── Timeline / Fork ──────────────────────────────────────
        RpcCommand::ForkFromVersion { document_id, version, label, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.fork_from_version(&document_id, version, &label));
        }
        RpcCommand::CherryPickBlock { block_id, target_context, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.cherry_pick_block(&block_id, target_context));
        }
        RpcCommand::GetDocumentHistory { document_id, limit, reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.get_document_history(&document_id, limit));
        }

        // ── Kernel Info ──────────────────────────────────────────
        RpcCommand::GetInfo { reply } => {
            rpc_call!(kernel, reply, err_tx, k, k.get_info());
        }

        // World-level commands are handled inline in run() — unreachable here
        RpcCommand::Whoami { reply } => {
            let _ = reply.send(Err(ActorError::Rpc("world command in kernel dispatch".into())));
        }
        RpcCommand::ListKernels { reply } => {
            let _ = reply.send(Err(ActorError::Rpc("world command in kernel dispatch".into())));
        }
    }
}

// ============================================================================
// Public spawn function
// ============================================================================

/// Spawn an RPC actor in the current `LocalSet` context.
///
/// Returns a `Send+Sync` [`ActorHandle`] that can be shared across threads.
///
/// # Backpressure
///
/// The actor uses a bounded channel (capacity 32). When all slots are occupied
/// by in-flight commands, callers naturally block on `.send().await` until
/// commands complete. This provides automatic backpressure without explicit
/// semaphores — the system slows down under load and recovers as RPCs finish.
///
/// # Subscriptions
///
/// The returned handle includes broadcast channels for server events and
/// connection status. Call [`ActorHandle::subscribe_events()`] and
/// [`ActorHandle::subscribe_status()`] to receive them. Subscriptions are
/// automatically registered on connect/reconnect.
///
/// # Safety
///
/// Must be called from within a `tokio::task::LocalSet` context because
/// Cap'n Proto RPC types (`RpcClient`, `KernelHandle`) are `!Send` and
/// must stay on the spawning thread.
pub fn spawn_actor(
    config: SshConfig,
    kernel_id: String,
    context_id: Option<ContextId>,
    instance: String,
    existing: Option<(RpcClient, KernelHandle)>,
) -> ActorHandle {
    let (tx, rx) = mpsc::channel::<ChannelCmd>(CHANNEL_CAPACITY);
    let (event_tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
    let (status_tx, _) = broadcast::channel(STATUS_BROADCAST_CAPACITY);

    let actor = RpcActor::new(
        config,
        kernel_id,
        context_id,
        instance,
        existing,
        event_tx.clone(),
        status_tx.clone(),
    );
    tokio::task::spawn_local(actor.run(rx));

    ActorHandle { tx, event_tx, status_tx }
}
