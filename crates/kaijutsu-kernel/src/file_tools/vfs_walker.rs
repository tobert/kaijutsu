//! VFS → WalkerFs adapter for kaish-glob integration.
//!
//! Bridges kaijutsu's `MountTable` to kaish-glob's `WalkerFs` trait,
//! enabling glob pattern matching and file walking over the virtual filesystem.

use std::path::Path;

use async_trait::async_trait;
use kaish_glob::{WalkerDirEntry, WalkerError, WalkerFs};

use crate::vfs::{DirEntry, MountTable, VfsOps};

/// Adapter that implements `kaish_glob::WalkerFs` for kaijutsu's `MountTable`.
pub struct VfsWalkerAdapter<'a>(pub &'a MountTable);

impl WalkerDirEntry for DirEntry {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_dir(&self) -> bool {
        self.kind.is_dir()
    }

    fn is_file(&self) -> bool {
        self.kind.is_file()
    }

    fn is_symlink(&self) -> bool {
        self.kind.is_symlink()
    }
}

#[async_trait]
impl WalkerFs for VfsWalkerAdapter<'_> {
    type DirEntry = DirEntry;

    async fn list_dir(&self, path: &Path) -> Result<Vec<DirEntry>, WalkerError> {
        self.0
            .readdir(path)
            .await
            .map_err(|e| WalkerError::Io(e.to_string()))
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>, WalkerError> {
        self.0
            .read_all(path)
            .await
            .map_err(|e| WalkerError::Io(e.to_string()))
    }

    async fn is_dir(&self, path: &Path) -> bool {
        self.0
            .getattr(path)
            .await
            .map(|attr| attr.kind.is_dir())
            .unwrap_or(false)
    }

    async fn exists(&self, path: &Path) -> bool {
        self.0.exists(path).await
    }
}
