//! Cap'n Proto RPC client for kaijutsu
//!
//! Provides typed interface to the World and Kernel capabilities.

use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use futures::AsyncReadExt;
use kaijutsu_crdt::{ContextId, KernelId};
use kaijutsu_types::{BlockId, BlockKind, BlockSnapshot, BlockSnapshotBuilder, DriftKind, PrincipalId, Role, Status, ToolKind};
use russh::client::Msg;
use russh::ChannelStream;
use tokio_util::compat::TokioAsyncReadCompatExt;

use crate::kaijutsu_capnp::world;

/// RPC client wrapper
///
/// Holds the World capability bootstrapped from the server.
///
/// IMPORTANT: Must be created and used within a `tokio::task::LocalSet` context
/// because capnp-rpc's RpcSystem is not Send.
#[derive(Clone)]
pub struct RpcClient {
    world: world::Client,
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

        // Spawn the RPC system to run in the background (requires LocalSet)
        tokio::task::spawn_local(rpc_system);

        Ok(Self { world, retained_channels: None, ssh_session: None })
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
    pub parent_id: Option<ContextId>,
    pub provider: String,
    pub model: String,
    pub created_at: u64,
    /// Long-running OTel trace ID for this context (16 bytes, or zeros if unavailable).
    pub trace_id: [u8; 16],
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
    pub async fn join_context(&self, context_id: ContextId, instance: &str) -> Result<ContextId, RpcError> {
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
    ) -> Result<BlockId, RpcError> {
        let mut request = self.kernel.shell_execute_request();
        request.get().set_code(code);
        request.get().set_context_id(context_id.as_bytes());
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
        let request = self.kernel.detach_request();
        request.send().promise.await?;
        Ok(())
    }

    // =========================================================================
    // Block-based CRDT sync methods
    // =========================================================================

    // NOTE: apply_block_op was removed - the unified CRDT model uses SerializedOps
    // for replication. See BlockDocument::ops_since() and apply_ops() in kaijutsu-crdt.

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
    #[tracing::instrument(skip(self), name = "rpc_client.get_context_state")]
    pub async fn get_context_state(
        &self,
        context_id: ContextId,
    ) -> Result<DocumentState, RpcError> {
        let mut request = self.kernel.get_context_state_request();
        request.get().set_context_id(context_id.as_bytes());
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let state = response.get()?.get_state()?;

        let context_id = parse_context_id(state.get_context_id()?)?;
        let version = state.get_version();
        let blocks_reader = state.get_blocks()?;
        let mut blocks = Vec::with_capacity(blocks_reader.len() as usize);

        for block in blocks_reader.iter() {
            let snapshot = parse_block_snapshot(&block)?;
            blocks.push(snapshot);
        }

        // Get full oplog for proper CRDT sync
        let ops = state.get_ops().map(|d| d.to_vec()).unwrap_or_default();

        Ok(DocumentState {
            context_id,
            blocks,
            version,
            ops,
        })
    }

    /// Compact a document's oplog, returning new size and sync generation.
    #[tracing::instrument(skip(self), name = "rpc_client.compact_context")]
    pub async fn compact_context(
        &self,
        context_id: ContextId,
    ) -> Result<(u64, u64), RpcError> {
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

    // =========================================================================
    // MCP Tool operations
    // =========================================================================

    /// Execute a tool via the kernel's tool registry.
    ///
    /// This is the general-purpose tool execution path (executeTool @16).
    /// Tools include git, drift, and any registered execution engines.
    #[tracing::instrument(skip(self, params), name = "rpc_client.execute_tool")]
    pub async fn execute_tool(
        &self,
        tool: &str,
        params: &str,
    ) -> Result<ToolResult, RpcError> {
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
            call.set_arguments(&serde_json::to_string(arguments).unwrap_or_default());
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

    /// Fork a document at a specific version, creating a new context.
    ///
    /// Returns the server-assigned ContextId for the new fork.
    #[tracing::instrument(skip(self), name = "rpc_client.fork_from_version")]
    pub async fn fork_from_version(
        &self,
        context_id: ContextId,
        version: u64,
        label: &str,
    ) -> Result<ContextId, RpcError> {
        let mut request = self.kernel.fork_from_version_request();
        {
            let mut params = request.get();
            params.set_context_id(context_id.as_bytes());
            params.set_version(version);
            params.set_context_label(label);
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        parse_context_id(response.get()?.get_new_context_id()?)
    }

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

    /// Configure the LLM provider and model for this kernel.
    #[tracing::instrument(skip(self), name = "rpc_client.configure_llm")]
    pub async fn configure_llm(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<bool, RpcError> {
        let mut request = self.kernel.configure_llm_request();
        {
            let mut params = request.get();
            params.set_provider(provider);
            params.set_model(model);
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

    /// Stage a drift push to another context.
    #[tracing::instrument(skip(self, content), name = "rpc_client.drift_push")]
    pub async fn drift_push(
        &self,
        target_ctx: ContextId,
        content: &str,
        summarize: bool,
    ) -> Result<u64, RpcError> {
        let mut request = self.kernel.drift_push_request();
        {
            let mut params = request.get();
            params.set_target_ctx(target_ctx.as_bytes());
            params.set_content(content);
            params.set_summarize(summarize);
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_staged_id())
    }

    /// Flush all staged drifts.
    #[tracing::instrument(skip(self), name = "rpc_client.drift_flush")]
    pub async fn drift_flush(&self) -> Result<u32, RpcError> {
        let mut request = self.kernel.drift_flush_request();
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_count())
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

    /// Pull summarized content from another context into this one.
    ///
    /// Reads the source context's conversation, distills it via LLM,
    /// and injects the summary as a Drift block in this kernel's document.
    #[tracing::instrument(skip(self, prompt), name = "rpc_client.drift_pull")]
    pub async fn drift_pull(
        &self,
        source_ctx: ContextId,
        prompt: Option<&str>,
    ) -> Result<BlockId, RpcError> {
        let mut request = self.kernel.drift_pull_request();
        request.get().set_source_ctx(source_ctx.as_bytes());
        if let Some(p) = prompt {
            request.get().set_prompt(p);
        }
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let block_id_reader = response.get()?.get_block_id()?;
        parse_block_id(&block_id_reader)
    }

    /// Merge a forked context back into its parent.
    ///
    /// Distills the source context's conversation and injects the summary
    /// into the parent context as a Drift block with DriftKind::Merge.
    #[tracing::instrument(skip(self), name = "rpc_client.drift_merge")]
    pub async fn drift_merge(
        &self,
        source_ctx: ContextId,
    ) -> Result<BlockId, RpcError> {
        let mut request = self.kernel.drift_merge_request();
        request.get().set_source_ctx(source_ctx.as_bytes());
        {
            let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
            let mut trace = request.get().init_trace();
            trace.set_traceparent(&traceparent);
            trace.set_tracestate(&tracestate);
        }
        let response = request.send().promise.await?;
        let block_id_reader = response.get()?.get_block_id()?;
        parse_block_id(&block_id_reader)
    }

    /// Rename a context's human-friendly label.
    #[tracing::instrument(skip(self), name = "rpc_client.rename_context")]
    pub async fn rename_context(
        &self,
        context_id: ContextId,
        label: &str,
    ) -> Result<(), RpcError> {
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
            providers.push(LlmProviderInfo {
                name: p.get_name()?.to_string()?,
                default_model: p.get_default_model()?.to_string()?,
                available: p.get_available(),
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
}

/// Read a `ShellValue` from a Cap'n Proto reader.
fn read_shell_value(reader: crate::kaijutsu_capnp::shell_value::Reader<'_>) -> Result<ShellValue, RpcError> {
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
fn write_shell_value(mut builder: crate::kaijutsu_capnp::shell_value::Builder<'_>, value: &ShellValue) {
    match value {
        ShellValue::Null => builder.set_null(()),
        ShellValue::Bool(b) => builder.set_bool(*b),
        ShellValue::Int(i) => builder.set_int(*i),
        ShellValue::Float(f) => builder.set_float(*f),
        ShellValue::String(s) => builder.set_string(s),
        ShellValue::Json(j) => builder.set_json(&serde_json::to_string(j).unwrap_or_default()),
        ShellValue::Blob(b) => builder.set_blob(b),
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Parse 16-byte Data into ContextId.
fn parse_context_id(data: &[u8]) -> Result<ContextId, RpcError> {
    ContextId::try_from_slice(data).ok_or_else(|| {
        RpcError::ServerError(format!("invalid context ID: expected 16 bytes, got {}", data.len()))
    })
}

/// Parse 16-byte Data into KernelId.
fn parse_kernel_id(data: &[u8]) -> Result<KernelId, RpcError> {
    KernelId::try_from_slice(data).ok_or_else(|| {
        RpcError::ServerError(format!("invalid kernel ID: expected 16 bytes, got {}", data.len()))
    })
}

/// Helper to parse ContextInfo from Cap'n Proto ContextHandleInfo.
fn parse_context_info(
    reader: &crate::kaijutsu_capnp::context_handle_info::Reader<'_>,
) -> Result<ContextInfo, RpcError> {
    let id = parse_context_id(reader.get_id()?)?;
    let label = reader.get_label()?.to_string()?;
    let parent_data = reader.get_parent_id()?;
    let parent_id = if parent_data.len() == 16 {
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

    Ok(ContextInfo {
        id,
        label,
        parent_id,
        provider: reader.get_provider()?.to_string()?,
        model: reader.get_model()?.to_string()?,
        created_at: reader.get_created_at(),
        trace_id,
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

    // Parse kind (5 variants — no ShellCommand/ShellOutput)
    let kind = match reader.get_kind()? {
        crate::kaijutsu_capnp::BlockKind::Text => BlockKind::Text,
        crate::kaijutsu_capnp::BlockKind::Thinking => BlockKind::Thinking,
        crate::kaijutsu_capnp::BlockKind::ToolCall => BlockKind::ToolCall,
        crate::kaijutsu_capnp::BlockKind::ToolResult => BlockKind::ToolResult,
        crate::kaijutsu_capnp::BlockKind::Drift => BlockKind::Drift,
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

    // Display hint
    if reader.has_display_hint() {
        if let Ok(hint) = reader.get_display_hint() {
            if let Ok(s) = hint.to_str() {
                if !s.is_empty() {
                    builder = builder.display_hint(s);
                }
            }
        }
    }

    // Drift-specific fields — source_context is now binary Data (16-byte ContextId)
    let source_data = reader.get_source_context()?;
    if source_data.len() == 16 {
        if let Some(ctx) = ContextId::try_from_slice(source_data) {
            if !ctx.is_nil() {
                builder = builder.source_context(ctx);
            }
        }
    }

    if reader.has_source_model() {
        if let Ok(model) = reader.get_source_model() {
            if let Ok(s) = model.to_str() {
                if !s.is_empty() {
                    builder = builder.source_model(s);
                }
            }
        }
    }

    // DriftKind — wire is now an enum, not a string
    if reader.get_has_drift_kind() {
        if let Ok(dk) = reader.get_drift_kind() {
            let drift_kind = match dk {
                crate::kaijutsu_capnp::DriftKind::Push => DriftKind::Push,
                crate::kaijutsu_capnp::DriftKind::Pull => DriftKind::Pull,
                crate::kaijutsu_capnp::DriftKind::Merge => DriftKind::Merge,
                crate::kaijutsu_capnp::DriftKind::Distill => DriftKind::Distill,
                crate::kaijutsu_capnp::DriftKind::Commit => DriftKind::Commit,
            };
            builder = builder.drift_kind(drift_kind);
        }
    }

    // ToolKind — wire enum
    if reader.get_has_tool_kind() {
        if let Ok(tk) = reader.get_tool_kind() {
            let tool_kind = match tk {
                crate::kaijutsu_capnp::ToolKind::Shell => ToolKind::Shell,
                crate::kaijutsu_capnp::ToolKind::Mcp => ToolKind::Mcp,
                crate::kaijutsu_capnp::ToolKind::Builtin => ToolKind::Builtin,
            };
            builder = builder.tool_kind(tool_kind);
        }
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

/// Full document state with blocks
#[derive(Debug, Clone)]
pub struct DocumentState {
    pub context_id: ContextId,
    pub blocks: Vec<BlockSnapshot>,
    pub version: u64,
    /// Full oplog bytes for CRDT sync (enables incremental ops to merge)
    pub ops: Vec<u8>,
}

/// Result from a kernel tool execution (executeTool @16).
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub request_id: String,
    pub success: bool,
    pub output: String,
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
}
