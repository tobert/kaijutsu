//! Embedded kaish executor using MountBackend + VFS adapters.
//!
//! Instead of spawning kaish as a subprocess, this module embeds the kaish
//! interpreter directly, routing I/O through the kaijutsu kernel's MountTable
//! for real filesystem access and VFS adapters for CRDT blocks.
//!
//! # Architecture
//!
//! ```text
//! kaijutsu-server
//!     │
//!     └── EmbeddedKaish
//!             │
//!             ├── kaish::Kernel (in-process)
//!             │       │
//!             │       ├── /v/docs → KaijutsuFilesystem (CRDT blocks)
//!             │       ├── /v/g   → GitFilesystem (CRDT git)
//!             │       ├── /v/jobs, /v/blobs → kaish builtins
//!             │       └── everything else → MountBackend
//!             │               │
//!             │               ├── File ops → MountTable → LocalBackend
//!             │               └── Tool calls → KaijutsuBackend / GitCrdtBackend
//!             │
//!             └── Shared state with kaijutsu kernel
//! ```
//!
//! This enables kaish scripts to access both CRDT blocks and real files,
//! with tool dispatch routed through the kernel's tool registry.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use kaish_kernel::interpreter::{EntryType as KaishEntryType, ExecResult, OutputData};
use kaish_kernel::vfs::Filesystem;
use kaish_kernel::{Kernel as KaishKernel, KernelBackend, KernelConfig as KaishConfig};

use kaijutsu_kernel::block_store::SharedBlockStore;
use kaijutsu_kernel::tools::{DisplayHint, EntryType};
use kaijutsu_kernel::Kernel as KaijutsuKernel;

use crate::docs_filesystem::KaijutsuFilesystem;
use crate::git_backend::GitCrdtBackend;
use crate::git_filesystem::GitFilesystem;
use crate::kaish_backend::KaijutsuBackend;
use crate::mount_backend::MountBackend;

/// Embedded kaish executor backed by CRDT blocks.
///
/// Unlike `KaishProcess` which spawns a subprocess, this embeds the kaish
/// interpreter directly and routes all I/O through `KaijutsuBackend`.
pub struct EmbeddedKaish {
    /// The embedded kaish kernel.
    kernel: KaishKernel,
    /// Kernel name/id.
    name: String,
    /// Git backend for repo management (if git support enabled).
    git_backend: Option<Arc<GitCrdtBackend>>,
}

impl EmbeddedKaish {
    /// Create a new embedded kaish executor.
    ///
    /// # Arguments
    ///
    /// * `name` - Name for this kaish kernel (for state persistence)
    /// * `blocks` - Shared block store for CRDT operations
    /// * `kernel` - Kaijutsu kernel for tool dispatch and VFS mounts
    ///
    /// # Example
    ///
    /// ```ignore
    /// let blocks = shared_block_store("agent-1");
    /// let kernel = Arc::new(KaijutsuKernel::new("agent-1").await);
    /// let kaish = EmbeddedKaish::new("my-kernel", blocks, kernel, None)?;
    /// let result = kaish.execute("echo hello").await?;
    /// ```
    pub fn new(
        name: &str,
        blocks: SharedBlockStore,
        kernel: Arc<KaijutsuKernel>,
        project_root: Option<PathBuf>,
    ) -> Result<Self> {
        let docs_backend = Arc::new(KaijutsuBackend::new(blocks, kernel.clone()));
        let mount_table = kernel.vfs().clone();

        let mount_backend: Arc<dyn KernelBackend> = Arc::new(MountBackend::new(
            mount_table, docs_backend.clone(), None,
        ));

        let docs_fs = Arc::new(KaijutsuFilesystem::new(docs_backend));

        // KaishConfig primarily sets the cwd and kernel name. The VFS mode
        // in the config is secondary to kaijutsu's MountTable — real filesystem
        // access is routed through MountBackend → MountTable → LocalBackend,
        // not through kaish's own VFS modes.
        //
        // `project_root` sets the cwd to a specific project directory (used by
        // MCP sessions that operate on a particular repo). When None, cwd
        // defaults to $HOME via `KaishConfig::named()`.
        let config = match project_root {
            Some(root) => KaishConfig::mcp_with_root(root),
            None => KaishConfig::named(name),
        };

        let kaish_kernel = KaishKernel::with_backend(mount_backend, config, |vfs| {
            vfs.mount_arc("/v/docs", docs_fs);
        })?;

        Ok(Self {
            kernel: kaish_kernel,
            name: name.to_string(),
            git_backend: None,
        })
    }

    /// Create a new embedded kaish executor with git support.
    ///
    /// This variant includes the `/v/g` namespace for CRDT-backed git worktrees.
    /// Use `register_repo()` to add repositories.
    ///
    /// # Arguments
    ///
    /// * `name` - Name for this kaish kernel
    /// * `blocks` - Shared block store for CRDT operations (docs)
    /// * `git_blocks` - Shared block store for git CRDT operations
    /// * `kernel` - Kaijutsu kernel for tool dispatch and VFS mounts
    ///
    /// # Example
    ///
    /// ```ignore
    /// let blocks = shared_block_store("agent-1");
    /// let git_blocks = shared_block_store("agent-1-git");
    /// let kernel = Arc::new(KaijutsuKernel::new("agent-1").await);
    /// let kaish = EmbeddedKaish::with_git("my-kernel", blocks, git_blocks, kernel, None)?;
    /// kaish.register_repo("myproject", "/home/user/src/myproject")?;
    /// ```
    pub fn with_git(
        name: &str,
        blocks: SharedBlockStore,
        git_blocks: SharedBlockStore,
        kernel: Arc<KaijutsuKernel>,
        project_root: Option<PathBuf>,
    ) -> Result<Self> {
        let docs_backend = Arc::new(KaijutsuBackend::new(blocks, kernel.clone()));
        let git_backend = Arc::new(GitCrdtBackend::new(git_blocks));
        let mount_table = kernel.vfs().clone();

        let mount_backend: Arc<dyn KernelBackend> = Arc::new(MountBackend::new(
            mount_table, docs_backend.clone(), Some(git_backend.clone()),
        ));

        let docs_fs = Arc::new(KaijutsuFilesystem::new(docs_backend));
        let git_fs = Arc::new(GitFilesystem::new(git_backend.clone()));

        // See comment in `new()` — config sets cwd/name, MountTable handles VFS
        let config = match project_root {
            Some(root) => KaishConfig::mcp_with_root(root),
            None => KaishConfig::named(name),
        };

        let kaish_kernel = KaishKernel::with_backend(mount_backend, config, |vfs| {
            vfs.mount_arc("/v/docs", docs_fs);
            vfs.mount_arc("/v/g", git_fs);
        })?;

        Ok(Self {
            kernel: kaish_kernel,
            name: name.to_string(),
            git_backend: Some(git_backend),
        })
    }

    /// Register a git repository for CRDT-backed access.
    ///
    /// Only available if created with `with_git()`.
    pub fn register_repo(&self, name: &str, path: impl Into<std::path::PathBuf>) -> Result<()> {
        let git = self.git_backend.as_ref()
            .ok_or_else(|| anyhow::anyhow!("git support not enabled - use with_git()"))?;
        git.register_repo(name, path)
            .map_err(|e| anyhow::anyhow!(e))
    }

    /// Unregister a git repository.
    pub fn unregister_repo(&self, name: &str) -> Result<()> {
        let git = self.git_backend.as_ref()
            .ok_or_else(|| anyhow::anyhow!("git support not enabled"))?;
        git.unregister_repo(name)
            .map_err(|e| anyhow::anyhow!(e))
    }

    /// List registered git repositories.
    pub fn list_repos(&self) -> Vec<String> {
        self.git_backend.as_ref()
            .map(|g| g.list_repos())
            .unwrap_or_default()
    }

    /// Get the git backend (if available).
    pub fn git_backend(&self) -> Option<&Arc<GitCrdtBackend>> {
        self.git_backend.as_ref()
    }

    /// Start the file watcher for external change detection.
    ///
    /// The watcher monitors registered repositories for changes made by
    /// external tools (cargo, clippy, go fmt, etc.) and syncs them to CRDT.
    ///
    /// Returns a handle that can be used to stop the watcher.
    pub fn start_watcher(&self) -> Result<crate::git_backend::WatcherHandle> {
        let git = self.git_backend.as_ref()
            .ok_or_else(|| anyhow::anyhow!("git support not enabled - use with_git()"))?;
        git.start_watcher()
            .map_err(|e| anyhow::anyhow!(e))
    }

    /// Set attribution for the next external command.
    ///
    /// Call this before running an external command (e.g., `cargo clippy --fix`)
    /// so that resulting file changes can be attributed.
    pub fn set_pending_attribution(&self, source: &str, command: Option<&str>) {
        if let Some(git) = self.git_backend.as_ref() {
            git.set_pending_attribution(source, command);
        }
    }

    /// Flush all dirty CRDT files to disk.
    pub async fn flush_git(&self) -> Result<()> {
        if let Some(git) = self.git_backend.as_ref() {
            git.flush_all().await
                .map_err(|e| anyhow::anyhow!("failed to flush: {}", e))
        } else {
            Ok(())
        }
    }

    /// Switch to a different branch for a repository.
    pub async fn switch_branch(&self, repo: &str, branch: &str) -> Result<()> {
        let git = self.git_backend.as_ref()
            .ok_or_else(|| anyhow::anyhow!("git support not enabled"))?;
        git.switch_branch(repo, branch).await
            .map_err(|e| anyhow::anyhow!("failed to switch branch: {}", e))
    }

    /// Get the current branch for a repository.
    pub fn get_current_branch(&self, repo: &str) -> Option<String> {
        self.git_backend.as_ref()?.get_current_branch(repo)
    }

    /// List all branches for a repository.
    pub fn list_branches(&self, repo: &str) -> Result<Vec<String>> {
        let git = self.git_backend.as_ref()
            .ok_or_else(|| anyhow::anyhow!("git support not enabled"))?;
        git.list_branches(repo)
            .map_err(|e| anyhow::anyhow!("failed to list branches: {}", e))
    }

    /// Execute kaish code and return the result.
    pub async fn execute(&self, code: &str) -> Result<ExecResult> {
        self.kernel.execute(code).await
    }

    /// Get a variable value.
    pub async fn get_var(&self, name: &str) -> Option<kaish_kernel::ast::Value> {
        self.kernel.get_var(name).await
    }

    /// Set a variable value.
    pub async fn set_var(&self, name: &str, value: kaish_kernel::ast::Value) {
        self.kernel.set_var(name, value).await
    }

    /// List all variable names.
    pub async fn list_vars(&self) -> Vec<String> {
        self.kernel.list_vars().await.into_iter().map(|(name, _)| name).collect()
    }

    /// Get the kernel name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Ping the kernel (health check) - always succeeds for embedded.
    pub async fn ping(&self) -> Result<String> {
        Ok("pong".to_string())
    }

    /// Shutdown the embedded kernel (no-op for embedded, just drop).
    pub async fn shutdown(self) -> Result<()> {
        // Nothing to do - kernel will be dropped
        Ok(())
    }

    /// Get current working directory.
    pub async fn cwd(&self) -> std::path::PathBuf {
        self.kernel.cwd().await
    }

    /// Set current working directory.
    pub async fn set_cwd(&self, path: std::path::PathBuf) {
        self.kernel.set_cwd(path).await
    }

    /// Get the last execution result ($?).
    pub async fn last_result(&self) -> Option<ExecResult> {
        Some(self.kernel.last_result().await)
    }

    // =========================================================================
    // Blob Storage (via kaish VFS at /v/blobs/)
    // =========================================================================

    /// Write a blob and return its reference.
    pub async fn write_blob(&self, data: &[u8], content_type: &str) -> Result<BlobInfo> {
        let vfs = self.kernel.vfs();
        let id = generate_blob_id();
        let path = std::path::PathBuf::from(format!("/v/blobs/{}", id));

        vfs.write(&path, data).await
            .map_err(|e| anyhow::anyhow!("failed to write blob: {}", e))?;

        Ok(BlobInfo {
            id,
            size: data.len() as u64,
            content_type: content_type.to_string(),
        })
    }

    /// Read a blob by ID.
    pub async fn read_blob(&self, id: &str) -> Result<Vec<u8>> {
        let vfs = self.kernel.vfs();
        let path = std::path::PathBuf::from(format!("/v/blobs/{}", id));

        vfs.read(&path).await
            .map_err(|e| anyhow::anyhow!("failed to read blob {}: {}", id, e))
    }

    /// Delete a blob by ID.
    pub async fn delete_blob(&self, id: &str) -> Result<bool> {
        let vfs = self.kernel.vfs();
        let path = std::path::PathBuf::from(format!("/v/blobs/{}", id));

        match vfs.remove(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(anyhow::anyhow!("failed to delete blob {}: {}", id, e)),
        }
    }

    /// List all blobs.
    pub async fn list_blobs(&self) -> Result<Vec<BlobInfo>> {
        let vfs = self.kernel.vfs();
        let path = std::path::PathBuf::from("/v/blobs");

        let entries = vfs.list(&path).await
            .map_err(|e| anyhow::anyhow!("failed to list blobs: {}", e))?;

        let mut blobs = Vec::new();
        for entry in entries {
            let blob_path = path.join(&entry.name);
            if let Ok(meta) = vfs.stat(&blob_path).await {
                blobs.push(BlobInfo {
                    id: entry.name,
                    size: meta.size,
                    content_type: "application/octet-stream".to_string(), // TODO: store metadata
                });
            }
        }
        Ok(blobs)
    }
}

/// Information about a stored blob.
#[derive(Debug, Clone)]
pub struct BlobInfo {
    pub id: String,
    pub size: u64,
    pub content_type: String,
}

/// Generate a unique blob ID.
fn generate_blob_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{:x}", timestamp)
}

// ============================================================================
// DisplayHint Conversion
// ============================================================================

/// Convert a kaish EntryType to kaijutsu EntryType.
pub fn convert_entry_type(et: &KaishEntryType) -> EntryType {
    match et {
        KaishEntryType::Text | KaishEntryType::File => EntryType::File,
        KaishEntryType::Directory => EntryType::Directory,
        KaishEntryType::Executable => EntryType::Executable,
        KaishEntryType::Symlink => EntryType::Symlink,
    }
}

/// Convert kaish `OutputData` to kaijutsu `DisplayHint`.
///
/// The mapping:
/// - `None` → `DisplayHint::None`
/// - Simple text → `DisplayHint::None` (text is already in stdout)
/// - Tabular (headers + cells) → `DisplayHint::Table`
/// - Nested children → `DisplayHint::Tree`
/// - Flat list → `DisplayHint::Table` (single-column)
pub fn convert_output_data(output: Option<&OutputData>) -> DisplayHint {
    let output = match output {
        Some(o) => o,
        None => return DisplayHint::None,
    };

    // Simple text — no structured hint needed
    if output.is_simple_text() {
        return DisplayHint::None;
    }

    // Tabular data
    if output.is_tabular() || output.headers.is_some() {
        let rows: Vec<Vec<String>> = output
            .root
            .iter()
            .map(|node| {
                let mut row = vec![node.name.clone()];
                row.extend(node.cells.iter().cloned());
                row
            })
            .collect();
        let entry_types: Vec<EntryType> = output
            .root
            .iter()
            .map(|node| convert_entry_type(&node.entry_type))
            .collect();
        return DisplayHint::Table {
            headers: output.headers.clone(),
            rows,
            entry_types: Some(entry_types),
        };
    }

    // Nested tree
    if !output.is_flat() {
        let canonical = output.to_canonical_string();
        let json_structure = serde_json::to_value(output).unwrap_or_default();
        let root_name = output
            .root
            .first()
            .map(|n| n.name.clone())
            .unwrap_or_default();
        return DisplayHint::Tree {
            root: root_name,
            structure: json_structure,
            traditional: canonical.clone(),
            compact: canonical,
        };
    }

    // Flat list without cells — single-column table
    let rows: Vec<Vec<String>> = output.root.iter().map(|n| vec![n.name.clone()]).collect();
    let entry_types: Vec<EntryType> = output
        .root
        .iter()
        .map(|node| convert_entry_type(&node.entry_type))
        .collect();
    DisplayHint::Table {
        headers: None,
        rows,
        entry_types: Some(entry_types),
    }
}

/// Serialize a DisplayHint to JSON for storage in CRDT blocks.
///
/// Returns None for DisplayHint::None to avoid storing unnecessary data.
pub fn serialize_display_hint(hint: &DisplayHint) -> Option<String> {
    match hint {
        DisplayHint::None => None,
        _ => serde_json::to_string(hint).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_kernel::block_store::shared_block_store;

    #[tokio::test]
    async fn test_embedded_kaish_creation() {
        let blocks = shared_block_store("test-agent");
        let kernel = Arc::new(KaijutsuKernel::new("test-agent").await);

        let kaish = EmbeddedKaish::new("test-kernel", blocks, kernel, None);
        assert!(kaish.is_ok());

        let kaish = kaish.unwrap();
        assert_eq!(kaish.name(), "test-kernel");
        assert_eq!(kaish.ping().await.unwrap(), "pong");
    }

    #[tokio::test]
    async fn test_embedded_kaish_variables() {
        let blocks = shared_block_store("test-vars");
        let kernel = Arc::new(KaijutsuKernel::new("test-vars").await);
        let kaish = EmbeddedKaish::new("test-vars", blocks, kernel, None).unwrap();

        // Set and get a variable
        kaish.set_var("X", kaish_kernel::ast::Value::String("hello".into())).await;
        let val = kaish.get_var("X").await;
        assert!(val.is_some());

        match val.unwrap() {
            kaish_kernel::ast::Value::String(s) => assert_eq!(s, "hello"),
            _ => panic!("Expected String value"),
        }
    }

    #[tokio::test]
    async fn test_named_config_cwd_is_home() {
        let blocks = shared_block_store("test-cwd-home");
        let kernel = Arc::new(KaijutsuKernel::new("test-cwd-home").await);
        let kaish = EmbeddedKaish::new("test-cwd-home", blocks, kernel, None).unwrap();

        let cwd = kaish.cwd().await;
        // KaishConfig::named() sets cwd to home_dir(). We can't control HOME
        // in parallel tests, so just verify it's a real existing directory.
        assert!(cwd.is_dir(), "cwd should be an existing directory, got {:?}", cwd);
        assert!(cwd.is_absolute(), "cwd should be absolute, got {:?}", cwd);
    }

    #[tokio::test]
    async fn test_mcp_config_cwd_is_project_root() {
        let tmp = tempfile::tempdir().unwrap();
        let blocks = shared_block_store("test-cwd-project");
        let kernel = Arc::new(KaijutsuKernel::new("test-cwd-project").await);
        let kaish = EmbeddedKaish::new(
            "test-cwd-project",
            blocks,
            kernel,
            Some(tmp.path().to_path_buf()),
        ).unwrap();

        let cwd = kaish.cwd().await;
        // Canonicalize both to handle symlinks (e.g., /tmp → /private/tmp on macOS)
        let expected = tmp.path().canonicalize().unwrap_or_else(|_| tmp.path().to_path_buf());
        let actual = cwd.canonicalize().unwrap_or(cwd.clone());
        assert_eq!(actual, expected, "cwd should be project root");
    }
}
