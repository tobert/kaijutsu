//! Filesystem adapter for CRDT-backed git worktrees.
//!
//! Wraps `GitCrdtBackend` as a kaish `Filesystem`, enabling it to be mounted
//! in the kaish VFS router at `/v/g`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use kaish_kernel::vfs::{DirEntry, EntryType, Filesystem, Metadata};
use kaish_kernel::{BackendError, KernelBackend};

use crate::git_backend::GitCrdtBackend;

/// Adapts `GitCrdtBackend` to the kaish `Filesystem` trait.
///
/// This allows CRDT-backed git worktrees to be mounted as `/v/g` in kaish's
/// VFS router, so agents can access git files via standard paths.
pub struct GitFilesystem {
    backend: Arc<GitCrdtBackend>,
}

impl GitFilesystem {
    /// Create a new filesystem adapter wrapping a GitCrdtBackend.
    pub fn new(backend: Arc<GitCrdtBackend>) -> Self {
        Self { backend }
    }
}

/// Convert a `BackendError` to an `io::Error`.
fn backend_to_io(err: BackendError) -> io::Error {
    match err {
        BackendError::NotFound(msg) => io::Error::new(io::ErrorKind::NotFound, msg),
        BackendError::AlreadyExists(msg) => io::Error::new(io::ErrorKind::AlreadyExists, msg),
        BackendError::PermissionDenied(msg) => {
            io::Error::new(io::ErrorKind::PermissionDenied, msg)
        }
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

/// Convert a kaish-kernel `EntryInfo` to a kaish `DirEntry`.
fn entry_info_to_dir_entry(info: &kaish_kernel::EntryInfo) -> DirEntry {
    let entry_type = if info.is_dir {
        EntryType::Directory
    } else if info.is_symlink {
        EntryType::Symlink
    } else {
        EntryType::File
    };
    DirEntry {
        name: info.name.clone(),
        entry_type,
        size: info.size,
        symlink_target: info.symlink_target.clone(),
    }
}

/// Convert a kaish-kernel `EntryInfo` to a kaish `Metadata`.
fn entry_info_to_metadata(info: &kaish_kernel::EntryInfo) -> Metadata {
    Metadata {
        is_dir: info.is_dir,
        is_file: info.is_file,
        is_symlink: info.is_symlink,
        size: info.size,
        modified: info.modified.map(|ts| {
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(ts)
        }),
    }
}

/// Prepend `/g` to a relative path for the backend.
///
/// The backend expects paths like `/g/b/{repo}/...`, but the filesystem
/// adapter receives paths relative to its mount point.
fn git_path(path: &Path) -> PathBuf {
    PathBuf::from("/g").join(path)
}

#[async_trait]
impl Filesystem for GitFilesystem {
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.backend
            .read(&git_path(path), None)
            .await
            .map_err(backend_to_io)
    }

    async fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        use kaish_kernel::WriteMode;
        self.backend
            .write(&git_path(path), data, WriteMode::Overwrite)
            .await
            .map_err(backend_to_io)
    }

    async fn list(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let entries = self
            .backend
            .list(&git_path(path))
            .await
            .map_err(backend_to_io)?;
        Ok(entries.iter().map(entry_info_to_dir_entry).collect())
    }

    async fn stat(&self, path: &Path) -> io::Result<Metadata> {
        let info = self
            .backend
            .stat(&git_path(path))
            .await
            .map_err(backend_to_io)?;
        Ok(entry_info_to_metadata(&info))
    }

    async fn mkdir(&self, path: &Path) -> io::Result<()> {
        self.backend
            .mkdir(&git_path(path))
            .await
            .map_err(backend_to_io)
    }

    async fn remove(&self, path: &Path) -> io::Result<()> {
        self.backend
            .remove(&git_path(path), false)
            .await
            .map_err(backend_to_io)
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn exists(&self, path: &Path) -> bool {
        self.backend.exists(&git_path(path)).await
    }

    async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.backend
            .rename(&git_path(from), &git_path(to))
            .await
            .map_err(backend_to_io)
    }

    fn real_path(&self, _path: &Path) -> Option<PathBuf> {
        // Git worktrees have real paths, but the CRDT layer doesn't expose them
        // through the KernelBackend trait. Future: resolve_real_path delegation.
        None
    }
}
