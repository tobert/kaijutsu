//! The Kernel: core primitive of kaijutsu.
//!
//! A kernel owns:
//! - A VFS (MountTable)
//! - State (variables, history, checkpoints)
//! - Tools (execution engines)
//! - LLM providers (for model access)
//! - Control plane (lease, consent mode)

use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::control::{ConsentMode, ControlPlane, LeaseError, LeaseHolder};
use crate::llm::{CompletionRequest, CompletionResponse, LlmProvider, LlmRegistry, LlmResult};
use crate::state::KernelState;
use crate::tools::{ExecResult, ExecutionEngine, ToolInfo, ToolRegistry};
use crate::vfs::{
    backends::MemoryBackend, DirEntry, FileAttr, MountTable, SetAttr, StatFs, VfsOps, VfsResult,
};

/// The Kernel: fundamental primitive of kaijutsu.
///
/// Everything is a kernel. A kernel:
/// - Owns `/` in its VFS
/// - Can mount worktrees, repos, other kernels
/// - Has a lease (who holds "the pen")
/// - Has a consent mode (collaborative vs autonomous)
/// - Can checkpoint, fork, and thread
pub struct Kernel {
    /// VFS mount table.
    vfs: Arc<MountTable>,
    /// Kernel state (behind RwLock for interior mutability).
    state: RwLock<KernelState>,
    /// Tool registry (behind RwLock for interior mutability).
    tools: RwLock<ToolRegistry>,
    /// LLM provider registry (behind RwLock for interior mutability).
    llm: RwLock<LlmRegistry>,
    /// Control plane (behind RwLock for interior mutability).
    control: RwLock<ControlPlane>,
}

impl std::fmt::Debug for Kernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kernel")
            .field("vfs", &self.vfs)
            .field("state", &"<locked>")
            .field("tools", &"<locked>")
            .field("llm", &"<locked>")
            .field("control", &"<locked>")
            .finish()
    }
}

impl Kernel {
    /// Create a new kernel with the given name.
    ///
    /// Automatically mounts a MemoryBackend at `/scratch`.
    pub async fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        let vfs = Arc::new(MountTable::new());

        // Mount scratch space
        vfs.mount("/scratch", MemoryBackend::new()).await;

        Self {
            vfs,
            state: RwLock::new(KernelState::new(&name)),
            tools: RwLock::new(ToolRegistry::new()),
            llm: RwLock::new(LlmRegistry::new()),
            control: RwLock::new(ControlPlane::new()),
        }
    }

    /// Create a new kernel with a specific ID.
    pub async fn with_id(id: Uuid, name: impl Into<String>) -> Self {
        let name = name.into();
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/scratch", MemoryBackend::new()).await;

        Self {
            vfs,
            state: RwLock::new(KernelState::with_id(id, &name)),
            tools: RwLock::new(ToolRegistry::new()),
            llm: RwLock::new(LlmRegistry::new()),
            control: RwLock::new(ControlPlane::new()),
        }
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
    pub async fn mount(&self, path: impl Into<std::path::PathBuf>, fs: impl VfsOps + 'static) {
        self.vfs.mount(path, fs).await;
    }

    /// Mount a filesystem (already wrapped in Arc) at the given path.
    pub async fn mount_arc(
        &self,
        path: impl Into<std::path::PathBuf>,
        fs: Arc<dyn VfsOps>,
    ) {
        self.vfs.mount_arc(path, fs).await;
    }

    /// Unmount a filesystem.
    pub async fn unmount(&self, path: impl AsRef<Path>) -> bool {
        self.vfs.unmount(path).await
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
    // Tools
    // ========================================================================

    /// Register a tool.
    pub async fn register_tool(&self, info: ToolInfo) {
        self.tools.write().await.register(info);
    }

    /// Register a tool with an execution engine.
    pub async fn register_tool_with_engine(
        &self,
        info: ToolInfo,
        engine: Arc<dyn ExecutionEngine>,
    ) {
        self.tools.write().await.register_with_engine(info, engine);
    }

    /// Equip a tool (must have an engine already registered).
    pub async fn equip(&self, name: &str) -> bool {
        self.tools.write().await.equip(name)
    }

    /// Equip a tool with an execution engine.
    pub async fn equip_with_engine(&self, name: &str, engine: Arc<dyn ExecutionEngine>) -> bool {
        self.tools.write().await.equip_with_engine(name, engine)
    }

    /// Unequip a tool.
    pub async fn unequip(&self, name: &str) -> bool {
        self.tools.write().await.unequip(name)
    }

    /// Get the tools registry (for RPC access).
    pub fn tools(&self) -> &RwLock<ToolRegistry> {
        &self.tools
    }

    /// List available tools.
    pub async fn list_tools(&self) -> Vec<ToolInfo> {
        self.tools.read().await.list().into_iter().cloned().collect()
    }

    /// List equipped tools.
    pub async fn list_equipped(&self) -> Vec<ToolInfo> {
        self.tools
            .read()
            .await
            .list_equipped()
            .into_iter()
            .cloned()
            .collect()
    }

    /// Set the default execution engine.
    pub async fn set_default_engine(&self, name: &str) {
        self.tools.write().await.set_default_engine(name);
    }

    /// Execute code using the default engine.
    pub async fn execute(&self, code: &str) -> anyhow::Result<ExecResult> {
        // Record in history
        let history_id = self.add_history(code).await;

        // Execute
        let result = self.tools.read().await.execute(code).await?;

        // Update history with result
        self.state.write().await.set_history_result(
            history_id,
            format!("{}{}", result.stdout, result.stderr),
            result.exit_code,
        );

        Ok(result)
    }

    /// Execute code using a specific engine.
    pub async fn execute_with(&self, engine_name: &str, code: &str) -> anyhow::Result<ExecResult> {
        let engine = self
            .tools
            .read()
            .await
            .get_engine(engine_name)
            .ok_or_else(|| anyhow::anyhow!("engine not found: {}", engine_name))?;

        let history_id = self.add_history(code).await;
        let result = engine.execute(code).await?;

        self.state.write().await.set_history_result(
            history_id,
            format!("{}{}", result.stdout, result.stderr),
            result.exit_code,
        );

        Ok(result)
    }

    // ========================================================================
    // LLM Providers
    // ========================================================================

    /// Register an LLM provider.
    pub async fn register_llm(&self, provider: Arc<dyn LlmProvider>) {
        self.llm.write().await.register(provider);
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
        self.llm.read().await.list().into_iter().map(|s| s.to_string()).collect()
    }

    /// Send a prompt to the default LLM provider.
    ///
    /// This is the simplest way to get a response from an LLM.
    pub async fn prompt(&self, prompt: &str) -> LlmResult<String> {
        self.llm.read().await.prompt(prompt).await
    }

    /// Send a completion request to the default LLM provider.
    pub async fn complete(&self, request: CompletionRequest) -> LlmResult<CompletionResponse> {
        let provider = self
            .llm
            .read()
            .await
            .default_provider()
            .ok_or_else(|| crate::llm::LlmError::Unavailable("no default LLM provider".into()))?;

        provider.complete(request).await
    }

    /// Send a prompt to a specific LLM provider.
    pub async fn prompt_with(&self, provider_name: &str, model: &str, prompt: &str) -> LlmResult<String> {
        let provider = self
            .llm
            .read()
            .await
            .get(provider_name)
            .ok_or_else(|| crate::llm::LlmError::Unavailable(format!("provider not found: {}", provider_name)))?;

        provider.prompt(model, prompt).await
    }

    // ========================================================================
    // Control Plane
    // ========================================================================

    /// Get the current consent mode.
    pub async fn consent_mode(&self) -> ConsentMode {
        self.control.read().await.consent_mode()
    }

    /// Set the consent mode.
    pub async fn set_consent_mode(&self, mode: ConsentMode) {
        self.control.write().await.set_consent_mode(mode);
    }

    /// Try to acquire the lease.
    pub async fn acquire_lease(&self, holder: LeaseHolder) -> Result<(), LeaseError> {
        self.control.write().await.acquire_lease(holder)
    }

    /// Release the lease.
    pub async fn release_lease(&self, holder: &LeaseHolder) -> Result<(), LeaseError> {
        self.control.write().await.release_lease(holder)
    }

    /// Check if a holder has the lease.
    pub async fn has_lease(&self, holder: &LeaseHolder) -> bool {
        self.control.read().await.has_lease(holder)
    }

    /// Check if the lease is available.
    pub async fn lease_available(&self) -> bool {
        self.control.read().await.lease_available()
    }

    // ========================================================================
    // Fork / Thread
    // ========================================================================

    /// Fork the kernel (deep copy, isolated).
    pub async fn fork(&self, new_name: impl Into<String>) -> Self {
        let new_name = new_name.into();
        let state = self.state.read().await.fork(&new_name);

        // Create new VFS with same mounts (but independent backends would
        // need to be cloned - for now, just create fresh scratch)
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/scratch", MemoryBackend::new()).await;

        // Copy mount info (but not backends - they'd need Clone impl)
        // This is a limitation - real fork would need backend cloning

        // Note: LLM providers are not copied - forked kernels start fresh
        // This matches tool behavior and avoids credential sharing concerns

        Self {
            vfs,
            state: RwLock::new(state),
            tools: RwLock::new(ToolRegistry::new()),
            llm: RwLock::new(LlmRegistry::new()),
            control: RwLock::new(ControlPlane::new()),
        }
    }

    /// Thread the kernel (light copy, shared VFS refs).
    pub async fn thread(&self, new_name: impl Into<String>) -> Self {
        let new_name = new_name.into();
        let state = self.state.read().await.thread(&new_name);

        // Share the VFS
        let vfs = Arc::clone(&self.vfs);

        // Note: LLM providers are not shared - threaded kernels get fresh registry
        // This avoids credential sharing and allows per-thread provider config

        Self {
            vfs,
            state: RwLock::new(state),
            tools: RwLock::new(ToolRegistry::new()),
            llm: RwLock::new(LlmRegistry::new()),
            control: RwLock::new(ControlPlane::new()),
        }
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
    use crate::llm::{CompletionRequest, CompletionResponse, LlmProvider, LlmResult, Message, ResponseBlock, Usage};
    use crate::tools::NoopEngine;

    /// Mock LLM provider for testing.
    struct MockLlmProvider {
        response: String,
    }

    impl MockLlmProvider {
        fn new(response: impl Into<String>) -> Self {
            Self { response: response.into() }
        }
    }

    #[async_trait]
    impl LlmProvider for MockLlmProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn available_models(&self) -> Vec<&str> {
            vec!["mock-model"]
        }

        async fn is_available(&self) -> bool {
            true
        }

        async fn complete(&self, _request: CompletionRequest) -> LlmResult<CompletionResponse> {
            Ok(CompletionResponse {
                content: self.response.clone(),
                blocks: vec![ResponseBlock::Text { text: self.response.clone() }],
                model: "mock-model".to_string(),
                stop_reason: Some("end_turn".to_string()),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 20,
                },
            })
        }
    }

    #[tokio::test]
    async fn test_kernel_creation() {
        let kernel = Kernel::new("test").await;
        assert_eq!(kernel.name().await, "test");
    }

    #[tokio::test]
    async fn test_scratch_mount() {
        let kernel = Kernel::new("test").await;

        // Should be able to write to /scratch
        kernel.create(Path::new("/scratch/test.txt"), 0o644).await.unwrap();
        kernel.write(Path::new("/scratch/test.txt"), 0, b"hello").await.unwrap();

        let data = kernel.read(Path::new("/scratch/test.txt"), 0, 100).await.unwrap();
        assert_eq!(data, b"hello");
    }

    #[tokio::test]
    async fn test_variables() {
        let kernel = Kernel::new("test").await;

        kernel.set_var("FOO", "bar").await;
        assert_eq!(kernel.get_var("FOO").await, Some("bar".to_string()));

        kernel.unset_var("FOO").await;
        assert_eq!(kernel.get_var("FOO").await, None);
    }

    #[tokio::test]
    async fn test_history() {
        let kernel = Kernel::new("test").await;

        kernel.add_history("echo hello").await;
        kernel.add_history("ls -la").await;

        let history = kernel.recent_history(10).await;
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].command, "echo hello");
    }

    #[tokio::test]
    async fn test_tools() {
        let kernel = Kernel::new("test").await;

        kernel.register_tool(ToolInfo::new("noop", "Noop engine", "test")).await;
        kernel.equip_with_engine("noop", Arc::new(NoopEngine)).await;
        kernel.set_default_engine("noop").await;

        let result = kernel.execute("hello").await.unwrap();
        assert!(result.success);
        assert!(result.stdout.contains("hello"));
    }

    #[tokio::test]
    async fn test_lease() {
        let kernel = Kernel::new("test").await;

        let holder = LeaseHolder::Human {
            username: "amy".into(),
        };

        assert!(kernel.lease_available().await);
        kernel.acquire_lease(holder.clone()).await.unwrap();
        assert!(!kernel.lease_available().await);
        assert!(kernel.has_lease(&holder).await);

        kernel.release_lease(&holder).await.unwrap();
        assert!(kernel.lease_available().await);
    }

    #[tokio::test]
    async fn test_thread() {
        let kernel = Kernel::new("parent").await;
        kernel.set_var("FOO", "bar").await;
        kernel.create(Path::new("/scratch/test.txt"), 0o644).await.unwrap();

        let threaded = kernel.thread("worker").await;

        // Should inherit vars
        assert_eq!(threaded.get_var("FOO").await, Some("bar".to_string()));

        // Should share VFS (same scratch file visible)
        let attr = threaded.getattr(Path::new("/scratch/test.txt")).await;
        assert!(attr.is_ok());
    }

    #[tokio::test]
    async fn test_fork() {
        let kernel = Kernel::new("parent").await;
        kernel.set_var("FOO", "bar").await;
        kernel.add_history("cmd1").await;

        let forked = kernel.fork("child").await;

        // Should have copied vars
        assert_eq!(forked.get_var("FOO").await, Some("bar".to_string()));

        // Should have copied history
        let history = forked.recent_history(10).await;
        assert_eq!(history.len(), 1);

        // But VFS is independent (new scratch)
        let original_scratch = kernel.getattr(Path::new("/scratch")).await;
        let forked_scratch = forked.getattr(Path::new("/scratch")).await;
        assert!(original_scratch.is_ok());
        assert!(forked_scratch.is_ok());
    }

    #[tokio::test]
    async fn test_llm_provider() {
        let kernel = Kernel::new("test").await;

        // Register mock provider
        let provider = Arc::new(MockLlmProvider::new("Hello from mock!"));
        kernel.register_llm(provider).await;
        kernel.set_default_llm("mock").await;

        // Check provider is listed
        let providers = kernel.list_llm_providers().await;
        assert_eq!(providers, vec!["mock"]);

        // Test prompt
        let response = kernel.prompt("Hello").await.unwrap();
        assert_eq!(response, "Hello from mock!");

        // Test complete
        let request = CompletionRequest::new("mock-model", vec![Message::user("Hi")]);
        let response = kernel.complete(request).await.unwrap();
        assert_eq!(response.content, "Hello from mock!");
        assert_eq!(response.usage.total(), 30);
    }

    #[tokio::test]
    async fn test_llm_no_provider() {
        let kernel = Kernel::new("test").await;

        // Should fail gracefully without provider
        let result = kernel.prompt("Hello").await;
        assert!(result.is_err());
    }
}
