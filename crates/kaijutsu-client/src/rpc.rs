//! Cap'n Proto RPC client for kaijutsu
//!
//! Provides typed interface to the World and Kernel capabilities.

use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use futures::AsyncReadExt;
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
            result.push(KernelInfo {
                id: kernel.get_id()?.to_string()?,
                name: kernel.get_name()?.to_string()?,
                user_count: kernel.get_user_count(),
                agent_count: kernel.get_agent_count(),
            });
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

    /// List user's active seats across all kernels
    pub async fn list_my_seats(&self) -> Result<Vec<SeatInfo>, RpcError> {
        let request = self.world.list_my_seats_request();
        let response = request.send().promise.await?;
        let seats = response.get()?.get_seats()?;

        let mut result = Vec::with_capacity(seats.len() as usize);
        for seat in seats.iter() {
            let id_reader = seat.get_id()?;
            result.push(SeatInfo {
                id: SeatId {
                    nick: id_reader.get_nick()?.to_string()?,
                    instance: id_reader.get_instance()?.to_string()?,
                    kernel: id_reader.get_kernel()?.to_string()?,
                    context: id_reader.get_context()?.to_string()?,
                },
                owner: seat.get_owner()?.to_string()?,
                document_id: seat.get_document_id()?.to_string()?,
            });
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
}

// ============================================================================
// Seat & Context Types (client-side stubs for dashboard wiring)
// ============================================================================

/// Unique identifier for a seat - the 4-tuple that identifies a user's position
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatId {
    /// Display name (user-chosen): "amy", "refactor-bot"
    pub nick: String,
    /// Device/model variant: "laptop", "haiku"
    pub instance: String,
    /// Kernel name: "kaijutsu-dev"
    pub kernel: String,
    /// Context within the kernel: "refactor", "planning"
    pub context: String,
}

/// Information about a seat (occupied position in a kernel/context)
#[derive(Debug, Clone)]
pub struct SeatInfo {
    pub id: SeatId,
    /// Strong identity: username from SSH auth
    pub owner: String,
    /// The kernel's main document ID for this seat
    pub document_id: String,
}

/// A context within a kernel - a collection of documents with a focus scope
#[derive(Debug, Clone)]
pub struct Context {
    pub name: String,
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
pub struct KernelHandle {
    kernel: crate::kaijutsu_capnp::kernel::Client,
}

impl KernelHandle {
    /// Get kernel info
    pub async fn get_info(&self) -> Result<KernelInfo, RpcError> {
        let request = self.kernel.get_info_request();
        let response = request.send().promise.await?;
        let info = response.get()?.get_info()?;
        Ok(KernelInfo {
            id: info.get_id()?.to_string()?,
            name: info.get_name()?.to_string()?,
            user_count: info.get_user_count(),
            agent_count: info.get_agent_count(),
        })
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
    // Context & Seat operations
    // =========================================================================

    /// List contexts in this kernel
    pub async fn list_contexts(&self) -> Result<Vec<Context>, RpcError> {
        let request = self.kernel.list_contexts_request();
        let response = request.send().promise.await?;
        let contexts = response.get()?.get_contexts()?;

        let mut result = Vec::with_capacity(contexts.len() as usize);
        for ctx in contexts.iter() {
            result.push(Context {
                name: ctx.get_name()?.to_string()?,
            });
        }
        Ok(result)
    }

    /// Join a context (creates a seat)
    pub async fn join_context(
        &self,
        context_name: &str,
        instance: &str,
    ) -> Result<SeatInfo, RpcError> {
        let mut request = self.kernel.join_context_request();
        request.get().set_context_name(context_name);
        request.get().set_instance(instance);
        let response = request.send().promise.await?;
        let seat = response.get()?.get_seat()?;
        let id_reader = seat.get_id()?;

        Ok(SeatInfo {
            id: SeatId {
                nick: id_reader.get_nick()?.to_string()?,
                instance: id_reader.get_instance()?.to_string()?,
                kernel: id_reader.get_kernel()?.to_string()?,
                context: id_reader.get_context()?.to_string()?,
            },
            owner: seat.get_owner()?.to_string()?,
            document_id: seat.get_document_id()?.to_string()?,
        })
    }

    /// Leave current seat
    pub async fn leave_seat(&self) -> Result<(), RpcError> {
        let request = self.kernel.leave_seat_request();
        request.send().promise.await?;
        Ok(())
    }

    // =========================================================================
    // MCP Tool operations
    // =========================================================================

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
