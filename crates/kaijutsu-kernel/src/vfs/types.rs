//! Core VFS types.
//!
//! These types are designed to be RPC-friendly (path-based, no inodes)
//! and can be serialized for Cap'n Proto transmission.

use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// File type enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileType {
    /// Regular file.
    File,
    /// Directory.
    Directory,
    /// Symbolic link.
    Symlink,
}

impl FileType {
    /// Returns true if this is a regular file.
    pub fn is_file(&self) -> bool {
        matches!(self, FileType::File)
    }

    /// Returns true if this is a directory.
    pub fn is_dir(&self) -> bool {
        matches!(self, FileType::Directory)
    }

    /// Returns true if this is a symbolic link.
    pub fn is_symlink(&self) -> bool {
        matches!(self, FileType::Symlink)
    }
}

/// File attributes (metadata).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAttr {
    /// Size in bytes.
    pub size: u64,
    /// File type.
    pub kind: FileType,
    /// Unix permissions (e.g., 0o644).
    pub perm: u32,
    /// Last modification time.
    pub mtime: SystemTime,
    /// Last access time (optional).
    pub atime: Option<SystemTime>,
    /// Creation time (optional).
    pub ctime: Option<SystemTime>,
    /// Number of hard links.
    pub nlink: u32,
    /// User ID (optional, for local fs).
    pub uid: Option<u32>,
    /// Group ID (optional, for local fs).
    pub gid: Option<u32>,
}

impl FileAttr {
    /// Create attributes for a new file.
    pub fn file(size: u64, perm: u32) -> Self {
        let now = SystemTime::now();
        Self {
            size,
            kind: FileType::File,
            perm,
            mtime: now,
            atime: Some(now),
            ctime: Some(now),
            nlink: 1,
            uid: None,
            gid: None,
        }
    }

    /// Create attributes for a new directory.
    pub fn directory(perm: u32) -> Self {
        let now = SystemTime::now();
        Self {
            size: 0,
            kind: FileType::Directory,
            perm,
            mtime: now,
            atime: Some(now),
            ctime: Some(now),
            nlink: 2, // . and ..
            uid: None,
            gid: None,
        }
    }

    /// Create attributes for a symlink.
    pub fn symlink(target_len: u64) -> Self {
        let now = SystemTime::now();
        Self {
            size: target_len,
            kind: FileType::Symlink,
            perm: 0o777,
            mtime: now,
            atime: Some(now),
            ctime: Some(now),
            nlink: 1,
            uid: None,
            gid: None,
        }
    }

    /// Returns true if this is a regular file.
    pub fn is_file(&self) -> bool {
        self.kind.is_file()
    }

    /// Returns true if this is a directory.
    pub fn is_dir(&self) -> bool {
        self.kind.is_dir()
    }

    /// Returns true if this is a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.kind.is_symlink()
    }
}

/// Directory entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    /// Entry name (not full path).
    pub name: String,
    /// Entry type.
    pub kind: FileType,
}

impl DirEntry {
    /// Create a new directory entry.
    pub fn new(name: impl Into<String>, kind: FileType) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }

    /// Create a file entry.
    pub fn file(name: impl Into<String>) -> Self {
        Self::new(name, FileType::File)
    }

    /// Create a directory entry.
    pub fn directory(name: impl Into<String>) -> Self {
        Self::new(name, FileType::Directory)
    }
}

/// Attributes to set (for setattr operation).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetAttr {
    /// New size (truncate/extend).
    pub size: Option<u64>,
    /// New modification time.
    pub mtime: Option<SystemTime>,
    /// New access time.
    pub atime: Option<SystemTime>,
    /// New permissions.
    pub perm: Option<u32>,
    /// New user ID.
    pub uid: Option<u32>,
    /// New group ID.
    pub gid: Option<u32>,
}

impl SetAttr {
    /// Create a new empty SetAttr.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the size.
    pub fn with_size(mut self, size: u64) -> Self {
        self.size = Some(size);
        self
    }

    /// Set the modification time.
    pub fn with_mtime(mut self, mtime: SystemTime) -> Self {
        self.mtime = Some(mtime);
        self
    }

    /// Set permissions.
    pub fn with_perm(mut self, perm: u32) -> Self {
        self.perm = Some(perm);
        self
    }
}

/// Filesystem statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatFs {
    /// Total blocks.
    pub blocks: u64,
    /// Free blocks.
    pub bfree: u64,
    /// Available blocks (to non-root).
    pub bavail: u64,
    /// Total inodes.
    pub files: u64,
    /// Free inodes.
    pub ffree: u64,
    /// Block size.
    pub bsize: u32,
    /// Maximum name length.
    pub namelen: u32,
    /// Fragment size.
    pub frsize: u32,
}

impl Default for StatFs {
    fn default() -> Self {
        Self {
            blocks: 1024 * 1024,      // 1M blocks
            bfree: 512 * 1024,        // 512K free
            bavail: 512 * 1024,       // same for non-root
            files: 1024 * 1024,       // 1M inodes
            ffree: 512 * 1024,        // 512K free
            bsize: 4096,              // 4KB blocks
            namelen: 255,             // standard
            frsize: 4096,             // same as bsize
        }
    }
}

/// Open file flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenFlags {
    /// Read access requested.
    pub read: bool,
    /// Write access requested.
    pub write: bool,
    /// Append mode.
    pub append: bool,
    /// Create if not exists.
    pub create: bool,
    /// Truncate on open.
    pub truncate: bool,
    /// Exclusive create (fail if exists).
    pub exclusive: bool,
}

impl Default for OpenFlags {
    fn default() -> Self {
        Self {
            read: true,
            write: false,
            append: false,
            create: false,
            truncate: false,
            exclusive: false,
        }
    }
}

impl OpenFlags {
    /// Read-only access.
    pub fn read() -> Self {
        Self::default()
    }

    /// Write access (also enables read).
    pub fn write() -> Self {
        Self {
            read: true,
            write: true,
            ..Default::default()
        }
    }

    /// Create with write access.
    pub fn create() -> Self {
        Self {
            read: true,
            write: true,
            create: true,
            ..Default::default()
        }
    }

    /// Create exclusively (fail if exists).
    pub fn create_exclusive() -> Self {
        Self {
            read: true,
            write: true,
            create: true,
            exclusive: true,
            ..Default::default()
        }
    }

    /// Create and truncate.
    pub fn create_truncate() -> Self {
        Self {
            read: true,
            write: true,
            create: true,
            truncate: true,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_type() {
        assert!(FileType::File.is_file());
        assert!(!FileType::File.is_dir());
        assert!(FileType::Directory.is_dir());
        assert!(FileType::Symlink.is_symlink());
    }

    #[test]
    fn test_file_attr_constructors() {
        let file = FileAttr::file(1024, 0o644);
        assert!(file.is_file());
        assert_eq!(file.size, 1024);
        assert_eq!(file.perm, 0o644);

        let dir = FileAttr::directory(0o755);
        assert!(dir.is_dir());
        assert_eq!(dir.perm, 0o755);
        assert_eq!(dir.nlink, 2);
    }

    #[test]
    fn test_dir_entry() {
        let file = DirEntry::file("test.txt");
        assert_eq!(file.name, "test.txt");
        assert!(file.kind.is_file());

        let dir = DirEntry::directory("subdir");
        assert!(dir.kind.is_dir());
    }

    #[test]
    fn test_setattr_builder() {
        let attr = SetAttr::new()
            .with_size(2048)
            .with_perm(0o600);
        assert_eq!(attr.size, Some(2048));
        assert_eq!(attr.perm, Some(0o600));
        assert!(attr.mtime.is_none());
    }

    #[test]
    fn test_open_flags() {
        let read = OpenFlags::read();
        assert!(read.read);
        assert!(!read.write);

        let create = OpenFlags::create_exclusive();
        assert!(create.create);
        assert!(create.exclusive);
        assert!(create.write);
    }
}
