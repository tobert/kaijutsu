//! A memory backend whose writes a test can pause or fail.
//!
//! Only `write` is intercepted: `write_all` (the file cache's flush path)
//! reaches it after its truncate or create, so a paused flush holds between
//! reading the buffer and landing it on disk, and a failed flush leaves the
//! file truncated or missing. That is the default `write_all`'s failure;
//! `LocalBackend` replaces the file whole and keeps the earlier contents.

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

use super::MemoryBackend;
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::ops::VfsOps;
use crate::vfs::types::{DirEntry, FileAttr, SetAttr, StatFs};

/// Test controls shared between a [`FaultyBackend`] and its test.
#[derive(Default)]
pub(crate) struct Faults {
    fail_writes: AtomicBool,
    pause_next_write: AtomicBool,
    paused: Notify,
    release: Notify,
}

impl Faults {
    /// Fail every `write` with a permission error until turned off.
    pub(crate) fn fail_writes(&self, on: bool) {
        self.fail_writes.store(on, Ordering::SeqCst);
    }

    /// Hold the next `write` until [`release`](Self::release).
    pub(crate) fn pause_next_write(&self) {
        self.pause_next_write.store(true, Ordering::SeqCst);
    }

    /// Wait until a write armed by [`pause_next_write`](Self::pause_next_write)
    /// is holding.
    pub(crate) async fn paused(&self) {
        self.paused.notified().await;
    }

    /// Let the held write continue.
    pub(crate) fn release(&self) {
        self.release.notify_one();
    }
}

pub(crate) struct FaultyBackend {
    inner: MemoryBackend,
    faults: Arc<Faults>,
}

impl FaultyBackend {
    pub(crate) fn new() -> (Self, Arc<Faults>) {
        let faults = Arc::new(Faults::default());
        (Self { inner: MemoryBackend::new(), faults: faults.clone() }, faults)
    }
}

#[async_trait]
impl VfsOps for FaultyBackend {
    async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
        self.inner.getattr(path).await
    }

    async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
        self.inner.readdir(path).await
    }

    async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        self.inner.read(path, offset, size).await
    }

    async fn readlink(&self, path: &Path) -> VfsResult<PathBuf> {
        self.inner.readlink(path).await
    }

    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32> {
        if self.faults.pause_next_write.swap(false, Ordering::SeqCst) {
            // `notify_one` stores a permit, so a test that starts waiting
            // after this line still wakes.
            self.faults.paused.notify_one();
            self.faults.release.notified().await;
        }
        if self.faults.fail_writes.load(Ordering::SeqCst) {
            return Err(VfsError::PermissionDenied(format!("injected write failure: {}", path.display())));
        }
        self.inner.write(path, offset, data).await
    }

    async fn create(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        self.inner.create(path, mode).await
    }

    async fn mkdir(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        self.inner.mkdir(path, mode).await
    }

    async fn unlink(&self, path: &Path) -> VfsResult<()> {
        self.inner.unlink(path).await
    }

    async fn rmdir(&self, path: &Path) -> VfsResult<()> {
        self.inner.rmdir(path).await
    }

    async fn rename(&self, from: &Path, to: &Path) -> VfsResult<()> {
        self.inner.rename(from, to).await
    }

    async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()> {
        self.inner.truncate(path, size).await
    }

    async fn setattr(&self, path: &Path, attr: SetAttr) -> VfsResult<FileAttr> {
        self.inner.setattr(path, attr).await
    }

    async fn symlink(&self, path: &Path, target: &Path) -> VfsResult<FileAttr> {
        self.inner.symlink(path, target).await
    }

    async fn link(&self, oldpath: &Path, newpath: &Path) -> VfsResult<FileAttr> {
        self.inner.link(oldpath, newpath).await
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn statfs(&self) -> VfsResult<StatFs> {
        self.inner.statfs().await
    }

    async fn real_path(&self, path: &Path) -> VfsResult<Option<PathBuf>> {
        self.inner.real_path(path).await
    }
}
