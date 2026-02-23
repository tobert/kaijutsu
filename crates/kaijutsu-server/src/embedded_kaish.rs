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
//!             │       ├── /v/jobs, /v/blobs → kaish builtins
//!             │       └── everything else → MountBackend
//!             │               │
//!             │               ├── File ops → MountTable → LocalBackend
//!             │               └── Tool calls → KaijutsuBackend
//!             │
//!             └── Shared state with kaijutsu kernel
//! ```
//!
//! This enables kaish scripts to access both CRDT blocks and real files,
//! with tool dispatch routed through the kernel's tool registry.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::Result;

use kaish_kernel::interpreter::ExecResult;
use kaish_kernel::vfs::Filesystem;
use kaish_kernel::{Kernel as KaishKernel, KernelBackend, KernelConfig as KaishConfig};

use kaijutsu_kernel::block_store::SharedBlockStore;
use kaijutsu_kernel::Kernel as KaijutsuKernel;
use kaijutsu_types::{ContextId, KernelId, PrincipalId, SessionId};

use crate::docs_filesystem::KaijutsuFilesystem;
use crate::kaish_backend::{KaijutsuBackend, SharedContextId};
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
    /// Shared mutable context ID — updated when the connection switches context.
    /// The same `Arc` is held by `KaijutsuBackend`, so updates propagate to tool calls.
    context_id: SharedContextId,
}

impl EmbeddedKaish {
    /// Create a new embedded kaish executor with default identity.
    ///
    /// Uses `PrincipalId::system()` and a fresh `ContextId`. For real connections,
    /// prefer `with_identity` which accepts the actual connection identity.
    pub fn new(
        name: &str,
        blocks: SharedBlockStore,
        kernel: Arc<KaijutsuKernel>,
        project_root: Option<PathBuf>,
    ) -> Result<Self> {
        Self::with_identity(
            name, blocks, kernel, project_root,
            PrincipalId::system(),
            ContextId::new(),
            SessionId::new(),
            KernelId::new(),
        )
    }

    /// Create an embedded kaish executor with explicit identity fields.
    ///
    /// Identity flows through to `ToolContext` for drift/whoami engines.
    /// The `context_id` is wrapped in `Arc<RwLock>` so that context switches
    /// (via `set_context_id`) propagate to all tool calls without rebuilding.
    pub fn with_identity(
        name: &str,
        blocks: SharedBlockStore,
        kernel: Arc<KaijutsuKernel>,
        project_root: Option<PathBuf>,
        principal_id: PrincipalId,
        context_id: ContextId,
        session_id: SessionId,
        kernel_id: KernelId,
    ) -> Result<Self> {
        let shared_context_id: SharedContextId = Arc::new(RwLock::new(context_id));
        let docs_backend = Arc::new(KaijutsuBackend::new(
            blocks,
            kernel.clone(),
            principal_id,
            shared_context_id.clone(),
            session_id,
            kernel_id,
        ));
        let mount_table = kernel.vfs().clone();

        let mount_backend: Arc<dyn KernelBackend> = Arc::new(MountBackend::new(
            mount_table, docs_backend.clone(),
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
            context_id: shared_context_id,
        })
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

    /// Update the context ID (e.g., after a context switch).
    ///
    /// Propagates to `KaijutsuBackend` via the shared `Arc<RwLock>`.
    pub fn set_context_id(&self, id: ContextId) {
        *self.context_id.write().expect("context_id lock poisoned") = id;
    }

    /// Read the current context ID.
    pub fn context_id(&self) -> ContextId {
        *self.context_id.read().expect("context_id lock poisoned")
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


#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_kernel::block_store::shared_block_store;

    #[tokio::test]
    async fn test_embedded_kaish_creation() {
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new("test-agent").await);

        let kaish = EmbeddedKaish::new("test-kernel", blocks, kernel, None);
        assert!(kaish.is_ok());

        let kaish = kaish.unwrap();
        assert_eq!(kaish.name(), "test-kernel");
        assert_eq!(kaish.ping().await.unwrap(), "pong");
    }

    #[tokio::test]
    async fn test_embedded_kaish_variables() {
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
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
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
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
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
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
