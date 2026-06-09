//! `ReadOnlyFs` — a structural read-only wrapper around any kaish
//! [`Filesystem`].
//!
//! The explorer's `read_only_shell` reaches the real filesystem and the CRDT
//! `FileDocumentCache` through a read-only [`MountBackend`](super::mount_backend),
//! but the CRDT *document views* `/v/docs` and `/v/input` are mounted directly
//! onto the kaish VFS — they never route through `MountBackend`, so a read-only
//! `MountBackend` alone would still let the explorer mutate the input doc and
//! document views.
//!
//! This wrapper closes that gap the same way kaish's own read-only mounts do:
//! reads delegate to the inner filesystem; every mutation is refused with
//! `PermissionDenied` *before* it reaches the inner fs. The explorer can read a
//! live CRDT document view but cannot author it — the differentiator from
//! kaibo, whose read-only sandbox has only host files + ephemeral scratch and
//! no CRDT surface at all.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use kaish_kernel::vfs::{DirEntry, Filesystem};

/// Wraps an inner [`Filesystem`], delegating reads and refusing every write.
pub struct ReadOnlyFs {
    inner: Arc<dyn Filesystem>,
}

impl ReadOnlyFs {
    /// Wrap `inner` so that reads pass through and mutations are refused.
    pub fn new(inner: Arc<dyn Filesystem>) -> Self {
        Self { inner }
    }

    /// The single refusal every mutating op funnels through, so the message is
    /// uniform and the read-only invariant is one line, not scattered.
    fn refuse(op: &str, path: &Path) -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{op} {}: read-only shell (no writes)", path.display()),
        )
    }
}

#[async_trait]
impl Filesystem for ReadOnlyFs {
    // ---- reads: delegate ----

    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.read(path).await
    }

    async fn list(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        self.inner.list(path).await
    }

    async fn stat(&self, path: &Path) -> io::Result<DirEntry> {
        self.inner.stat(path).await
    }

    async fn lstat(&self, path: &Path) -> io::Result<DirEntry> {
        self.inner.lstat(path).await
    }

    async fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path).await
    }

    async fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.inner.read_link(path).await
    }

    fn real_path(&self, path: &Path) -> Option<PathBuf> {
        // A real path is only useful to read-side tools here (the inner fs for
        // `/v/*` is virtual and returns None anyway). Reads through it are still
        // reads; the write methods below are the gate that matters.
        self.inner.real_path(path)
    }

    fn read_only(&self) -> bool {
        true
    }

    // ---- mutations: refuse before reaching the inner fs ----

    async fn write(&self, path: &Path, _data: &[u8]) -> io::Result<()> {
        Err(Self::refuse("write", path))
    }

    async fn mkdir(&self, path: &Path) -> io::Result<()> {
        Err(Self::refuse("mkdir", path))
    }

    async fn remove(&self, path: &Path) -> io::Result<()> {
        Err(Self::refuse("remove", path))
    }

    async fn set_mtime(&self, path: &Path, _mtime: SystemTime) -> io::Result<()> {
        Err(Self::refuse("touch", path))
    }

    async fn rename(&self, from: &Path, _to: &Path) -> io::Result<()> {
        Err(Self::refuse("rename", from))
    }

    async fn symlink(&self, _target: &Path, link: &Path) -> io::Result<()> {
        Err(Self::refuse("symlink", link))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaish_kernel::vfs::MemoryFs;

    #[tokio::test]
    async fn delegates_reads_and_refuses_writes() {
        let inner = Arc::new(MemoryFs::new());
        // Seed through the inner fs directly (the writable side).
        inner.write(Path::new("/seed.txt"), b"hello").await.unwrap();

        let ro = ReadOnlyFs::new(inner.clone());
        assert!(ro.read_only());

        // Read passes through.
        assert_eq!(ro.read(Path::new("/seed.txt")).await.unwrap(), b"hello");
        assert!(ro.exists(Path::new("/seed.txt")).await);

        // Writes are refused with PermissionDenied.
        for err in [
            ro.write(Path::new("/seed.txt"), b"x").await.unwrap_err(),
            ro.mkdir(Path::new("/d")).await.unwrap_err(),
            ro.remove(Path::new("/seed.txt")).await.unwrap_err(),
            ro.rename(Path::new("/seed.txt"), Path::new("/m.txt"))
                .await
                .unwrap_err(),
        ] {
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        }

        // The refused mutations changed nothing in the inner fs.
        assert_eq!(inner.read(Path::new("/seed.txt")).await.unwrap(), b"hello");
    }
}
