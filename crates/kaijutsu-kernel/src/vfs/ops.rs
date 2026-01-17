//! VFS operations trait.
//!
//! This trait defines the core filesystem operations in a way that's
//! designed for RPC (path-based, no inodes, explicit offset/size).

use async_trait::async_trait;
use std::path::Path;

use super::types::{DirEntry, FileAttr, SetAttr, StatFs};
use super::VfsResult;

/// Core VFS operations trait.
///
/// All operations are path-based (no inode numbers) for RPC-friendliness.
/// The FUSE client handles inode ↔ path mapping locally.
///
/// Paths are always relative to the backend's root. The MountTable handles
/// routing and path translation.
#[async_trait]
pub trait VfsOps: Send + Sync {
    // ========================================================================
    // Reading
    // ========================================================================

    /// Get file attributes.
    async fn getattr(&self, path: &Path) -> VfsResult<FileAttr>;

    /// Read directory entries.
    ///
    /// Returns all entries in the directory (no pagination).
    /// For very large directories, consider using an iterator-based approach.
    async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>>;

    /// Read file contents.
    ///
    /// Reads up to `size` bytes starting at `offset`.
    /// Returns fewer bytes if EOF is reached.
    async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>>;

    /// Read symbolic link target.
    async fn readlink(&self, path: &Path) -> VfsResult<std::path::PathBuf>;

    // ========================================================================
    // Writing
    // ========================================================================

    /// Write data to a file.
    ///
    /// Writes `data` at the specified `offset`.
    /// Returns the number of bytes written.
    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32>;

    /// Create a new file.
    ///
    /// Returns the attributes of the newly created file.
    async fn create(&self, path: &Path, mode: u32) -> VfsResult<FileAttr>;

    /// Create a new directory.
    ///
    /// Returns the attributes of the newly created directory.
    async fn mkdir(&self, path: &Path, mode: u32) -> VfsResult<FileAttr>;

    /// Remove a file.
    async fn unlink(&self, path: &Path) -> VfsResult<()>;

    /// Remove an empty directory.
    async fn rmdir(&self, path: &Path) -> VfsResult<()>;

    /// Rename a file or directory.
    async fn rename(&self, from: &Path, to: &Path) -> VfsResult<()>;

    /// Truncate a file to the specified size.
    async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()>;

    /// Set file attributes.
    async fn setattr(&self, path: &Path, attr: SetAttr) -> VfsResult<FileAttr>;

    /// Create a symbolic link.
    ///
    /// Creates a symlink at `path` pointing to `target`.
    async fn symlink(&self, path: &Path, target: &Path) -> VfsResult<FileAttr>;

    /// Create a hard link.
    ///
    /// Creates a hard link at `newpath` pointing to `oldpath`.
    async fn link(&self, oldpath: &Path, newpath: &Path) -> VfsResult<FileAttr>;

    // ========================================================================
    // Metadata
    // ========================================================================

    /// Returns true if this filesystem is read-only.
    fn read_only(&self) -> bool;

    /// Get filesystem statistics.
    async fn statfs(&self) -> VfsResult<StatFs>;

    // ========================================================================
    // Convenience methods (default implementations)
    // ========================================================================

    /// Check if a path exists.
    async fn exists(&self, path: &Path) -> bool {
        self.getattr(path).await.is_ok()
    }

    /// Read entire file contents.
    ///
    /// Convenience method that reads the whole file.
    async fn read_all(&self, path: &Path) -> VfsResult<Vec<u8>> {
        let attr = self.getattr(path).await?;
        self.read(path, 0, attr.size as u32).await
    }

    /// Write entire file contents.
    ///
    /// Convenience method that truncates and writes the whole file.
    async fn write_all(&self, path: &Path, data: &[u8]) -> VfsResult<()> {
        // Create or truncate
        if self.exists(path).await {
            self.truncate(path, 0).await?;
        } else {
            self.create(path, 0o644).await?;
        }
        self.write(path, 0, data).await?;
        Ok(())
    }
}
