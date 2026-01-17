//! VFS error types.

use std::io;
use thiserror::Error;

/// VFS error type.
#[derive(Debug, Error)]
pub enum VfsError {
    /// File or directory not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// Path already exists.
    #[error("already exists: {0}")]
    AlreadyExists(String),

    /// Permission denied.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// Filesystem is read-only.
    #[error("filesystem is read-only")]
    ReadOnly,

    /// Expected a directory.
    #[error("not a directory: {0}")]
    NotADirectory(String),

    /// Expected a file.
    #[error("is a directory: {0}")]
    IsADirectory(String),

    /// Directory not empty.
    #[error("directory not empty: {0}")]
    DirectoryNotEmpty(String),

    /// Path escapes root (security violation).
    #[error("path escapes root: {0}")]
    PathEscapesRoot(String),

    /// Invalid path.
    #[error("invalid path: {0}")]
    InvalidPath(String),

    /// No mount point for path.
    #[error("no mount point for path: {0}")]
    NoMountPoint(String),

    /// Not a symbolic link.
    #[error("not a symbolic link: {0}")]
    NotASymlink(String),

    /// Cross-device link.
    #[error("cross-device link")]
    CrossDeviceLink,

    /// Too many symbolic links.
    #[error("too many symbolic links")]
    TooManySymlinks,

    /// File name too long.
    #[error("file name too long")]
    NameTooLong,

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// Other error.
    #[error("{0}")]
    Other(String),
}

impl VfsError {
    /// Create a NotFound error.
    pub fn not_found(path: impl Into<String>) -> Self {
        Self::NotFound(path.into())
    }

    /// Create an AlreadyExists error.
    pub fn already_exists(path: impl Into<String>) -> Self {
        Self::AlreadyExists(path.into())
    }

    /// Create a PermissionDenied error.
    pub fn permission_denied(path: impl Into<String>) -> Self {
        Self::PermissionDenied(path.into())
    }

    /// Create a NotADirectory error.
    pub fn not_a_directory(path: impl Into<String>) -> Self {
        Self::NotADirectory(path.into())
    }

    /// Create an IsADirectory error.
    pub fn is_a_directory(path: impl Into<String>) -> Self {
        Self::IsADirectory(path.into())
    }

    /// Create a DirectoryNotEmpty error.
    pub fn directory_not_empty(path: impl Into<String>) -> Self {
        Self::DirectoryNotEmpty(path.into())
    }

    /// Create a PathEscapesRoot error.
    pub fn path_escapes_root(path: impl Into<String>) -> Self {
        Self::PathEscapesRoot(path.into())
    }

    /// Create an InvalidPath error.
    pub fn invalid_path(path: impl Into<String>) -> Self {
        Self::InvalidPath(path.into())
    }

    /// Create a NoMountPoint error.
    pub fn no_mount_point(path: impl Into<String>) -> Self {
        Self::NoMountPoint(path.into())
    }

    /// Create an Other error.
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }
}

/// Convert VfsError to std::io::Error for compatibility.
impl From<VfsError> for io::Error {
    fn from(e: VfsError) -> Self {
        match e {
            VfsError::NotFound(msg) => io::Error::new(io::ErrorKind::NotFound, msg),
            VfsError::AlreadyExists(msg) => io::Error::new(io::ErrorKind::AlreadyExists, msg),
            VfsError::PermissionDenied(msg) => {
                io::Error::new(io::ErrorKind::PermissionDenied, msg)
            }
            VfsError::ReadOnly => {
                io::Error::new(io::ErrorKind::PermissionDenied, "filesystem is read-only")
            }
            VfsError::NotADirectory(msg) => io::Error::new(io::ErrorKind::NotADirectory, msg),
            VfsError::IsADirectory(msg) => io::Error::new(io::ErrorKind::IsADirectory, msg),
            VfsError::DirectoryNotEmpty(msg) => {
                io::Error::new(io::ErrorKind::DirectoryNotEmpty, msg)
            }
            VfsError::PathEscapesRoot(msg) => {
                io::Error::new(io::ErrorKind::PermissionDenied, msg)
            }
            VfsError::InvalidPath(msg) => io::Error::new(io::ErrorKind::InvalidInput, msg),
            VfsError::NoMountPoint(msg) => io::Error::new(io::ErrorKind::NotFound, msg),
            VfsError::NotASymlink(msg) => io::Error::new(io::ErrorKind::InvalidInput, msg),
            VfsError::CrossDeviceLink => {
                io::Error::other("cross-device link")
            }
            VfsError::TooManySymlinks => {
                io::Error::other("too many symbolic links")
            }
            VfsError::NameTooLong => {
                io::Error::new(io::ErrorKind::InvalidInput, "file name too long")
            }
            VfsError::Io(e) => e,
            VfsError::Other(msg) => io::Error::other(msg),
        }
    }
}

/// VFS result type.
pub type VfsResult<T> = Result<T, VfsError>;
