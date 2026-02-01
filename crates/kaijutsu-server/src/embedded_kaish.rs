//! Embedded kaish executor using KaijutsuBackend.
//!
//! Instead of spawning kaish as a subprocess, this module embeds the kaish
//! interpreter directly, using `KaijutsuBackend` for file operations and
//! tool dispatch.
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
//!             │       └── ExecContext.backend = KaijutsuBackend
//!             │               │
//!             │               ├── File ops → BlockStore (CRDT)
//!             │               └── Tool calls → KaijutsuKernel
//!             │
//!             └── Shared state with kaijutsu kernel
//! ```
//!
//! This enables kaish scripts to read/write blocks as files and call
//! kaijutsu tools directly, without IPC overhead.

use std::sync::Arc;

use anyhow::Result;

use kaish_kernel::interpreter::{DisplayHint as KaishDisplayHint, EntryType as KaishEntryType, ExecResult};
use kaish_kernel::vfs::Filesystem;
use kaish_kernel::{Kernel as KaishKernel, KernelBackend, KernelConfig as KaishConfig};

use kaijutsu_kernel::block_store::SharedBlockStore;
use kaijutsu_kernel::tools::{DisplayHint, EntryType};
use kaijutsu_kernel::Kernel as KaijutsuKernel;

use crate::composite_backend::CompositeBackend;
use crate::git_backend::GitCrdtBackend;
use crate::kaish_backend::KaijutsuBackend;

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
    /// * `kernel` - Kaijutsu kernel for tool dispatch
    ///
    /// # Example
    ///
    /// ```ignore
    /// let blocks = shared_block_store("agent-1");
    /// let kernel = Arc::new(KaijutsuKernel::new("agent-1").await);
    /// let kaish = EmbeddedKaish::new("my-kernel", blocks, kernel)?;
    /// let result = kaish.execute("echo hello").await?;
    /// ```
    pub fn new(
        name: &str,
        blocks: SharedBlockStore,
        kernel: Arc<KaijutsuKernel>,
    ) -> Result<Self> {
        // Create the CRDT-backed backend for file operations
        let backend: Arc<dyn KernelBackend> = Arc::new(KaijutsuBackend::new(blocks, kernel));

        // Configure kaish kernel to start in /docs namespace
        // Use isolated mode since all file ops go through the CRDT backend
        let config = KaishConfig::isolated()
            .with_cwd(std::path::PathBuf::from("/docs"));

        // Create kaish kernel with our CRDT backend
        let kaish_kernel = KaishKernel::with_backend(backend, config)?;

        Ok(Self {
            kernel: kaish_kernel,
            name: name.to_string(),
            git_backend: None,
        })
    }

    /// Create a new embedded kaish executor with git support.
    ///
    /// This variant includes the `/g/` namespace for CRDT-backed git worktrees.
    /// Use `register_repo()` to add repositories.
    ///
    /// # Arguments
    ///
    /// * `name` - Name for this kaish kernel
    /// * `blocks` - Shared block store for CRDT operations (docs)
    /// * `git_blocks` - Shared block store for git CRDT operations
    /// * `kernel` - Kaijutsu kernel for tool dispatch
    ///
    /// # Example
    ///
    /// ```ignore
    /// let blocks = shared_block_store("agent-1");
    /// let git_blocks = shared_block_store("agent-1-git");
    /// let kernel = Arc::new(KaijutsuKernel::new("agent-1").await);
    /// let kaish = EmbeddedKaish::with_git("my-kernel", blocks, git_blocks, kernel)?;
    /// kaish.register_repo("myproject", "/home/user/src/myproject")?;
    /// ```
    pub fn with_git(
        name: &str,
        blocks: SharedBlockStore,
        git_blocks: SharedBlockStore,
        kernel: Arc<KaijutsuKernel>,
    ) -> Result<Self> {
        // Create both backends
        let docs_backend = Arc::new(KaijutsuBackend::new(blocks, kernel));
        let git_backend = Arc::new(GitCrdtBackend::new(git_blocks));

        // Create composite backend
        let composite: Arc<dyn KernelBackend> = Arc::new(CompositeBackend::new(
            docs_backend,
            git_backend.clone(),
        ));

        // Configure kaish kernel to start in /docs namespace
        // Use isolated mode since all file ops go through the composite backend
        let config = KaishConfig::isolated()
            .with_cwd(std::path::PathBuf::from("/docs"));

        let kaish_kernel = KaishKernel::with_backend(composite, config)?;

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
        KaishEntryType::File => EntryType::File,
        KaishEntryType::Directory => EntryType::Directory,
        KaishEntryType::Executable => EntryType::Executable,
        KaishEntryType::Symlink => EntryType::Symlink,
    }
}

/// Convert a kaish DisplayHint to kaijutsu DisplayHint.
pub fn convert_display_hint(kaish_hint: &KaishDisplayHint) -> DisplayHint {
    match kaish_hint {
        KaishDisplayHint::None => DisplayHint::None,

        KaishDisplayHint::Formatted { user, model } => DisplayHint::Formatted {
            user: user.clone(),
            model: model.clone(),
        },

        KaishDisplayHint::Table { headers, rows, entry_types } => DisplayHint::Table {
            headers: headers.clone(),
            rows: rows.clone(),
            entry_types: entry_types.as_ref().map(|ets| {
                ets.iter().map(convert_entry_type).collect()
            }),
        },

        KaishDisplayHint::Tree { root, structure, traditional, compact } => DisplayHint::Tree {
            root: root.clone(),
            structure: structure.clone(),
            traditional: traditional.clone(),
            compact: compact.clone(),
        },
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

        let kaish = EmbeddedKaish::new("test-kernel", blocks, kernel);
        assert!(kaish.is_ok());

        let kaish = kaish.unwrap();
        assert_eq!(kaish.name(), "test-kernel");
        assert_eq!(kaish.ping().await.unwrap(), "pong");
    }

    #[tokio::test]
    async fn test_embedded_kaish_variables() {
        let blocks = shared_block_store("test-vars");
        let kernel = Arc::new(KaijutsuKernel::new("test-vars").await);
        let kaish = EmbeddedKaish::new("test-vars", blocks, kernel).unwrap();

        // Set and get a variable
        kaish.set_var("X", kaish_kernel::ast::Value::String("hello".into())).await;
        let val = kaish.get_var("X").await;
        assert!(val.is_some());

        match val.unwrap() {
            kaish_kernel::ast::Value::String(s) => assert_eq!(s, "hello"),
            _ => panic!("Expected String value"),
        }
    }
}
