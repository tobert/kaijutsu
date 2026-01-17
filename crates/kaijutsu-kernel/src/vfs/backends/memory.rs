//! In-memory filesystem backend.
//!
//! Used for `/scratch` and testing. All data is ephemeral.

use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::ops::VfsOps;
use crate::vfs::types::{DirEntry, FileAttr, FileType, SetAttr, StatFs};

/// Entry in the memory filesystem.
#[derive(Debug, Clone)]
enum Entry {
    File {
        data: Vec<u8>,
        attr: FileAttr,
    },
    Directory {
        attr: FileAttr,
    },
    Symlink {
        target: PathBuf,
        attr: FileAttr,
    },
}

impl Entry {
    fn attr(&self) -> &FileAttr {
        match self {
            Entry::File { attr, .. } => attr,
            Entry::Directory { attr } => attr,
            Entry::Symlink { attr, .. } => attr,
        }
    }

    fn attr_mut(&mut self) -> &mut FileAttr {
        match self {
            Entry::File { attr, .. } => attr,
            Entry::Directory { attr } => attr,
            Entry::Symlink { attr, .. } => attr,
        }
    }
}

/// In-memory filesystem backend.
///
/// Thread-safe via internal `RwLock`. All data is lost when dropped.
#[derive(Debug)]
pub struct MemoryBackend {
    entries: RwLock<HashMap<PathBuf, Entry>>,
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryBackend {
    /// Create a new empty in-memory filesystem.
    pub fn new() -> Self {
        let mut entries = HashMap::new();
        // Root directory always exists
        entries.insert(
            PathBuf::from(""),
            Entry::Directory {
                attr: FileAttr::directory(0o755),
            },
        );
        Self {
            entries: RwLock::new(entries),
        }
    }

    /// Normalize a path: remove leading `/`, resolve `.` and `..`.
    fn normalize(path: &Path) -> PathBuf {
        let mut result = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::RootDir => {}
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    result.pop();
                }
                std::path::Component::Normal(s) => {
                    result.push(s);
                }
                std::path::Component::Prefix(_) => {}
            }
        }
        result
    }

    /// Ensure all parent directories exist.
    fn ensure_parents(&self, path: &Path) -> VfsResult<()> {
        let mut entries = self
            .entries
            .write()
            .map_err(|_| VfsError::other("lock poisoned"))?;

        let mut current = PathBuf::new();
        for component in path.parent().into_iter().flat_map(|p| p.components()) {
            if let std::path::Component::Normal(s) = component {
                current.push(s);
                entries.entry(current.clone()).or_insert(Entry::Directory {
                    attr: FileAttr::directory(0o755),
                });
            }
        }
        Ok(())
    }

    /// Get the path string for error messages.
    fn path_str(path: &Path) -> String {
        path.display().to_string()
    }
}

#[async_trait]
impl VfsOps for MemoryBackend {
    async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
        let normalized = Self::normalize(path);
        let entries = self
            .entries
            .read()
            .map_err(|_| VfsError::other("lock poisoned"))?;

        // Handle root directory
        if normalized.as_os_str().is_empty() {
            return Ok(FileAttr::directory(0o755));
        }

        entries
            .get(&normalized)
            .map(|e| e.attr().clone())
            .ok_or_else(|| VfsError::not_found(Self::path_str(&normalized)))
    }

    async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
        let normalized = Self::normalize(path);
        let entries = self
            .entries
            .read()
            .map_err(|_| VfsError::other("lock poisoned"))?;

        // Verify the path is a directory
        if !normalized.as_os_str().is_empty() {
            match entries.get(&normalized) {
                Some(Entry::Directory { .. }) => {}
                Some(_) => {
                    return Err(VfsError::not_a_directory(Self::path_str(&normalized)));
                }
                None => {
                    return Err(VfsError::not_found(Self::path_str(&normalized)));
                }
            }
        }

        // Find all direct children
        let prefix = if normalized.as_os_str().is_empty() {
            PathBuf::new()
        } else {
            normalized.clone()
        };

        let mut result = Vec::new();
        for (entry_path, entry) in entries.iter() {
            if let Some(parent) = entry_path.parent() {
                if parent == prefix && entry_path != &normalized {
                    if let Some(name) = entry_path.file_name() {
                        let kind = match entry {
                            Entry::File { .. } => FileType::File,
                            Entry::Directory { .. } => FileType::Directory,
                            Entry::Symlink { .. } => FileType::Symlink,
                        };
                        result.push(DirEntry {
                            name: name.to_string_lossy().into_owned(),
                            kind,
                        });
                    }
                }
            }
        }

        // Sort for consistent ordering
        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(result)
    }

    async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        let normalized = Self::normalize(path);
        let entries = self
            .entries
            .read()
            .map_err(|_| VfsError::other("lock poisoned"))?;

        match entries.get(&normalized) {
            Some(Entry::File { data, .. }) => {
                let start = (offset as usize).min(data.len());
                let end = (start + size as usize).min(data.len());
                Ok(data[start..end].to_vec())
            }
            Some(Entry::Directory { .. }) => {
                Err(VfsError::is_a_directory(Self::path_str(&normalized)))
            }
            Some(Entry::Symlink { .. }) => {
                // For symlinks, we'd need to resolve. For now, error.
                Err(VfsError::other("cannot read symlink as file"))
            }
            None => Err(VfsError::not_found(Self::path_str(&normalized))),
        }
    }

    async fn readlink(&self, path: &Path) -> VfsResult<PathBuf> {
        let normalized = Self::normalize(path);
        let entries = self
            .entries
            .read()
            .map_err(|_| VfsError::other("lock poisoned"))?;

        match entries.get(&normalized) {
            Some(Entry::Symlink { target, .. }) => Ok(target.clone()),
            Some(_) => Err(VfsError::NotASymlink(Self::path_str(&normalized))),
            None => Err(VfsError::not_found(Self::path_str(&normalized))),
        }
    }

    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32> {
        let normalized = Self::normalize(path);

        let mut entries = self
            .entries
            .write()
            .map_err(|_| VfsError::other("lock poisoned"))?;

        match entries.get_mut(&normalized) {
            Some(Entry::File {
                data: file_data,
                attr,
            }) => {
                let offset = offset as usize;
                // Extend if necessary
                if offset + data.len() > file_data.len() {
                    file_data.resize(offset + data.len(), 0);
                }
                file_data[offset..offset + data.len()].copy_from_slice(data);
                attr.size = file_data.len() as u64;
                attr.mtime = SystemTime::now();
                Ok(data.len() as u32)
            }
            Some(Entry::Directory { .. }) => {
                Err(VfsError::is_a_directory(Self::path_str(&normalized)))
            }
            Some(Entry::Symlink { .. }) => Err(VfsError::other("cannot write to symlink")),
            None => Err(VfsError::not_found(Self::path_str(&normalized))),
        }
    }

    async fn create(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        let normalized = Self::normalize(path);

        // Ensure parent directories exist
        self.ensure_parents(&normalized)?;

        let mut entries = self
            .entries
            .write()
            .map_err(|_| VfsError::other("lock poisoned"))?;

        // Check if already exists
        if entries.contains_key(&normalized) {
            return Err(VfsError::already_exists(Self::path_str(&normalized)));
        }

        let attr = FileAttr::file(0, mode);
        entries.insert(
            normalized,
            Entry::File {
                data: Vec::new(),
                attr: attr.clone(),
            },
        );

        Ok(attr)
    }

    async fn mkdir(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        let normalized = Self::normalize(path);

        // Ensure parent directories exist
        self.ensure_parents(&normalized)?;

        let mut entries = self
            .entries
            .write()
            .map_err(|_| VfsError::other("lock poisoned"))?;

        // Check if something already exists
        if let Some(existing) = entries.get(&normalized) {
            return match existing {
                Entry::Directory { attr } => Ok(attr.clone()),
                _ => Err(VfsError::already_exists(Self::path_str(&normalized))),
            };
        }

        let attr = FileAttr::directory(mode);
        entries.insert(normalized, Entry::Directory { attr: attr.clone() });
        Ok(attr)
    }

    async fn unlink(&self, path: &Path) -> VfsResult<()> {
        let normalized = Self::normalize(path);

        if normalized.as_os_str().is_empty() {
            return Err(VfsError::permission_denied("cannot remove root"));
        }

        let mut entries = self
            .entries
            .write()
            .map_err(|_| VfsError::other("lock poisoned"))?;

        match entries.get(&normalized) {
            Some(Entry::Directory { .. }) => {
                Err(VfsError::is_a_directory(Self::path_str(&normalized)))
            }
            Some(_) => {
                entries.remove(&normalized);
                Ok(())
            }
            None => Err(VfsError::not_found(Self::path_str(&normalized))),
        }
    }

    async fn rmdir(&self, path: &Path) -> VfsResult<()> {
        let normalized = Self::normalize(path);

        if normalized.as_os_str().is_empty() {
            return Err(VfsError::permission_denied("cannot remove root"));
        }

        let mut entries = self
            .entries
            .write()
            .map_err(|_| VfsError::other("lock poisoned"))?;

        // Check if it's a directory
        match entries.get(&normalized) {
            Some(Entry::Directory { .. }) => {}
            Some(_) => {
                return Err(VfsError::not_a_directory(Self::path_str(&normalized)));
            }
            None => {
                return Err(VfsError::not_found(Self::path_str(&normalized)));
            }
        }

        // Check for children
        let has_children = entries
            .keys()
            .any(|k| k.parent() == Some(&normalized) && k != &normalized);

        if has_children {
            return Err(VfsError::directory_not_empty(Self::path_str(&normalized)));
        }

        entries.remove(&normalized);
        Ok(())
    }

    async fn rename(&self, from: &Path, to: &Path) -> VfsResult<()> {
        let from_normalized = Self::normalize(from);
        let to_normalized = Self::normalize(to);

        // Ensure parent of destination exists
        self.ensure_parents(&to_normalized)?;

        let mut entries = self
            .entries
            .write()
            .map_err(|_| VfsError::other("lock poisoned"))?;

        // Remove source entry
        let entry = entries
            .remove(&from_normalized)
            .ok_or_else(|| VfsError::not_found(Self::path_str(&from_normalized)))?;

        // If it's a directory, we need to rename all children too
        if matches!(entry, Entry::Directory { .. }) {
            let children: Vec<_> = entries
                .keys()
                .filter(|k| k.starts_with(&from_normalized))
                .cloned()
                .collect();

            for child in children {
                if let Some(child_entry) = entries.remove(&child) {
                    let relative = child.strip_prefix(&from_normalized).unwrap();
                    let new_path = to_normalized.join(relative);
                    entries.insert(new_path, child_entry);
                }
            }
        }

        // Insert at new location (possibly overwriting)
        entries.insert(to_normalized, entry);
        Ok(())
    }

    async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()> {
        let normalized = Self::normalize(path);

        let mut entries = self
            .entries
            .write()
            .map_err(|_| VfsError::other("lock poisoned"))?;

        match entries.get_mut(&normalized) {
            Some(Entry::File { data, attr }) => {
                data.resize(size as usize, 0);
                attr.size = size;
                attr.mtime = SystemTime::now();
                Ok(())
            }
            Some(Entry::Directory { .. }) => {
                Err(VfsError::is_a_directory(Self::path_str(&normalized)))
            }
            Some(Entry::Symlink { .. }) => Err(VfsError::other("cannot truncate symlink")),
            None => Err(VfsError::not_found(Self::path_str(&normalized))),
        }
    }

    async fn setattr(&self, path: &Path, set: SetAttr) -> VfsResult<FileAttr> {
        let normalized = Self::normalize(path);

        let mut entries = self
            .entries
            .write()
            .map_err(|_| VfsError::other("lock poisoned"))?;

        let entry = entries
            .get_mut(&normalized)
            .ok_or_else(|| VfsError::not_found(Self::path_str(&normalized)))?;

        // Handle size change (requires access to data for files)
        if let Some(size) = set.size {
            if let Entry::File { data, attr } = entry {
                data.resize(size as usize, 0);
                attr.size = size;
            }
        }

        // Handle other attribute changes
        let attr = entry.attr_mut();
        if let Some(mtime) = set.mtime {
            attr.mtime = mtime;
        }
        if let Some(atime) = set.atime {
            attr.atime = Some(atime);
        }
        if let Some(perm) = set.perm {
            attr.perm = perm;
        }
        if let Some(uid) = set.uid {
            attr.uid = Some(uid);
        }
        if let Some(gid) = set.gid {
            attr.gid = Some(gid);
        }

        Ok(entry.attr().clone())
    }

    async fn symlink(&self, path: &Path, target: &Path) -> VfsResult<FileAttr> {
        let normalized = Self::normalize(path);

        // Ensure parent directories exist
        self.ensure_parents(&normalized)?;

        let mut entries = self
            .entries
            .write()
            .map_err(|_| VfsError::other("lock poisoned"))?;

        // Check if already exists
        if entries.contains_key(&normalized) {
            return Err(VfsError::already_exists(Self::path_str(&normalized)));
        }

        let target_str = target.to_string_lossy();
        let attr = FileAttr::symlink(target_str.len() as u64);
        entries.insert(
            normalized,
            Entry::Symlink {
                target: target.to_path_buf(),
                attr: attr.clone(),
            },
        );

        Ok(attr)
    }

    async fn link(&self, oldpath: &Path, newpath: &Path) -> VfsResult<FileAttr> {
        // Memory backend doesn't support hard links
        let _ = (oldpath, newpath);
        Err(VfsError::other("hard links not supported in memory backend"))
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn statfs(&self) -> VfsResult<StatFs> {
        Ok(StatFs::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_and_read() {
        let fs = MemoryBackend::new();
        fs.create(Path::new("test.txt"), 0o644).await.unwrap();
        fs.write(Path::new("test.txt"), 0, b"hello world")
            .await
            .unwrap();

        let data = fs.read(Path::new("test.txt"), 0, 100).await.unwrap();
        assert_eq!(data, b"hello world");
    }

    #[tokio::test]
    async fn test_partial_read() {
        let fs = MemoryBackend::new();
        fs.create(Path::new("test.txt"), 0o644).await.unwrap();
        fs.write(Path::new("test.txt"), 0, b"hello world")
            .await
            .unwrap();

        let data = fs.read(Path::new("test.txt"), 6, 5).await.unwrap();
        assert_eq!(data, b"world");
    }

    #[tokio::test]
    async fn test_mkdir_and_readdir() {
        let fs = MemoryBackend::new();
        fs.mkdir(Path::new("subdir"), 0o755).await.unwrap();
        fs.create(Path::new("subdir/file.txt"), 0o644)
            .await
            .unwrap();
        fs.create(Path::new("root.txt"), 0o644).await.unwrap();

        let entries = fs.readdir(Path::new("")).await.unwrap();
        let names: Vec<_> = entries.iter().map(|e| &e.name).collect();
        assert!(names.contains(&&"subdir".to_string()));
        assert!(names.contains(&&"root.txt".to_string()));

        let subentries = fs.readdir(Path::new("subdir")).await.unwrap();
        assert_eq!(subentries.len(), 1);
        assert_eq!(subentries[0].name, "file.txt");
    }

    #[tokio::test]
    async fn test_unlink() {
        let fs = MemoryBackend::new();
        fs.create(Path::new("test.txt"), 0o644).await.unwrap();
        assert!(fs.getattr(Path::new("test.txt")).await.is_ok());

        fs.unlink(Path::new("test.txt")).await.unwrap();
        assert!(fs.getattr(Path::new("test.txt")).await.is_err());
    }

    #[tokio::test]
    async fn test_rmdir() {
        let fs = MemoryBackend::new();
        fs.mkdir(Path::new("empty"), 0o755).await.unwrap();
        fs.rmdir(Path::new("empty")).await.unwrap();
        assert!(fs.getattr(Path::new("empty")).await.is_err());
    }

    #[tokio::test]
    async fn test_rmdir_not_empty() {
        let fs = MemoryBackend::new();
        fs.mkdir(Path::new("nonempty"), 0o755).await.unwrap();
        fs.create(Path::new("nonempty/file.txt"), 0o644)
            .await
            .unwrap();

        let result = fs.rmdir(Path::new("nonempty")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_rename() {
        let fs = MemoryBackend::new();
        fs.create(Path::new("old.txt"), 0o644).await.unwrap();
        fs.write(Path::new("old.txt"), 0, b"content").await.unwrap();

        fs.rename(Path::new("old.txt"), Path::new("new.txt"))
            .await
            .unwrap();

        assert!(fs.getattr(Path::new("old.txt")).await.is_err());
        let data = fs.read(Path::new("new.txt"), 0, 100).await.unwrap();
        assert_eq!(data, b"content");
    }

    #[tokio::test]
    async fn test_symlink() {
        let fs = MemoryBackend::new();
        fs.symlink(Path::new("link"), Path::new("/target/path"))
            .await
            .unwrap();

        let target = fs.readlink(Path::new("link")).await.unwrap();
        assert_eq!(target, Path::new("/target/path"));

        let attr = fs.getattr(Path::new("link")).await.unwrap();
        assert!(attr.is_symlink());
    }

    #[tokio::test]
    async fn test_truncate() {
        let fs = MemoryBackend::new();
        fs.create(Path::new("test.txt"), 0o644).await.unwrap();
        fs.write(Path::new("test.txt"), 0, b"hello world")
            .await
            .unwrap();

        fs.truncate(Path::new("test.txt"), 5).await.unwrap();

        let data = fs.read(Path::new("test.txt"), 0, 100).await.unwrap();
        assert_eq!(data, b"hello");
    }

    #[tokio::test]
    async fn test_auto_create_parents() {
        let fs = MemoryBackend::new();
        fs.create(Path::new("a/b/c/file.txt"), 0o644).await.unwrap();

        assert!(fs.getattr(Path::new("a")).await.unwrap().is_dir());
        assert!(fs.getattr(Path::new("a/b")).await.unwrap().is_dir());
        assert!(fs.getattr(Path::new("a/b/c")).await.unwrap().is_dir());
    }

    #[tokio::test]
    async fn test_path_normalization() {
        let fs = MemoryBackend::new();
        fs.create(Path::new("/a/b/c.txt"), 0o644).await.unwrap();

        // Various path forms should all work
        assert!(fs.getattr(Path::new("a/b/c.txt")).await.is_ok());
        assert!(fs.getattr(Path::new("/a/b/c.txt")).await.is_ok());
        assert!(fs.getattr(Path::new("a/./b/c.txt")).await.is_ok());
        assert!(fs.getattr(Path::new("a/b/../b/c.txt")).await.is_ok());
    }
}
