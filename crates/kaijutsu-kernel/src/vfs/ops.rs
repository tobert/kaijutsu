//! VFS operations trait.
//!
//! This trait defines the core filesystem operations in a way that's
//! designed for RPC (path-based, no inodes, explicit offset/size).

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use std::path::{Path, PathBuf};

use super::VfsResult;
use super::types::{DirEntry, FileAttr, SetAttr, StatFs};

/// Chunk size for [`VfsOps::open_read_stream`]'s default loop-`read`
/// implementation. Matches `MAX_READ_LEN` in `crates/kaijutsu-server/src/sftp.rs`
/// (the SFTP `READ` window) so a future wire-backed backend (`ShareFs`,
/// `docs/slash-r.md` slice 1) drives the same cadence its own streaming
/// override would use, rather than picking an unrelated size.
pub const STREAM_CHUNK_SIZE: u32 = 256 * 1024;

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

    /// Whether this backend owns its paths as single-block CRDT *config
    /// documents* (the rc/config trees). The editor binds such a path straight
    /// to its owning block; routing it through a file-doc cache would mint a
    /// shadow copy and revive the dual-ownership write-through bug class
    /// (`docs/config-crdt-ownership.md`). Default `false`; `ConfigCrdtFs`
    /// overrides to `true`. Asking the owning backend keeps the editor and the
    /// VFS from disagreeing on ownership — no hardcoded path prefix.
    fn owns_config_docs(&self) -> bool {
        false
    }

    /// Get filesystem statistics.
    async fn statfs(&self) -> VfsResult<StatFs>;

    /// Whether ambient sweeps (`MountTable::snapshot` — the FSN backdrop
    /// walk, the semantic indexer, any future project crawler) must refuse to
    /// descend past this backend's mount root. Default `false`; `ShareFs`
    /// (`/r`, `docs/slash-r.md`) overrides `true` — every `readdir` there is a
    /// network round trip to somebody's laptop, and the kernel's own ambient
    /// machinery crawling a client's disk unprompted is exactly the risk the
    /// forward-SFTP doc flagged for editor indexers, reversed. A
    /// backend-level flag, not a path blocklist, so a future opaque mount
    /// needs no `snapshot` special-casing.
    fn opaque_to_sweeps(&self) -> bool {
        false
    }

    /// Resolve a virtual path to its real filesystem path.
    ///
    /// Returns `Ok(Some(path))` for backends backed by real files (LocalBackend).
    /// Returns `Ok(None)` for virtual backends (MemoryBackend).
    /// Returns `Err` if the path doesn't exist or escapes the mount root.
    async fn real_path(&self, path: &Path) -> VfsResult<Option<PathBuf>>;

    /// The real host directory backing this mount's root, when the whole mount
    /// is a 1:1 view of one (LocalBackend). **Sync** — the seam for callers
    /// that can't await, like subprocess cwd resolution
    /// (`MountBackend::resolve_real_path`, a sync kaish trait method). Purely
    /// structural: no existence check, no per-path symlink resolution — pair
    /// it with [`Self::real_path`] when those matter. Virtual/CRDT backends
    /// keep the `None` default.
    fn real_root(&self) -> Option<PathBuf> {
        None
    }

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

    /// Open a streaming, chunked read over `path` — the substrate for
    /// `vfs::pump` (`docs/slash-r.md` slice 0: `cp` across mounts, CAS
    /// ingest, and eventually share sync all sit on this).
    ///
    /// The default implementation loops this backend's own [`Self::read`] at
    /// [`STREAM_CHUNK_SIZE`] — correct and free for local/memory/CRDT
    /// backends, where `read` is stateless and cheap to call repeatedly. A
    /// backend whose `read` is expensive per call (a network protocol with
    /// its own OPEN/READ/CLOSE framing, e.g. the future `ShareFs`) MUST
    /// override this to hold one handle open across the whole stream —
    /// looping the default over such a backend would reopen and close a
    /// remote handle *per chunk* (RTT-amplification: three round trips per
    /// 256 KiB at network latency is dead on arrival for a multi-GB file).
    ///
    /// Read contract, pinned by tests in `vfs::pump`:
    /// - a zero-length `read` return is EOF: the stream ends, successfully.
    /// - a **short** read (non-empty, less than the requested chunk) before
    ///   EOF is legal; the next request's offset advances by the *actual*
    ///   bytes returned, never the requested size. The stream keeps pulling
    ///   until a zero-length read.
    /// - a `read` error ends the stream with that error as its final item.
    ///
    /// Object-safe behind `Arc<dyn VfsOps>`: the returned stream borrows
    /// `&self`/`path`, so callers must keep the backend's `Arc` (and the
    /// path) alive for as long as they drive the stream — true of every
    /// caller here, since `pump` always holds the source `Arc` for its
    /// whole run.
    fn open_read_stream<'a>(&'a self, path: &'a Path) -> BoxStream<'a, VfsResult<Bytes>> {
        Box::pin(futures::stream::unfold(
            (self, path, 0u64, false),
            |(this, path, offset, done)| async move {
                if done {
                    return None;
                }
                match this.read(path, offset, STREAM_CHUNK_SIZE).await {
                    Ok(chunk) if chunk.is_empty() => None,
                    Ok(chunk) => {
                        let advanced = offset + chunk.len() as u64;
                        Some((Ok(Bytes::from(chunk)), (this, path, advanced, false)))
                    }
                    Err(e) => Some((Err(e), (this, path, offset, true))),
                }
            },
        ))
    }
}
