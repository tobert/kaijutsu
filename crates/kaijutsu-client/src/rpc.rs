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

    /// Send a message to the kernel
    pub async fn send(&self, content: &str) -> Result<Row, RpcError> {
        let mut request = self.kernel.send_request();
        request.get().set_content(content);
        let response = request.send().promise.await?;
        let row = response.get()?.get_row()?;
        Row::from_capnp(&row)
    }

    /// Mention an agent
    pub async fn mention(&self, agent: &str, content: &str) -> Result<Row, RpcError> {
        let mut request = self.kernel.mention_request();
        request.get().set_agent(agent);
        request.get().set_content(content);
        let response = request.send().promise.await?;
        let row = response.get()?.get_row()?;
        Row::from_capnp(&row)
    }

    /// Get kernel history
    pub async fn get_history(&self, limit: u32, before_id: u64) -> Result<Vec<Row>, RpcError> {
        let mut request = self.kernel.get_history_request();
        request.get().set_limit(limit);
        request.get().set_before_id(before_id);
        let response = request.send().promise.await?;
        let rows = response.get()?.get_rows()?;

        let mut result = Vec::with_capacity(rows.len() as usize);
        for row in rows.iter() {
            result.push(Row::from_capnp(&row)?);
        }
        Ok(result)
    }

    // kaish execution methods (directly on Kernel)

    /// Execute code in the kernel
    pub async fn execute(&self, code: &str) -> Result<u64, RpcError> {
        let mut request = self.kernel.execute_request();
        request.get().set_code(code);
        let response = request.send().promise.await?;
        Ok(response.get()?.get_exec_id())
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
    // Cell CRDT sync methods
    // =========================================================================

    /// List all cells in the kernel
    pub async fn list_cells(&self) -> Result<Vec<CellInfo>, RpcError> {
        let request = self.kernel.list_cells_request();
        let response = request.send().promise.await?;
        let cells = response.get()?.get_cells()?;

        let mut result = Vec::with_capacity(cells.len() as usize);
        for c in cells.iter() {
            result.push(CellInfo::from_capnp(&c)?);
        }
        Ok(result)
    }

    /// Get full cell state including CRDT document
    pub async fn get_cell(&self, cell_id: &str) -> Result<CellState, RpcError> {
        let mut request = self.kernel.get_cell_request();
        request.get().set_cell_id(cell_id);
        let response = request.send().promise.await?;
        let cell = response.get()?.get_cell()?;
        CellState::from_capnp(&cell)
    }

    /// Create a new cell
    pub async fn create_cell(
        &self,
        kind: CellKind,
        language: Option<&str>,
        parent_id: Option<&str>,
    ) -> Result<CellState, RpcError> {
        let mut request = self.kernel.create_cell_request();
        {
            let mut builder = request.get();
            builder.set_kind(kind.to_capnp());
            if let Some(lang) = language {
                builder.set_language(lang);
            }
            if let Some(parent) = parent_id {
                builder.set_parent_id(parent);
            }
        }
        let response = request.send().promise.await?;
        let cell = response.get()?.get_cell()?;
        CellState::from_capnp(&cell)
    }

    /// Delete a cell
    pub async fn delete_cell(&self, cell_id: &str) -> Result<(), RpcError> {
        let mut request = self.kernel.delete_cell_request();
        request.get().set_cell_id(cell_id);
        request.send().promise.await?;
        Ok(())
    }

    /// Apply a CRDT operation to a cell
    pub async fn apply_op(&self, op: CellOp) -> Result<u64, RpcError> {
        let mut request = self.kernel.apply_op_request();
        {
            let mut op_builder = request.get().init_op();
            op_builder.set_cell_id(&op.cell_id);
            op_builder.set_client_version(op.client_version);
            let mut crdt_op = op_builder.init_op();
            match op.op {
                CrdtOp::Insert { pos, ref text } => {
                    let mut insert = crdt_op.init_insert();
                    insert.set_pos(pos);
                    insert.set_text(text);
                }
                CrdtOp::Delete { pos, len } => {
                    let mut delete = crdt_op.init_delete();
                    delete.set_pos(pos);
                    delete.set_len(len);
                }
                CrdtOp::FullState(ref data) => {
                    crdt_op.set_full_state(data);
                }
            }
        }
        let response = request.send().promise.await?;
        Ok(response.get()?.get_new_version())
    }

    /// Sync cells with the server (get patches for known cells, full state for new)
    pub async fn sync_cells(
        &self,
        versions: Vec<CellVersion>,
    ) -> Result<(Vec<CellPatch>, Vec<CellState>), RpcError> {
        let mut request = self.kernel.sync_cells_request();
        {
            let mut from_versions = request.get().init_from_versions(versions.len() as u32);
            for (i, v) in versions.iter().enumerate() {
                let mut cv = from_versions.reborrow().get(i as u32);
                cv.set_cell_id(&v.cell_id);
                cv.set_version(v.version);
            }
        }
        let response = request.send().promise.await?;
        let result = response.get()?;

        // Parse patches
        let patches_reader = result.get_patches()?;
        let mut patches = Vec::with_capacity(patches_reader.len() as usize);
        for p in patches_reader.iter() {
            patches.push(CellPatch::from_capnp(&p)?);
        }

        // Parse new cells
        let new_cells_reader = result.get_new_cells()?;
        let mut new_cells = Vec::with_capacity(new_cells_reader.len() as usize);
        for c in new_cells_reader.iter() {
            new_cells.push(CellState::from_capnp(&c)?);
        }

        Ok((patches, new_cells))
    }

    // =========================================================================
    // Block-based CRDT sync methods (new architecture)
    // =========================================================================

    // NOTE: apply_block_op was removed - the unified CRDT model uses SerializedOps
    // for replication. See BlockDocument::ops_since() and apply_ops() in kaijutsu-crdt.

    /// Get block cell state
    pub async fn get_block_cell_state(
        &self,
        cell_id: &str,
    ) -> Result<(Vec<(kaijutsu_crdt::BlockId, kaijutsu_crdt::BlockContentSnapshot)>, u64), RpcError>
    {
        let mut request = self.kernel.get_block_cell_state_request();
        request.get().set_cell_id(cell_id);
        let response = request.send().promise.await?;
        let state = response.get()?.get_state()?;

        let version = state.get_version();
        let blocks_reader = state.get_blocks()?;
        let mut blocks = Vec::with_capacity(blocks_reader.len() as usize);

        for block in blocks_reader.iter() {
            let id = parse_block_id(&block.get_id()?)?;
            let content = parse_block_content(&block.get_content()?)?;
            blocks.push((id, content));
        }

        Ok((blocks, version))
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
            req.set_cell_id(cell_id);
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
}

/// Helper to build block content from snapshot
fn build_block_content(
    mut builder: crate::kaijutsu_capnp::block_content::Builder<'_>,
    content: &kaijutsu_crdt::BlockContentSnapshot,
) {
    use kaijutsu_crdt::BlockContentSnapshot;

    match content {
        BlockContentSnapshot::Thinking { text, collapsed } => {
            let mut thinking = builder.init_thinking();
            thinking.set_text(text);
            thinking.set_collapsed(*collapsed);
        }
        BlockContentSnapshot::Text { text } => {
            builder.set_text(text);
        }
        BlockContentSnapshot::ToolUse { id, name, input } => {
            let mut tool_use = builder.init_tool_use();
            tool_use.set_id(id);
            tool_use.set_name(name);
            tool_use.set_input(&input.to_string());
        }
        BlockContentSnapshot::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let mut tool_result = builder.init_tool_result();
            tool_result.set_tool_use_id(tool_use_id);
            tool_result.set_content(content);
            tool_result.set_is_error(*is_error);
        }
    }
}

/// Helper to parse block ID from Cap'n Proto
fn parse_block_id(
    reader: &crate::kaijutsu_capnp::block_id::Reader<'_>,
) -> Result<kaijutsu_crdt::BlockId, RpcError> {
    Ok(kaijutsu_crdt::BlockId {
        cell_id: reader.get_cell_id()?.to_string()?,
        agent_id: reader.get_agent_id()?.to_string()?,
        seq: reader.get_seq(),
    })
}

/// Helper to parse block content from Cap'n Proto
fn parse_block_content(
    reader: &crate::kaijutsu_capnp::block_content::Reader<'_>,
) -> Result<kaijutsu_crdt::BlockContentSnapshot, RpcError> {
    use crate::kaijutsu_capnp::block_content::Which;
    use kaijutsu_crdt::BlockContentSnapshot;

    match reader.which()? {
        Which::Thinking(thinking) => {
            Ok(BlockContentSnapshot::Thinking {
                text: thinking.get_text()?.to_string()?,
                collapsed: thinking.get_collapsed(),
            })
        }
        Which::Text(text) => Ok(BlockContentSnapshot::Text {
            text: text?.to_string()?,
        }),
        Which::ToolUse(tool_use) => {
            let input_str = tool_use.get_input()?.to_string()?;
            let input: serde_json::Value = match serde_json::from_str(&input_str) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("Failed to parse block content as JSON: {}", e);
                    serde_json::Value::Null
                }
            };
            Ok(BlockContentSnapshot::ToolUse {
                id: tool_use.get_id()?.to_string()?,
                name: tool_use.get_name()?.to_string()?,
                input,
            })
        }
        Which::ToolResult(tool_result) => {
            Ok(BlockContentSnapshot::ToolResult {
                tool_use_id: tool_result.get_tool_use_id()?.to_string()?,
                content: tool_result.get_content()?.to_string()?,
                is_error: tool_result.get_is_error(),
            })
        }
    }
}

#[derive(Debug, Clone)]
pub struct Row {
    pub id: u64,
    pub parent_id: u64,
    pub row_type: RowType,
    pub sender: String,
    pub content: String,
    pub timestamp: u64,
}

impl Row {
    fn from_capnp(row: &crate::kaijutsu_capnp::row::Reader<'_>) -> Result<Self, RpcError> {
        Ok(Self {
            id: row.get_id(),
            parent_id: row.get_parent_id(),
            row_type: RowType::from_capnp(row.get_row_type()?),
            sender: row.get_sender()?.to_string()?,
            content: row.get_content()?.to_string()?,
            timestamp: row.get_timestamp(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowType {
    Chat,
    AgentResponse,
    ToolCall,
    ToolResult,
    SystemMessage,
}

impl RowType {
    fn from_capnp(rt: crate::kaijutsu_capnp::RowType) -> Self {
        match rt {
            crate::kaijutsu_capnp::RowType::Chat => RowType::Chat,
            crate::kaijutsu_capnp::RowType::AgentResponse => RowType::AgentResponse,
            crate::kaijutsu_capnp::RowType::ToolCall => RowType::ToolCall,
            crate::kaijutsu_capnp::RowType::ToolResult => RowType::ToolResult,
            crate::kaijutsu_capnp::RowType::SystemMessage => RowType::SystemMessage,
        }
    }
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
// Cell Types
// ============================================================================

/// Type of cell content
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    Code,
    Markdown,
    Output,
    System,
    UserMessage,
    AgentMessage,
}

impl CellKind {
    fn from_capnp(kind: crate::kaijutsu_capnp::CellKind) -> Self {
        match kind {
            crate::kaijutsu_capnp::CellKind::Code => CellKind::Code,
            crate::kaijutsu_capnp::CellKind::Markdown => CellKind::Markdown,
            crate::kaijutsu_capnp::CellKind::Output => CellKind::Output,
            crate::kaijutsu_capnp::CellKind::System => CellKind::System,
            crate::kaijutsu_capnp::CellKind::UserMessage => CellKind::UserMessage,
            crate::kaijutsu_capnp::CellKind::AgentMessage => CellKind::AgentMessage,
        }
    }

    fn to_capnp(self) -> crate::kaijutsu_capnp::CellKind {
        match self {
            CellKind::Code => crate::kaijutsu_capnp::CellKind::Code,
            CellKind::Markdown => crate::kaijutsu_capnp::CellKind::Markdown,
            CellKind::Output => crate::kaijutsu_capnp::CellKind::Output,
            CellKind::System => crate::kaijutsu_capnp::CellKind::System,
            CellKind::UserMessage => crate::kaijutsu_capnp::CellKind::UserMessage,
            CellKind::AgentMessage => crate::kaijutsu_capnp::CellKind::AgentMessage,
        }
    }
}

/// Basic cell metadata
#[derive(Debug, Clone)]
pub struct CellInfo {
    pub id: String,
    pub kind: CellKind,
    pub language: Option<String>,
    pub parent_id: Option<String>,
}

impl CellInfo {
    fn from_capnp(info: &crate::kaijutsu_capnp::cell_info::Reader<'_>) -> Result<Self, RpcError> {
        Ok(Self {
            id: info.get_id()?.to_string()?,
            kind: CellKind::from_capnp(info.get_kind()?),
            language: {
                let lang = info.get_language()?;
                if lang.is_empty() {
                    None
                } else {
                    Some(lang.to_string()?)
                }
            },
            parent_id: {
                let parent = info.get_parent_id()?;
                if parent.is_empty() {
                    None
                } else {
                    Some(parent.to_string()?)
                }
            },
        })
    }
}

/// Full cell state including content and CRDT document
#[derive(Debug, Clone)]
pub struct CellState {
    pub info: CellInfo,
    pub content: String,
    pub version: u64,
    pub encoded_doc: Option<Vec<u8>>,
}

impl CellState {
    fn from_capnp(cell: &crate::kaijutsu_capnp::cell_state::Reader<'_>) -> Result<Self, RpcError> {
        Ok(Self {
            info: CellInfo::from_capnp(&cell.get_info()?)?,
            content: cell.get_content()?.to_string()?,
            version: cell.get_version(),
            encoded_doc: {
                let data = cell.get_encoded_doc()?;
                if data.is_empty() {
                    None
                } else {
                    Some(data.to_vec())
                }
            },
        })
    }
}

/// A patch for syncing a cell from one version to another
#[derive(Debug, Clone)]
pub struct CellPatch {
    pub cell_id: String,
    pub from_version: u64,
    pub to_version: u64,
    pub ops: Vec<u8>,
}

impl CellPatch {
    fn from_capnp(patch: &crate::kaijutsu_capnp::cell_patch::Reader<'_>) -> Result<Self, RpcError> {
        Ok(Self {
            cell_id: patch.get_cell_id()?.to_string()?,
            from_version: patch.get_from_version(),
            to_version: patch.get_to_version(),
            ops: patch.get_ops()?.to_vec(),
        })
    }
}

/// Client-side cell version for sync requests
#[derive(Debug, Clone)]
pub struct CellVersion {
    pub cell_id: String,
    pub version: u64,
}

/// A CRDT operation to apply to a cell
#[derive(Debug, Clone)]
pub struct CellOp {
    pub cell_id: String,
    pub client_version: u64,
    pub op: CrdtOp,
}

/// The actual CRDT operation
#[derive(Debug, Clone)]
pub enum CrdtOp {
    Insert { pos: u64, text: String },
    Delete { pos: u64, len: u64 },
    FullState(Vec<u8>),
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
