//! Filesystem adapter for CRDT blocks.
//!
//! Wraps `KaijutsuBackend` as a kaish `Filesystem`, enabling it to be mounted
//! in the kaish VFS router at `/v/docs`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use kaish_kernel::vfs::{DirEntry, Filesystem};
use kaish_kernel::{BackendError, KernelBackend};

use crate::kaish_backend::KaijutsuBackend;

/// Adapts `KaijutsuBackend` to the kaish `Filesystem` trait.
///
/// This allows CRDT block operations to be mounted as `/v/docs` in kaish's
/// VFS router, so agents can access blocks via standard file paths.
pub struct KaijutsuFilesystem {
    backend: Arc<KaijutsuBackend>,
}

impl KaijutsuFilesystem {
    /// Create a new filesystem adapter wrapping a KaijutsuBackend.
    pub fn new(backend: Arc<KaijutsuBackend>) -> Self {
        Self { backend }
    }
}

/// Convert a `BackendError` to an `io::Error`.
fn backend_to_io(err: BackendError) -> io::Error {
    match err {
        BackendError::NotFound(msg) => io::Error::new(io::ErrorKind::NotFound, msg),
        BackendError::AlreadyExists(msg) => io::Error::new(io::ErrorKind::AlreadyExists, msg),
        BackendError::PermissionDenied(msg) => io::Error::new(io::ErrorKind::PermissionDenied, msg),
        BackendError::IsDirectory(msg) => io::Error::new(io::ErrorKind::IsADirectory, msg),
        BackendError::NotDirectory(msg) => io::Error::new(io::ErrorKind::NotADirectory, msg),
        BackendError::ReadOnly => {
            io::Error::new(io::ErrorKind::PermissionDenied, "read-only filesystem")
        }
        BackendError::Io(msg) => io::Error::other(msg),
        BackendError::InvalidOperation(msg) => io::Error::new(io::ErrorKind::InvalidInput, msg),
        BackendError::ToolNotFound(msg) => io::Error::new(io::ErrorKind::NotFound, msg),
        BackendError::Conflict(e) => io::Error::other(e.to_string()),
    }
}

/// Prepend `/docs` to a relative path for the backend.
///
/// The backend expects paths like `/docs/{doc_id}/{block_key}`, but the
/// filesystem adapter receives paths relative to its mount point.
/// Normalizes `.` and `..` components before joining.
fn docs_path(path: &Path) -> PathBuf {
    let normalized: PathBuf = path
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .collect();
    if normalized.as_os_str().is_empty() {
        PathBuf::from("/docs")
    } else {
        PathBuf::from("/docs").join(normalized)
    }
}

#[async_trait]
impl Filesystem for KaijutsuFilesystem {
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.backend
            .read(&docs_path(path), None)
            .await
            .map_err(backend_to_io)
    }

    async fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        use kaish_kernel::WriteMode;
        self.backend
            .write(&docs_path(path), data, WriteMode::Overwrite)
            .await
            .map_err(backend_to_io)
    }

    async fn list(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        self.backend
            .list(&docs_path(path))
            .await
            .map_err(backend_to_io)
    }

    async fn stat(&self, path: &Path) -> io::Result<DirEntry> {
        self.backend
            .stat(&docs_path(path))
            .await
            .map_err(backend_to_io)
    }

    async fn mkdir(&self, path: &Path) -> io::Result<()> {
        self.backend
            .mkdir(&docs_path(path))
            .await
            .map_err(backend_to_io)
    }

    async fn remove(&self, path: &Path) -> io::Result<()> {
        self.backend
            .remove(&docs_path(path), false)
            .await
            .map_err(backend_to_io)
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn exists(&self, path: &Path) -> bool {
        self.backend.exists(&docs_path(path)).await
    }

    async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.backend
            .rename(&docs_path(from), &docs_path(to))
            .await
            .map_err(backend_to_io)
    }

    fn real_path(&self, _path: &Path) -> Option<PathBuf> {
        None // CRDT blocks have no real filesystem path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_kernel::Kernel as KaijutsuKernel;
    use kaijutsu_kernel::block_store::shared_block_store;
    use kaijutsu_types::PrincipalId;

    #[tokio::test]
    async fn test_docs_filesystem_read_only() {
        let blocks = shared_block_store(PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new("test-docs-fs").await);
        let sid = kaijutsu_types::SessionId::new();
        let session_contexts = crate::context_engine::session_context_map();
        session_contexts.insert(sid, kaijutsu_types::ContextId::new());
        let backend = Arc::new(KaijutsuBackend::new(
            blocks,
            kernel,
            PrincipalId::system(),
            session_contexts,
            sid,
            kaijutsu_types::KernelId::new(),
        ));
        let fs = KaijutsuFilesystem::new(backend);
        assert!(!fs.read_only());
    }

    #[tokio::test]
    async fn test_docs_filesystem_real_path() {
        let blocks = shared_block_store(PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new("test-docs-fs-rp").await);
        let sid = kaijutsu_types::SessionId::new();
        let session_contexts = crate::context_engine::session_context_map();
        session_contexts.insert(sid, kaijutsu_types::ContextId::new());
        let backend = Arc::new(KaijutsuBackend::new(
            blocks,
            kernel,
            PrincipalId::system(),
            session_contexts,
            sid,
            kaijutsu_types::KernelId::new(),
        ));
        let fs = KaijutsuFilesystem::new(backend);
        assert!(fs.real_path(Path::new("some/path")).is_none());
    }

    #[tokio::test]
    async fn test_docs_filesystem_list_root() {
        let blocks = shared_block_store(PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new("test-docs-fs-list").await);
        let sid = kaijutsu_types::SessionId::new();
        let session_contexts = crate::context_engine::session_context_map();
        session_contexts.insert(sid, kaijutsu_types::ContextId::new());
        let backend = Arc::new(KaijutsuBackend::new(
            blocks,
            kernel,
            PrincipalId::system(),
            session_contexts,
            sid,
            kaijutsu_types::KernelId::new(),
        ));
        let fs = KaijutsuFilesystem::new(backend);

        // Listing root of docs should succeed (may be empty)
        let entries = fs.list(Path::new("")).await;
        assert!(entries.is_ok());
    }
}
