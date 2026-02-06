//! Actor-based RPC client for persistent connections.
//!
//! Provides a `Send+Sync` [`ActorHandle`] that wraps Cap'n Proto's `!Send`
//! types. The actor runs in a `spawn_local` task, processing commands
//! sequentially from an mpsc channel while maintaining a persistent SSH
//! connection with auto-reconnect.
//!
//! ```text
//!   ActorHandle (Send+Sync)     mpsc      RpcActor (spawn_local, !Send)
//!   ┌─────────────────────┐  ────────▶  ┌──────────────────────────────┐
//!   │ .drift_push()       │             │ RpcClient + KernelHandle     │
//!   │ .execute_tool()     │  ◀────────  │ auto-reconnect               │
//!   │ .push_ops()         │   oneshot   │ persistent SSH connection    │
//!   └─────────────────────┘             └──────────────────────────────┘
//! ```

use kaijutsu_crdt::BlockId;
use tokio::sync::{mpsc, oneshot};

use crate::rpc::{ContextInfo, DocumentState, StagedDriftInfo, ToolResult};
use crate::{connect_ssh, KernelHandle, RpcClient, SshConfig};

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
// Commands (internal)
// ============================================================================

/// Internal command sent from ActorHandle → RpcActor via mpsc.
enum RpcCommand {
    // Drift
    DriftPush {
        target_ctx: String,
        content: String,
        summarize: bool,
        reply: oneshot::Sender<Result<u64, ActorError>>,
    },
    DriftFlush {
        reply: oneshot::Sender<Result<u32, ActorError>>,
    },
    DriftQueue {
        reply: oneshot::Sender<Result<Vec<StagedDriftInfo>, ActorError>>,
    },
    DriftCancel {
        staged_id: u64,
        reply: oneshot::Sender<Result<bool, ActorError>>,
    },
    DriftPull {
        source_ctx: String,
        prompt: Option<String>,
        reply: oneshot::Sender<Result<BlockId, ActorError>>,
    },
    DriftMerge {
        source_ctx: String,
        reply: oneshot::Sender<Result<BlockId, ActorError>>,
    },

    // Context
    ListAllContexts {
        reply: oneshot::Sender<Result<Vec<ContextInfo>, ActorError>>,
    },
    GetContextId {
        reply: oneshot::Sender<Result<(String, String), ActorError>>,
    },

    // CRDT sync
    PushOps {
        document_id: String,
        ops: Vec<u8>,
        reply: oneshot::Sender<Result<u64, ActorError>>,
    },
    GetDocumentState {
        document_id: String,
        reply: oneshot::Sender<Result<DocumentState, ActorError>>,
    },

    // Generic tool execution
    ExecuteTool {
        tool: String,
        params: String,
        reply: oneshot::Sender<Result<ToolResult, ActorError>>,
    },
}

// ============================================================================
// ActorHandle (Send + Sync public API)
// ============================================================================

/// Send+Sync handle to an RPC actor running in a LocalSet.
///
/// Each method sends a command via mpsc and awaits the oneshot reply.
/// The handle can be cloned and shared across threads.
#[derive(Clone)]
pub struct ActorHandle {
    tx: mpsc::UnboundedSender<RpcCommand>,
}

impl ActorHandle {
    // ── Drift ────────────────────────────────────────────────────────────

    /// Stage a drift push to another context.
    pub async fn drift_push(
        &self,
        target_ctx: &str,
        content: &str,
        summarize: bool,
    ) -> Result<u64, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(RpcCommand::DriftPush {
            target_ctx: target_ctx.to_string(),
            content: content.to_string(),
            summarize,
            reply,
        }).map_err(|_| ActorError::Shutdown)?;
        rx.await.map_err(|_| ActorError::Shutdown)?
    }

    /// Flush all staged drifts.
    pub async fn drift_flush(&self) -> Result<u32, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(RpcCommand::DriftFlush { reply })
            .map_err(|_| ActorError::Shutdown)?;
        rx.await.map_err(|_| ActorError::Shutdown)?
    }

    /// View the drift staging queue.
    pub async fn drift_queue(&self) -> Result<Vec<StagedDriftInfo>, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(RpcCommand::DriftQueue { reply })
            .map_err(|_| ActorError::Shutdown)?;
        rx.await.map_err(|_| ActorError::Shutdown)?
    }

    /// Cancel a staged drift.
    pub async fn drift_cancel(&self, staged_id: u64) -> Result<bool, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(RpcCommand::DriftCancel { staged_id, reply })
            .map_err(|_| ActorError::Shutdown)?;
        rx.await.map_err(|_| ActorError::Shutdown)?
    }

    /// Pull summarized content from another context.
    pub async fn drift_pull(
        &self,
        source_ctx: &str,
        prompt: Option<&str>,
    ) -> Result<BlockId, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(RpcCommand::DriftPull {
            source_ctx: source_ctx.to_string(),
            prompt: prompt.map(String::from),
            reply,
        }).map_err(|_| ActorError::Shutdown)?;
        rx.await.map_err(|_| ActorError::Shutdown)?
    }

    /// Merge a forked context back into its parent.
    pub async fn drift_merge(&self, source_ctx: &str) -> Result<BlockId, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(RpcCommand::DriftMerge {
            source_ctx: source_ctx.to_string(),
            reply,
        }).map_err(|_| ActorError::Shutdown)?;
        rx.await.map_err(|_| ActorError::Shutdown)?
    }

    // ── Context ──────────────────────────────────────────────────────────

    /// List all registered drift contexts.
    pub async fn list_all_contexts(&self) -> Result<Vec<ContextInfo>, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(RpcCommand::ListAllContexts { reply })
            .map_err(|_| ActorError::Shutdown)?;
        rx.await.map_err(|_| ActorError::Shutdown)?
    }

    /// Get this kernel's context short ID and name.
    pub async fn get_context_id(&self) -> Result<(String, String), ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(RpcCommand::GetContextId { reply })
            .map_err(|_| ActorError::Shutdown)?;
        rx.await.map_err(|_| ActorError::Shutdown)?
    }

    // ── CRDT Sync ────────────────────────────────────────────────────────

    /// Push CRDT operations to the server.
    pub async fn push_ops(
        &self,
        document_id: &str,
        ops: &[u8],
    ) -> Result<u64, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(RpcCommand::PushOps {
            document_id: document_id.to_string(),
            ops: ops.to_vec(),
            reply,
        }).map_err(|_| ActorError::Shutdown)?;
        rx.await.map_err(|_| ActorError::Shutdown)?
    }

    /// Get full document state from the server.
    pub async fn get_document_state(
        &self,
        document_id: &str,
    ) -> Result<DocumentState, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(RpcCommand::GetDocumentState {
            document_id: document_id.to_string(),
            reply,
        }).map_err(|_| ActorError::Shutdown)?;
        rx.await.map_err(|_| ActorError::Shutdown)?
    }

    // ── Generic Tool Execution ───────────────────────────────────────────

    /// Execute a tool on the server (git, etc).
    pub async fn execute_tool(
        &self,
        tool: &str,
        params: &str,
    ) -> Result<ToolResult, ActorError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(RpcCommand::ExecuteTool {
            tool: tool.to_string(),
            params: params.to_string(),
            reply,
        }).map_err(|_| ActorError::Shutdown)?;
        rx.await.map_err(|_| ActorError::Shutdown)?
    }
}

// ============================================================================
// RpcActor (internal, !Send, runs in spawn_local)
// ============================================================================

/// The actual actor that holds !Send Cap'n Proto types.
struct RpcActor {
    config: SshConfig,
    kernel_id: String,
    context_name: String,
    instance: String,
    /// Live connection state (None = disconnected, will reconnect)
    connection: Option<ConnectionState>,
}

/// Held by the actor when connected.
struct ConnectionState {
    #[allow(dead_code)]
    client: RpcClient,
    kernel: KernelHandle,
}

/// Ensure connected, call an async RPC method, disconnect on error.
///
/// Uses a macro instead of a generic closure to avoid the classic Rust
/// "async closure captures don't outlive the future" lifetime issue.
/// The macro expands in-place so the borrow checker sees the real code flow.
macro_rules! rpc_call {
    ($self:ident, $reply:ident, $k:ident, $call:expr) => {{
        // Phase 1: connect + call (borrows self via ensure_connected → kernel ref)
        let rpc_result = match $self.ensure_connected().await {
            Ok($k) => $call.await.map_err(|e| e.to_string()),
            Err(e) => { let _ = $reply.send(Err(e)); return; }
        };
        // Phase 2: handle result (kernel ref dropped, self is free)
        let result = match rpc_result {
            Ok(val) => Ok(val),
            Err(msg) => {
                log::warn!("RPC error, will reconnect on next call: {msg}");
                $self.disconnect();
                Err(ActorError::Rpc(msg))
            }
        };
        let _ = $reply.send(result);
    }};
}

impl RpcActor {
    fn new(
        config: SshConfig,
        kernel_id: String,
        context_name: String,
        instance: String,
        existing: Option<(RpcClient, KernelHandle)>,
    ) -> Self {
        let connection = existing.map(|(client, kernel)| ConnectionState { client, kernel });
        Self {
            config,
            kernel_id,
            context_name,
            instance,
            connection,
        }
    }

    /// Ensure we have a live connection, reconnecting if needed.
    async fn ensure_connected(&mut self) -> Result<&KernelHandle, ActorError> {
        if self.connection.is_some() {
            return Ok(&self.connection.as_ref().unwrap().kernel);
        }

        log::info!(
            "Actor reconnecting to {}:{} kernel={} context={}",
            self.config.host, self.config.port, self.kernel_id, self.context_name
        );

        let client = connect_ssh(self.config.clone())
            .await
            .map_err(|e| ActorError::ConnectionLost(format!("SSH: {e}")))?;

        let kernel = client.attach_kernel(&self.kernel_id)
            .await
            .map_err(|e| ActorError::Rpc(format!("attach_kernel: {e}")))?;

        // Join context to register our presence
        let _seat = kernel.join_context(&self.context_name, &self.instance)
            .await
            .map_err(|e| ActorError::Rpc(format!("join_context: {e}")))?;

        self.connection = Some(ConnectionState { client, kernel });
        Ok(&self.connection.as_ref().unwrap().kernel)
    }

    /// Drop the connection so next call triggers reconnect.
    fn disconnect(&mut self) {
        self.connection = None;
    }

    /// Process commands until the channel closes.
    ///
    /// TODO: Commands run sequentially — a slow RPC (e.g. drift_pull with LLM)
    /// blocks all queued commands behind it. If this becomes a UX issue, refactor
    /// to spawn_local a child task per command (KernelHandle's inner capnp Client
    /// is Clone, so concurrent calls should be safe).
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<RpcCommand>) {
        while let Some(cmd) = rx.recv().await {
            self.handle_command(cmd).await;
        }
        log::debug!("Actor shutting down: channel closed");
    }

    async fn handle_command(&mut self, cmd: RpcCommand) {
        match cmd {
            // ── Drift ────────────────────────────────────────────────
            RpcCommand::DriftPush { target_ctx, content, summarize, reply } => {
                rpc_call!(self, reply, k, k.drift_push(&target_ctx, &content, summarize));
            }
            RpcCommand::DriftFlush { reply } => {
                rpc_call!(self, reply, k, k.drift_flush());
            }
            RpcCommand::DriftQueue { reply } => {
                rpc_call!(self, reply, k, k.drift_queue());
            }
            RpcCommand::DriftCancel { staged_id, reply } => {
                rpc_call!(self, reply, k, k.drift_cancel(staged_id));
            }
            RpcCommand::DriftPull { source_ctx, prompt, reply } => {
                rpc_call!(self, reply, k, k.drift_pull(&source_ctx, prompt.as_deref()));
            }
            RpcCommand::DriftMerge { source_ctx, reply } => {
                rpc_call!(self, reply, k, k.drift_merge(&source_ctx));
            }

            // ── Context ──────────────────────────────────────────────
            RpcCommand::ListAllContexts { reply } => {
                rpc_call!(self, reply, k, k.list_all_contexts());
            }
            RpcCommand::GetContextId { reply } => {
                rpc_call!(self, reply, k, k.get_context_id());
            }

            // ── CRDT Sync ────────────────────────────────────────────
            RpcCommand::PushOps { document_id, ops, reply } => {
                rpc_call!(self, reply, k, k.push_ops(&document_id, &ops));
            }
            RpcCommand::GetDocumentState { document_id, reply } => {
                rpc_call!(self, reply, k, k.get_document_state(&document_id));
            }

            // ── Tool Execution ───────────────────────────────────────
            RpcCommand::ExecuteTool { tool, params, reply } => {
                rpc_call!(self, reply, k, k.execute_tool(&tool, &params));
            }
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
/// # Safety
///
/// Must be called from within a `tokio::task::LocalSet` context because
/// Cap'n Proto RPC types (`RpcClient`, `KernelHandle`) are `!Send` and
/// must stay on the spawning thread.
///
/// # Parameters
///
/// - `config`: SSH connection parameters
/// - `kernel_id`: Kernel to attach to
/// - `context_name`: Context to join (creates if doesn't exist)
/// - `instance`: Instance identifier for the seat
/// - `existing`: Optional pre-connected client/kernel to avoid double-connect
pub fn spawn_actor(
    config: SshConfig,
    kernel_id: String,
    context_name: String,
    instance: String,
    existing: Option<(RpcClient, KernelHandle)>,
) -> ActorHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    let actor = RpcActor::new(config, kernel_id, context_name, instance, existing);
    tokio::task::spawn_local(actor.run(rx));
    ActorHandle { tx }
}
