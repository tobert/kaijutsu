//! Cap'n Proto RPC client for kaijutsu
//!
//! Provides typed interface to the World and Kernel capabilities.

use capnp_rpc::{RpcSystem, rpc_twoparty_capnp, twoparty};
use futures::AsyncReadExt;
use kaijutsu_crdt::{ContextId, KernelId};
use kaijutsu_types::{
    BlockFilter, BlockId, BlockKind, BlockQuery, BlockSnapshot, BlockSnapshotBuilder, ContentType,
    DriftKind, ErrorCategory, ErrorPayload, ErrorSeverity, ErrorSpan, PrincipalId, Role, Status,
    ToolKind,
};
use russh::ChannelStream;
use russh::client::Msg;
use tokio_util::compat::TokioAsyncReadCompatExt;

use crate::kaijutsu_capnp::world;

/// Aborts the Cap'n Proto RPC system task when the last reference is dropped.
///
/// Without this, `spawn_local(rpc_system)` runs forever — the task owns the
/// underlying SSH stream, so the server never sees a disconnect even after
/// the actor exits and drops `ConnectionState`.
#[derive(Clone)]
struct RpcSystemGuard(#[allow(dead_code)] std::rc::Rc<RpcSystemGuardInner>);

#[allow(dead_code)]
struct RpcSystemGuardInner(tokio::task::AbortHandle);

impl Drop for RpcSystemGuardInner {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// RPC client wrapper
///
/// Holds the World capability bootstrapped from the server.
///
/// IMPORTANT: Must be created and used within a `tokio::task::LocalSet` context
/// because capnp-rpc's RpcSystem is not Send.
#[derive(Clone)]
pub struct RpcClient {
    world: world::Client,
    /// Aborts the RPC system task when the last RpcClient clone is dropped.
    /// This closes the underlying stream, causing the server to detect
    /// the disconnect and stop its FlowBus bridge task.
    _rpc_guard: RpcSystemGuard,
    /// Retained SSH channels to prevent SSH_MSG_CHANNEL_CLOSE on drop.
    /// These are unused but must stay alive for the connection's lifetime.
    #[allow(dead_code)]
    retained_channels: Option<std::sync::Arc<(russh::Channel<Msg>, russh::Channel<Msg>)>>,
    /// Retained SSH session for clean disconnect and keepalive.
    /// Without this, no SSH_MSG_DISCONNECT can be sent on shutdown.
    #[allow(dead_code)]
    ssh_session: Option<std::rc::Rc<std::cell::RefCell<crate::ssh::SshClient>>>,
}

impl RpcClient {
    /// Initialize RPC over an SSH channel stream
    ///
    /// MUST be called within a `tokio::task::LocalSet::run_until()` context.
    pub async fn new(channel_stream: ChannelStream<Msg>) -> Result<Self, RpcError> {
        let compat_stream = TokioAsyncReadCompatExt::compat(channel_stream);
        Self::from_stream(compat_stream).await
    }

    /// Initialize RPC from any AsyncRead+AsyncWrite stream
    ///
    /// Useful for testing with Unix sockets or in-memory streams.
    pub async fn from_stream<S>(stream: S) -> Result<Self, RpcError>
    where
        S: futures::AsyncRead + futures::AsyncWrite + Unpin + 'static,
    {
        let (reader, writer) = stream.split();

        let rpc_network = Box::new(twoparty::VatNetwork::new(
            futures::io::BufReader::new(reader),
            futures::io::BufWriter::new(writer),
            rpc_twoparty_capnp::Side::Client,
            Default::default(),
        ));

        let mut rpc_system = RpcSystem::new(rpc_network, None);
        let world: world::Client = rpc_system.bootstrap(rpc_twoparty_capnp::Side::Server);

        // Spawn the RPC system to run in the background (requires LocalSet).
        // Store the abort handle so we can cancel it when the connection drops.
        let handle = tokio::task::spawn_local(rpc_system);
        let rpc_guard = RpcSystemGuard(std::rc::Rc::new(RpcSystemGuardInner(handle.abort_handle())));

        Ok(Self {
            world,
            _rpc_guard: rpc_guard,
            retained_channels: None,
            ssh_session: None,
        })
    }

    /// Retain SSH channels to prevent them from being dropped (which closes them).
    pub fn retain_ssh_channels(
        &mut self,
        control: russh::Channel<Msg>,
        events: russh::Channel<Msg>,
    ) {
        self.retained_channels = Some(std::sync::Arc::new((control, events)));
    }

    /// Retain the SSH session handle for clean disconnect and keepalive.
    pub fn retain_ssh_session(&mut self, ssh: crate::ssh::SshClient) {
        self.ssh_session = Some(std::rc::Rc::new(std::cell::RefCell::new(ssh)));
    }

    /// Get current identity from the server
    #[tracing::instrument(skip(self), name = "rpc_client.whoami")]
    pub async fn whoami(&self) -> Result<Identity, RpcError> {
        let request = self.world.whoami_request();
        let response = request.send().promise.await?;
        let identity = response.get()?.get_identity()?;

        Ok(Identity {
            username: identity.get_username()?.to_string()?,
            display_name: identity.get_display_name()?.to_string()?,
        })
    }

    /// List available kernels
    #[tracing::instrument(skip(self), name = "rpc_client.list_kernels")]
    pub async fn list_kernels(&self) -> Result<Vec<KernelInfo>, RpcError> {
        let request = self.world.list_kernels_request();
        let response = request.send().promise.await?;
        let kernels = response.get()?.get_kernels()?;

        let mut result = Vec::with_capacity(kernels.len() as usize);
        for kernel in kernels.iter() {
            result.push(parse_kernel_info(&kernel)?);
        }
        Ok(result)
    }

    /// Attach to the server's kernel.
    ///
    /// Returns the kernel handle and its ID (assigned by the server).
    #[tracing::instrument(skip(self), name = "rpc_client.attach_kernel")]
    pub async fn attach_kernel(&self) -> Result<(KernelHandle, KernelId), RpcError> {
        let mut request = self.world.attach_kernel_request();
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let reader = response.get()?;
        let kernel = reader.get_kernel()?;
        let kernel_id = parse_kernel_id(reader.get_kernel_id()?)?;

        Ok((KernelHandle { kernel }, kernel_id))
    }
}

// ============================================================================
// Types
// ============================================================================

#[derive(Debug, Clone)]
pub struct Identity {
    pub username: String,
    pub display_name: String,
}

#[derive(Debug, Clone)]
pub struct KernelInfo {
    pub id: KernelId,
    pub name: String,
    pub user_count: u32,
    pub agent_count: u32,
    pub contexts: Vec<ContextInfo>,
}

// ============================================================================
// Context Membership
// ============================================================================

/// Lightweight context membership — tracks what context we joined and as whom.
#[derive(Debug, Clone)]
pub struct ContextMembership {
    pub context_id: ContextId,
    pub kernel_id: KernelId,
    pub nick: String,
    pub instance: String,
}

/// Context within a kernel (rich info from ContextHandleInfo wire type)
#[derive(Debug, Clone)]
pub struct ContextInfo {
    pub id: ContextId,
    pub label: String,
    pub forked_from: Option<ContextId>,
    pub provider: String,
    pub model: String,
    pub created_at: u64,
    /// Long-running OTel trace ID for this context (16 bytes, or zeros if unavailable).
    pub trace_id: [u8; 16],
    /// How this context was forked (e.g. "full", "shallow", "compact", "subtree").
    pub fork_kind: Option<String>,
    /// Whether this context has been archived.
    pub archived: bool,
    /// Synthesis keywords (empty if not yet synthesized).
    pub keywords: Vec<String>,
    /// Preview of the most representative block (empty if none).
    pub top_block_preview: Option<String>,
}

/// Preset template info from the server.
#[derive(Debug, Clone)]
pub struct PresetInfo {
    pub id: Vec<u8>,
    pub label: String,
    pub description: String,
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone)]
pub struct KernelConfig {
    pub name: String,
    pub consent_mode: ConsentMode,
    pub mounts: Vec<MountSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConsentMode {
    #[default]
    Collaborative,
    Autonomous,
}

#[derive(Debug, Clone)]
pub struct MountSpec {
    pub path: String,
    pub source: String,
    pub writable: bool,
}

/// Handle to an attached kernel
#[derive(Clone)]
pub struct KernelHandle {
    kernel: crate::kaijutsu_capnp::kernel::Client,
}

impl KernelHandle {
    /// Get kernel info
    #[tracing::instrument(skip(self), name = "rpc_client.get_info")]
    pub async fn get_info(&self) -> Result<KernelInfo, RpcError> {
        let mut request = self.kernel.get_info_request();
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let info = response.get()?.get_info()?;
        parse_kernel_info(&info)
    }

    // =========================================================================
    // Context management
    // =========================================================================

    /// List all contexts in this kernel (includes drift info).
    #[tracing::instrument(skip(self), name = "rpc_client.list_contexts")]
    pub async fn list_contexts(&self) -> Result<Vec<ContextInfo>, RpcError> {
        let mut request = self.kernel.list_contexts_request();
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let contexts = response.get()?.get_contexts()?;

        let mut result = Vec::with_capacity(contexts.len() as usize);
        for ctx in contexts.iter() {
            result.push(parse_context_info(&ctx)?);
        }
        Ok(result)
    }

    /// Create a new context with an optional label.
    ///
    /// Returns the server-assigned ContextId.
    #[tracing::instrument(skip(self), name = "rpc_client.create_context")]
    pub async fn create_context(&self, label: &str) -> Result<ContextId, RpcError> {
        let mut request = self.kernel.create_context_request();
        request.get().set_label(label);
        let response = request.send().promise.await?;
        parse_context_id(response.get()?.get_id()?)
    }

    /// Join a context by ID.
    ///
    /// Returns the document_id for the joined context. The `instance` param
    /// identifies which client connected (for logging/debugging).
    #[tracing::instrument(skip(self), name = "rpc_client.join_context")]
    pub async fn join_context(
        &self,
        context_id: ContextId,
        instance: &str,
    ) -> Result<ContextId, RpcError> {
        let mut request = self.kernel.join_context_request();
        request.get().set_context_id(context_id.as_bytes());
        request.get().set_instance(instance);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        parse_context_id(response.get()?.get_context_id()?)
    }

    // kaish execution methods

    /// Execute code in the kernel
    #[tracing::instrument(skip(self, code), name = "rpc_client.execute")]
    pub async fn execute(&self, code: &str) -> Result<u64, RpcError> {
        let mut request = self.kernel.execute_request();
        request.get().set_code(code);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_exec_id())
    }

    /// Execute shell command with block output (kaish REPL mode)
    ///
    /// Creates ShellCommand and ShellOutput blocks in the specified cell.
    /// Output is streamed via block events.
    /// Returns the BlockId of the command block.
    #[tracing::instrument(skip(self, code), name = "rpc_client.shell_execute")]
    pub async fn shell_execute(
        &self,
        code: &str,
        context_id: ContextId,
        user_initiated: bool,
    ) -> Result<BlockId, RpcError> {
        let mut request = self.kernel.shell_execute_request();
        request.get().set_code(code);
        request.get().set_context_id(context_id.as_bytes());
        request.get().set_user_initiated(user_initiated);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let block_id = response.get()?.get_command_block_id()?;
        parse_block_id(&block_id)
    }

    /// Toggle block exclusion from conversation hydration.
    ///
    /// Excluded blocks are displayed but omitted from LLM context.
    #[tracing::instrument(skip(self), name = "rpc_client.set_block_excluded")]
    pub async fn set_block_excluded(
        &self,
        context_id: ContextId,
        block_id: &BlockId,
        excluded: bool,
    ) -> Result<u64, RpcError> {
        let mut request = self.kernel.set_block_excluded_request();
        request.get().set_context_id(context_id.as_bytes());
        set_block_id_builder(&mut request.get().init_block_id(), block_id);
        request.get().set_excluded(excluded);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_ack_version())
    }

    /// Subscribe to output events from `execute()` RPCs.
    ///
    /// Returns an unbounded receiver that yields stdout, stderr, and exit code
    /// events tagged with their exec_id. The subscription is persistent for
    /// the lifetime of the RPC connection.
    #[tracing::instrument(skip(self), name = "rpc_client.subscribe_output")]
    pub async fn subscribe_output(
        &self,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<crate::subscriptions::OutputEvent>, RpcError>
    {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let forwarder = crate::subscriptions::KernelOutputForwarder { tx };
        let callback: crate::kaijutsu_capnp::kernel_output::Client =
            capnp_rpc::new_client(forwarder);
        let mut request = self.kernel.subscribe_output_request();
        request.get().set_callback(callback);
        request.send().promise.await?;
        Ok(rx)
    }

    /// Interrupt an execution
    #[tracing::instrument(skip(self), name = "rpc_client.interrupt")]
    pub async fn interrupt(&self, exec_id: u64) -> Result<(), RpcError> {
        let mut request = self.kernel.interrupt_request();
        request.get().set_exec_id(exec_id);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        request.send().promise.await?;
        Ok(())
    }

    /// Get completions
    #[tracing::instrument(skip(self, partial), name = "rpc_client.complete")]
    pub async fn complete(&self, partial: &str, cursor: u32) -> Result<Vec<Completion>, RpcError> {
        let mut request = self.kernel.complete_request();
        request.get().set_partial(partial);
        request.get().set_cursor(cursor);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let completions = response.get()?.get_completions()?;

        let mut result = Vec::with_capacity(completions.len() as usize);
        for c in completions.iter() {
            result.push(Completion {
                text: c.get_text()?.to_string()?,
                display_text: c.get_display_text()?.to_string()?,
                kind: CompletionKind::from_capnp(c.get_kind()?),
            });
        }
        Ok(result)
    }

    /// Get command history
    #[tracing::instrument(skip(self), name = "rpc_client.get_command_history")]
    pub async fn get_command_history(&self, limit: u32) -> Result<Vec<HistoryEntry>, RpcError> {
        let mut request = self.kernel.get_command_history_request();
        request.get().set_limit(limit);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let entries = response.get()?.get_entries()?;

        let mut result = Vec::with_capacity(entries.len() as usize);
        for e in entries.iter() {
            result.push(HistoryEntry {
                id: e.get_id(),
                code: e.get_code()?.to_string()?,
                timestamp: e.get_timestamp(),
            });
        }
        Ok(result)
    }

    /// Detach from the kernel
    #[tracing::instrument(skip(self), name = "rpc_client.detach")]
    pub async fn detach(self) -> Result<(), RpcError> {
        let request = self.kernel.detach_agent_request();
        request.send().promise.await?;
        Ok(())
    }

    // =========================================================================
    // Block-based CRDT sync methods
    // =========================================================================

    // Blocks sync via pushOps (SerializedOps). The former apply_block_op RPC
    // was removed on both client and server; its wire slot (@12) is retained
    // as a no-op placeholder. See BlockDocument::ops_since() and apply_ops().

    /// Push CRDT operations to the server for bidirectional sync.
    ///
    /// Returns the acknowledged version so the client knows ops were accepted.
    /// The ops should be serialized using serde_json from SerializedOpsOwned.
    #[tracing::instrument(skip(self, ops), name = "rpc_client.push_ops")]
    pub async fn push_ops(&self, context_id: ContextId, ops: &[u8]) -> Result<u64, RpcError> {
        let mut request = self.kernel.push_ops_request();
        request.get().set_context_id(context_id.as_bytes());
        request.get().set_ops(ops);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_ack_version())
    }

    /// Get document state (blocks and CRDT oplog)
    /// Compact a document's oplog, returning new size and sync generation.
    #[tracing::instrument(skip(self), name = "rpc_client.compact_context")]
    pub async fn compact_context(&self, context_id: ContextId) -> Result<(u64, u64), RpcError> {
        let mut request = self.kernel.compact_context_request();
        request.get().set_context_id(context_id.as_bytes());
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let r = response.get()?;
        Ok((r.get_new_size(), r.get_generation()))
    }

    // =========================================================================
    // Block Queries (getBlocks / getContextSync)
    // =========================================================================

    /// Fetch blocks by query (all, byIds, or byFilter).
    #[tracing::instrument(skip(self), name = "rpc_client.get_blocks")]
    pub async fn get_blocks(
        &self,
        context_id: ContextId,
        query: &BlockQuery,
    ) -> Result<Vec<BlockSnapshot>, RpcError> {
        let mut request = self.kernel.get_blocks_request();
        request.get().set_context_id(context_id.as_bytes());
        set_block_query_builder(request.get().init_query(), query);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let blocks_reader = response.get()?.get_blocks()?;
        let mut blocks = Vec::with_capacity(blocks_reader.len() as usize);
        for block in blocks_reader.iter() {
            blocks.push(parse_block_snapshot(&block)?);
        }
        Ok(blocks)
    }

    /// Fetch CRDT sync state (ops + version) without blocks.
    #[tracing::instrument(skip(self), name = "rpc_client.get_context_sync")]
    pub async fn get_context_sync(&self, context_id: ContextId) -> Result<SyncState, RpcError> {
        let mut request = self.kernel.get_context_sync_request();
        request.get().set_context_id(context_id.as_bytes());
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let r = response.get()?;
        let context_id = parse_context_id(r.get_context_id()?)?;
        let ops = r.get_ops().map(|d| d.to_vec()).unwrap_or_default();
        let version = r.get_version();
        Ok(SyncState {
            context_id,
            ops,
            version,
        })
    }

    // =========================================================================
    // LLM operations
    // =========================================================================

    /// Send a prompt to the server-side LLM
    ///
    /// Returns a prompt ID that can be used to track the response.
    /// The response will be streamed via block events if subscribed.
    #[tracing::instrument(skip(self, content), name = "rpc_client.prompt")]
    pub async fn prompt(
        &self,
        content: &str,
        model: Option<&str>,
        context_id: ContextId,
    ) -> Result<String, RpcError> {
        let mut request = self.kernel.prompt_request();
        {
            let mut req = request.get().init_request();
            req.set_content(content);
            if let Some(m) = model {
                req.set_model(m);
            }
            req.set_context_id(context_id.as_bytes());
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_prompt_id()?.to_string()?)
    }

    /// Subscribe to block events (for LLM streaming updates)
    ///
    /// The callback will receive block insertions, edits, and other events.
    #[tracing::instrument(skip(self, callback), name = "rpc_client.subscribe_blocks")]
    pub async fn subscribe_blocks(
        &self,
        callback: crate::kaijutsu_capnp::block_events::Client,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.subscribe_blocks_request();
        request.get().set_callback(callback);
        request.send().promise.await?;
        Ok(())
    }

    /// Subscribe to block events with server-side filtering.
    ///
    /// Like `subscribe_blocks` but the server applies the filter before sending,
    /// reducing bandwidth and client CPU during high-throughput streaming.
    #[tracing::instrument(
        skip(self, callback, filter),
        name = "rpc_client.subscribe_blocks_filtered"
    )]
    pub async fn subscribe_blocks_filtered(
        &self,
        callback: crate::kaijutsu_capnp::block_events::Client,
        filter: &kaijutsu_types::BlockEventFilter,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.subscribe_blocks_filtered_request();
        {
            let mut params = request.get();
            params.set_callback(callback);
            let mut fb = params.init_filter();
            set_block_event_filter_builder(&mut fb, filter);
        }
        request.send().promise.await?;
        Ok(())
    }

    // =========================================================================
    // MCP Tool operations
    // =========================================================================

    /// Execute a tool via the kernel's tool registry.
    ///
    /// This is the general-purpose tool execution path (executeTool @16).
    /// Tools include git, drift, and any registered execution engines.
    #[tracing::instrument(skip(self, params), name = "rpc_client.execute_tool")]
    pub async fn execute_tool(&self, tool: &str, params: &str) -> Result<ToolResult, RpcError> {
        let mut request = self.kernel.execute_tool_request();
        {
            let mut call = request.get().init_call();
            call.set_tool(tool);
            call.set_params(params);
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let result = response.get()?.get_result()?;

        Ok(ToolResult {
            request_id: result.get_request_id()?.to_string()?,
            success: result.get_success(),
            output: result.get_output()?.to_string()?,
        })
    }

    /// Get schemas for all registered kernel tools.
    #[tracing::instrument(skip(self), name = "rpc_client.get_tool_schemas")]
    pub async fn get_tool_schemas(&self) -> Result<Vec<ToolSchema>, RpcError> {
        let mut request = self.kernel.get_tool_schemas_request();
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let schemas = response.get()?.get_schemas()?;

        let mut result = Vec::with_capacity(schemas.len() as usize);
        for i in 0..schemas.len() {
            let s = schemas.get(i);
            result.push(ToolSchema {
                name: s.get_name()?.to_string()?,
                description: s.get_description()?.to_string()?,
                category: s.get_category()?.to_string()?,
                input_schema: s.get_input_schema()?.to_string()?,
            });
        }
        Ok(result)
    }

    /// Call an MCP tool
    ///
    /// Invokes a tool on a registered MCP server and returns the result.
    #[tracing::instrument(skip(self, arguments), name = "rpc_client.call_mcp_tool")]
    pub async fn call_mcp_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: &serde_json::Value,
    ) -> Result<McpToolResult, RpcError> {
        let mut request = self.kernel.call_mcp_tool_request();
        {
            let mut call = request.get().init_call();
            call.set_server(server);
            call.set_tool(tool);
            call.set_arguments(
                &serde_json::to_string(arguments).map_err(|e| {
                    RpcError::Other(format!("Failed to serialize MCP arguments: {e}"))
                })?,
            );
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let result = response.get()?.get_result()?;

        Ok(McpToolResult {
            content: result.get_content()?.to_string()?,
            is_error: result.get_is_error(),
        })
    }

    // =========================================================================
    // MCP Resource operations (push-first with caching)
    // =========================================================================

    /// List resources from an MCP server
    ///
    /// Returns a list of resources available from the specified server.
    /// Results may be cached on the server for efficiency.
    #[tracing::instrument(skip(self), name = "rpc_client.list_mcp_resources")]
    pub async fn list_mcp_resources(&self, server: &str) -> Result<Vec<McpResource>, RpcError> {
        let mut request = self.kernel.list_mcp_resources_request();
        request.get().set_server(server);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let resources = response.get()?.get_resources()?;

        let mut result = Vec::with_capacity(resources.len() as usize);
        for r in resources.iter() {
            result.push(McpResource {
                uri: r.get_uri()?.to_string()?,
                name: r.get_name()?.to_string()?,
                description: if r.get_has_description() {
                    Some(r.get_description()?.to_string()?)
                } else {
                    None
                },
                mime_type: if r.get_has_mime_type() {
                    Some(r.get_mime_type()?.to_string()?)
                } else {
                    None
                },
            });
        }
        Ok(result)
    }

    /// Read a resource from an MCP server
    ///
    /// Returns the contents of the specified resource.
    /// Results may be cached on the server for efficiency.
    #[tracing::instrument(skip(self), name = "rpc_client.read_mcp_resource")]
    pub async fn read_mcp_resource(
        &self,
        server: &str,
        uri: &str,
    ) -> Result<Option<McpResourceContents>, RpcError> {
        let mut request = self.kernel.read_mcp_resource_request();
        request.get().set_server(server);
        request.get().set_uri(uri);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let result = response.get()?;

        if !result.get_has_contents() {
            return Ok(None);
        }

        let contents = result.get_contents()?;
        let uri = contents.get_uri()?.to_string()?;
        let mime_type = if contents.get_has_mime_type() {
            Some(contents.get_mime_type()?.to_string()?)
        } else {
            None
        };

        // Check which union variant is set
        use crate::kaijutsu_capnp::mcp_resource_contents::Which;
        match contents.which()? {
            Which::Text(text) => Ok(Some(McpResourceContents::Text {
                uri,
                mime_type,
                text: text?.to_string()?,
            })),
            Which::Blob(blob) => Ok(Some(McpResourceContents::Blob {
                uri,
                mime_type,
                blob: blob?.to_vec(),
            })),
        }
    }

    /// Subscribe to MCP resource events
    ///
    /// The callback will receive notifications when resources are updated
    /// or when a server's resource list changes.
    #[tracing::instrument(skip(self, callback), name = "rpc_client.subscribe_mcp_resources")]
    pub async fn subscribe_mcp_resources(
        &self,
        callback: crate::kaijutsu_capnp::resource_events::Client,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.subscribe_mcp_resources_request();
        request.get().set_callback(callback);
        request.send().promise.await?;
        Ok(())
    }

    /// Subscribe to MCP elicitation requests
    ///
    /// The callback will receive elicitation requests from MCP servers that
    /// require user input. The callback must return a response for each request.
    ///
    /// Elicitation is a server-initiated pattern where MCP servers can request
    /// confirmation or input from the user for operations that require consent.
    #[tracing::instrument(skip(self, callback), name = "rpc_client.subscribe_mcp_elicitations")]
    pub async fn subscribe_mcp_elicitations(
        &self,
        callback: crate::kaijutsu_capnp::elicitation_events::Client,
    ) -> Result<(), RpcError> {
        let mut request = self.kernel.subscribe_mcp_elicitations_request();
        request.get().set_callback(callback);
        request.send().promise.await?;
        Ok(())
    }

    // =========================================================================
    // Timeline / Fork operations
    // =========================================================================

    /// Cherry-pick a block from one context into another.
    ///
    /// Returns the new block ID in the target context.
    #[tracing::instrument(skip(self), name = "rpc_client.cherry_pick_block")]
    pub async fn cherry_pick_block(
        &self,
        block_id: &BlockId,
        target_context: ContextId,
    ) -> Result<BlockId, RpcError> {
        let mut request = self.kernel.cherry_pick_block_request();
        {
            let mut params = request.get();
            let mut source = params.reborrow().init_source_block_id();
            source.set_context_id(block_id.context_id.as_bytes());
            source.set_agent_id(block_id.agent_id.as_bytes());
            source.set_seq(block_id.seq);
            params.set_target_context_id(target_context.as_bytes());
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let new_block = response.get()?.get_new_block_id()?;
        parse_block_id(&new_block)
    }

    /// Get document history (version snapshots).
    ///
    /// Returns a list of version snapshots for timeline navigation.
    #[tracing::instrument(skip(self), name = "rpc_client.get_context_history")]
    pub async fn get_context_history(
        &self,
        context_id: ContextId,
        limit: u32,
    ) -> Result<Vec<VersionSnapshot>, RpcError> {
        let mut request = self.kernel.get_context_history_request();
        {
            let mut params = request.get();
            params.set_context_id(context_id.as_bytes());
            params.set_limit(limit);
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let snapshots = response.get()?.get_snapshots()?;

        let mut result = Vec::with_capacity(snapshots.len() as usize);
        for snap in snapshots.iter() {
            result.push(VersionSnapshot {
                version: snap.get_version(),
                timestamp: snap.get_timestamp(),
                block_count: snap.get_block_count(),
                change_kind: match snap.get_change_kind()? {
                    crate::kaijutsu_capnp::ChangeKind::BlockAdded => "block_added".to_string(),
                    crate::kaijutsu_capnp::ChangeKind::BlockDeleted => "block_deleted".to_string(),
                    crate::kaijutsu_capnp::ChangeKind::Edit => "edit".to_string(),
                    crate::kaijutsu_capnp::ChangeKind::StatusChange => "status_change".to_string(),
                },
            });
        }
        Ok(result)
    }

    // ========================================================================
    // Drift: Cross-Context Communication
    // ========================================================================

    /// Get this kernel's context ID and label.
    #[tracing::instrument(skip(self), name = "rpc_client.get_context_id")]
    pub async fn get_context_id(&self) -> Result<(ContextId, String), RpcError> {
        let mut request = self.kernel.get_context_id_request();
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let reader = response.get()?;
        let id = parse_context_id(reader.get_id()?)?;
        let label = reader.get_label()?.to_string()?;
        Ok((id, label))
    }

    /// Set the LLM provider and model for a specific context.
    #[tracing::instrument(skip(self), name = "rpc_client.set_context_model")]
    pub async fn set_context_model(
        &self,
        context_id: ContextId,
        provider: &str,
        model: &str,
    ) -> Result<bool, RpcError> {
        let mut request = self.kernel.configure_llm_request();
        {
            let mut params = request.get();
            params.set_provider(provider);
            params.set_model(model);
            params.set_context_id(context_id.as_bytes());
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let reader = response.get()?;
        if reader.get_success() {
            Ok(true)
        } else {
            let error = reader.get_error()?.to_string()?;
            Err(RpcError::ServerError(error))
        }
    }

    /// View the drift staging queue.
    #[tracing::instrument(skip(self), name = "rpc_client.drift_queue")]
    pub async fn drift_queue(&self) -> Result<Vec<StagedDriftInfo>, RpcError> {
        let request = self.kernel.drift_queue_request();
        let response = request.send().promise.await?;
        let staged = response.get()?.get_staged()?;

        let mut result = Vec::with_capacity(staged.len() as usize);
        for entry in staged.iter() {
            let dk = match entry.get_drift_kind()? {
                crate::kaijutsu_capnp::DriftKind::Push => DriftKind::Push,
                crate::kaijutsu_capnp::DriftKind::Pull => DriftKind::Pull,
                crate::kaijutsu_capnp::DriftKind::Merge => DriftKind::Merge,
                crate::kaijutsu_capnp::DriftKind::Distill => DriftKind::Distill,
                crate::kaijutsu_capnp::DriftKind::Commit => DriftKind::Commit,
                crate::kaijutsu_capnp::DriftKind::Notification => DriftKind::Notification,
                crate::kaijutsu_capnp::DriftKind::Fork => DriftKind::Fork,
            };
            result.push(StagedDriftInfo {
                id: entry.get_id(),
                source_ctx: parse_context_id(entry.get_source_ctx()?)?,
                target_ctx: parse_context_id(entry.get_target_ctx()?)?,
                content: entry.get_content()?.to_string()?,
                source_model: entry.get_source_model()?.to_string()?,
                drift_kind: dk,
                created_at: entry.get_created_at(),
            });
        }
        Ok(result)
    }

    /// Cancel a staged drift.
    #[tracing::instrument(skip(self), name = "rpc_client.drift_cancel")]
    pub async fn drift_cancel(&self, staged_id: u64) -> Result<bool, RpcError> {
        let mut request = self.kernel.drift_cancel_request();
        request.get().set_staged_id(staged_id);
        let response = request.send().promise.await?;
        Ok(response.get()?.get_success())
    }

    /// Rename a context's human-friendly label.
    #[tracing::instrument(skip(self), name = "rpc_client.rename_context")]
    pub async fn rename_context(&self, context_id: ContextId, label: &str) -> Result<(), RpcError> {
        let mut request = self.kernel.rename_context_request();
        request.get().set_context_id(context_id.as_bytes());
        request.get().set_label(label);
        request.send().promise.await?;
        Ok(())
    }

    // ========================================================================
    // LLM Configuration
    // ========================================================================

    /// Get current LLM configuration
    #[tracing::instrument(skip(self), name = "rpc_client.get_llm_config")]
    pub async fn get_llm_config(&self) -> Result<LlmConfigInfo, RpcError> {
        let mut request = self.kernel.get_llm_config_request();
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let config = response.get()?.get_config()?;

        let providers_reader = config.get_providers()?;
        let mut providers = Vec::with_capacity(providers_reader.len() as usize);
        for p in providers_reader.iter() {
            let models: Vec<String> = if p.has_models() {
                p.get_models()?
                    .iter()
                    .filter_map(|m| m.ok().and_then(|s| s.to_string().ok()))
                    .collect()
            } else {
                Vec::new()
            };
            providers.push(LlmProviderInfo {
                name: p.get_name()?.to_string()?,
                default_model: p.get_default_model()?.to_string()?,
                available: p.get_available(),
                models,
            });
        }

        Ok(LlmConfigInfo {
            default_provider: config.get_default_provider()?.to_string()?,
            default_model: config.get_default_model()?.to_string()?,
            providers,
        })
    }

    /// Set the default LLM provider
    #[tracing::instrument(skip(self), name = "rpc_client.set_default_provider")]
    pub async fn set_default_provider(&self, provider: &str) -> Result<bool, RpcError> {
        let mut request = self.kernel.set_default_provider_request();
        request.get().set_provider(provider);
        let response = request.send().promise.await?;
        let result = response.get()?;
        if !result.get_success() {
            let error = result.get_error()?.to_str()?;
            if !error.is_empty() {
                return Err(RpcError::ServerError(error.to_string()));
            }
        }
        Ok(result.get_success())
    }

    /// Set the default model for a provider
    #[tracing::instrument(skip(self), name = "rpc_client.set_default_model")]
    pub async fn set_default_model(&self, provider: &str, model: &str) -> Result<bool, RpcError> {
        let mut request = self.kernel.set_default_model_request();
        request.get().set_provider(provider);
        request.get().set_model(model);
        let response = request.send().promise.await?;
        let result = response.get()?;
        if !result.get_success() {
            let error = result.get_error()?.to_str()?;
            if !error.is_empty() {
                return Err(RpcError::ServerError(error.to_string()));
            }
        }
        Ok(result.get_success())
    }

    // ========================================================================
    // Tool Filter Configuration
    // ========================================================================

    /// Get current tool filter configuration
    #[tracing::instrument(skip(self), name = "rpc_client.get_tool_filter")]
    pub async fn get_tool_filter(&self) -> Result<ClientToolFilter, RpcError> {
        let request = self.kernel.get_tool_filter_request();
        let response = request.send().promise.await?;
        let filter = response.get()?.get_filter()?;

        use crate::kaijutsu_capnp::tool_filter_config;
        match filter.which()? {
            tool_filter_config::All(()) => Ok(ClientToolFilter::All),
            tool_filter_config::AllowList(list) => {
                let list = list?;
                let mut tools = Vec::with_capacity(list.len() as usize);
                for i in 0..list.len() {
                    tools.push(list.get(i)?.to_str()?.to_string());
                }
                Ok(ClientToolFilter::AllowList(tools))
            }
            tool_filter_config::DenyList(list) => {
                let list = list?;
                let mut tools = Vec::with_capacity(list.len() as usize);
                for i in 0..list.len() {
                    tools.push(list.get(i)?.to_str()?.to_string());
                }
                Ok(ClientToolFilter::DenyList(tools))
            }
        }
    }

    /// Set tool filter configuration
    #[tracing::instrument(skip(self, filter), name = "rpc_client.set_tool_filter")]
    pub async fn set_tool_filter(&self, filter: &ClientToolFilter) -> Result<bool, RpcError> {
        let mut request = self.kernel.set_tool_filter_request();
        {
            let mut filter_builder = request.get().init_filter();
            match filter {
                ClientToolFilter::All => {
                    filter_builder.set_all(());
                }
                ClientToolFilter::AllowList(tools) => {
                    let mut list = filter_builder.init_allow_list(tools.len() as u32);
                    for (i, tool) in tools.iter().enumerate() {
                        list.set(i as u32, tool);
                    }
                }
                ClientToolFilter::DenyList(tools) => {
                    let mut list = filter_builder.init_deny_list(tools.len() as u32);
                    for (i, tool) in tools.iter().enumerate() {
                        list.set(i as u32, tool);
                    }
                }
            }
        }
        let response = request.send().promise.await?;
        let result = response.get()?;
        if !result.get_success() {
            let error = result.get_error()?.to_str()?;
            if !error.is_empty() {
                return Err(RpcError::ServerError(error.to_string()));
            }
        }
        Ok(result.get_success())
    }

    // =========================================================================
    // Per-Context Tool Filter
    // =========================================================================

    /// Set tool filter for a specific context.
    #[tracing::instrument(skip(self, filter), name = "rpc_client.set_context_tool_filter")]
    pub async fn set_context_tool_filter(
        &self,
        context_id: ContextId,
        filter: &ClientToolFilter,
    ) -> Result<bool, RpcError> {
        let mut request = self.kernel.set_context_tool_filter_request();
        {
            let mut params = request.get();
            params.set_context_id(context_id.as_bytes());
            let mut filter_builder = params.init_filter();
            match filter {
                ClientToolFilter::All => {
                    filter_builder.set_all(());
                }
                ClientToolFilter::AllowList(tools) => {
                    let mut list = filter_builder.init_allow_list(tools.len() as u32);
                    for (i, tool) in tools.iter().enumerate() {
                        list.set(i as u32, tool);
                    }
                }
                ClientToolFilter::DenyList(tools) => {
                    let mut list = filter_builder.init_deny_list(tools.len() as u32);
                    for (i, tool) in tools.iter().enumerate() {
                        list.set(i as u32, tool);
                    }
                }
            }
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let result = response.get()?;
        if !result.get_success() {
            let error = result.get_error()?.to_str()?;
            if !error.is_empty() {
                return Err(RpcError::ServerError(error.to_string()));
            }
        }
        Ok(result.get_success())
    }

    /// Get tool filter for a specific context.
    #[tracing::instrument(skip(self), name = "rpc_client.get_context_tool_filter")]
    pub async fn get_context_tool_filter(
        &self,
        context_id: ContextId,
    ) -> Result<Option<ClientToolFilter>, RpcError> {
        let mut request = self.kernel.get_context_tool_filter_request();
        {
            let mut params = request.get();
            params.set_context_id(context_id.as_bytes());
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let result = response.get()?;

        if !result.get_has_filter() {
            return Ok(None);
        }

        let filter = result.get_filter()?;
        use crate::kaijutsu_capnp::tool_filter_config;
        match filter.which()? {
            tool_filter_config::All(()) => Ok(Some(ClientToolFilter::All)),
            tool_filter_config::AllowList(list) => {
                let list = list?;
                let mut tools = Vec::with_capacity(list.len() as usize);
                for i in 0..list.len() {
                    tools.push(list.get(i)?.to_str()?.to_string());
                }
                Ok(Some(ClientToolFilter::AllowList(tools)))
            }
            tool_filter_config::DenyList(list) => {
                let list = list?;
                let mut tools = Vec::with_capacity(list.len() as usize);
                for i in 0..list.len() {
                    tools.push(list.get(i)?.to_str()?.to_string());
                }
                Ok(Some(ClientToolFilter::DenyList(tools)))
            }
        }
    }

    // =========================================================================
    // Context Interrupt
    // =========================================================================

    /// Interrupt a running LLM stream or shell jobs for a context.
    ///
    /// `immediate=false` → soft interrupt (stop after current tool turn).
    /// `immediate=true`  → hard interrupt (abort stream + kill kaish jobs).
    /// Returns `false` when the context has no active stream (no-op).
    #[tracing::instrument(skip(self), name = "rpc_client.interrupt_context")]
    pub async fn interrupt_context(
        &self,
        context_id: ContextId,
        immediate: bool,
    ) -> Result<bool, RpcError> {
        let mut request = self.kernel.interrupt_context_request();
        {
            let mut params = request.get();
            params.set_context_id(context_id.as_bytes());
            params.set_immediate(immediate);
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_success())
    }

    /// List all presets for this kernel.
    pub async fn list_presets(&self) -> Result<Vec<PresetInfo>, RpcError> {
        let mut request = self.kernel.list_presets_request();
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let presets = response.get()?.get_presets()?;

        let mut result = Vec::with_capacity(presets.len() as usize);
        for p in presets.iter() {
            result.push(PresetInfo {
                id: p.get_id()?.to_vec(),
                label: p.get_label()?.to_string()?,
                description: p.get_description()?.to_string()?,
                provider: p.get_provider()?.to_string()?,
                model: p.get_model()?.to_string()?,
            });
        }
        Ok(result)
    }

    // =========================================================================
    // Agent Invocation
    // =========================================================================

    /// Attach as an agent with a commands callback.
    ///
    /// Creates an `AgentCommands` capnp server backed by an mpsc channel.
    /// Returns the channel receiver so the caller can process invocations.
    /// Attach as an agent with a commands callback.
    ///
    /// The `invocation_tx` sender receives incoming invocations from the
    /// kernel. Use `std::sync::mpsc` so the receiver can be polled from
    /// any executor (including Bevy's non-tokio task pool).
    #[tracing::instrument(skip(self, config, invocation_tx), name = "rpc_client.attach_agent")]
    pub async fn attach_agent(
        &self,
        config: &crate::actor::AgentConfig,
        invocation_tx: std::sync::mpsc::Sender<crate::actor::AgentInvocation>,
    ) -> Result<crate::actor::AgentAttachResult, RpcError> {
        use crate::kaijutsu_capnp::AgentCapability as CapEnum;

        // Create capnp server for the callback
        let commands_impl = AgentCommandsImpl { tx: invocation_tx };
        let commands_client: crate::kaijutsu_capnp::agent_commands::Client =
            capnp_rpc::new_client(commands_impl);

        let mut request = self.kernel.attach_agent_request();
        {
            let mut cfg = request.get().init_config();
            cfg.set_nick(&config.nick);
            cfg.set_instance(&config.instance);
            cfg.set_provider(&config.provider);
            cfg.set_model_id(&config.model_id);

            let mut caps = cfg.reborrow().init_capabilities(config.capabilities.len() as u32);
            for (i, c) in config.capabilities.iter().enumerate() {
                let cap = match c.as_str() {
                    "spell_check" => CapEnum::SpellCheck,
                    "grammar" => CapEnum::Grammar,
                    "format" => CapEnum::Format,
                    "review" => CapEnum::Review,
                    "generate" => CapEnum::Generate,
                    "refactor" => CapEnum::Refactor,
                    "explain" => CapEnum::Explain,
                    "translate" => CapEnum::Translate,
                    "summarize" => CapEnum::Summarize,
                    _ => CapEnum::Custom,
                };
                caps.set(i as u32, cap);
            }

            request.get().set_commands(commands_client);
        }

        let response = request.send().promise.await?;
        let info = response.get()?.get_info()?;
        let result = crate::actor::AgentAttachResult {
            nick: info.get_nick()?.to_string()?,
            instance: info.get_instance()?.to_string()?,
        };

        Ok(result)
    }

    /// Invoke another agent through the kernel.
    #[tracing::instrument(skip(self, params), name = "rpc_client.invoke_agent")]
    pub async fn invoke_agent(
        &self,
        nick: &str,
        action: &str,
        params: &[u8],
    ) -> Result<Vec<u8>, RpcError> {
        let mut request = self.kernel.invoke_agent_request();
        {
            let mut p = request.get();
            p.set_nick(nick);
            p.set_action(action);
            p.set_params(params);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_result()?.to_vec())
    }

    // =========================================================================
    // Shell Variable Introspection
    // =========================================================================

    /// Get a shell variable by name.
    #[tracing::instrument(skip(self), name = "rpc_client.get_shell_var")]
    pub async fn get_shell_var(&self, name: &str) -> Result<(Option<ShellValue>, bool), RpcError> {
        let mut request = self.kernel.get_shell_var_request();
        request.get().set_name(name);
        let response = request.send().promise.await?;
        let result = response.get()?;
        let found = result.get_found();
        if found {
            let value = read_shell_value(result.get_value()?)?;
            Ok((Some(value), true))
        } else {
            Ok((None, false))
        }
    }

    /// Set a shell variable.
    #[tracing::instrument(skip(self, value), name = "rpc_client.set_shell_var")]
    pub async fn set_shell_var(&self, name: &str, value: &ShellValue) -> Result<(), RpcError> {
        let mut request = self.kernel.set_shell_var_request();
        request.get().set_name(name);
        write_shell_value(request.get().init_value(), value);
        let response = request.send().promise.await?;
        let result = response.get()?;
        if !result.get_success() {
            let error = result.get_error()?.to_str()?;
            if !error.is_empty() {
                return Err(RpcError::ServerError(error.to_string()));
            }
        }
        Ok(())
    }

    /// List all shell variables with their values.
    #[tracing::instrument(skip(self), name = "rpc_client.list_shell_vars")]
    pub async fn list_shell_vars(&self) -> Result<Vec<(String, ShellValue)>, RpcError> {
        let request = self.kernel.list_shell_vars_request();
        let response = request.send().promise.await?;
        let vars = response.get()?.get_vars()?;

        let mut result = Vec::with_capacity(vars.len() as usize);
        for var in vars.iter() {
            let name = var.get_name()?.to_string()?;
            let value = read_shell_value(var.get_value()?)?;
            result.push((name, value));
        }
        Ok(result)
    }

    // =========================================================================
    // Input Document (CRDT compose scratchpad)
    // =========================================================================

    /// High-level edit on the input document: insert text at position, delete characters.
    ///
    /// Returns the acknowledged version.
    #[tracing::instrument(skip(self, insert), name = "rpc_client.edit_input")]
    pub async fn edit_input(
        &self,
        context_id: ContextId,
        pos: u64,
        insert: &str,
        delete: u64,
    ) -> Result<u64, RpcError> {
        let mut request = self.kernel.edit_input_request();
        request.get().set_context_id(context_id.as_bytes());
        request.get().set_pos(pos);
        request.get().set_insert(insert);
        request.get().set_delete(delete);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_ack_version())
    }

    /// Get the full input document state for a context.
    ///
    /// Returns the current content, CRDT oplog, and version.
    #[tracing::instrument(skip(self), name = "rpc_client.get_input_state")]
    pub async fn get_input_state(&self, context_id: ContextId) -> Result<InputState, RpcError> {
        let mut request = self.kernel.get_input_state_request();
        request.get().set_context_id(context_id.as_bytes());
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let result = response.get()?;
        Ok(InputState {
            content: result.get_content()?.to_string()?,
            ops: result.get_ops().map(|d| d.to_vec()).unwrap_or_default(),
            version: result.get_version(),
        })
    }

    /// Push raw CRDT operations to the input document.
    ///
    /// For CRDT-aware clients that maintain their own DTE document.
    /// Returns the acknowledged version.
    #[tracing::instrument(skip(self, ops), name = "rpc_client.push_input_ops")]
    pub async fn push_input_ops(&self, context_id: ContextId, ops: &[u8]) -> Result<u64, RpcError> {
        let mut request = self.kernel.push_input_ops_request();
        request.get().set_context_id(context_id.as_bytes());
        request.get().set_ops(ops);
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_ack_version())
    }

    /// Submit the input document: snapshot to conversation block and clear.
    ///
    /// `is_shell` selects the routing mode (shell command vs chat prompt).
    #[tracing::instrument(skip(self), name = "rpc_client.submit_input")]
    pub async fn submit_input(
        &self,
        context_id: ContextId,
        is_shell: bool,
    ) -> Result<SubmitResult, RpcError> {
        let mut request = self.kernel.submit_input_request();
        request.get().set_context_id(context_id.as_bytes());
        request.get().set_mode(if is_shell {
            crate::kaijutsu_capnp::InputMode::Shell
        } else {
            crate::kaijutsu_capnp::InputMode::Chat
        });
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let result = response.get()?;
        let block_id = parse_block_id(&result.get_command_block_id()?)?;
        Ok(SubmitResult { block_id })
    }

    /// Clear the input document for a context (discard draft).
    ///
    /// The server clears the CRDT input doc and emits `InputCleared` to all
    /// subscribers. Use this for Escape×3 (discard draft) — `submit_input`
    /// already clears internally.
    #[tracing::instrument(skip(self), name = "rpc_client.clear_input")]
    pub async fn clear_input(&self, context_id: ContextId) -> Result<(), RpcError> {
        let mut request = self.kernel.clear_input_request();
        request.get().set_context_id(context_id.as_bytes());
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        request.send().promise.await?;
        Ok(())
    }
}

/// Read a `ShellValue` from a Cap'n Proto reader.
fn read_shell_value(
    reader: crate::kaijutsu_capnp::shell_value::Reader<'_>,
) -> Result<ShellValue, RpcError> {
    use crate::kaijutsu_capnp::shell_value;
    match reader.which().map_err(|e| RpcError::Capnp(e.into()))? {
        shell_value::Null(()) => Ok(ShellValue::Null),
        shell_value::Bool(b) => Ok(ShellValue::Bool(b)),
        shell_value::Int(i) => Ok(ShellValue::Int(i)),
        shell_value::Float(f) => Ok(ShellValue::Float(f)),
        shell_value::String(s) => Ok(ShellValue::String(s?.to_string()?)),
        shell_value::Json(j) => {
            let json_str = j?.to_str()?;
            let parsed: serde_json::Value = serde_json::from_str(json_str)
                .map_err(|e| RpcError::ServerError(format!("invalid JSON: {}", e)))?;
            Ok(ShellValue::Json(parsed))
        }
        shell_value::Blob(b) => Ok(ShellValue::Blob(b?.to_string()?)),
    }
}

/// Write a `ShellValue` into a Cap'n Proto builder.
fn write_shell_value(
    mut builder: crate::kaijutsu_capnp::shell_value::Builder<'_>,
    value: &ShellValue,
) {
    match value {
        ShellValue::Null => builder.set_null(()),
        ShellValue::Bool(b) => builder.set_bool(*b),
        ShellValue::Int(i) => builder.set_int(*i),
        ShellValue::Float(f) => builder.set_float(*f),
        ShellValue::String(s) => builder.set_string(s),
        ShellValue::Json(j) => builder.set_json(serde_json::to_string(j).unwrap_or_default()),
        ShellValue::Blob(b) => builder.set_blob(b),
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

// ============================================================================
// Block Query Builder Helpers
// ============================================================================

fn set_block_query_builder(
    mut builder: crate::kaijutsu_capnp::block_query::Builder<'_>,
    query: &BlockQuery,
) {
    match query {
        BlockQuery::All => builder.set_all(()),
        BlockQuery::ByIds(ids) => {
            let mut list = builder.init_by_ids(ids.len() as u32);
            for (i, id) in ids.iter().enumerate() {
                let mut b = list.reborrow().get(i as u32);
                set_block_id_builder(&mut b, id);
            }
        }
        BlockQuery::ByFilter(filter) => {
            let fb = builder.init_by_filter();
            set_block_filter_builder(fb, filter);
        }
    }
}

fn set_block_filter_builder(
    mut builder: crate::kaijutsu_capnp::block_filter::Builder<'_>,
    filter: &BlockFilter,
) {
    if !filter.kinds.is_empty() {
        builder.set_has_kinds(true);
        let mut list = builder.reborrow().init_kinds(filter.kinds.len() as u32);
        for (i, kind) in filter.kinds.iter().enumerate() {
            list.set(
                i as u32,
                match kind {
                    BlockKind::Text => crate::kaijutsu_capnp::BlockKind::Text,
                    BlockKind::Thinking => crate::kaijutsu_capnp::BlockKind::Thinking,
                    BlockKind::ToolCall => crate::kaijutsu_capnp::BlockKind::ToolCall,
                    BlockKind::ToolResult => crate::kaijutsu_capnp::BlockKind::ToolResult,
                    BlockKind::Drift => crate::kaijutsu_capnp::BlockKind::Drift,
                    BlockKind::File => crate::kaijutsu_capnp::BlockKind::File,
                    BlockKind::Error => crate::kaijutsu_capnp::BlockKind::Error,
                    BlockKind::Notification => crate::kaijutsu_capnp::BlockKind::Notification,
                },
            );
        }
    }

    if !filter.roles.is_empty() {
        builder.set_has_roles(true);
        let mut list = builder.reborrow().init_roles(filter.roles.len() as u32);
        for (i, role) in filter.roles.iter().enumerate() {
            list.set(
                i as u32,
                match role {
                    Role::User => crate::kaijutsu_capnp::Role::User,
                    Role::Model => crate::kaijutsu_capnp::Role::Model,
                    Role::System => crate::kaijutsu_capnp::Role::System,
                    Role::Tool => crate::kaijutsu_capnp::Role::Tool,
                    Role::Asset => crate::kaijutsu_capnp::Role::Asset,
                },
            );
        }
    }

    if !filter.statuses.is_empty() {
        builder.set_has_statuses(true);
        let mut list = builder
            .reborrow()
            .init_statuses(filter.statuses.len() as u32);
        for (i, status) in filter.statuses.iter().enumerate() {
            list.set(
                i as u32,
                match status {
                    Status::Pending => crate::kaijutsu_capnp::Status::Pending,
                    Status::Running => crate::kaijutsu_capnp::Status::Running,
                    Status::Done => crate::kaijutsu_capnp::Status::Done,
                    Status::Error => crate::kaijutsu_capnp::Status::Error,
                },
            );
        }
    }

    builder.set_exclude_compacted(filter.exclude_compacted);
    builder.set_limit(filter.limit);
    builder.set_max_depth(filter.max_depth);

    if let Some(ref parent_id) = filter.parent_id {
        builder.set_has_parent_id(true);
        let mut pid = builder.reborrow().init_parent_id();
        set_block_id_builder(&mut pid, parent_id);
    }
}

fn set_block_event_filter_builder(
    builder: &mut crate::kaijutsu_capnp::block_event_filter::Builder<'_>,
    filter: &kaijutsu_types::BlockEventFilter,
) {
    if !filter.context_ids.is_empty() {
        builder.set_has_context_ids(true);
        let mut list = builder
            .reborrow()
            .init_context_ids(filter.context_ids.len() as u32);
        for (i, ctx_id) in filter.context_ids.iter().enumerate() {
            list.set(i as u32, ctx_id.as_bytes());
        }
    }

    if !filter.event_types.is_empty() {
        builder.set_has_event_types(true);
        let mut list = builder
            .reborrow()
            .init_event_types(filter.event_types.len() as u32);
        for (i, kind) in filter.event_types.iter().enumerate() {
            list.set(
                i as u32,
                match kind {
                    kaijutsu_types::BlockFlowKind::Inserted => {
                        crate::kaijutsu_capnp::BlockFlowKind::Inserted
                    }
                    kaijutsu_types::BlockFlowKind::TextOps => {
                        crate::kaijutsu_capnp::BlockFlowKind::TextOps
                    }
                    kaijutsu_types::BlockFlowKind::Deleted => {
                        crate::kaijutsu_capnp::BlockFlowKind::Deleted
                    }
                    kaijutsu_types::BlockFlowKind::StatusChanged => {
                        crate::kaijutsu_capnp::BlockFlowKind::StatusChanged
                    }
                    kaijutsu_types::BlockFlowKind::CollapsedChanged => {
                        crate::kaijutsu_capnp::BlockFlowKind::CollapsedChanged
                    }
                    kaijutsu_types::BlockFlowKind::ExcludedChanged => {
                        crate::kaijutsu_capnp::BlockFlowKind::ExcludedChanged
                    }
                    kaijutsu_types::BlockFlowKind::Moved => {
                        crate::kaijutsu_capnp::BlockFlowKind::Moved
                    }
                    kaijutsu_types::BlockFlowKind::SyncReset => {
                        crate::kaijutsu_capnp::BlockFlowKind::SyncReset
                    }
                    kaijutsu_types::BlockFlowKind::OutputChanged => {
                        crate::kaijutsu_capnp::BlockFlowKind::OutputChanged
                    }
                    kaijutsu_types::BlockFlowKind::MetadataChanged => {
                        crate::kaijutsu_capnp::BlockFlowKind::MetadataChanged
                    }
                    kaijutsu_types::BlockFlowKind::ContextSwitched => {
                        crate::kaijutsu_capnp::BlockFlowKind::ContextSwitched
                    }
                },
            );
        }
    }

    if !filter.block_kinds.is_empty() {
        builder.set_has_block_kinds(true);
        let mut list = builder
            .reborrow()
            .init_block_kinds(filter.block_kinds.len() as u32);
        for (i, kind) in filter.block_kinds.iter().enumerate() {
            list.set(
                i as u32,
                match kind {
                    BlockKind::Text => crate::kaijutsu_capnp::BlockKind::Text,
                    BlockKind::Thinking => crate::kaijutsu_capnp::BlockKind::Thinking,
                    BlockKind::ToolCall => crate::kaijutsu_capnp::BlockKind::ToolCall,
                    BlockKind::ToolResult => crate::kaijutsu_capnp::BlockKind::ToolResult,
                    BlockKind::Drift => crate::kaijutsu_capnp::BlockKind::Drift,
                    BlockKind::File => crate::kaijutsu_capnp::BlockKind::File,
                    BlockKind::Error => crate::kaijutsu_capnp::BlockKind::Error,
                    BlockKind::Notification => crate::kaijutsu_capnp::BlockKind::Notification,
                },
            );
        }
    }
}

fn set_block_id_builder(builder: &mut crate::kaijutsu_capnp::block_id::Builder, id: &BlockId) {
    builder.set_context_id(id.context_id.as_bytes());
    builder.set_agent_id(id.agent_id.as_bytes());
    builder.set_seq(id.seq);
}

fn entry_type_from_capnp(et: crate::kaijutsu_capnp::EntryType) -> kaijutsu_types::OutputEntryType {
    use crate::kaijutsu_capnp::EntryType;
    use kaijutsu_types::OutputEntryType;
    match et {
        EntryType::Text => OutputEntryType::Text,
        EntryType::File => OutputEntryType::File,
        EntryType::Directory => OutputEntryType::Directory,
        EntryType::Executable => OutputEntryType::Executable,
        EntryType::Symlink => OutputEntryType::Symlink,
    }
}

fn parse_output_node(
    reader: crate::kaijutsu_capnp::output_node::Reader<'_>,
) -> Result<kaijutsu_types::OutputNode, capnp::Error> {
    let name = reader.get_name()?.to_str()?.to_owned();
    let entry_type = entry_type_from_capnp(reader.get_entry_type()?);
    let text = if reader.get_has_text() {
        Some(reader.get_text()?.to_str()?.to_owned())
    } else {
        None
    };
    let cells_reader = reader.get_cells()?;
    let mut cells = Vec::with_capacity(cells_reader.len() as usize);
    for i in 0..cells_reader.len() {
        cells.push(cells_reader.get(i)?.to_str()?.to_owned());
    }
    let children_reader = reader.get_children()?;
    let mut children = Vec::with_capacity(children_reader.len() as usize);
    for i in 0..children_reader.len() {
        children.push(parse_output_node(children_reader.get(i))?);
    }
    Ok(kaijutsu_types::OutputNode {
        name,
        entry_type,
        text,
        cells,
        children,
    })
}

pub(crate) fn parse_output_data(
    reader: crate::kaijutsu_capnp::output_data::Reader<'_>,
) -> Result<kaijutsu_types::OutputData, capnp::Error> {
    let headers = if reader.get_has_headers() {
        let hlist = reader.get_headers()?;
        let mut v = Vec::with_capacity(hlist.len() as usize);
        for i in 0..hlist.len() {
            v.push(hlist.get(i)?.to_str()?.to_owned());
        }
        Some(v)
    } else {
        None
    };
    let root_reader = reader.get_root()?;
    let mut root = Vec::with_capacity(root_reader.len() as usize);
    for i in 0..root_reader.len() {
        root.push(parse_output_node(root_reader.get(i))?);
    }
    Ok(kaijutsu_types::OutputData { headers, root })
}

fn parse_context_id(data: &[u8]) -> Result<ContextId, RpcError> {
    ContextId::try_from_slice(data).ok_or_else(|| {
        RpcError::ServerError(format!(
            "invalid context ID: expected 16 bytes, got {}",
            data.len()
        ))
    })
}

/// Parse 16-byte Data into KernelId.
fn parse_kernel_id(data: &[u8]) -> Result<KernelId, RpcError> {
    KernelId::try_from_slice(data).ok_or_else(|| {
        RpcError::ServerError(format!(
            "invalid kernel ID: expected 16 bytes, got {}",
            data.len()
        ))
    })
}

/// Helper to parse ContextInfo from Cap'n Proto ContextHandleInfo.
fn parse_context_info(
    reader: &crate::kaijutsu_capnp::context_handle_info::Reader<'_>,
) -> Result<ContextInfo, RpcError> {
    let id = parse_context_id(reader.get_id()?)?;
    let label = reader.get_label()?.to_string()?;
    // Wire field is still named `parentId` — Rust side renamed to `forked_from`
    let parent_data = reader.get_parent_id()?;
    let forked_from = if parent_data.len() == 16 {
        let pid = ContextId::try_from_slice(parent_data);
        pid.filter(|id| !id.is_nil())
    } else {
        None
    };

    let trace_data = reader.get_trace_id()?;
    let trace_id = if trace_data.len() == 16 {
        let mut buf = [0u8; 16];
        buf.copy_from_slice(trace_data);
        buf
    } else {
        [0u8; 16]
    };

    let fork_kind_str = reader.get_fork_kind()?.to_str().unwrap_or("");
    let fork_kind = if fork_kind_str.is_empty() {
        None
    } else {
        Some(fork_kind_str.to_string())
    };
    let archived = reader.get_archived_at() > 0;

    // Parse synthesis keywords
    let keywords = if reader.has_keywords() {
        reader
            .get_keywords()?
            .into_iter()
            .filter_map(|k| k.ok().map(|s| s.to_string().unwrap_or_default()))
            .collect()
    } else {
        Vec::new()
    };

    let top_block_preview = if reader.has_top_block_preview() {
        let s = reader.get_top_block_preview()?.to_str().unwrap_or("");
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    } else {
        None
    };

    Ok(ContextInfo {
        id,
        label,
        forked_from,
        provider: reader.get_provider()?.to_string()?,
        model: reader.get_model()?.to_string()?,
        created_at: reader.get_created_at(),
        trace_id,
        fork_kind,
        archived,
        keywords,
        top_block_preview,
    })
}

/// Helper to parse KernelInfo from Cap'n Proto
fn parse_kernel_info(
    reader: &crate::kaijutsu_capnp::kernel_info::Reader<'_>,
) -> Result<KernelInfo, RpcError> {
    let id = parse_kernel_id(reader.get_id()?)?;
    let contexts_reader = reader.get_contexts()?;
    let mut contexts = Vec::with_capacity(contexts_reader.len() as usize);
    for ctx in contexts_reader.iter() {
        contexts.push(parse_context_info(&ctx)?);
    }

    Ok(KernelInfo {
        id,
        name: reader.get_name()?.to_string()?,
        user_count: reader.get_user_count(),
        agent_count: reader.get_agent_count(),
        contexts,
    })
}

/// Helper to parse block ID from Cap'n Proto (binary 16-byte UUIDs).
pub(crate) fn parse_block_id(
    reader: &crate::kaijutsu_capnp::block_id::Reader<'_>,
) -> Result<BlockId, RpcError> {
    let context_id = ContextId::try_from_slice(reader.get_context_id()?)
        .ok_or_else(|| RpcError::ServerError("invalid context_id in BlockId".into()))?;
    let agent_id = PrincipalId::try_from_slice(reader.get_agent_id()?)
        .ok_or_else(|| RpcError::ServerError("invalid agent_id in BlockId".into()))?;
    Ok(BlockId::new(context_id, agent_id, reader.get_seq()))
}

/// Helper to parse a flat BlockSnapshot from Cap'n Proto using BlockSnapshotBuilder.
pub(crate) fn parse_block_snapshot(
    reader: &crate::kaijutsu_capnp::block_snapshot::Reader<'_>,
) -> Result<BlockSnapshot, RpcError> {
    // Parse block ID
    let id = parse_block_id(&reader.get_id()?)?;

    // Parse kind (8 variants)
    let kind = match reader.get_kind()? {
        crate::kaijutsu_capnp::BlockKind::Text => BlockKind::Text,
        crate::kaijutsu_capnp::BlockKind::Thinking => BlockKind::Thinking,
        crate::kaijutsu_capnp::BlockKind::ToolCall => BlockKind::ToolCall,
        crate::kaijutsu_capnp::BlockKind::ToolResult => BlockKind::ToolResult,
        crate::kaijutsu_capnp::BlockKind::Drift => BlockKind::Drift,
        crate::kaijutsu_capnp::BlockKind::File => BlockKind::File,
        crate::kaijutsu_capnp::BlockKind::Error => BlockKind::Error,
        crate::kaijutsu_capnp::BlockKind::Notification => BlockKind::Notification,
    };

    let mut builder = BlockSnapshotBuilder::new(id, kind);

    // Parse parent_id if present
    if reader.get_has_parent_id() {
        builder = builder.parent_id(parse_block_id(&reader.get_parent_id()?)?);
    }

    // Parse role
    let role = match reader.get_role()? {
        crate::kaijutsu_capnp::Role::User => Role::User,
        crate::kaijutsu_capnp::Role::Model => Role::Model,
        crate::kaijutsu_capnp::Role::System => Role::System,
        crate::kaijutsu_capnp::Role::Tool => Role::Tool,
        crate::kaijutsu_capnp::Role::Asset => Role::Asset,
    };
    builder = builder.role(role);

    // Parse status
    let status = match reader.get_status()? {
        crate::kaijutsu_capnp::Status::Pending => Status::Pending,
        crate::kaijutsu_capnp::Status::Running => Status::Running,
        crate::kaijutsu_capnp::Status::Done => Status::Done,
        crate::kaijutsu_capnp::Status::Error => Status::Error,
    };
    builder = builder.status(status);

    // Content
    builder = builder.content(reader.get_content()?.to_str()?);
    builder = builder.collapsed(reader.get_collapsed());

    // Tool-specific fields
    if reader.has_tool_name() {
        let name = reader.get_tool_name()?.to_str()?;
        if !name.is_empty() {
            builder = builder.tool_name(name);
        }
    }

    if reader.has_tool_input() {
        let input = reader.get_tool_input()?.to_str()?;
        if !input.is_empty() {
            builder = builder.tool_input(input);
        }
    }

    if reader.has_tool_call_id() {
        builder = builder.tool_call_id(parse_block_id(&reader.get_tool_call_id()?)?);
    }

    if reader.get_has_exit_code() {
        builder = builder.exit_code(reader.get_exit_code());
    }

    if reader.get_is_error() {
        builder = builder.is_error(true);
    }

    // Structured output data
    if let Ok(output_data_reader) = reader.get_output_data()
        && let Ok(data) = parse_output_data(output_data_reader)
        && (!data.root.is_empty() || data.headers.is_some())
    {
        builder = builder.output(data);
    }

    // Drift-specific fields — source_context is now binary Data (16-byte ContextId)
    let source_data = reader.get_source_context()?;
    if source_data.len() == 16
        && let Some(ctx) = ContextId::try_from_slice(source_data)
        && !ctx.is_nil()
    {
        builder = builder.source_context(ctx);
    }

    if reader.has_source_model()
        && let Ok(model) = reader.get_source_model()
        && let Ok(s) = model.to_str()
        && !s.is_empty()
    {
        builder = builder.source_model(s);
    }

    // DriftKind — wire is now an enum, not a string
    if reader.get_has_drift_kind()
        && let Ok(dk) = reader.get_drift_kind()
    {
        let drift_kind = match dk {
            crate::kaijutsu_capnp::DriftKind::Push => DriftKind::Push,
            crate::kaijutsu_capnp::DriftKind::Pull => DriftKind::Pull,
            crate::kaijutsu_capnp::DriftKind::Merge => DriftKind::Merge,
            crate::kaijutsu_capnp::DriftKind::Distill => DriftKind::Distill,
            crate::kaijutsu_capnp::DriftKind::Commit => DriftKind::Commit,
            crate::kaijutsu_capnp::DriftKind::Notification => DriftKind::Notification,
            crate::kaijutsu_capnp::DriftKind::Fork => DriftKind::Fork,
        };
        builder = builder.drift_kind(drift_kind);
    }

    // ToolKind — wire enum
    if reader.get_has_tool_kind()
        && let Ok(tk) = reader.get_tool_kind()
    {
        let tool_kind = match tk {
            crate::kaijutsu_capnp::ToolKind::Shell => ToolKind::Shell,
            crate::kaijutsu_capnp::ToolKind::Mcp => ToolKind::Mcp,
            crate::kaijutsu_capnp::ToolKind::Builtin => ToolKind::Builtin,
        };
        builder = builder.tool_kind(tool_kind);
    }

    // tool_use_id (LLM-assigned tool invocation ID)
    if reader.has_tool_use_id()
        && let Ok(tui) = reader.get_tool_use_id()
        && let Ok(s) = tui.to_str()
        && !s.is_empty()
    {
        builder = builder.tool_use_id(s);
    }

    // File path (for BlockKind::File blocks)
    if reader.has_file_path()
        && let Ok(path) = reader.get_file_path()
        && let Ok(s) = path.to_str()
        && !s.is_empty()
    {
        builder = builder.file_path(s);
    }

    // Content type hint (MIME type)
    if reader.has_content_type()
        && let Ok(ct) = reader.get_content_type()
        && let Ok(s) = ct.to_str()
        && !s.is_empty()
    {
        builder = builder.content_type(ContentType::from_mime(s));
    }

    // Ephemeral flag (human-only, excluded from LLM hydration)
    if reader.get_ephemeral() {
        builder = builder.ephemeral(true);
    }

    // Error payload (for Error blocks)
    if reader.get_has_error_payload()
        && let Ok(ep) = reader.get_error_payload()
    {
        let category = match ep.get_category()? {
            crate::kaijutsu_capnp::ErrorCategory::Tool => ErrorCategory::Tool,
            crate::kaijutsu_capnp::ErrorCategory::Stream => ErrorCategory::Stream,
            crate::kaijutsu_capnp::ErrorCategory::Rpc => ErrorCategory::Rpc,
            crate::kaijutsu_capnp::ErrorCategory::Render => ErrorCategory::Render,
            crate::kaijutsu_capnp::ErrorCategory::Parse => ErrorCategory::Parse,
            crate::kaijutsu_capnp::ErrorCategory::Validation => ErrorCategory::Validation,
            crate::kaijutsu_capnp::ErrorCategory::Kernel => ErrorCategory::Kernel,
        };
        let severity = match ep.get_severity()? {
            crate::kaijutsu_capnp::ErrorSeverity::Warning => ErrorSeverity::Warning,
            crate::kaijutsu_capnp::ErrorSeverity::Error => ErrorSeverity::Error,
            crate::kaijutsu_capnp::ErrorSeverity::Fatal => ErrorSeverity::Fatal,
        };
        let code = ep.get_code().ok()
            .and_then(|s| s.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let detail = ep.get_detail().ok()
            .and_then(|s| s.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let span = if ep.get_has_span() {
            Some(ErrorSpan {
                line: ep.get_span_line(),
                column: ep.get_span_column(),
                length: ep.get_span_length(),
            })
        } else {
            None
        };
        let source_kind = if ep.get_has_source_kind() {
            ep.get_source_kind().ok().map(|sk| match sk {
                crate::kaijutsu_capnp::BlockKind::Text => BlockKind::Text,
                crate::kaijutsu_capnp::BlockKind::Thinking => BlockKind::Thinking,
                crate::kaijutsu_capnp::BlockKind::ToolCall => BlockKind::ToolCall,
                crate::kaijutsu_capnp::BlockKind::ToolResult => BlockKind::ToolResult,
                crate::kaijutsu_capnp::BlockKind::Drift => BlockKind::Drift,
                crate::kaijutsu_capnp::BlockKind::File => BlockKind::File,
                crate::kaijutsu_capnp::BlockKind::Error => BlockKind::Error,
                crate::kaijutsu_capnp::BlockKind::Notification => BlockKind::Notification,
            })
        } else {
            None
        };
        builder = builder.error_payload(ErrorPayload {
            category,
            severity,
            code,
            detail,
            span,
            source_kind,
        });
    }

    // Notification payload (for Notification blocks)
    if reader.get_has_notification_payload()
        && let Ok(np) = reader.get_notification_payload()
    {
        let instance = np.get_instance().ok()
            .and_then(|s| s.to_str().ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let kind = match np.get_kind()? {
            crate::kaijutsu_capnp::NotificationKind::ToolAdded => {
                kaijutsu_types::NotificationKind::ToolAdded
            }
            crate::kaijutsu_capnp::NotificationKind::ToolRemoved => {
                kaijutsu_types::NotificationKind::ToolRemoved
            }
            crate::kaijutsu_capnp::NotificationKind::Log => {
                kaijutsu_types::NotificationKind::Log
            }
            crate::kaijutsu_capnp::NotificationKind::PromptsChanged => {
                kaijutsu_types::NotificationKind::PromptsChanged
            }
            crate::kaijutsu_capnp::NotificationKind::Coalesced => {
                kaijutsu_types::NotificationKind::Coalesced
            }
        };
        let level = if np.get_has_level() {
            np.get_level().ok().map(|l| match l {
                crate::kaijutsu_capnp::LogLevel::Trace => kaijutsu_types::LogLevel::Trace,
                crate::kaijutsu_capnp::LogLevel::Debug => kaijutsu_types::LogLevel::Debug,
                crate::kaijutsu_capnp::LogLevel::Info => kaijutsu_types::LogLevel::Info,
                crate::kaijutsu_capnp::LogLevel::Warn => kaijutsu_types::LogLevel::Warn,
                crate::kaijutsu_capnp::LogLevel::Error => kaijutsu_types::LogLevel::Error,
            })
        } else {
            None
        };
        let tool = np.get_tool().ok()
            .and_then(|s| s.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let count = if np.get_has_count() {
            Some(np.get_count() as usize)
        } else {
            None
        };
        let detail = np.get_detail().ok()
            .and_then(|s| s.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        builder = builder.notification_payload(kaijutsu_types::NotificationPayload {
            instance,
            kind,
            level,
            tool,
            count,
            detail,
        });
    }

    Ok(builder.build())
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    pub display_text: String,
    pub kind: CompletionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Command,
    Path,
    Variable,
    Keyword,
}

impl CompletionKind {
    fn from_capnp(kind: crate::kaijutsu_capnp::CompletionKind) -> Self {
        match kind {
            crate::kaijutsu_capnp::CompletionKind::Command => CompletionKind::Command,
            crate::kaijutsu_capnp::CompletionKind::Path => CompletionKind::Path,
            crate::kaijutsu_capnp::CompletionKind::Variable => CompletionKind::Variable,
            crate::kaijutsu_capnp::CompletionKind::Keyword => CompletionKind::Keyword,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub id: u64,
    pub code: String,
    pub timestamp: u64,
}

// ============================================================================
// Timeline Types
// ============================================================================

/// A version snapshot in document history.
#[derive(Debug, Clone)]
pub struct VersionSnapshot {
    /// Version number
    pub version: u64,
    /// Unix timestamp (ms) when this version was created
    pub timestamp: u64,
    /// Number of blocks at this version
    pub block_count: u32,
    /// What changed ("block_added", "block_edited", etc.)
    pub change_kind: String,
}

// ============================================================================
// Drift Types
// ============================================================================

/// Info about a staged drift operation.
#[derive(Debug, Clone)]
pub struct StagedDriftInfo {
    pub id: u64,
    pub source_ctx: ContextId,
    pub target_ctx: ContextId,
    pub content: String,
    pub source_model: String,
    pub drift_kind: DriftKind,
    pub created_at: u64,
}

// ============================================================================
// LLM Configuration Types
// ============================================================================

/// Information about a single LLM provider
#[derive(Debug, Clone)]
pub struct LlmProviderInfo {
    pub name: String,
    pub default_model: String,
    pub available: bool,
    /// All available model IDs for this provider (from aliases + default).
    pub models: Vec<String>,
}

/// Current LLM configuration for a kernel
#[derive(Debug, Clone)]
pub struct LlmConfigInfo {
    pub default_provider: String,
    pub default_model: String,
    pub providers: Vec<LlmProviderInfo>,
}

/// Tool filter configuration
#[derive(Debug, Clone)]
pub enum ClientToolFilter {
    /// All tools available
    All,
    /// Only these tools available
    AllowList(Vec<String>),
    /// All except these tools available
    DenyList(Vec<String>),
}

/// Shell variable value (mirrors kaish `ast::Value`).
#[derive(Debug, Clone, PartialEq)]
pub enum ShellValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    /// JSON-serialized structured data.
    Json(serde_json::Value),
    /// Blob reference path.
    Blob(String),
}

// ============================================================================
// Block Types
// ============================================================================

/// CRDT sync state (ops + version, no blocks).
///
/// Used by `get_context_sync` for lightweight CRDT bootstrapping and resync.
#[derive(Debug, Clone)]
pub struct SyncState {
    pub context_id: ContextId,
    pub ops: Vec<u8>,
    pub version: u64,
}

/// Result from submitting the input document (submitInput @78).
#[derive(Debug, Clone)]
pub struct SubmitResult {
    pub block_id: BlockId,
}

/// Full input document state for a context.
#[derive(Debug, Clone)]
pub struct InputState {
    pub content: String,
    pub ops: Vec<u8>,
    pub version: u64,
}

/// Result from a kernel tool execution (executeTool @16).
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub request_id: String,
    pub success: bool,
    pub output: String,
}

/// Schema for a kernel tool (getToolSchemas @11).
#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub category: String,
    pub input_schema: String,
}

/// Result from an MCP tool call
#[derive(Debug, Clone)]
pub struct McpToolResult {
    pub content: String,
    pub is_error: bool,
}

/// Information about an MCP resource
#[derive(Debug, Clone)]
pub struct McpResource {
    /// Resource URI (e.g., "file:///path/to/file")
    pub uri: String,
    /// Resource name
    pub name: String,
    /// Optional description
    pub description: Option<String>,
    /// Optional MIME type
    pub mime_type: Option<String>,
}

/// Contents of an MCP resource
#[derive(Debug, Clone)]
pub enum McpResourceContents {
    /// Text content
    Text {
        uri: String,
        mime_type: Option<String>,
        text: String,
    },
    /// Binary content (already decoded from base64)
    Blob {
        uri: String,
        mime_type: Option<String>,
        blob: Vec<u8>,
    },
}

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("Cap'n Proto error: {0}")]
    Capnp(#[from] capnp::Error),
    #[error("Not in schema: {0}")]
    NotInSchema(#[from] capnp::NotInSchema),
    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("Not connected to server")]
    NotConnected,
    #[error("Capability no longer valid")]
    CapabilityLost,
    #[error("Server error: {0}")]
    ServerError(String),
    #[error("{0}")]
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use capnp::message::Builder as MessageBuilder;

    /// Helper: build a BlockSnapshot capnp message, set fields, then parse it back
    /// through `parse_block_snapshot` to verify roundtrip fidelity.
    fn roundtrip_snapshot(snap: &BlockSnapshot) -> BlockSnapshot {
        let mut message = MessageBuilder::new_default();
        let mut builder = message.init_root::<crate::kaijutsu_capnp::block_snapshot::Builder>();

        // Set ID
        {
            let mut id = builder.reborrow().init_id();
            id.set_context_id(snap.id.context_id.as_bytes());
            id.set_agent_id(snap.id.agent_id.as_bytes());
            id.set_seq(snap.id.seq);
        }

        // Set kind
        builder.set_kind(match snap.kind {
            BlockKind::Text => crate::kaijutsu_capnp::BlockKind::Text,
            BlockKind::Thinking => crate::kaijutsu_capnp::BlockKind::Thinking,
            BlockKind::ToolCall => crate::kaijutsu_capnp::BlockKind::ToolCall,
            BlockKind::ToolResult => crate::kaijutsu_capnp::BlockKind::ToolResult,
            BlockKind::Drift => crate::kaijutsu_capnp::BlockKind::Drift,
            BlockKind::File => crate::kaijutsu_capnp::BlockKind::File,
            BlockKind::Error => crate::kaijutsu_capnp::BlockKind::Error,
            BlockKind::Notification => crate::kaijutsu_capnp::BlockKind::Notification,
        });

        // Set role
        builder.set_role(match snap.role {
            Role::User => crate::kaijutsu_capnp::Role::User,
            Role::Model => crate::kaijutsu_capnp::Role::Model,
            Role::System => crate::kaijutsu_capnp::Role::System,
            Role::Tool => crate::kaijutsu_capnp::Role::Tool,
            Role::Asset => crate::kaijutsu_capnp::Role::Asset,
        });

        // Set status
        builder.set_status(match snap.status {
            Status::Pending => crate::kaijutsu_capnp::Status::Pending,
            Status::Running => crate::kaijutsu_capnp::Status::Running,
            Status::Done => crate::kaijutsu_capnp::Status::Done,
            Status::Error => crate::kaijutsu_capnp::Status::Error,
        });

        builder.set_content(&snap.content);
        builder.set_collapsed(snap.collapsed);

        // Set file_path if present
        if let Some(ref path) = snap.file_path {
            builder.set_file_path(path);
        }

        // Set tool_kind if present
        if let Some(tk) = snap.tool_kind {
            builder.set_has_tool_kind(true);
            builder.set_tool_kind(match tk {
                ToolKind::Shell => crate::kaijutsu_capnp::ToolKind::Shell,
                ToolKind::Mcp => crate::kaijutsu_capnp::ToolKind::Mcp,
                ToolKind::Builtin => crate::kaijutsu_capnp::ToolKind::Builtin,
            });
        }

        // Set notification payload if present (D-36).
        if let Some(ref payload) = snap.notification {
            builder.set_has_notification_payload(true);
            let mut np = builder.reborrow().init_notification_payload();
            np.set_instance(&payload.instance);
            np.set_kind(match payload.kind {
                kaijutsu_types::NotificationKind::ToolAdded => {
                    crate::kaijutsu_capnp::NotificationKind::ToolAdded
                }
                kaijutsu_types::NotificationKind::ToolRemoved => {
                    crate::kaijutsu_capnp::NotificationKind::ToolRemoved
                }
                kaijutsu_types::NotificationKind::Log => {
                    crate::kaijutsu_capnp::NotificationKind::Log
                }
                kaijutsu_types::NotificationKind::PromptsChanged => {
                    crate::kaijutsu_capnp::NotificationKind::PromptsChanged
                }
                kaijutsu_types::NotificationKind::Coalesced => {
                    crate::kaijutsu_capnp::NotificationKind::Coalesced
                }
            });
            if let Some(level) = payload.level {
                np.set_has_level(true);
                np.set_level(match level {
                    kaijutsu_types::LogLevel::Trace => crate::kaijutsu_capnp::LogLevel::Trace,
                    kaijutsu_types::LogLevel::Debug => crate::kaijutsu_capnp::LogLevel::Debug,
                    kaijutsu_types::LogLevel::Info => crate::kaijutsu_capnp::LogLevel::Info,
                    kaijutsu_types::LogLevel::Warn => crate::kaijutsu_capnp::LogLevel::Warn,
                    kaijutsu_types::LogLevel::Error => crate::kaijutsu_capnp::LogLevel::Error,
                });
            }
            if let Some(ref tool) = payload.tool {
                np.set_tool(tool);
            }
            if let Some(count) = payload.count {
                np.set_has_count(true);
                np.set_count(count as u32);
            }
            if let Some(ref detail) = payload.detail {
                np.set_detail(detail);
            }
        }

        // Parse back
        let reader = message
            .get_root_as_reader::<crate::kaijutsu_capnp::block_snapshot::Reader>()
            .unwrap();
        parse_block_snapshot(&reader).unwrap()
    }

    #[test]
    fn test_parse_block_snapshot_file_path_roundtrip() {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let id = BlockId {
            context_id: ctx,
            agent_id: agent,
            seq: 1,
        };

        let snap = BlockSnapshotBuilder::new(id, BlockKind::File)
            .role(Role::Asset)
            .status(Status::Done)
            .content("file content here")
            .file_path("/src/main.rs")
            .build();

        let parsed = roundtrip_snapshot(&snap);

        assert_eq!(parsed.file_path.as_deref(), Some("/src/main.rs"));
        assert_eq!(parsed.kind, BlockKind::File);
        assert_eq!(parsed.role, Role::Asset);
    }

    #[test]
    fn test_parse_block_snapshot_no_file_path() {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let id = BlockId {
            context_id: ctx,
            agent_id: agent,
            seq: 2,
        };

        let snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
            .role(Role::User)
            .status(Status::Done)
            .content("hello")
            .build();

        let parsed = roundtrip_snapshot(&snap);

        assert_eq!(parsed.file_path, None);
        assert_eq!(parsed.kind, BlockKind::Text);
    }

    #[test]
    fn test_parse_block_snapshot_tool_kind_roundtrip() {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let id = BlockId {
            context_id: ctx,
            agent_id: agent,
            seq: 3,
        };

        let snap = BlockSnapshotBuilder::new(id, BlockKind::ToolCall)
            .role(Role::Model)
            .status(Status::Done)
            .content("{}")
            .tool_kind(ToolKind::Mcp)
            .build();

        let parsed = roundtrip_snapshot(&snap);

        assert_eq!(parsed.tool_kind, Some(ToolKind::Mcp));
    }

    // ── Phase 2: NotificationPayload capnp roundtrip (D-36) ───────────────
    //
    // Capnp wire maps use two fragile patterns for optional fields:
    //   (a) explicit `has_<field>` flags paired with a value slot
    //       (level, count) — forgetting the flag-setter on encode silently
    //       drops the value; forgetting the flag-check on decode reads
    //       whatever garbage was in the slot.
    //   (b) empty-string sentinels for Option<String> (tool, detail) —
    //       the encoder omits a None, the decoder filters "" back to None.
    //
    // A pure-Rust unit test can't catch either failure. These roundtrips
    // encode into a capnp builder, re-read through `parse_block_snapshot`,
    // and compare the payload structurally. Coverage split across three
    // tests so a failure pinpoints which axis broke (kind variants, flag
    // pairs, or None-sentinel handling) instead of failing one fat assert.
    //
    // `roundtrip_snapshot` was extended above to include the notification
    // encoder branch; these tests exercise that branch + the decoder in
    // `parse_block_snapshot`.

    fn notif_ctx_id() -> BlockId {
        BlockId {
            context_id: ContextId::new(),
            agent_id: PrincipalId::system(),
            seq: 42,
        }
    }

    #[test]
    fn test_notification_payload_capnp_roundtrip_full() {
        // Exercise every field populated at once — the "happy path" for a
        // Log notification (most populated kind).
        let id = notif_ctx_id();
        let payload = kaijutsu_types::NotificationPayload {
            instance: "gpal".into(),
            kind: kaijutsu_types::NotificationKind::Log,
            level: Some(kaijutsu_types::LogLevel::Warn),
            tool: Some("consult_gemini".into()),
            count: Some(3),
            detail: Some("upstream timeout; retrying".into()),
        };
        let snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
            .role(Role::System)
            .content("[gpal] warn: upstream timeout; retrying")
            .notification_payload(payload.clone())
            .build();

        let parsed = roundtrip_snapshot(&snap);

        assert_eq!(parsed.kind, BlockKind::Notification);
        assert_eq!(parsed.notification, Some(payload));
    }

    #[test]
    fn test_notification_payload_capnp_roundtrip_minimal() {
        // Minimal payload: only `instance` + `kind` populated. This locks
        // the `has_*` flag / empty-string-sentinel discipline: a None on
        // encode must round-trip back to None, not Some("") or Some(0).
        let id = notif_ctx_id();
        let payload = kaijutsu_types::NotificationPayload {
            instance: "builtin.block".into(),
            kind: kaijutsu_types::NotificationKind::PromptsChanged,
            level: None,
            tool: None,
            count: None,
            detail: None,
        };
        let snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
            .role(Role::System)
            .notification_payload(payload.clone())
            .build();

        let parsed = roundtrip_snapshot(&snap);

        let parsed_payload = parsed.notification.expect("notification must survive");
        assert_eq!(parsed_payload.instance, "builtin.block");
        assert_eq!(
            parsed_payload.kind,
            kaijutsu_types::NotificationKind::PromptsChanged
        );
        assert_eq!(parsed_payload.level, None, "has_level=false must yield None");
        assert_eq!(parsed_payload.tool, None, "empty tool must yield None");
        assert_eq!(parsed_payload.count, None, "has_count=false must yield None");
        assert_eq!(parsed_payload.detail, None, "empty detail must yield None");
    }

    #[test]
    fn test_notification_payload_capnp_roundtrip_all_kind_variants() {
        // One test per NotificationKind variant — catches a mis-ordered
        // capnp enum ordinal (e.g. `toolAdded @1; toolRemoved @0` swap)
        // which would silently alias ToolAdded ↔ ToolRemoved on the wire.
        let id = notif_ctx_id();
        let kinds = [
            kaijutsu_types::NotificationKind::ToolAdded,
            kaijutsu_types::NotificationKind::ToolRemoved,
            kaijutsu_types::NotificationKind::Log,
            kaijutsu_types::NotificationKind::PromptsChanged,
            kaijutsu_types::NotificationKind::Coalesced,
        ];
        for kind in kinds {
            let payload = kaijutsu_types::NotificationPayload {
                instance: "svc".into(),
                kind,
                level: None,
                tool: None,
                count: None,
                detail: None,
            };
            let snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
                .role(Role::System)
                .notification_payload(payload.clone())
                .build();
            let parsed = roundtrip_snapshot(&snap);
            assert_eq!(
                parsed.notification.map(|p| p.kind),
                Some(kind),
                "NotificationKind::{:?} did not roundtrip through capnp",
                kind,
            );
        }
    }

    #[test]
    fn test_notification_payload_capnp_roundtrip_all_log_levels() {
        // Same ordinal-aliasing risk for LogLevel. One test per variant.
        let id = notif_ctx_id();
        let levels = [
            kaijutsu_types::LogLevel::Trace,
            kaijutsu_types::LogLevel::Debug,
            kaijutsu_types::LogLevel::Info,
            kaijutsu_types::LogLevel::Warn,
            kaijutsu_types::LogLevel::Error,
        ];
        for level in levels {
            let payload = kaijutsu_types::NotificationPayload {
                instance: "svc".into(),
                kind: kaijutsu_types::NotificationKind::Log,
                level: Some(level),
                tool: None,
                count: None,
                detail: Some("m".into()),
            };
            let snap = BlockSnapshotBuilder::new(id, BlockKind::Text)
                .role(Role::System)
                .notification_payload(payload.clone())
                .build();
            let parsed = roundtrip_snapshot(&snap);
            assert_eq!(
                parsed.notification.and_then(|p| p.level),
                Some(level),
                "LogLevel::{:?} did not roundtrip through capnp",
                level,
            );
        }
    }
}

// ============================================================================
// AgentCommands capnp server (client-side callback)
// ============================================================================

/// Implements the `AgentCommands` Cap'n Proto interface on the client side.
///
/// Lives in `spawn_local` (is `!Send`). Forwards invocations to the caller
/// via an mpsc channel so they can be processed on any thread.
struct AgentCommandsImpl {
    tx: std::sync::mpsc::Sender<crate::actor::AgentInvocation>,
}

impl crate::kaijutsu_capnp::agent_commands::Server for AgentCommandsImpl {
    async fn invoke(
        self: capnp::capability::Rc<Self>,
        params: crate::kaijutsu_capnp::agent_commands::InvokeParams,
        mut results: crate::kaijutsu_capnp::agent_commands::InvokeResults,
    ) -> Result<(), capnp::Error> {
        let action = params
            .get()?
            .get_action()?
            .to_str()
            .unwrap_or_default()
            .to_string();
        let invoke_params = params.get()?.get_params()?.to_vec();

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let invocation = crate::actor::AgentInvocation {
            action,
            params: invoke_params,
            reply: reply_tx,
        };

        // std::sync::mpsc::Sender::send is non-blocking and works from any executor
        self.tx
            .send(invocation)
            .map_err(|_| capnp::Error::failed("agent handler disconnected".into()))?;

        // Await the reply with timeout — prevents indefinite hang if the app
        // stalls or crashes. 15s is generous for a frame-rate-driven poll loop.
        let response = tokio::time::timeout(
            crate::constants::AGENT_INVOCATION_TIMEOUT,
            reply_rx,
        )
        .await
        .map_err(|_| {
            capnp::Error::failed(format!(
                "agent invocation timed out after {}s waiting for app dispatch",
                crate::constants::AGENT_INVOCATION_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|_| capnp::Error::failed("agent handler dropped reply".into()))?;

        match response {
            Ok(data) => {
                results.get().set_result(&data);
                Ok(())
            }
            Err(e) => Err(capnp::Error::failed(e)),
        }
    }
}
