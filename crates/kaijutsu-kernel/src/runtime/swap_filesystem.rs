//! Read-only VFS mount exposing unflushed file buffers ("swaps") at
//! `/v/swap/<kernel_id>/<mirrored real path>`.
//!
//! `docs/file-buffers.md` rule 2 makes a dirty buffer a swap: it survives a
//! cold cache, but it is only reachable through `KernelDb::list_dirty_file_buffers`
//! and `FileDocumentCache::read_content`, both kernel-internal. This mount is
//! the discoverable read side — `ls`/`cat`/`grep` over `/v/swap` reach the
//! same durable swap-marker table and file cache every other surface uses,
//! with no separate storage and no separate `kj` verb.
//!
//! # Layout
//!
//! A swap for `/home/atobey/src/kaijutsu/docs/issues.md` on kernel
//! `1234...` appears at `/v/swap/1234.../home/atobey/src/kaijutsu/docs/issues.md`.
//! The kernel-id segment is `KernelId::to_hex()` — the same "id as path
//! component" spelling block document paths use for context/principal ids.
//! It exists because an absolute path is only meaningful relative to the
//! root it came from: a backup, a copied data dir, or eventually more than
//! one kernel's swaps in view all need to say whose disk a swap belongs to.
//! `KernelId` persists across restarts (unlike a per-boot id), so a swap
//! stranded by a restart stays findable at the same path.
//!
//! Only this kernel's own id ever appears under the mount root — the child
//! list is built from `self.kernel_id`, never by scanning the dirty-buffer
//! paths for a leading segment, so a foreign kernel id can never leak in by
//! accident of what happens to be in the table.
//!
//! `list` on any directory under the kernel-id segment answers from a trie
//! over the dirty paths' remaining components; `list` on the kernel-id
//! segment itself is that trie's root.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::file_tools::FileDocumentCache;
use crate::kernel_db::{DirtyFileBuffer, KernelDb};
use kaijutsu_types::KernelId;
use kaish_kernel::vfs::{DirEntry, DirEntryKind, Filesystem};

/// Read-only view over `dirty_file_buffers`, mirroring each swap's real path
/// under this kernel's identity segment. See the module doc for the layout.
pub struct SwapFilesystem {
    kernel_db: Arc<Mutex<KernelDb>>,
    file_cache: Arc<FileDocumentCache>,
    kernel_id: KernelId,
}

impl SwapFilesystem {
    /// Build a swap mount over `kernel_db` (the swap-marker table) and
    /// `file_cache` (the same instance every other file surface reads
    /// through, so a swap's content here matches what `vi`/the file tools
    /// would see). `kernel_id` stamps the mount's identity segment.
    pub fn new(
        kernel_db: Arc<Mutex<KernelDb>>,
        file_cache: Arc<FileDocumentCache>,
        kernel_id: KernelId,
    ) -> Self {
        Self {
            kernel_db,
            file_cache,
            kernel_id,
        }
    }

    /// This mount's identity segment — `KernelId::to_hex()`, 32 hex chars,
    /// no hyphens, always path-safe.
    fn kernel_segment(&self) -> String {
        self.kernel_id.to_hex()
    }

    /// Every currently-dirty path, from the durable swap-marker table. This
    /// is the single source of truth for what exists under the mount —
    /// never a raw VFS/disk listing, since a swap's whole point is content
    /// disk does not have.
    fn dirty_paths(&self) -> io::Result<Vec<DirtyFileBuffer>> {
        self.kernel_db
            .lock()
            .list_dirty_file_buffers()
            .map_err(|e| io::Error::other(format!("failed to list swaps: {e}")))
    }

    /// Split a VFS-relative path (leading `/` already stripped by the
    /// router) into the kernel-id segment and the remaining real-path
    /// components below it. `None` for the mount root (empty path).
    fn split_relative(path: &Path) -> Option<(String, Vec<String>)> {
        let path_str = path.to_string_lossy();
        let trimmed = path_str.trim_start_matches('/').trim_end_matches('/');
        if trimmed.is_empty() {
            return None;
        }
        let mut parts = trimmed.split('/');
        let kernel_seg = parts.next().unwrap().to_string();
        let rest: Vec<String> = parts.map(|s| s.to_string()).collect();
        Some((kernel_seg, rest))
    }

    /// Reject a foreign kernel-id segment as a clean not-found — never a
    /// silent fall-through to this kernel's own swaps. Only one kernel id
    /// is valid under this mount by construction (`self.kernel_id`), so
    /// this check is what keeps that true even once a data dir holds more
    /// than one kernel's rows.
    fn check_kernel_segment(&self, kernel_seg: &str) -> io::Result<()> {
        if kernel_seg == self.kernel_segment() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no such kernel under /v/swap: {kernel_seg}"),
            ))
        }
    }

    /// The real absolute path `rest`'s components name, joined with a
    /// leading `/` (mirroring is exact — no flattening, no escaping).
    fn real_path(rest: &[String]) -> String {
        format!("/{}", rest.join("/"))
    }

    /// Turn one trie level into `DirEntry`s, reading each file child's swap
    /// content for its byte length. A listing that reported zero for every
    /// swap would misdescribe the one thing a reader comes here to find, so
    /// the extra read per file entry is deliberate; the entry count is
    /// bounded by the number of unflushed buffers, not by the tree.
    /// A child whose content cannot be read is reported at size 0 rather
    /// than failing the whole listing — an unreadable swap should still be
    /// visible, and `read` reports the real error.
    async fn entries_with_sizes(
        &self,
        rest: &[String],
        children: BTreeMap<String, bool>,
    ) -> io::Result<Vec<DirEntry>> {
        let mut out = Vec::with_capacity(children.len());
        for (name, is_dir) in children {
            if is_dir {
                out.push(DirEntry::directory(name));
                continue;
            }
            let mut components = rest.to_vec();
            components.push(name.clone());
            let size = match self
                .file_cache
                .read_content(&Self::real_path(&components))
                .await
            {
                Ok(content) => content.len() as u64,
                Err(_) => 0,
            };
            out.push(DirEntry::file(name, size));
        }
        Ok(out)
    }
}

/// `dirty_file_buffers.dirtied_at` is milliseconds since the Unix epoch
/// (`unixepoch('subsec') * 1000`, schema default). Saturating rather than
/// panicking on an out-of-range value keeps `stat` from crashing a caller
/// over a metadata field — the content path (`read`) is unaffected either
/// way.
fn dirtied_at_to_system_time(dirtied_at_ms: i64) -> SystemTime {
    if dirtied_at_ms >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_millis(dirtied_at_ms as u64)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_millis(dirtied_at_ms.unsigned_abs())
    }
}

#[async_trait]
impl Filesystem for SwapFilesystem {
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let Some((kernel_seg, rest)) = Self::split_relative(path) else {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                "/v/swap is a directory",
            ));
        };
        self.check_kernel_segment(&kernel_seg)?;
        if rest.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("/v/swap/{kernel_seg} is a directory"),
            ));
        }

        let real = Self::real_path(&rest);
        let dirty = self.dirty_paths()?;
        if !dirty.iter().any(|d| d.path == real) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no swap for {real}"),
            ));
        }

        // The unsaved buffer, not disk — read_content resolves through the
        // same FileDocumentCache the editor and file tools use, which
        // serves a dirty entry's in-memory content (or a cold-recovered
        // swap's block-store content) rather than re-reading the file.
        self.file_cache
            .read_content(&real)
            .await
            .map(|s| s.into_bytes())
            .map_err(io::Error::other)
    }

    async fn write(&self, path: &Path, _data: &[u8]) -> io::Result<()> {
        Err(Self::refuse("write", path))
    }

    async fn list(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let Some((kernel_seg, rest)) = Self::split_relative(path) else {
            // Mount root: by construction the only child is this kernel's
            // own identity segment, never derived from scanning dirty
            // paths — see the module doc.
            return Ok(vec![DirEntry::directory(self.kernel_segment())]);
        };
        self.check_kernel_segment(&kernel_seg)?;

        let dirty = self.dirty_paths()?;
        // name -> is_dir. A trie level: every dirty path whose real-path
        // components extend `rest` contributes its next component, marked
        // a directory if more components follow it, a file if it ends
        // there.
        let mut children: BTreeMap<String, bool> = BTreeMap::new();
        let mut any_under = false;
        for buf in &dirty {
            let components: Vec<&str> = buf
                .path
                .trim_start_matches('/')
                .split('/')
                .filter(|s| !s.is_empty())
                .collect();
            if components.len() <= rest.len() {
                continue;
            }
            if !rest.iter().enumerate().all(|(i, c)| c == components[i]) {
                continue;
            }
            any_under = true;
            let next = components[rest.len()];
            let is_dir = components.len() > rest.len() + 1;
            children
                .entry(next.to_string())
                .and_modify(|d| *d = *d || is_dir)
                .or_insert(is_dir);
        }

        if rest.is_empty() {
            // The kernel-id segment always exists (it names this mount's
            // own kernel) even with zero swaps recorded — an empty list is
            // correct, not an error.
            return self.entries_with_sizes(&rest, children).await;
        }

        if !any_under {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no swap under {}", Self::real_path(&rest)),
            ));
        }

        self.entries_with_sizes(&rest, children).await
    }

    async fn stat(&self, path: &Path) -> io::Result<DirEntry> {
        let Some((kernel_seg, rest)) = Self::split_relative(path) else {
            return Ok(DirEntry::directory("swap"));
        };
        self.check_kernel_segment(&kernel_seg)?;

        if rest.is_empty() {
            return Ok(DirEntry::directory(kernel_seg));
        }

        let real = Self::real_path(&rest);
        let dirty = self.dirty_paths()?;
        if let Some(buf) = dirty.iter().find(|d| d.path == real) {
            let content = self
                .file_cache
                .read_content(&real)
                .await
                .map_err(io::Error::other)?;
            return Ok(DirEntry {
                name: rest.last().cloned().unwrap_or_default(),
                kind: DirEntryKind::File,
                size: content.len() as u64,
                modified: Some(dirtied_at_to_system_time(buf.dirtied_at)),
                permissions: Some(0o444),
                symlink_target: None,
            });
        }

        let is_dir = dirty.iter().any(|d| {
            let components: Vec<&str> = d
                .path
                .trim_start_matches('/')
                .split('/')
                .filter(|s| !s.is_empty())
                .collect();
            components.len() > rest.len()
                && rest.iter().enumerate().all(|(i, c)| c == components[i])
        });
        if is_dir {
            return Ok(DirEntry::directory(rest.last().cloned().unwrap_or_default()));
        }

        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no swap for {real}"),
        ))
    }

    async fn mkdir(&self, path: &Path) -> io::Result<()> {
        Err(Self::refuse("mkdir", path))
    }

    async fn remove(&self, path: &Path) -> io::Result<()> {
        Err(Self::refuse("remove", path))
    }

    fn read_only(&self) -> bool {
        true
    }

    fn real_path(&self, _path: &Path) -> Option<PathBuf> {
        None // Virtual view over the block store; no real filesystem path.
    }
}

impl SwapFilesystem {
    /// The single refusal every mutating op funnels through — same
    /// `PermissionDenied` kind `ReadOnlyFs` refuses with, so a caller
    /// scripting against either mount sees the same error shape. Swaps are
    /// evidence: a player clears one by acknowledging and flushing, or by
    /// discarding the buffer (docs/file-buffers.md rule 4), never by
    /// unlinking the virtual file.
    fn refuse(op: &str, path: &Path) -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{op} {}: /v/swap is read-only — acknowledge and flush, or discard the buffer",
                path.display()
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::shared_block_store;
    use crate::vfs::backends::MemoryBackend;
    use crate::vfs::{MountTable, VfsOps};
    use kaijutsu_types::PrincipalId;

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    /// One kernel db + file cache + swap filesystem, sharing the same
    /// `Arc<Mutex<KernelDb>>` the production wiring shares between
    /// `FileDocumentCache` and `SwapFilesystem` — never a second instance.
    async fn test_fs() -> (Arc<MountTable>, Arc<FileDocumentCache>, SwapFilesystem, KernelId) {
        let db = Arc::new(Mutex::new(KernelDb::in_memory().unwrap()));
        let blocks = shared_block_store(PrincipalId::system());
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/tmp", MemoryBackend::new()).await;
        let file_cache = Arc::new(FileDocumentCache::new(blocks, vfs.clone(), db.clone()));
        let kernel_id = KernelId::new();
        let swap_fs = SwapFilesystem::new(db, file_cache.clone(), kernel_id);
        (vfs, file_cache, swap_fs, kernel_id)
    }

    /// Dirty a path the way production does: load/replace through the
    /// cache, then `mark_dirty` (which also writes the durable
    /// `dirty_file_buffers` row `list_dirty_file_buffers` reads).
    async fn dirty(file_cache: &FileDocumentCache, path: &str, unsaved_content: &str) {
        file_cache
            .create_or_replace(path, unsaved_content)
            .await
            .unwrap();
        file_cache.mark_dirty(path).unwrap();
    }

    #[tokio::test]
    async fn swap_appears_at_mirrored_path_and_serves_unsaved_content() {
        let (vfs, file_cache, swap_fs, kernel_id) = test_fs().await;
        vfs.write_all(p("/tmp/a.txt"), b"disk content")
            .await
            .unwrap();
        dirty(&file_cache, "/tmp/a.txt", "unsaved content").await;

        let vfs_path = format!("{}/tmp/a.txt", kernel_id.to_hex());
        assert!(swap_fs.exists(Path::new(&vfs_path)).await);
        let data = swap_fs.read(Path::new(&vfs_path)).await.unwrap();
        assert_eq!(
            String::from_utf8(data).unwrap(),
            "unsaved content",
            "must serve the swap, not what's on disk"
        );
    }

    #[tokio::test]
    async fn path_with_no_swap_is_absent() {
        let (vfs, file_cache, swap_fs, kernel_id) = test_fs().await;
        vfs.write_all(p("/tmp/clean.txt"), b"disk content")
            .await
            .unwrap();
        // Load through the cache but never mark dirty — a clean cache entry,
        // not a swap.
        file_cache.read_content("/tmp/clean.txt").await.unwrap();

        let vfs_path = format!("{}/tmp/clean.txt", kernel_id.to_hex());
        assert!(!swap_fs.exists(Path::new(&vfs_path)).await);
        let err = swap_fs.read(Path::new(&vfs_path)).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn foreign_kernel_segment_is_not_found_not_a_fallthrough() {
        let (vfs, file_cache, swap_fs, _kernel_id) = test_fs().await;
        vfs.write_all(p("/tmp/a.txt"), b"disk").await.unwrap();
        dirty(&file_cache, "/tmp/a.txt", "unsaved").await;

        let foreign = KernelId::new();
        let vfs_path = format!("{}/tmp/a.txt", foreign.to_hex());
        let err = swap_fs.read(Path::new(&vfs_path)).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(!swap_fs.exists(Path::new(&vfs_path)).await);
    }

    #[tokio::test]
    async fn list_at_root_kernel_segment_and_intermediate_directory() {
        let (vfs, file_cache, swap_fs, kernel_id) = test_fs().await;
        vfs.write_all(p("/tmp/proj/a.txt"), b"disk-a")
            .await
            .unwrap();
        vfs.write_all(p("/tmp/proj/sub/b.txt"), b"disk-b")
            .await
            .unwrap();
        dirty(&file_cache, "/tmp/proj/a.txt", "unsaved-a").await;
        dirty(&file_cache, "/tmp/proj/sub/b.txt", "unsaved-b").await;

        // Level 1: mount root — only this kernel's own identity segment.
        let root_entries = swap_fs.list(Path::new("")).await.unwrap();
        assert_eq!(root_entries.len(), 1);
        assert_eq!(root_entries[0].name, kernel_id.to_hex());
        assert_eq!(root_entries[0].kind, DirEntryKind::Directory);

        // Level 2: the kernel-id segment — first real-path components.
        let seg_entries = swap_fs
            .list(Path::new(&kernel_id.to_hex()))
            .await
            .unwrap();
        let names: Vec<&str> = seg_entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["tmp"]);
        assert_eq!(seg_entries[0].kind, DirEntryKind::Directory);

        // Level 3: an intermediate directory shared by both swaps —
        // exercises the trie with two paths under one prefix.
        let proj_path = format!("{}/tmp/proj", kernel_id.to_hex());
        let mut proj_entries = swap_fs.list(Path::new(&proj_path)).await.unwrap();
        proj_entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(proj_entries.len(), 2);
        assert_eq!(proj_entries[0].name, "a.txt");
        assert_eq!(proj_entries[0].kind, DirEntryKind::File);
        assert_eq!(proj_entries[1].name, "sub");
        assert_eq!(proj_entries[1].kind, DirEntryKind::Directory);
    }

    /// A listing reports each swap's real byte length. Zero for every entry
    /// would misdescribe the one thing a reader opens this mount to find —
    /// how much unsaved work is sitting there — and `ls` is the surface the
    /// mount exists to serve.
    #[tokio::test]
    async fn list_reports_each_swap_real_size() {
        let (vfs, file_cache, swap_fs, kernel_id) = test_fs().await;
        vfs.write_all(p("/tmp/sz/a.txt"), b"disk").await.unwrap();
        vfs.write_all(p("/tmp/sz/b.txt"), b"disk").await.unwrap();
        // Distinct lengths, and one multi-byte, so a byte/char confusion or a
        // constant would both show up.
        dirty(&file_cache, "/tmp/sz/a.txt", "unsaved-a").await;
        dirty(&file_cache, "/tmp/sz/b.txt", "改善 unsaved").await;

        let dir = format!("{}/tmp/sz", kernel_id.to_hex());
        let mut entries = swap_fs.list(Path::new(&dir)).await.unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(entries[0].size, "unsaved-a".len() as u64);
        assert_eq!(
            entries[1].size,
            "改善 unsaved".len() as u64,
            "size is bytes, not chars"
        );
    }

    #[tokio::test]
    async fn stat_carries_dirtied_at_as_modified() {
        let (vfs, file_cache, swap_fs, kernel_id) = test_fs().await;
        vfs.write_all(p("/tmp/a.txt"), b"disk").await.unwrap();

        let before = SystemTime::now();
        dirty(&file_cache, "/tmp/a.txt", "unsaved").await;
        let after = SystemTime::now();

        let vfs_path = format!("{}/tmp/a.txt", kernel_id.to_hex());
        let entry = swap_fs.stat(Path::new(&vfs_path)).await.unwrap();
        assert_eq!(entry.kind, DirEntryKind::File);
        assert_eq!(entry.size, "unsaved".len() as u64);
        let modified = entry.modified.expect("dirtied_at must populate modified");
        assert!(modified >= before - Duration::from_secs(1));
        assert!(modified <= after + Duration::from_secs(1));
    }

    #[tokio::test]
    async fn write_mkdir_remove_all_refuse() {
        let (vfs, file_cache, swap_fs, kernel_id) = test_fs().await;
        vfs.write_all(p("/tmp/a.txt"), b"disk").await.unwrap();
        dirty(&file_cache, "/tmp/a.txt", "unsaved").await;
        assert!(swap_fs.read_only());

        let vfs_path = format!("{}/tmp/a.txt", kernel_id.to_hex());
        let write_err = swap_fs
            .write(Path::new(&vfs_path), b"x")
            .await
            .unwrap_err();
        assert_eq!(write_err.kind(), io::ErrorKind::PermissionDenied);

        let mkdir_err = swap_fs
            .mkdir(Path::new(&format!("{}/tmp/newdir", kernel_id.to_hex())))
            .await
            .unwrap_err();
        assert_eq!(mkdir_err.kind(), io::ErrorKind::PermissionDenied);

        let remove_err = swap_fs.remove(Path::new(&vfs_path)).await.unwrap_err();
        assert_eq!(remove_err.kind(), io::ErrorKind::PermissionDenied);

        // Confirmed unchanged: the swap is still there afterward.
        let data = swap_fs.read(Path::new(&vfs_path)).await.unwrap();
        assert_eq!(String::from_utf8(data).unwrap(), "unsaved");
    }

    #[tokio::test]
    async fn multibyte_content_round_trips() {
        let (vfs, file_cache, swap_fs, kernel_id) = test_fs().await;
        let text = "改善 — the standard we accept …";
        vfs.write_all(p("/tmp/mb.txt"), b"disk placeholder")
            .await
            .unwrap();
        dirty(&file_cache, "/tmp/mb.txt", text).await;

        let vfs_path = format!("{}/tmp/mb.txt", kernel_id.to_hex());
        let data = swap_fs.read(Path::new(&vfs_path)).await.unwrap();
        assert_eq!(String::from_utf8(data).unwrap(), text);

        let entry = swap_fs.stat(Path::new(&vfs_path)).await.unwrap();
        assert_eq!(entry.size, text.len() as u64, "size is bytes, not chars");
    }
}
