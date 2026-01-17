//! VFS mount table with longest-prefix routing.
//!
//! Routes filesystem operations to the appropriate backend based on path.

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

use super::error::{VfsError, VfsResult};
use super::ops::VfsOps;
use super::types::{DirEntry, FileAttr, FileType, SetAttr, StatFs};

/// Information about a mount point.
#[derive(Debug, Clone)]
pub struct MountInfo {
    /// The mount path (e.g., "/mnt/project").
    pub path: PathBuf,
    /// Whether this mount is read-only.
    pub read_only: bool,
}

/// Routes filesystem operations to mounted backends.
///
/// Mount points are matched by longest prefix. For example, if `/mnt` and
/// `/mnt/project` are both mounted, a path like `/mnt/project/src/main.rs`
/// will be routed to the `/mnt/project` mount.
pub struct MountTable {
    /// Mount points, keyed by normalized path.
    mounts: RwLock<BTreeMap<PathBuf, Arc<dyn VfsOps>>>,
}

impl std::fmt::Debug for MountTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MountTable")
            .field("mounts", &"<locked>")
            .finish()
    }
}

impl Default for MountTable {
    fn default() -> Self {
        Self::new()
    }
}

impl MountTable {
    /// Create a new empty mount table.
    pub fn new() -> Self {
        Self {
            mounts: RwLock::new(BTreeMap::new()),
        }
    }

    /// Mount a filesystem at the given path.
    ///
    /// The path should be absolute (start with `/`). If a filesystem is
    /// already mounted at this path, it will be replaced.
    pub async fn mount(&self, path: impl Into<PathBuf>, fs: impl VfsOps + 'static) {
        let path = Self::normalize_mount_path(path.into());
        let mut mounts = self.mounts.write().await;
        mounts.insert(path, Arc::new(fs));
    }

    /// Mount a filesystem (already wrapped in Arc) at the given path.
    pub async fn mount_arc(&self, path: impl Into<PathBuf>, fs: Arc<dyn VfsOps>) {
        let path = Self::normalize_mount_path(path.into());
        let mut mounts = self.mounts.write().await;
        mounts.insert(path, fs);
    }

    /// Unmount the filesystem at the given path.
    ///
    /// Returns `true` if a mount was removed, `false` if nothing was mounted there.
    pub async fn unmount(&self, path: impl AsRef<Path>) -> bool {
        let path = Self::normalize_mount_path(path.as_ref().to_path_buf());
        let mut mounts = self.mounts.write().await;
        mounts.remove(&path).is_some()
    }

    /// List all current mounts.
    pub async fn list_mounts(&self) -> Vec<MountInfo> {
        let mounts = self.mounts.read().await;
        mounts
            .iter()
            .map(|(path, fs)| MountInfo {
                path: path.clone(),
                read_only: fs.read_only(),
            })
            .collect()
    }

    /// Normalize a mount path: ensure it starts with `/` and has no trailing slash.
    fn normalize_mount_path(path: PathBuf) -> PathBuf {
        let s = path.to_string_lossy();
        let s = s.trim_end_matches('/');
        if s.is_empty() {
            PathBuf::from("/")
        } else if !s.starts_with('/') {
            PathBuf::from(format!("/{}", s))
        } else {
            PathBuf::from(s)
        }
    }

    /// Find the mount point for a given path.
    ///
    /// Returns the mount and the path relative to that mount.
    async fn find_mount(&self, path: &Path) -> VfsResult<(Arc<dyn VfsOps>, PathBuf)> {
        let path_str = path.to_string_lossy();
        let normalized = if path_str.starts_with('/') {
            path.to_path_buf()
        } else {
            PathBuf::from(format!("/{}", path_str))
        };

        let mounts = self.mounts.read().await;

        // Find longest matching mount point
        let mut best_match: Option<(&PathBuf, &Arc<dyn VfsOps>)> = None;

        for (mount_path, fs) in mounts.iter() {
            let mount_str = mount_path.to_string_lossy();

            // Check if the path starts with this mount point
            let is_match = if mount_str == "/" {
                true // Root matches everything
            } else {
                let normalized_str = normalized.to_string_lossy();
                normalized_str == mount_str.as_ref()
                    || normalized_str.starts_with(&format!("{}/", mount_str))
            };

            if is_match {
                // Keep the longest match
                if best_match.is_none()
                    || mount_path.as_os_str().len()
                        > best_match.expect("checked is_none").0.as_os_str().len()
                {
                    best_match = Some((mount_path, fs));
                }
            }
        }

        match best_match {
            Some((mount_path, fs)) => {
                // Calculate relative path
                let mount_str = mount_path.to_string_lossy();
                let normalized_str = normalized.to_string_lossy();

                let relative = if mount_str == "/" {
                    normalized_str.trim_start_matches('/').to_string()
                } else {
                    normalized_str
                        .strip_prefix(mount_str.as_ref())
                        .unwrap_or("")
                        .trim_start_matches('/')
                        .to_string()
                };

                Ok((Arc::clone(fs), PathBuf::from(relative)))
            }
            None => Err(VfsError::no_mount_point(path.display().to_string())),
        }
    }

    /// List the root directory, synthesizing entries from mount points.
    async fn list_root(&self) -> VfsResult<Vec<DirEntry>> {
        let mounts = self.mounts.read().await;
        let mut entries = Vec::new();
        let mut seen_names = std::collections::HashSet::new();

        for mount_path in mounts.keys() {
            let mount_str = mount_path.to_string_lossy();
            if mount_str == "/" {
                // Root mount: list its contents directly
                if let Some(fs) = mounts.get(mount_path) {
                    if let Ok(root_entries) = fs.readdir(Path::new("")).await {
                        for entry in root_entries {
                            if seen_names.insert(entry.name.clone()) {
                                entries.push(entry);
                            }
                        }
                    }
                }
            } else {
                // Non-root mount: extract first path component
                let first_component = mount_str
                    .trim_start_matches('/')
                    .split('/')
                    .next()
                    .unwrap_or("");

                if !first_component.is_empty() && seen_names.insert(first_component.to_string()) {
                    entries.push(DirEntry {
                        name: first_component.to_string(),
                        kind: FileType::Directory,
                    });
                }
            }
        }

        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }
}

#[async_trait]
impl VfsOps for MountTable {
    async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
        // Special case: root always exists
        let path_str = path.to_string_lossy();
        if path_str.is_empty() || path_str == "/" {
            return Ok(FileAttr::directory(0o755));
        }

        // Check if path is a mount point itself
        let normalized = Self::normalize_mount_path(path.to_path_buf());
        {
            let mounts = self.mounts.read().await;
            if mounts.contains_key(&normalized) {
                return Ok(FileAttr::directory(0o755));
            }
        }

        let (fs, relative) = self.find_mount(path).await?;
        fs.getattr(&relative).await
    }

    async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
        // Special case: listing root might need to show mount points
        let path_str = path.to_string_lossy();
        if path_str.is_empty() || path_str == "/" {
            return self.list_root().await;
        }

        let (fs, relative) = self.find_mount(path).await?;
        fs.readdir(&relative).await
    }

    async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        let (fs, relative) = self.find_mount(path).await?;
        fs.read(&relative, offset, size).await
    }

    async fn readlink(&self, path: &Path) -> VfsResult<PathBuf> {
        let (fs, relative) = self.find_mount(path).await?;
        fs.readlink(&relative).await
    }

    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32> {
        let (fs, relative) = self.find_mount(path).await?;
        fs.write(&relative, offset, data).await
    }

    async fn create(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        let (fs, relative) = self.find_mount(path).await?;
        fs.create(&relative, mode).await
    }

    async fn mkdir(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        let (fs, relative) = self.find_mount(path).await?;
        fs.mkdir(&relative, mode).await
    }

    async fn unlink(&self, path: &Path) -> VfsResult<()> {
        let (fs, relative) = self.find_mount(path).await?;
        fs.unlink(&relative).await
    }

    async fn rmdir(&self, path: &Path) -> VfsResult<()> {
        let (fs, relative) = self.find_mount(path).await?;
        fs.rmdir(&relative).await
    }

    async fn rename(&self, from: &Path, to: &Path) -> VfsResult<()> {
        // Both paths must be in the same mount
        let (from_fs, from_relative) = self.find_mount(from).await?;
        let (to_fs, to_relative) = self.find_mount(to).await?;

        // Check if they're the same mount (by Arc pointer)
        if !Arc::ptr_eq(&from_fs, &to_fs) {
            return Err(VfsError::CrossDeviceLink);
        }

        from_fs.rename(&from_relative, &to_relative).await
    }

    async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()> {
        let (fs, relative) = self.find_mount(path).await?;
        fs.truncate(&relative, size).await
    }

    async fn setattr(&self, path: &Path, attr: SetAttr) -> VfsResult<FileAttr> {
        let (fs, relative) = self.find_mount(path).await?;
        fs.setattr(&relative, attr).await
    }

    async fn symlink(&self, path: &Path, target: &Path) -> VfsResult<FileAttr> {
        let (fs, relative) = self.find_mount(path).await?;
        fs.symlink(&relative, target).await
    }

    async fn link(&self, oldpath: &Path, newpath: &Path) -> VfsResult<FileAttr> {
        // Both paths must be in the same mount
        let (old_fs, old_relative) = self.find_mount(oldpath).await?;
        let (new_fs, new_relative) = self.find_mount(newpath).await?;

        if !Arc::ptr_eq(&old_fs, &new_fs) {
            return Err(VfsError::CrossDeviceLink);
        }

        old_fs.link(&old_relative, &new_relative).await
    }

    fn read_only(&self) -> bool {
        // Mount table itself isn't read-only; individual mounts might be
        false
    }

    async fn statfs(&self) -> VfsResult<StatFs> {
        // Return stats from root mount if available
        let mounts = self.mounts.read().await;
        if let Some(root_fs) = mounts.get(&PathBuf::from("/")) {
            return root_fs.statfs().await;
        }
        Ok(StatFs::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::backends::MemoryBackend;

    #[tokio::test]
    async fn test_basic_mount() {
        let table = MountTable::new();
        let scratch = MemoryBackend::new();
        scratch.create(Path::new("test.txt"), 0o644).await.unwrap();
        scratch
            .write(Path::new("test.txt"), 0, b"hello")
            .await
            .unwrap();

        table.mount("/scratch", scratch).await;

        let data = table
            .read(Path::new("/scratch/test.txt"), 0, 100)
            .await
            .unwrap();
        assert_eq!(data, b"hello");
    }

    #[tokio::test]
    async fn test_multiple_mounts() {
        let table = MountTable::new();

        let scratch = MemoryBackend::new();
        scratch.create(Path::new("a.txt"), 0o644).await.unwrap();
        scratch.write(Path::new("a.txt"), 0, b"scratch").await.unwrap();
        table.mount("/scratch", scratch).await;

        let data = MemoryBackend::new();
        data.create(Path::new("b.txt"), 0o644).await.unwrap();
        data.write(Path::new("b.txt"), 0, b"data").await.unwrap();
        table.mount("/data", data).await;

        assert_eq!(
            table.read(Path::new("/scratch/a.txt"), 0, 100).await.unwrap(),
            b"scratch"
        );
        assert_eq!(
            table.read(Path::new("/data/b.txt"), 0, 100).await.unwrap(),
            b"data"
        );
    }

    #[tokio::test]
    async fn test_nested_mount() {
        let table = MountTable::new();

        let outer = MemoryBackend::new();
        outer.create(Path::new("outer.txt"), 0o644).await.unwrap();
        outer
            .write(Path::new("outer.txt"), 0, b"outer")
            .await
            .unwrap();
        table.mount("/mnt", outer).await;

        let inner = MemoryBackend::new();
        inner.create(Path::new("inner.txt"), 0o644).await.unwrap();
        inner
            .write(Path::new("inner.txt"), 0, b"inner")
            .await
            .unwrap();
        table.mount("/mnt/project", inner).await;

        // /mnt/outer.txt should come from outer mount
        assert_eq!(
            table.read(Path::new("/mnt/outer.txt"), 0, 100).await.unwrap(),
            b"outer"
        );

        // /mnt/project/inner.txt should come from inner mount
        assert_eq!(
            table
                .read(Path::new("/mnt/project/inner.txt"), 0, 100)
                .await
                .unwrap(),
            b"inner"
        );
    }

    #[tokio::test]
    async fn test_list_root() {
        let table = MountTable::new();
        table.mount("/scratch", MemoryBackend::new()).await;
        table.mount("/mnt/a", MemoryBackend::new()).await;
        table.mount("/mnt/b", MemoryBackend::new()).await;

        let entries = table.readdir(Path::new("/")).await.unwrap();
        let names: Vec<_> = entries.iter().map(|e| &e.name).collect();

        assert!(names.contains(&&"scratch".to_string()));
        assert!(names.contains(&&"mnt".to_string()));
    }

    #[tokio::test]
    async fn test_unmount() {
        let table = MountTable::new();

        let fs = MemoryBackend::new();
        fs.create(Path::new("test.txt"), 0o644).await.unwrap();
        fs.write(Path::new("test.txt"), 0, b"data").await.unwrap();
        table.mount("/scratch", fs).await;

        assert!(table.read(Path::new("/scratch/test.txt"), 0, 100).await.is_ok());

        table.unmount("/scratch").await;

        assert!(table.read(Path::new("/scratch/test.txt"), 0, 100).await.is_err());
    }

    #[tokio::test]
    async fn test_list_mounts() {
        let table = MountTable::new();
        table.mount("/scratch", MemoryBackend::new()).await;
        table.mount("/data", MemoryBackend::new()).await;

        let mounts = table.list_mounts().await;
        assert_eq!(mounts.len(), 2);

        let paths: Vec<_> = mounts.iter().map(|m| &m.path).collect();
        assert!(paths.contains(&&PathBuf::from("/scratch")));
        assert!(paths.contains(&&PathBuf::from("/data")));
    }

    #[tokio::test]
    async fn test_no_mount_error() {
        let table = MountTable::new();
        let result = table.read(Path::new("/nothing/here.txt"), 0, 100).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_root_mount() {
        let table = MountTable::new();

        let root = MemoryBackend::new();
        root.create(Path::new("at-root.txt"), 0o644).await.unwrap();
        root.write(Path::new("at-root.txt"), 0, b"root file")
            .await
            .unwrap();
        table.mount("/", root).await;

        let data = table.read(Path::new("/at-root.txt"), 0, 100).await.unwrap();
        assert_eq!(data, b"root file");
    }

    #[tokio::test]
    async fn test_write_through_table() {
        let table = MountTable::new();
        table.mount("/scratch", MemoryBackend::new()).await;

        table
            .create(Path::new("/scratch/new.txt"), 0o644)
            .await
            .unwrap();
        table
            .write(Path::new("/scratch/new.txt"), 0, b"created")
            .await
            .unwrap();

        let data = table.read(Path::new("/scratch/new.txt"), 0, 100).await.unwrap();
        assert_eq!(data, b"created");
    }

    #[tokio::test]
    async fn test_stat_mount_point() {
        let table = MountTable::new();
        table.mount("/scratch", MemoryBackend::new()).await;

        let attr = table.getattr(Path::new("/scratch")).await.unwrap();
        assert!(attr.is_dir());
    }

    #[tokio::test]
    async fn test_stat_root() {
        let table = MountTable::new();
        let attr = table.getattr(Path::new("/")).await.unwrap();
        assert!(attr.is_dir());
    }

    #[tokio::test]
    async fn test_cross_mount_rename_fails() {
        let table = MountTable::new();
        table.mount("/a", MemoryBackend::new()).await;
        table.mount("/b", MemoryBackend::new()).await;

        table.create(Path::new("/a/file.txt"), 0o644).await.unwrap();

        let result = table.rename(Path::new("/a/file.txt"), Path::new("/b/file.txt")).await;
        assert!(matches!(result, Err(VfsError::CrossDeviceLink)));
    }
}
