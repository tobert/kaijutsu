//! The Kernel: core primitive of kaijutsu.
//!
//! A kernel owns:
//! - A VFS (MountTable)
//! - State (variables, history, checkpoints)
//! - Tools (execution engines)
//! - LLM providers (for model access)
//! - Control plane (consent mode)

use async_trait::async_trait;
use kaijutsu_types::PrincipalId;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use uuid::Uuid;

use kaijutsu_cas::FileStore;

use crate::agents::{
    AgentActivityEvent, AgentCapability, AgentConfig, AgentError, AgentInfo, AgentRegistry,
    AgentStatus, InvokeRequest,
};
use crate::control::ConsentMode;
use crate::drift::{SharedDriftRouter, shared_drift_router};
use crate::execution::{ExecContext, ExecResult};
use crate::flows::{SharedBlockFlowBus, shared_block_flow_bus};
use crate::llm::{LlmRegistry, RigProvider};
use crate::mcp::Broker;
use crate::state::KernelState;
use crate::vfs::{DirEntry, FileAttr, MountTable, SetAttr, StatFs, VfsOps, VfsResult};

/// The Kernel: fundamental primitive of kaijutsu.
///
/// Everything is a kernel. A kernel:
/// - Owns `/` in its VFS
/// - Can mount worktrees, repos, other kernels
/// - Has a consent mode (collaborative vs autonomous)
/// - Can checkpoint, fork, and thread
pub struct Kernel {
    /// VFS mount table.
    vfs: Arc<MountTable>,
    /// Kernel state (behind RwLock for interior mutability).
    state: RwLock<KernelState>,
    /// LLM provider registry (behind RwLock for interior mutability).
    llm: RwLock<LlmRegistry>,
    /// Agent registry (behind RwLock for interior mutability).
    agents: RwLock<AgentRegistry>,
    /// Consent mode (collaborative vs autonomous).
    consent_mode: RwLock<ConsentMode>,
    /// FlowBus for block events.
    block_flows: SharedBlockFlowBus,
    /// DriftRouter for cross-context communication.
    drift: SharedDriftRouter,
    /// Content-addressed store for binary blobs (images, etc.).
    cas: Arc<FileStore>,
    /// Image generation backend registry.
    image_backends: RwLock<crate::image::ImageBackendRegistry>,
    /// MCP-centric tool broker (Phase 1; sits alongside the old `tools`
    /// registry until M4 swaps call sites).
    broker: Arc<Broker>,
}

impl std::fmt::Debug for Kernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kernel")
            .field("vfs", &self.vfs)
            .field("state", &"<locked>")
            .field("tools", &"<locked>")
            .field("llm", &"<locked>")
            .field("consent_mode", &"<locked>")
            .field("drift", &"<shared>")
            .finish()
    }
}

/// Default capacity for the block flow bus.
const DEFAULT_FLOW_CAPACITY: usize = 1024;

fn default_data_dir() -> PathBuf {
    kaish_kernel::xdg_data_home()
        .join("kaijutsu")
        .join("kernel")
}

impl Kernel {
    /// Resolve the CAS base path from a data_dir.
    fn cas_for_data_dir(data_dir: Option<&Path>) -> Arc<FileStore> {
        let cas_path = match data_dir {
            Some(dir) => dir.join("cas"),
            None => default_data_dir().join("cas"),
        };
        Arc::new(FileStore::at_path(cas_path))
    }

    /// Create a new kernel with the given name.
    ///
    /// `data_dir` is the kernel's on-disk data directory. Pass `None` to use
    /// the XDG default (`~/.local/share/kaijutsu/kernel`). CAS lives at
    /// `{data_dir}/cas/` and creates directories lazily on first write.
    pub async fn new(name: impl Into<String>, data_dir: Option<&Path>) -> Self {
        let name = name.into();
        let vfs = Arc::new(MountTable::new());

        Self {
            vfs,
            state: RwLock::new(KernelState::new(&name)),
            llm: RwLock::new(LlmRegistry::new()),
            agents: RwLock::new(AgentRegistry::new()),
            consent_mode: RwLock::new(ConsentMode::default()),
            block_flows: shared_block_flow_bus(DEFAULT_FLOW_CAPACITY),
            drift: shared_drift_router(),
            cas: Self::cas_for_data_dir(data_dir),
            image_backends: RwLock::new(crate::image::ImageBackendRegistry::new()),
            broker: Arc::new(Broker::new()),
        }
    }

    /// Create a new kernel with a shared FlowBus.
    ///
    /// Use this when you need to share the flow bus with other components
    /// (like BlockStore) before creating the kernel.
    pub async fn with_flows(
        name: impl Into<String>,
        block_flows: SharedBlockFlowBus,
        data_dir: Option<&Path>,
    ) -> Self {
        let name = name.into();
        let vfs = Arc::new(MountTable::new());

        Self {
            vfs,
            state: RwLock::new(KernelState::new(&name)),
            llm: RwLock::new(LlmRegistry::new()),
            agents: RwLock::new(AgentRegistry::new()),
            consent_mode: RwLock::new(ConsentMode::default()),
            block_flows,
            drift: shared_drift_router(),
            cas: Self::cas_for_data_dir(data_dir),
            image_backends: RwLock::new(crate::image::ImageBackendRegistry::new()),
            broker: Arc::new(Broker::new()),
        }
    }

    /// Get the MCP tool broker (Phase 1).
    pub fn broker(&self) -> &Arc<Broker> {
        &self.broker
    }

    /// Dispatch a tool call through the broker using the internal
    /// `ExecContext` call-site shape.
    ///
    /// This is the shim kaijutsu-server / kaijutsu-mcp call from the legacy
    /// dispatch sites; it resolves the tool through the context's
    /// `ContextToolBinding`, executes via the broker, and flattens the
    /// `KernelToolResult` back into an `ExecResult` so the surrounding
    /// agentic-loop error handling keeps working without further rewriting.
    ///
    /// Resolves `tool_name` through the context's `ContextToolBinding`,
    /// auto-populating the binding on first call with all registered
    /// instances.
    pub async fn dispatch_tool_via_broker(
        &self,
        tool_name: &str,
        params_json: &str,
        tool_ctx: &ExecContext,
    ) -> Result<ExecResult, crate::mcp::McpError> {
        use crate::mcp::{
            CallContext, ContextToolBinding, InstanceId, KernelCallParams, McpError, ToolContent,
            TraceContext,
        };
        use tokio_util::sync::CancellationToken;

        // Ensure a binding exists for this context. First-touch, populate
        // with every registered instance so the LLM sees everything.
        let broker = self.broker.clone();
        let binding = match broker.binding(&tool_ctx.context_id).await {
            Some(b) if !b.allowed_instances.is_empty() => b,
            _ => {
                let instances = broker.list_instances().await;
                let binding = ContextToolBinding::with_instances(instances);
                broker.set_binding(tool_ctx.context_id, binding).await;
                // Kick the resolver so `name_map` populates.
                let seed_ctx = CallContext::new(
                    tool_ctx.principal_id,
                    tool_ctx.context_id,
                    tool_ctx.session_id,
                    tool_ctx.kernel_id,
                );
                let _ = broker
                    .list_visible_tools(tool_ctx.context_id, &seed_ctx)
                    .await?;
                broker
                    .binding(&tool_ctx.context_id)
                    .await
                    .unwrap_or_default()
            }
        };

        let (instance, tool) = binding.resolve(tool_name).cloned().ok_or_else(|| {
            McpError::ToolNotFound {
                instance: InstanceId::new(""),
                tool: tool_name.to_string(),
            }
        })?;

        let arguments: serde_json::Value = if params_json.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(params_json).map_err(McpError::InvalidParams)?
        };

        let call_ctx = CallContext::new(
            tool_ctx.principal_id,
            tool_ctx.context_id,
            tool_ctx.session_id,
            tool_ctx.kernel_id,
        )
        .with_cwd(tool_ctx.cwd.clone())
        .with_trace(TraceContext::from_current_span());

        let result = broker
            .call_tool(
                KernelCallParams {
                    instance,
                    tool,
                    arguments,
                },
                &call_ctx,
                CancellationToken::new(),
            )
            .await?;

        // Flatten KernelToolResult → ExecResult. Preserve the is_error →
        // success=false convention so the existing llm_stream result arm
        // keeps working without modification.
        let mut text = String::new();
        for c in &result.content {
            match c {
                ToolContent::Text(s) => text.push_str(s),
                ToolContent::Json(v) => text.push_str(&v.to_string()),
            }
        }
        if let Some(s) = &result.structured
            && text.is_empty()
        {
            text = serde_json::to_string_pretty(s).unwrap_or_default();
        }
        if result.is_error {
            Ok(ExecResult::failure(1, text))
        } else {
            Ok(ExecResult::success(text))
        }
    }

    /// Enumerate every tool currently registered on the broker, without
    /// binding filtering. Returns `(tool_name, instance, schema,
    /// description)` quadruples. Used by admin/introspection paths (kaish
    /// CLI, capnp `get_tool_schemas`) that want the global surface.
    pub async fn list_all_registered_tools(
        &self,
    ) -> Vec<(String, crate::mcp::InstanceId, serde_json::Value, Option<String>)> {
        use crate::mcp::CallContext;
        let broker = self.broker.clone();
        let ctx = CallContext::new(
            PrincipalId::system(),
            kaijutsu_types::ContextId::new(),
            kaijutsu_types::SessionId::new(),
            kaijutsu_types::KernelId::new(),
        );
        let mut out = Vec::new();
        for instance in broker.list_instances().await {
            // Snapshot the server Arc to avoid holding the registry lock
            // across the list_tools await.
            let server = {
                let instances_guard = broker.instances_snapshot().await;
                instances_guard.get(&instance).cloned()
            };
            if let Some(server) = server
                && let Ok(tools) = server.list_tools(&ctx).await
            {
                for kt in tools {
                    out.push((
                        kt.name.clone(),
                        kt.instance.clone(),
                        kt.input_schema,
                        kt.description,
                    ));
                }
            }
        }
        out
    }

    /// List tool definitions visible to a context via the broker.
    /// Auto-populates the binding on first call. Returns `(name, schema,
    /// description)` triples suitable for LLM tool-definition construction.
    pub async fn list_tool_defs_via_broker(
        &self,
        context_id: kaijutsu_types::ContextId,
        principal_id: PrincipalId,
    ) -> Vec<(String, serde_json::Value, Option<String>)> {
        use crate::mcp::{CallContext, ContextToolBinding};

        let broker = self.broker.clone();
        if broker
            .binding(&context_id)
            .await
            .map(|b| b.allowed_instances.is_empty())
            .unwrap_or(true)
        {
            let instances = broker.list_instances().await;
            broker
                .set_binding(context_id, ContextToolBinding::with_instances(instances))
                .await;
        }
        let ctx = CallContext::new(
            principal_id,
            context_id,
            kaijutsu_types::SessionId::new(),
            kaijutsu_types::KernelId::new(),
        );
        match broker.list_visible_tools(context_id, &ctx).await {
            Ok(visible) => visible
                .into_iter()
                .map(|(visible_name, kt)| (visible_name, kt.input_schema, kt.description))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Register the Phase 1 builtin virtual MCP servers
    /// (`BlockToolsServer`, `FileToolsServer`, `KernelInfoServer`) on the
    /// broker.
    ///
    /// Callers pass the `SharedBlockStore` + `FileDocumentCache` they already
    /// have (the kernel does not own a `BlockStore`). Safe to call multiple
    /// times — subsequent calls replace the previous registrations.
    ///
    /// Registered under: `builtin.block`, `builtin.file`, `builtin.kernel_info`.
    pub async fn register_builtin_mcp_servers(
        &self,
        documents: crate::block_store::SharedBlockStore,
        file_cache: Arc<crate::file_tools::FileDocumentCache>,
        workspace_guard: Option<crate::file_tools::WorkspaceGuard>,
    ) -> crate::mcp::McpResult<()> {
        use crate::mcp::servers::{BlockToolsServer, FileToolsServer, KernelInfoServer};
        use crate::mcp::InstancePolicy;

        // Wire the block store into the broker so Phase 2 notification
        // emission can reach bound contexts (D-37). Done before registering
        // so the initial tool snapshots are captured but `register_silently`
        // suppresses the bootstrap ToolAdded noise (D-38).
        self.broker.set_documents(documents.clone()).await;

        self.broker
            .register_silently(
                Arc::new(BlockToolsServer::new(documents, self.cas.clone())),
                InstancePolicy::default(),
            )
            .await?;

        self.broker
            .register_silently(
                Arc::new(FileToolsServer::new(
                    file_cache,
                    self.vfs.clone(),
                    workspace_guard,
                )),
                InstancePolicy::default(),
            )
            .await?;

        self.broker
            .register_silently(
                Arc::new(KernelInfoServer::new(self.drift.clone())),
                InstancePolicy::default(),
            )
            .await?;

        Ok(())
    }

    /// Get the block flows bus.
    pub fn block_flows(&self) -> &SharedBlockFlowBus {
        &self.block_flows
    }

    /// Get the drift router.
    pub fn drift(&self) -> &SharedDriftRouter {
        &self.drift
    }

    /// Get the content-addressed store.
    pub fn cas(&self) -> &Arc<FileStore> {
        &self.cas
    }

    /// Get the image backend registry.
    pub fn image_backends(&self) -> &RwLock<crate::image::ImageBackendRegistry> {
        &self.image_backends
    }

    // ========================================================================
    // Identity
    // ========================================================================

    /// Get the kernel ID.
    pub async fn id(&self) -> Uuid {
        self.state.read().await.id
    }

    /// Get the kernel name.
    pub async fn name(&self) -> String {
        self.state.read().await.name.clone()
    }

    /// Set the kernel name.
    pub async fn set_name(&self, name: impl Into<String>) {
        self.state.write().await.name = name.into();
    }

    // ========================================================================
    // VFS
    // ========================================================================

    /// Get the VFS mount table.
    pub fn vfs(&self) -> &Arc<MountTable> {
        &self.vfs
    }

    /// Mount a filesystem at the given path.
    /// Returns false if the mount table is frozen.
    pub async fn mount(
        &self,
        path: impl Into<std::path::PathBuf>,
        fs: impl VfsOps + 'static,
    ) -> bool {
        self.vfs.mount(path, fs).await
    }

    /// Mount a filesystem (already wrapped in Arc) at the given path.
    /// Returns false if the mount table is frozen.
    pub async fn mount_arc(
        &self,
        path: impl Into<std::path::PathBuf>,
        fs: Arc<dyn VfsOps>,
    ) -> bool {
        self.vfs.mount_arc(path, fs).await
    }

    /// Unmount a filesystem.
    pub async fn unmount(&self, path: impl AsRef<Path>) -> bool {
        self.vfs.unmount(path).await
    }

    /// Freeze the mount table — no more mount/unmount after this.
    pub fn freeze_mounts(&self) {
        self.vfs.freeze();
    }

    /// List all mounts.
    pub async fn list_mounts(&self) -> Vec<crate::vfs::MountInfo> {
        self.vfs.list_mounts().await
    }

    // ========================================================================
    // State
    // ========================================================================

    /// Get a variable value.
    pub async fn get_var(&self, name: &str) -> Option<String> {
        self.state.read().await.get_var(name).map(|s| s.to_string())
    }

    /// Set a variable value.
    pub async fn set_var(&self, name: impl Into<String>, value: impl Into<String>) {
        self.state.write().await.set_var(name, value);
    }

    /// Unset a variable.
    pub async fn unset_var(&self, name: &str) -> Option<String> {
        self.state.write().await.unset_var(name)
    }

    /// Add a command to history.
    pub async fn add_history(&self, command: impl Into<String>) -> u64 {
        self.state.write().await.add_history(command)
    }

    /// Add a command with result to history.
    pub async fn add_history_with_result(
        &self,
        command: impl Into<String>,
        output: impl Into<String>,
        exit_code: i32,
    ) -> u64 {
        self.state
            .write()
            .await
            .add_history_with_result(command, output, exit_code)
    }

    /// Get recent history.
    pub async fn recent_history(&self, limit: usize) -> Vec<crate::state::HistoryEntry> {
        self.state.read().await.recent_history(limit).to_vec()
    }

    /// Create a checkpoint.
    pub async fn checkpoint(&self, name: impl Into<String>) -> Uuid {
        self.state.write().await.checkpoint(name)
    }

    /// Restore to a checkpoint.
    pub async fn restore_checkpoint(&self, id: Uuid) -> bool {
        self.state.write().await.restore_checkpoint(id)
    }

    // ========================================================================
    // LLM Providers
    // ========================================================================

    /// Register an LLM provider.
    pub async fn register_llm(&self, name: impl Into<String>, provider: Arc<RigProvider>) {
        self.llm.write().await.register(name, provider);
    }

    /// Set the default LLM provider.
    pub async fn set_default_llm(&self, name: &str) -> bool {
        self.llm.write().await.set_default(name)
    }

    /// Get the LLM registry (for direct access).
    pub fn llm(&self) -> &RwLock<LlmRegistry> {
        &self.llm
    }

    /// List registered LLM providers.
    pub async fn list_llm_providers(&self) -> Vec<String> {
        self.llm
            .read()
            .await
            .list()
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    }

    // ========================================================================
    // Consent Mode
    // ========================================================================

    /// Get the current consent mode.
    pub async fn consent_mode(&self) -> ConsentMode {
        *self.consent_mode.read().await
    }

    /// Set the consent mode.
    pub async fn set_consent_mode(&self, mode: ConsentMode) {
        *self.consent_mode.write().await = mode;
    }

    // ========================================================================
    // Agents
    // ========================================================================

    /// Attach an agent to this kernel.
    ///
    /// The optional `invoke_sender` enables kernel → agent invocation.
    pub async fn attach_agent(
        &self,
        config: AgentConfig,
        invoke_sender: Option<tokio::sync::mpsc::Sender<InvokeRequest>>,
    ) -> Result<AgentInfo, AgentError> {
        self.agents.write().await.attach(config, invoke_sender)
    }

    /// Invoke an agent's capability by nick.
    ///
    /// Dispatches the request to the agent's registered channel and awaits
    /// the response. The kernel-side timeout (30s) is a safety net — the
    /// client-side timeout (15s) should fire first, producing a clean
    /// `Disconnected` rather than `Timeout`.
    pub async fn invoke_agent(
        &self,
        nick: &str,
        action: &str,
        params: Vec<u8>,
    ) -> Result<Vec<u8>, AgentError> {
        const AGENT_INVOKE_TIMEOUT: Duration = Duration::from_secs(30);

        let sender = {
            let registry = self.agents.read().await;
            registry
                .get_invoke_sender(nick)
                .ok_or_else(|| AgentError::NotFound(nick.to_string()))?
        };
        // RwLock released before the async send

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let request = InvokeRequest {
            action: action.to_string(),
            params,
            reply: reply_tx,
        };

        sender.send(request).await.map_err(|_| {
            AgentError::Disconnected(format!("{}: channel closed", nick))
        })?;

        let response = tokio::time::timeout(AGENT_INVOKE_TIMEOUT, reply_rx)
            .await
            .map_err(|_| {
                AgentError::Timeout(format!(
                    "{}: no reply after {}s",
                    nick,
                    AGENT_INVOKE_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|_| {
                AgentError::Disconnected(format!("{}: handler dropped reply", nick))
            })?;

        response.result.map_err(AgentError::InvocationFailed)
    }

    /// Detach an agent from this kernel.
    pub async fn detach_agent(&self, nick: &str) -> Option<AgentInfo> {
        self.agents.write().await.detach(nick)
    }

    /// Get information about an attached agent.
    pub async fn get_agent(&self, nick: &str) -> Option<AgentInfo> {
        self.agents.read().await.get(nick).cloned()
    }

    /// List all attached agents.
    pub async fn list_agents(&self) -> Vec<AgentInfo> {
        self.agents
            .read()
            .await
            .list()
            .into_iter()
            .cloned()
            .collect()
    }

    /// Get agents with a specific capability.
    pub async fn agents_with_capability(&self, cap: AgentCapability) -> Vec<AgentInfo> {
        self.agents
            .read()
            .await
            .with_capability(cap)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Update an agent's capabilities.
    pub async fn set_agent_capabilities(
        &self,
        nick: &str,
        capabilities: Vec<AgentCapability>,
    ) -> Result<(), AgentError> {
        self.agents
            .write()
            .await
            .set_capabilities(nick, capabilities)
    }

    /// Update an agent's status.
    pub async fn set_agent_status(
        &self,
        nick: &str,
        status: AgentStatus,
    ) -> Result<(), AgentError> {
        self.agents.write().await.set_status(nick, status)
    }

    /// Subscribe to agent activity events.
    pub async fn subscribe_agent_events(
        &self,
    ) -> tokio::sync::broadcast::Receiver<AgentActivityEvent> {
        self.agents.read().await.subscribe()
    }

    /// Emit an agent activity event.
    pub async fn emit_agent_event(&self, event: AgentActivityEvent) {
        // Extract agent nick for updating last_activity
        let agent_nick = match &event {
            AgentActivityEvent::Started { agent, .. } => agent.clone(),
            AgentActivityEvent::Progress { agent, .. } => agent.clone(),
            AgentActivityEvent::Completed { agent, .. } => agent.clone(),
            AgentActivityEvent::CursorMoved { agent, .. } => agent.clone(),
        };

        // Update last_activity
        if let Some(agent) = self.agents.write().await.get_mut(&agent_nick) {
            agent.touch();
        }

        // Emit the event
        self.agents.read().await.emit(event);
    }

    /// Get the agent registry (for direct access).
    pub fn agents(&self) -> &RwLock<AgentRegistry> {
        &self.agents
    }

    /// Count of attached agents.
    pub async fn agent_count(&self) -> usize {
        self.agents.read().await.count()
    }

}

// Delegate VfsOps to the mount table
#[async_trait]
impl VfsOps for Kernel {
    async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
        self.vfs.getattr(path).await
    }

    async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
        self.vfs.readdir(path).await
    }

    async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        self.vfs.read(path, offset, size).await
    }

    async fn readlink(&self, path: &Path) -> VfsResult<std::path::PathBuf> {
        self.vfs.readlink(path).await
    }

    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32> {
        self.vfs.write(path, offset, data).await
    }

    async fn create(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        self.vfs.create(path, mode).await
    }

    async fn mkdir(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        self.vfs.mkdir(path, mode).await
    }

    async fn unlink(&self, path: &Path) -> VfsResult<()> {
        self.vfs.unlink(path).await
    }

    async fn rmdir(&self, path: &Path) -> VfsResult<()> {
        self.vfs.rmdir(path).await
    }

    async fn rename(&self, from: &Path, to: &Path) -> VfsResult<()> {
        self.vfs.rename(from, to).await
    }

    async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()> {
        self.vfs.truncate(path, size).await
    }

    async fn setattr(&self, path: &Path, attr: SetAttr) -> VfsResult<FileAttr> {
        self.vfs.setattr(path, attr).await
    }

    async fn symlink(&self, path: &Path, target: &Path) -> VfsResult<FileAttr> {
        self.vfs.symlink(path, target).await
    }

    async fn link(&self, oldpath: &Path, newpath: &Path) -> VfsResult<FileAttr> {
        self.vfs.link(oldpath, newpath).await
    }

    fn read_only(&self) -> bool {
        self.vfs.read_only()
    }

    async fn statfs(&self) -> VfsResult<StatFs> {
        self.vfs.statfs().await
    }

    async fn real_path(&self, path: &Path) -> VfsResult<Option<std::path::PathBuf>> {
        self.vfs.real_path(path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_kernel_creation() {
        let kernel = Kernel::new("test", None).await;
        assert_eq!(kernel.name().await, "test");
    }

    #[tokio::test]
    async fn test_variables() {
        let kernel = Kernel::new("test", None).await;

        kernel.set_var("FOO", "bar").await;
        assert_eq!(kernel.get_var("FOO").await, Some("bar".to_string()));

        kernel.unset_var("FOO").await;
        assert_eq!(kernel.get_var("FOO").await, None);
    }

    #[tokio::test]
    async fn test_history() {
        let kernel = Kernel::new("test", None).await;

        kernel.add_history("echo hello").await;
        kernel.add_history("ls -la").await;

        let history = kernel.recent_history(10).await;
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].command, "echo hello");
    }

    #[tokio::test]
    async fn test_llm_provider() {
        let kernel = Kernel::new("test", None).await;

        // Register a provider (uses fake key, won't actually call API)
        let provider = Arc::new(RigProvider::Anthropic(
            rig::providers::anthropic::Client::new("fake-key").unwrap(),
        ));
        kernel.register_llm("anthropic", provider).await;
        kernel.set_default_llm("anthropic").await;

        // Check provider is listed
        let providers = kernel.list_llm_providers().await;
        assert_eq!(providers, vec!["anthropic"]);
    }

    #[tokio::test]
    async fn test_llm_no_provider() {
        let kernel = Kernel::new("test", None).await;

        // Should fail gracefully without provider
        let result = kernel.llm().read().await.prompt("Hello").await;
        assert!(result.is_err());
    }
}
