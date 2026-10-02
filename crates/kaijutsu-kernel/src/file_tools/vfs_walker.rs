//! VFS → WalkerFs adapter for kaish-glob integration.
//!
//! Bridges kaijutsu's `MountTable` to kaish-glob's `WalkerFs` trait,
//! enabling glob pattern matching and file walking over the virtual filesystem.

use std::path::{Path, PathBuf};

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

    fn walk_boundaries(&self) -> Vec<PathBuf> {
        self.0.walk_boundaries()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::backends::MemoryBackend;
    use std::path::PathBuf;

    /// The file tools' walks keep the shell's rule: a walk does not enter a
    /// kernel tree it did not start in.
    #[tokio::test]
    async fn a_walk_over_the_mount_table_stops_at_kernel_trees() {
        let table = MountTable::new();
        table.mount("/", MemoryBackend::new()).await;
        table.mount("/v/cas", MemoryBackend::new()).await;
        table.mkdir(Path::new("/home"), 0o755).await.unwrap();
        table.create(Path::new("/home/a.txt"), 0o644).await.unwrap();
        table.create(Path::new("/v/cas/b.txt"), 0o644).await.unwrap();

        let adapter = VfsWalkerAdapter(&table);
        let walk = kaish_glob::FileWalker::new(&adapter, "/").walk().await.unwrap();
        assert_eq!(walk.paths, [PathBuf::from("/home/a.txt")]);
        assert_eq!(walk.skipped_mounts, [PathBuf::from("/v")]);
    }
}
