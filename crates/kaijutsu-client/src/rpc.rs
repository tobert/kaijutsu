//! Cap'n Proto RPC client for kaijutsu
//!
//! Provides typed interface to the World and Kernel capabilities.

use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use futures::AsyncReadExt;
use kaijutsu_crdt::DriftKind;
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
pub struct RpcClient {
    world: world::Client,
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

        Ok(Self { world })
    }

    /// Get current identity from the server
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

    /// Attach to a kernel by ID
    pub async fn attach_kernel(&self, id: &str) -> Result<KernelHandle, RpcError> {
        let mut request = self.world.attach_kernel_request();
        request.get().set_id(id);
        let response = request.send().promise.await?;
        let kernel = response.get()?.get_kernel()?;

        Ok(KernelHandle { kernel })
    }

    /// Create a new kernel
    pub async fn create_kernel(&self, config: KernelConfig) -> Result<KernelHandle, RpcError> {
        let mut request = self.world.create_kernel_request();
        {
            let mut cfg = request.get().init_config();
            cfg.set_name(&config.name);
            cfg.set_consent_mode(match config.consent_mode {
                ConsentMode::Collaborative => crate::kaijutsu_capnp::ConsentMode::Collaborative,
                ConsentMode::Autonomous => crate::kaijutsu_capnp::ConsentMode::Autonomous,
            });
            let mut mounts = cfg.init_mounts(config.mounts.len() as u32);
            for (i, mount) in config.mounts.iter().enumerate() {
                let mut m = mounts.reborrow().get(i as u32);
                m.set_path(&mount.path);
                m.set_source(&mount.source);
                m.set_writable(mount.writable);
            }
        }
        let response = request.send().promise.await?;
        let kernel = response.get()?.get_kernel()?;

        Ok(KernelHandle { kernel })
    }

    /// Take a seat in a kernel context
    pub async fn take_seat(&self, seat_id: &SeatId) -> Result<SeatHandle, RpcError> {
        let mut request = self.world.take_seat_request();
        {
            let mut id = request.get().init_seat();
            id.set_nick(&seat_id.nick);
            id.set_instance(&seat_id.instance);
            id.set_kernel(&seat_id.kernel);
            id.set_context(&seat_id.context);
        }
        let response = request.send().promise.await?;
        let handle = response.get()?.get_handle()?;
        Ok(SeatHandle {
            handle,
            id: seat_id.clone(),
        })
    }

    /// List all seats owned by current user
    pub async fn list_my_seats(&self) -> Result<Vec<SeatInfo>, RpcError> {
        let request = self.world.list_my_seats_request();
        let response = request.send().promise.await?;
        let seats = response.get()?.get_seats()?;

        let mut result = Vec::with_capacity(seats.len() as usize);
        for seat in seats.iter() {
            result.push(parse_seat_info(&seat)?);
        }
        Ok(result)
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
    pub id: String,
    pub name: String,
    pub user_count: u32,
    pub agent_count: u32,
    pub contexts: Vec<Context>,
    pub seat_count: u32,
}

// ============================================================================
// Seat and Context Types
// ============================================================================

/// Seat identifier - the 4-tuple
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatId {
    pub nick: String,
    pub instance: String,
    pub kernel: String,
    pub context: String,
}

impl SeatId {
    /// Create a new SeatId
    pub fn new(nick: impl Into<String>, instance: impl Into<String>, kernel: impl Into<String>, context: impl Into<String>) -> Self {
        Self {
            nick: nick.into(),
            instance: instance.into(),
            kernel: kernel.into(),
            context: context.into(),
        }
    }

    /// Format as display string: @nick:instance@kernel:context
    pub fn display(&self) -> String {
        format!("@{}:{}@{}:{}", self.nick, self.instance, self.kernel, self.context)
    }
}

/// Seat status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatStatus {
    Active,
    Idle,
    Away,
}

/// Info about a seat
#[derive(Debug, Clone)]
pub struct SeatInfo {
    pub id: SeatId,
    pub owner: String,
    pub status: SeatStatus,
    pub last_activity: u64,
    pub cursor_block: Option<String>,
}

/// Document attached to a context
#[derive(Debug, Clone)]
pub struct ContextDocument {
    pub id: String,
    pub attached_by: String,
    pub attached_at: u64,
}

/// Context within a kernel
#[derive(Debug, Clone)]
pub struct Context {
    pub name: String,
    pub documents: Vec<ContextDocument>,
    pub seats: Vec<SeatInfo>,
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
    pub async fn get_info(&self) -> Result<KernelInfo, RpcError> {
        let request = self.kernel.get_info_request();
        let response = request.send().promise.await?;
        let info = response.get()?.get_info()?;
        parse_kernel_info(&info)
    }

    // =========================================================================
    // Context management
    // =========================================================================

    /// List all contexts in this kernel
    pub async fn list_contexts(&self) -> Result<Vec<Context>, RpcError> {
        let request = self.kernel.list_contexts_request();
        let response = request.send().promise.await?;
        let contexts = response.get()?.get_contexts()?;

        let mut result = Vec::with_capacity(contexts.len() as usize);
        for ctx in contexts.iter() {
            result.push(parse_context(&ctx)?);
        }
        Ok(result)
    }

    /// Create a new context
    pub async fn create_context(&self, name: &str) -> Result<Context, RpcError> {
        let mut request = self.kernel.create_context_request();
        request.get().set_name(name);
        let response = request.send().promise.await?;
        let ctx = response.get()?.get_context()?;
        parse_context(&ctx)
    }

    /// Join a context (creates if doesn't exist)
    pub async fn join_context(&self, context_name: &str, instance: &str) -> Result<SeatHandle, RpcError> {
        let mut request = self.kernel.join_context_request();
        request.get().set_context_name(context_name);
        request.get().set_instance(instance);
        let response = request.send().promise.await?;
        let handle = response.get()?.get_seat()?;

        // Note: We don't know the nick here since it's derived server-side
        // The caller should get the actual seat info from the handle
        Ok(SeatHandle {
            handle,
            id: SeatId {
                nick: String::new(), // Will be filled in by get_state()
                instance: instance.to_string(),
                kernel: String::new(),
                context: context_name.to_string(),
            },
        })
    }

    /// Attach a document to a context
    pub async fn attach_document(&self, context_name: &str, document_id: &str) -> Result<(), RpcError> {
        let mut request = self.kernel.attach_document_request();
        request.get().set_context_name(context_name);
        request.get().set_document_id(document_id);
        request.send().promise.await?;
        Ok(())
    }

    /// Detach a document from a context
    pub async fn detach_document(&self, context_name: &str, document_id: &str) -> Result<(), RpcError> {
        let mut request = self.kernel.detach_document_request();
        request.get().set_context_name(context_name);
        request.get().set_document_id(document_id);
        request.send().promise.await?;
        Ok(())
    }

    // kaish execution methods

    /// Execute code in the kernel
    pub async fn execute(&self, code: &str) -> Result<u64, RpcError> {
        let mut request = self.kernel.execute_request();
        request.get().set_code(code);
        let response = request.send().promise.await?;
        Ok(response.get()?.get_exec_id())
    }

    /// Execute shell command with block output (kaish REPL mode)
    ///
    /// Creates ShellCommand and ShellOutput blocks in the specified cell.
    /// Output is streamed via block events.
    /// Returns the BlockId of the command block.
    pub async fn shell_execute(
        &self,
        code: &str,
        cell_id: &str,
    ) -> Result<kaijutsu_crdt::BlockId, RpcError> {
        let mut request = self.kernel.shell_execute_request();
        request.get().set_code(code);
        request.get().set_document_id(cell_id);
        let response = request.send().promise.await?;
        let block_id = response.get()?.get_command_block_id()?;
        Ok(kaijutsu_crdt::BlockId {
            document_id: block_id.get_document_id()?.to_string()?,
            agent_id: block_id.get_agent_id()?.to_string()?,
            seq: block_id.get_seq(),
        })
    }

    /// Interrupt an execution
    pub async fn interrupt(&self, exec_id: u64) -> Result<(), RpcError> {
        let mut request = self.kernel.interrupt_request();
        request.get().set_exec_id(exec_id);
        request.send().promise.await?;
        Ok(())
    }

    /// Get completions
    pub async fn complete(&self, partial: &str, cursor: u32) -> Result<Vec<Completion>, RpcError> {
        let mut request = self.kernel.complete_request();
        request.get().set_partial(partial);
        request.get().set_cursor(cursor);
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
    pub async fn get_command_history(&self, limit: u32) -> Result<Vec<HistoryEntry>, RpcError> {
        let mut request = self.kernel.get_command_history_request();
        request.get().set_limit(limit);
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
    pub async fn push_ops(&self, document_id: &str, ops: &[u8]) -> Result<u64, RpcError> {
        let mut request = self.kernel.push_ops_request();
        request.get().set_document_id(document_id);
        request.get().set_ops(ops);
        let response = request.send().promise.await?;
        Ok(response.get()?.get_ack_version())
    }

    /// Get document state (blocks and CRDT oplog)
    pub async fn get_document_state(
        &self,
        document_id: &str,
    ) -> Result<DocumentState, RpcError> {
        let mut request = self.kernel.get_document_state_request();
        request.get().set_document_id(document_id);
        let response = request.send().promise.await?;
        let state = response.get()?.get_state()?;

        let document_id = state.get_document_id()?.to_string()?;
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
            document_id,
            blocks,
            version,
            ops,
        })
    }

    // =========================================================================
    // LLM operations
    // =========================================================================

    /// Send a prompt to the server-side LLM
    ///
    /// Returns a prompt ID that can be used to track the response.
    /// The response will be streamed via block events if subscribed.
    pub async fn prompt(
        &self,
        content: &str,
        model: Option<&str>,
        cell_id: &str,
    ) -> Result<String, RpcError> {
        let mut request = self.kernel.prompt_request();
        {
            let mut req = request.get().init_request();
            req.set_content(content);
            if let Some(m) = model {
                req.set_model(m);
            }
            req.set_document_id(cell_id);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_prompt_id()?.to_string()?)
    }

    /// Subscribe to block events (for LLM streaming updates)
    ///
    /// The callback will receive block insertions, edits, and other events.
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
        let response = request.send().promise.await?;
        let result = response.get()?.get_result()?;

        Ok(McpToolResult {
            success: result.get_success(),
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
    pub async fn list_mcp_resources(&self, server: &str) -> Result<Vec<McpResource>, RpcError> {
        let mut request = self.kernel.list_mcp_resources_request();
        request.get().set_server(server);
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
    pub async fn read_mcp_resource(
        &self,
        server: &str,
        uri: &str,
    ) -> Result<Option<McpResourceContents>, RpcError> {
        let mut request = self.kernel.read_mcp_resource_request();
        request.get().set_server(server);
        request.get().set_uri(uri);
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
    /// Returns the newly created context.
    pub async fn fork_from_version(
        &self,
        document_id: &str,
        version: u64,
        context_name: &str,
    ) -> Result<Context, RpcError> {
        let mut request = self.kernel.fork_from_version_request();
        {
            let mut params = request.get();
            params.set_document_id(document_id);
            params.set_version(version);
            params.set_context_name(context_name);
        }
        let response = request.send().promise.await?;
        let ctx = response.get()?.get_context()?;
        parse_context(&ctx)
    }

    /// Cherry-pick a block from one context into another.
    ///
    /// Returns the new block ID in the target context.
    pub async fn cherry_pick_block(
        &self,
        block_id: &kaijutsu_crdt::BlockId,
        target_context: &str,
    ) -> Result<kaijutsu_crdt::BlockId, RpcError> {
        let mut request = self.kernel.cherry_pick_block_request();
        {
            let mut params = request.get();
            let mut source = params.reborrow().init_source_block_id();
            source.set_document_id(&block_id.document_id);
            source.set_agent_id(&block_id.agent_id);
            source.set_seq(block_id.seq);
            params.set_target_context(target_context);
        }
        let response = request.send().promise.await?;
        let new_block = response.get()?.get_new_block_id()?;
        parse_block_id(&new_block)
    }

    /// Get document history (version snapshots).
    ///
    /// Returns a list of version snapshots for timeline navigation.
    pub async fn get_document_history(
        &self,
        document_id: &str,
        limit: u32,
    ) -> Result<Vec<VersionSnapshot>, RpcError> {
        let mut request = self.kernel.get_document_history_request();
        {
            let mut params = request.get();
            params.set_document_id(document_id);
            params.set_limit(limit);
        }
        let response = request.send().promise.await?;
        let snapshots = response.get()?.get_snapshots()?;

        let mut result = Vec::with_capacity(snapshots.len() as usize);
        for snap in snapshots.iter() {
            result.push(VersionSnapshot {
                version: snap.get_version(),
                timestamp: snap.get_timestamp(),
                block_count: snap.get_block_count(),
                change_kind: snap.get_change_kind()?.to_string()?,
            });
        }
        Ok(result)
    }

    // ========================================================================
    // Drift: Cross-Context Communication
    // ========================================================================

    /// Get this kernel's context short ID and name.
    pub async fn get_context_id(&self) -> Result<(String, String), RpcError> {
        let request = self.kernel.get_context_id_request();
        let response = request.send().promise.await?;
        let reader = response.get()?;
        let short_id = reader.get_short_id()?.to_string()?;
        let name = reader.get_name()?.to_string()?;
        Ok((short_id, name))
    }

    /// Configure the LLM provider and model for this kernel.
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
    pub async fn drift_push(
        &self,
        target_ctx: &str,
        content: &str,
        summarize: bool,
    ) -> Result<u64, RpcError> {
        let mut request = self.kernel.drift_push_request();
        {
            let mut params = request.get();
            params.set_target_ctx(target_ctx);
            params.set_content(content);
            params.set_summarize(summarize);
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_staged_id())
    }

    /// Flush all staged drifts.
    pub async fn drift_flush(&self) -> Result<u32, RpcError> {
        let request = self.kernel.drift_flush_request();
        let response = request.send().promise.await?;
        Ok(response.get()?.get_count())
    }

    /// View the drift staging queue.
    pub async fn drift_queue(&self) -> Result<Vec<StagedDriftInfo>, RpcError> {
        let request = self.kernel.drift_queue_request();
        let response = request.send().promise.await?;
        let staged = response.get()?.get_staged()?;

        let mut result = Vec::with_capacity(staged.len() as usize);
        for entry in staged.iter() {
            result.push(StagedDriftInfo {
                id: entry.get_id(),
                source_ctx: entry.get_source_ctx()?.to_string()?,
                target_ctx: entry.get_target_ctx()?.to_string()?,
                content: entry.get_content()?.to_string()?,
                source_model: entry.get_source_model()?.to_string()?,
                drift_kind: entry.get_drift_kind()?.to_string()?,
                created_at: entry.get_created_at(),
            });
        }
        Ok(result)
    }

    /// Cancel a staged drift.
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
    pub async fn drift_pull(
        &self,
        source_ctx: &str,
        prompt: Option<&str>,
    ) -> Result<kaijutsu_crdt::BlockId, RpcError> {
        let mut request = self.kernel.drift_pull_request();
        request.get().set_source_ctx(source_ctx);
        if let Some(p) = prompt {
            request.get().set_prompt(p);
        }
        let response = request.send().promise.await?;
        let block_id_reader = response.get()?.get_block_id()?;
        parse_block_id(&block_id_reader)
    }

    /// Merge a forked context back into its parent.
    ///
    /// Distills the source context's conversation and injects the summary
    /// into the parent context as a Drift block with DriftKind::Merge.
    pub async fn drift_merge(
        &self,
        source_ctx: &str,
    ) -> Result<kaijutsu_crdt::BlockId, RpcError> {
        let mut request = self.kernel.drift_merge_request();
        request.get().set_source_ctx(source_ctx);
        let response = request.send().promise.await?;
        let block_id_reader = response.get()?.get_block_id()?;
        parse_block_id(&block_id_reader)
    }

    /// List all registered contexts.
    pub async fn list_all_contexts(&self) -> Result<Vec<ContextInfo>, RpcError> {
        let request = self.kernel.list_all_contexts_request();
        let response = request.send().promise.await?;
        let contexts = response.get()?.get_contexts()?;

        let mut result = Vec::with_capacity(contexts.len() as usize);
        for ctx in contexts.iter() {
            result.push(ContextInfo {
                short_id: ctx.get_short_id()?.to_string()?,
                name: ctx.get_name()?.to_string()?,
                kernel_id: ctx.get_kernel_id()?.to_string()?,
                provider: ctx.get_provider()?.to_string()?,
                model: ctx.get_model()?.to_string()?,
                parent_id: if ctx.get_has_parent_id() {
                    Some(ctx.get_parent_id()?.to_string()?)
                } else {
                    None
                },
                created_at: ctx.get_created_at(),
            });
        }
        Ok(result)
    }

    // ========================================================================
    // LLM Configuration
    // ========================================================================

    /// Get current LLM configuration
    pub async fn get_llm_config(&self) -> Result<LlmConfigInfo, RpcError> {
        let request = self.kernel.get_llm_config_request();
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
}

// ============================================================================
// SeatHandle
// ============================================================================

/// Handle to an active seat
pub struct SeatHandle {
    handle: crate::kaijutsu_capnp::seat_handle::Client,
    id: SeatId,
}

impl SeatHandle {
    /// Get seat ID
    pub fn id(&self) -> &SeatId {
        &self.id
    }

    /// Get current seat state
    pub async fn get_state(&self) -> Result<SeatInfo, RpcError> {
        let request = self.handle.get_state_request();
        let response = request.send().promise.await?;
        let info = response.get()?.get_info()?;
        parse_seat_info(&info)
    }

    /// Update cursor position
    pub async fn update_cursor(&self, block_id: &str) -> Result<(), RpcError> {
        let mut request = self.handle.update_cursor_request();
        request.get().set_block_id(block_id);
        request.send().promise.await?;
        Ok(())
    }

    /// Set seat status
    pub async fn set_status(&self, status: SeatStatus) -> Result<(), RpcError> {
        let mut request = self.handle.set_status_request();
        request.get().set_status(match status {
            SeatStatus::Active => crate::kaijutsu_capnp::SeatStatus::Active,
            SeatStatus::Idle => crate::kaijutsu_capnp::SeatStatus::Idle,
            SeatStatus::Away => crate::kaijutsu_capnp::SeatStatus::Away,
        });
        request.send().promise.await?;
        Ok(())
    }

    /// Leave the seat
    pub async fn leave(self) -> Result<(), RpcError> {
        let request = self.handle.leave_request();
        request.send().promise.await?;
        Ok(())
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Helper to parse SeatInfo from Cap'n Proto
fn parse_seat_info(
    reader: &crate::kaijutsu_capnp::seat_info::Reader<'_>,
) -> Result<SeatInfo, RpcError> {
    let id_reader = reader.get_id()?;
    let id = SeatId {
        nick: id_reader.get_nick()?.to_string()?,
        instance: id_reader.get_instance()?.to_string()?,
        kernel: id_reader.get_kernel()?.to_string()?,
        context: id_reader.get_context()?.to_string()?,
    };

    let status = match reader.get_status()? {
        crate::kaijutsu_capnp::SeatStatus::Active => SeatStatus::Active,
        crate::kaijutsu_capnp::SeatStatus::Idle => SeatStatus::Idle,
        crate::kaijutsu_capnp::SeatStatus::Away => SeatStatus::Away,
    };

    let cursor_block = {
        let s = reader.get_cursor_block()?.to_string()?;
        if s.is_empty() { None } else { Some(s) }
    };

    Ok(SeatInfo {
        id,
        owner: reader.get_owner()?.to_string()?,
        status,
        last_activity: reader.get_last_activity(),
        cursor_block,
    })
}

/// Helper to parse Context from Cap'n Proto
fn parse_context(
    reader: &crate::kaijutsu_capnp::context::Reader<'_>,
) -> Result<Context, RpcError> {
    let name = reader.get_name()?.to_string()?;

    let docs_reader = reader.get_documents()?;
    let mut documents = Vec::with_capacity(docs_reader.len() as usize);
    for doc in docs_reader.iter() {
        documents.push(ContextDocument {
            id: doc.get_id()?.to_string()?,
            attached_by: doc.get_attached_by()?.to_string()?,
            attached_at: doc.get_attached_at(),
        });
    }

    let seats_reader = reader.get_seats()?;
    let mut seats = Vec::with_capacity(seats_reader.len() as usize);
    for seat in seats_reader.iter() {
        seats.push(parse_seat_info(&seat)?);
    }

    Ok(Context {
        name,
        documents,
        seats,
    })
}

/// Helper to parse KernelInfo from Cap'n Proto
fn parse_kernel_info(
    reader: &crate::kaijutsu_capnp::kernel_info::Reader<'_>,
) -> Result<KernelInfo, RpcError> {
    let contexts_reader = reader.get_contexts()?;
    let mut contexts = Vec::with_capacity(contexts_reader.len() as usize);
    for ctx in contexts_reader.iter() {
        contexts.push(parse_context(&ctx)?);
    }

    Ok(KernelInfo {
        id: reader.get_id()?.to_string()?,
        name: reader.get_name()?.to_string()?,
        user_count: reader.get_user_count(),
        agent_count: reader.get_agent_count(),
        contexts,
        seat_count: reader.get_seat_count(),
    })
}

/// Helper to parse block ID from Cap'n Proto
fn parse_block_id(
    reader: &crate::kaijutsu_capnp::block_id::Reader<'_>,
) -> Result<kaijutsu_crdt::BlockId, RpcError> {
    Ok(kaijutsu_crdt::BlockId {
        document_id: reader.get_document_id()?.to_string()?,
        agent_id: reader.get_agent_id()?.to_string()?,
        seq: reader.get_seq(),
    })
}

/// Helper to parse a flat BlockSnapshot from Cap'n Proto
fn parse_block_snapshot(
    reader: &crate::kaijutsu_capnp::block_snapshot::Reader<'_>,
) -> Result<kaijutsu_crdt::BlockSnapshot, RpcError> {
    use kaijutsu_crdt::{BlockKind, BlockSnapshot, Role, Status};

    // Parse block ID
    let id = parse_block_id(&reader.get_id()?)?;

    // Parse parent_id if present
    let parent_id = if reader.get_has_parent_id() {
        Some(parse_block_id(&reader.get_parent_id()?)?)
    } else {
        None
    };

    // Parse role
    let role = match reader.get_role()? {
        crate::kaijutsu_capnp::Role::User => Role::User,
        crate::kaijutsu_capnp::Role::Model => Role::Model,
        crate::kaijutsu_capnp::Role::System => Role::System,
        crate::kaijutsu_capnp::Role::Tool => Role::Tool,
    };

    // Parse status
    let status = match reader.get_status()? {
        crate::kaijutsu_capnp::Status::Pending => Status::Pending,
        crate::kaijutsu_capnp::Status::Running => Status::Running,
        crate::kaijutsu_capnp::Status::Done => Status::Done,
        crate::kaijutsu_capnp::Status::Error => Status::Error,
    };

    // Parse kind
    let kind = match reader.get_kind()? {
        crate::kaijutsu_capnp::BlockKind::Text => BlockKind::Text,
        crate::kaijutsu_capnp::BlockKind::Thinking => BlockKind::Thinking,
        crate::kaijutsu_capnp::BlockKind::ToolCall => BlockKind::ToolCall,
        crate::kaijutsu_capnp::BlockKind::ToolResult => BlockKind::ToolResult,
        crate::kaijutsu_capnp::BlockKind::ShellCommand => BlockKind::ShellCommand,
        crate::kaijutsu_capnp::BlockKind::ShellOutput => BlockKind::ShellOutput,
        crate::kaijutsu_capnp::BlockKind::Drift => BlockKind::Drift,
    };

    // Parse content
    let content = reader.get_content()?.to_string()?;
    let collapsed = reader.get_collapsed();
    let author = reader.get_author()?.to_string()?;
    let created_at = reader.get_created_at();

    // Parse tool-specific fields
    let tool_name = if reader.has_tool_name() {
        Some(reader.get_tool_name()?.to_string()?)
    } else {
        None
    };

    let tool_input = if reader.has_tool_input() {
        let input_str = reader.get_tool_input()?.to_string()?;
        match serde_json::from_str(&input_str) {
            Ok(v) => Some(v),
            Err(e) => {
                log::warn!("Failed to parse tool_input as JSON: {}", e);
                None
            }
        }
    } else {
        None
    };

    let tool_call_id = if reader.get_has_tool_call_id() {
        Some(parse_block_id(&reader.get_tool_call_id()?)?)
    } else {
        None
    };

    let exit_code = if reader.get_has_exit_code() {
        Some(reader.get_exit_code())
    } else {
        None
    };

    let is_error = reader.get_is_error();

    // Read display hint from wire protocol
    let display_hint = if reader.get_has_display_hint() {
        reader.get_display_hint()
            .ok()
            .and_then(|s| s.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned())
    } else {
        None
    };

    Ok(BlockSnapshot {
        id,
        parent_id,
        role,
        status,
        kind,
        content,
        collapsed,
        author,
        created_at,
        tool_name,
        tool_input,
        tool_call_id,
        exit_code,
        is_error,
        display_hint,
        source_context: if reader.get_has_source_context() {
            reader.get_source_context().ok()
                .and_then(|s| s.to_str().ok())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_owned())
        } else { None },
        source_model: if reader.get_has_source_model() {
            reader.get_source_model().ok()
                .and_then(|s| s.to_str().ok())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_owned())
        } else { None },
        drift_kind: if reader.get_has_drift_kind() {
            reader.get_drift_kind().ok()
                .and_then(|s| s.to_str().ok())
                .and_then(|s| DriftKind::from_str(s))
        } else { None },
    })
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
    pub source_ctx: String,
    pub target_ctx: String,
    pub content: String,
    pub source_model: String,
    pub drift_kind: String,
    pub created_at: u64,
}

/// Info about a registered drift context.
#[derive(Debug, Clone)]
pub struct ContextInfo {
    pub short_id: String,
    pub name: String,
    pub kernel_id: String,
    pub provider: String,
    pub model: String,
    pub parent_id: Option<String>,
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

// ============================================================================
// Block Types
// ============================================================================

/// Full document state with blocks
#[derive(Debug, Clone)]
pub struct DocumentState {
    pub document_id: String,
    pub blocks: Vec<kaijutsu_crdt::BlockSnapshot>,
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
    pub success: bool,
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
