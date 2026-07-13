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

    /// A `/r` client share's session is gone (never registered, or the
    /// channel dropped — `docs/slash-r.md`). Distinct from [`VfsError::Io`]
    /// so callers can tell "somebody's laptop went away" from a genuine
    /// local I/O fault: a half-written file plus a vanished mount beats a
    /// hung `cp`.
    #[error("/r share {0}: session disconnected")]
    ShareDisconnected(String),

    /// A `/r` client share's wire op exceeded its deadline
    /// (`docs/slash-r.md` "Every remote op gets a timeout") — a hung laptop
    /// must not park a kernel task forever. Distinct from
    /// [`VfsError::ShareDisconnected`]: the session may still be alive, this
    /// one op just didn't answer in time.
    #[error("/r share {0}: operation timed out")]
    ShareTimeout(String),

    /// Other error.
    #[error("{0}")]
    Other(String),
}

impl VfsError {
    /// Whether this error means "the caller may not look here" — either the
    /// VFS's own [`VfsError::PermissionDenied`] or a host-backend
    /// [`VfsError::Io`] carrying `EACCES`/`EPERM`. The snapshot walker
    /// (`MountTable::snapshot`) branches on this: an unreadable directory is
    /// real information to *render* (a denied seam, `docs/scenes/vfs.md`
    /// "truth seams"), not a reason to fail a whole walk — host `/etc` alone
    /// carries a dozen root-only directories.
    pub fn is_permission_denied(&self) -> bool {
        match self {
            VfsError::PermissionDenied(_) => true,
            VfsError::Io(e) => e.kind() == io::ErrorKind::PermissionDenied,
            _ => false,
        }
    }

    /// Whether this error means "there is no such entry" — the VFS's own
    /// [`VfsError::NotFound`] or a host-backend [`VfsError::Io`] carrying
    /// `ENOENT`. The snapshot walker skips a CHILD that vanishes between its
    /// parent's `readdir` and its own `getattr` (live pseudo-filesystems —
    /// `/proc` — churn mid-walk; a gone entry is the tree changing, not a
    /// fault; live-caught 2026-07-12 on an exiting PID).
    pub fn is_not_found(&self) -> bool {
        match self {
            VfsError::NotFound(_) => true,
            VfsError::Io(e) => e.kind() == io::ErrorKind::NotFound,
            _ => false,
        }
    }

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

impl kaijutsu_types::IntoErrorPayload for VfsError {
    fn into_error_payload(self) -> kaijutsu_types::ErrorPayload {
        use kaijutsu_types::{ErrorCategory, ErrorPayload, ErrorSeverity};
        let severity = match &self {
            VfsError::PathEscapesRoot(_) => ErrorSeverity::Fatal,
            VfsError::InvalidPath(_) => ErrorSeverity::Error,
            _ => ErrorSeverity::Error,
        };
        let category = match &self {
            VfsError::InvalidPath(_) | VfsError::PathEscapesRoot(_) => ErrorCategory::Validation,
            _ => ErrorCategory::Tool,
        };
        ErrorPayload {
            category,
            severity,
            code: None,
            detail: Some(self.to_string()),
            span: None,
            source_kind: None,
        }
    }
}

/// Convert VfsError to std::io::Error for compatibility.
impl From<VfsError> for io::Error {
    fn from(e: VfsError) -> Self {
        match e {
            VfsError::NotFound(msg) => io::Error::new(io::ErrorKind::NotFound, msg),
            VfsError::AlreadyExists(msg) => io::Error::new(io::ErrorKind::AlreadyExists, msg),
            VfsError::PermissionDenied(msg) => io::Error::new(io::ErrorKind::PermissionDenied, msg),
            VfsError::ReadOnly => {
                io::Error::new(io::ErrorKind::PermissionDenied, "filesystem is read-only")
            }
            VfsError::NotADirectory(msg) => io::Error::new(io::ErrorKind::NotADirectory, msg),
            VfsError::IsADirectory(msg) => io::Error::new(io::ErrorKind::IsADirectory, msg),
            VfsError::DirectoryNotEmpty(msg) => {
                io::Error::new(io::ErrorKind::DirectoryNotEmpty, msg)
            }
            VfsError::PathEscapesRoot(msg) => io::Error::new(io::ErrorKind::PermissionDenied, msg),
            VfsError::InvalidPath(msg) => io::Error::new(io::ErrorKind::InvalidInput, msg),
            VfsError::NoMountPoint(msg) => io::Error::new(io::ErrorKind::NotFound, msg),
            VfsError::NotASymlink(msg) => io::Error::new(io::ErrorKind::InvalidInput, msg),
            VfsError::CrossDeviceLink => io::Error::other("cross-device link"),
            VfsError::TooManySymlinks => io::Error::other("too many symbolic links"),
            VfsError::NameTooLong => {
                io::Error::new(io::ErrorKind::InvalidInput, "file name too long")
            }
            VfsError::Io(e) => e,
            VfsError::ShareDisconnected(msg) => {
                io::Error::new(io::ErrorKind::NotConnected, msg)
            }
            VfsError::ShareTimeout(msg) => io::Error::new(io::ErrorKind::TimedOut, msg),
            VfsError::Other(msg) => io::Error::other(msg),
        }
    }
}

/// VFS result type.
pub type VfsResult<T> = Result<T, VfsError>;
