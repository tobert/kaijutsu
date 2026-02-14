//! Local filesystem backend.
//!
//! Provides access to real filesystem paths, with path security
//! to prevent escaping the root directory.

use async_trait::async_trait;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio::fs;

use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::ops::VfsOps;
use crate::vfs::types::{DirEntry, FileAttr, FileType, SetAttr, StatFs};

/// Local filesystem backend.
///
/// All operations are relative to `root`. For example, if `root` is
/// `/home/amy/project`, then `read("src/main.rs")` reads
/// `/home/amy/project/src/main.rs`.
///
/// Path security is enforced: attempts to escape via `..` are blocked.
#[derive(Debug, Clone)]
pub struct LocalBackend {
    root: PathBuf,
    read_only: bool,
}

impl LocalBackend {
    /// Create a new local filesystem rooted at the given path.
    ///
    /// The root is canonicalized at construction time to handle symlinks
    /// (e.g. macOS `/tmp` → `/private/tmp`).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root: PathBuf = root.into();
        let root = root.canonicalize().unwrap_or(root);
        Self {
            root,
            read_only: false,
        }
    }

    /// Create a read-only local filesystem.
    pub fn read_only(root: impl Into<PathBuf>) -> Self {
        let root: PathBuf = root.into();
        let root = root.canonicalize().unwrap_or(root);
        Self {
            root,
            read_only: true,
        }
    }

    /// Set whether this filesystem is read-only.
    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    /// Get the root path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a relative path to an absolute path within the root.
    ///
    /// Returns an error if the path escapes the root (via `..`).
    async fn resolve(&self, path: &Path) -> VfsResult<PathBuf> {
        // Strip leading slash if present
        let path = path.strip_prefix("/").unwrap_or(path);

        // Handle empty path (root)
        if path.as_os_str().is_empty() {
            return Ok(self.root.clone());
        }

        // Join with root
        let full = self.root.join(path);

        // Canonicalize to resolve symlinks and ..
        // For non-existent paths, we need to check parent
        let canonical = if full.exists() {
            full.canonicalize().map_err(VfsError::from)?
        } else {
            // For new files, canonicalize parent and append filename
            let parent = full.parent().ok_or_else(|| VfsError::invalid_path("no parent"))?;

            let filename = full
                .file_name()
                .ok_or_else(|| VfsError::invalid_path("no filename"))?;

            if parent.exists() {
                parent.canonicalize().map_err(VfsError::from)?.join(filename)
            } else {
                // Parent doesn't exist, will fail on actual operation
                full
            }
        };

        // Verify we haven't escaped the root
        let canonical_root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        if !canonical.starts_with(&canonical_root) {
            return Err(VfsError::path_escapes_root(format!(
                "{} is not under {}",
                canonical.display(),
                canonical_root.display()
            )));
        }

        Ok(canonical)
    }

    /// Check if write operations are allowed.
    fn check_writable(&self) -> VfsResult<()> {
        if self.read_only {
            Err(VfsError::ReadOnly)
        } else {
            Ok(())
        }
    }

    /// Convert std::fs::Metadata to FileAttr.
    fn metadata_to_attr(meta: &std::fs::Metadata) -> FileAttr {
        let kind = if meta.is_dir() {
            FileType::Directory
        } else if meta.file_type().is_symlink() {
            FileType::Symlink
        } else {
            FileType::File
        };

        FileAttr {
            size: meta.len(),
            kind,
            perm: meta.permissions().mode(),
            mtime: meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            atime: meta.accessed().ok(),
            ctime: meta.created().ok(),
            nlink: meta.nlink() as u32,
            uid: Some(meta.uid()),
            gid: Some(meta.gid()),
        }
    }

}

#[async_trait]
impl VfsOps for LocalBackend {
    async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
        let full_path = self.resolve(path).await?;
        let meta = fs::symlink_metadata(&full_path).await.map_err(VfsError::from)?;
        Ok(Self::metadata_to_attr(&meta))
    }

    async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
        let full_path = self.resolve(path).await?;
        let mut entries = Vec::new();
        let mut dir = fs::read_dir(&full_path).await.map_err(VfsError::from)?;

        while let Some(entry) = dir.next_entry().await.map_err(VfsError::from)? {
            let file_type = entry.file_type().await.map_err(VfsError::from)?;
            let kind = if file_type.is_dir() {
                FileType::Directory
            } else if file_type.is_symlink() {
                FileType::Symlink
            } else {
                FileType::File
            };

            entries.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind,
            });
        }

        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let full_path = self.resolve(path).await?;
        let mut file = fs::File::open(&full_path).await.map_err(VfsError::from)?;

        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(VfsError::from)?;

        let mut buffer = vec![0u8; size as usize];
        let bytes_read = file.read(&mut buffer).await.map_err(VfsError::from)?;
        buffer.truncate(bytes_read);

        Ok(buffer)
    }

    async fn readlink(&self, path: &Path) -> VfsResult<PathBuf> {
        // Don't use resolve() here - it follows symlinks via canonicalize()
        // Instead, just join and do a simpler security check
        let path = path.strip_prefix("/").unwrap_or(path);
        let full_path = self.root.join(path);

        // Security check: ensure path doesn't escape root via components
        for component in path.components() {
            if matches!(component, std::path::Component::ParentDir) {
                // Could escape - do a more thorough check
                let canonical_root = self.root.canonicalize().unwrap_or_else(|_| self.root.clone());
                let parent = full_path.parent().unwrap_or(&full_path);
                if parent.exists() {
                    let canonical_parent = parent.canonicalize().map_err(VfsError::from)?;
                    if !canonical_parent.starts_with(&canonical_root) {
                        return Err(VfsError::path_escapes_root(path.display().to_string()));
                    }
                }
                break;
            }
        }

        fs::read_link(&full_path).await.map_err(VfsError::from)
    }

    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32> {
        use tokio::io::{AsyncSeekExt, AsyncWriteExt};

        self.check_writable()?;
        let full_path = self.resolve(path).await?;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(&full_path)
            .await
            .map_err(VfsError::from)?;

        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(VfsError::from)?;

        file.write_all(data).await.map_err(VfsError::from)?;

        Ok(data.len() as u32)
    }

    async fn create(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        use std::os::unix::fs::OpenOptionsExt;

        self.check_writable()?;
        let full_path = self.resolve(path).await?;

        // Ensure parent directory exists
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).await.map_err(VfsError::from)?;
        }

        // Create file with specified mode
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&full_path)
            .map_err(VfsError::from)?;

        let meta = file.metadata().map_err(VfsError::from)?;
        Ok(Self::metadata_to_attr(&meta))
    }

    async fn mkdir(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        use std::os::unix::fs::DirBuilderExt;

        self.check_writable()?;
        let full_path = self.resolve(path).await?;

        std::fs::DirBuilder::new()
            .mode(mode)
            .recursive(true)
            .create(&full_path)
            .map_err(VfsError::from)?;

        let meta = fs::metadata(&full_path).await.map_err(VfsError::from)?;
        Ok(Self::metadata_to_attr(&meta))
    }

    async fn unlink(&self, path: &Path) -> VfsResult<()> {
        self.check_writable()?;
        let full_path = self.resolve(path).await?;
        fs::remove_file(&full_path).await.map_err(VfsError::from)
    }

    async fn rmdir(&self, path: &Path) -> VfsResult<()> {
        self.check_writable()?;
        let full_path = self.resolve(path).await?;
        fs::remove_dir(&full_path).await.map_err(VfsError::from)
    }

    async fn rename(&self, from: &Path, to: &Path) -> VfsResult<()> {
        self.check_writable()?;
        let from_path = self.resolve(from).await?;
        let to_path = self.resolve(to).await?;

        // Ensure parent of destination exists
        if let Some(parent) = to_path.parent() {
            fs::create_dir_all(parent).await.map_err(VfsError::from)?;
        }

        fs::rename(&from_path, &to_path)
            .await
            .map_err(VfsError::from)
    }

    async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()> {
        self.check_writable()?;
        let full_path = self.resolve(path).await?;

        let file = fs::OpenOptions::new()
            .write(true)
            .open(&full_path)
            .await
            .map_err(VfsError::from)?;

        file.set_len(size).await.map_err(VfsError::from)
    }

    async fn setattr(&self, path: &Path, attr: SetAttr) -> VfsResult<FileAttr> {
        self.check_writable()?;
        let full_path = self.resolve(path).await?;

        // Handle size
        if let Some(size) = attr.size {
            let file = fs::OpenOptions::new()
                .write(true)
                .open(&full_path)
                .await
                .map_err(VfsError::from)?;
            file.set_len(size).await.map_err(VfsError::from)?;
        }

        // Handle permissions
        if let Some(perm) = attr.perm {
            let permissions = std::fs::Permissions::from_mode(perm);
            fs::set_permissions(&full_path, permissions)
                .await
                .map_err(VfsError::from)?;
        }

        // Handle times (requires filetime crate for proper support)
        // For now, we just touch mtime if needed
        if attr.mtime.is_some() || attr.atime.is_some() {
            // Best effort - touch the file
            let _ = fs::OpenOptions::new()
                .write(true)
                .open(&full_path)
                .await;
        }

        // Handle uid/gid (requires nix crate or libc)
        if attr.uid.is_some() || attr.gid.is_some() {
            // Would use nix::unistd::chown here
            // For now, skip silently
        }

        self.getattr(path).await
    }

    async fn symlink(&self, path: &Path, target: &Path) -> VfsResult<FileAttr> {
        self.check_writable()?;
        let full_path = self.resolve(path).await?;

        // Ensure parent directory exists
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).await.map_err(VfsError::from)?;
        }

        std::os::unix::fs::symlink(target, &full_path).map_err(VfsError::from)?;

        self.getattr(path).await
    }

    async fn link(&self, oldpath: &Path, newpath: &Path) -> VfsResult<FileAttr> {
        self.check_writable()?;
        let old_full = self.resolve(oldpath).await?;
        let new_full = self.resolve(newpath).await?;

        // Ensure parent of new path exists
        if let Some(parent) = new_full.parent() {
            fs::create_dir_all(parent).await.map_err(VfsError::from)?;
        }

        fs::hard_link(&old_full, &new_full)
            .await
            .map_err(VfsError::from)?;

        self.getattr(newpath).await
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    async fn statfs(&self) -> VfsResult<StatFs> {
        #[cfg(unix)]
        {
            use rustix::fs::statvfs;

            let stat = statvfs(&self.root).map_err(|e| VfsError::Io(e.into()))?;

            Ok(StatFs {
                blocks: stat.f_blocks,
                bfree: stat.f_bfree,
                bavail: stat.f_bavail,
                files: stat.f_files,
                ffree: stat.f_ffree,
                bsize: stat.f_bsize as u32,
                namelen: stat.f_namemax as u32,
                frsize: stat.f_frsize as u32,
            })
        }

        #[cfg(not(unix))]
        {
            Ok(StatFs::default())
        }
    }

    async fn real_path(&self, path: &Path) -> VfsResult<Option<PathBuf>> {
        // Strip leading slash if present
        let path = path.strip_prefix("/").unwrap_or(path);
        let full = self.root.join(path);

        // Use dunce for clean canonical paths (no \\?\ on Windows)
        let canonical = dunce::canonicalize(&full).map_err(VfsError::from)?;

        // Security check: ensure path is under root
        let canonical_root = dunce::canonicalize(&self.root)
            .unwrap_or_else(|_| self.root.clone());
        if !canonical.starts_with(&canonical_root) {
            return Err(VfsError::PermissionDenied(
                format!("path escapes mount root: {}", path.display())
            ));
        }

        Ok(Some(canonical))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup() -> (LocalBackend, TempDir) {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path());
        (backend, dir)
    }

    #[tokio::test]
    async fn test_create_and_read() {
        let (backend, _dir) = setup().await;

        backend.create(Path::new("test.txt"), 0o644).await.unwrap();
        backend
            .write(Path::new("test.txt"), 0, b"hello world")
            .await
            .unwrap();

        let data = backend.read(Path::new("test.txt"), 0, 100).await.unwrap();
        assert_eq!(data, b"hello world");
    }

    #[tokio::test]
    async fn test_partial_read() {
        let (backend, _dir) = setup().await;

        backend.create(Path::new("test.txt"), 0o644).await.unwrap();
        backend
            .write(Path::new("test.txt"), 0, b"hello world")
            .await
            .unwrap();

        let data = backend.read(Path::new("test.txt"), 6, 5).await.unwrap();
        assert_eq!(data, b"world");
    }

    #[tokio::test]
    async fn test_mkdir_and_readdir() {
        let (backend, _dir) = setup().await;

        backend.mkdir(Path::new("subdir"), 0o755).await.unwrap();
        backend
            .create(Path::new("subdir/file.txt"), 0o644)
            .await
            .unwrap();
        backend.create(Path::new("root.txt"), 0o644).await.unwrap();

        let entries = backend.readdir(Path::new("")).await.unwrap();
        let names: Vec<_> = entries.iter().map(|e| &e.name).collect();
        assert!(names.contains(&&"subdir".to_string()));
        assert!(names.contains(&&"root.txt".to_string()));
    }

    #[tokio::test]
    async fn test_read_only() {
        let (mut backend, _dir) = setup().await;
        backend.set_read_only(true);

        let result = backend.create(Path::new("test.txt"), 0o644).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_path_escape_blocked() {
        let (backend, _dir) = setup().await;

        let result = backend.read(Path::new("../../../etc/passwd"), 0, 100).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_symlink() {
        let (backend, _dir) = setup().await;

        backend.create(Path::new("target.txt"), 0o644).await.unwrap();
        backend
            .write(Path::new("target.txt"), 0, b"content")
            .await
            .unwrap();

        backend
            .symlink(Path::new("link.txt"), Path::new("target.txt"))
            .await
            .unwrap();

        let target = backend.readlink(Path::new("link.txt")).await.unwrap();
        assert_eq!(target, Path::new("target.txt"));
    }

    #[tokio::test]
    async fn test_rename() {
        let (backend, _dir) = setup().await;

        backend.create(Path::new("old.txt"), 0o644).await.unwrap();
        backend
            .write(Path::new("old.txt"), 0, b"content")
            .await
            .unwrap();

        backend
            .rename(Path::new("old.txt"), Path::new("new.txt"))
            .await
            .unwrap();

        assert!(backend.getattr(Path::new("old.txt")).await.is_err());
        let data = backend.read(Path::new("new.txt"), 0, 100).await.unwrap();
        assert_eq!(data, b"content");
    }

    #[tokio::test]
    async fn test_truncate() {
        let (backend, _dir) = setup().await;

        backend.create(Path::new("test.txt"), 0o644).await.unwrap();
        backend
            .write(Path::new("test.txt"), 0, b"hello world")
            .await
            .unwrap();

        backend.truncate(Path::new("test.txt"), 5).await.unwrap();

        let data = backend.read(Path::new("test.txt"), 0, 100).await.unwrap();
        assert_eq!(data, b"hello");
    }

    #[tokio::test]
    async fn test_hard_link() {
        let (backend, _dir) = setup().await;

        backend.create(Path::new("original.txt"), 0o644).await.unwrap();
        backend
            .write(Path::new("original.txt"), 0, b"shared content")
            .await
            .unwrap();

        backend
            .link(Path::new("original.txt"), Path::new("linked.txt"))
            .await
            .unwrap();

        let data = backend.read(Path::new("linked.txt"), 0, 100).await.unwrap();
        assert_eq!(data, b"shared content");

        // Both should show nlink >= 2
        let attr = backend.getattr(Path::new("original.txt")).await.unwrap();
        assert!(attr.nlink >= 2);
    }

    #[tokio::test]
    async fn test_real_path() {
        let (backend, dir) = setup().await;

        // Create a file
        std::fs::write(dir.path().join("test.txt"), "hello").unwrap();

        // Resolve it
        let real = backend.real_path(Path::new("test.txt")).await.unwrap();
        assert!(real.is_some());
        let real = real.unwrap();
        assert!(real.is_absolute());
        assert!(real.ends_with("test.txt"));
    }

    #[tokio::test]
    async fn test_real_path_escape_prevention() {
        let (backend, _dir) = setup().await;

        // Attempt escape
        let result = backend.real_path(Path::new("../etc/passwd")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_real_path_nonexistent() {
        let (backend, _dir) = setup().await;

        // Non-existent path should error (can't canonicalize)
        let result = backend.real_path(Path::new("nonexistent.txt")).await;
        assert!(result.is_err());
    }
}
