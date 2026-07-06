//! `CasFs` — a read-only VFS backend over the kernel's content-addressed store.
//!
//! Mounted at `/v/cas`, this renders the CAS object pool as immutable files so
//! every surface that reaches the kernel `MountTable` — kaish, the file tools,
//! the Bevy app, **and** SFTP — can `ls`/`cat`/fetch an object by hash without a new
//! RPC. It is the substrate for client CAS sync (`docs/slash-v.md` track B): a
//! clip sink resolves media from a local XDG cache and pulls misses from here.
//!
//! ```text
//! /v/cas/
//! └── <ab>/            # shard dirs — the hash's LEADING two hex chars
//!     └── <full-hash>  # the raw bytes, immutable; leaf name is the FULL 32-hex hash
//! ```
//!
//! A greppable `index` TSV (`hash  mime  size  path`) is designed in
//! `docs/slash-v.md` but **not shipped**: nothing consumes it yet (the client
//! resolver addresses objects by exact hash, never by browsing), and a naive
//! walk-the-pool-per-read index is under-designed for scale. It lands with a
//! real consumer and a cache (keyed on a pool-version stamp, or split per
//! shard) — see `docs/issues.md`.
//!
//! ## Read-only by construction, not by flag
//!
//! Every mutating op returns [`VfsError::ReadOnly`] from the backend itself.
//! Ingest stays `kj cas put` (see `docs/slash-v.md` "Ingest"); this tree never
//! writes.
//!
//! ## Sharding — leading two hex chars (unlike contexts)
//!
//! Contexts shard on the UUIDv7 *trailing* byte because v7's leading bytes are a
//! clock. BLAKE3 output is uniform in every byte, so `/v/cas` shards on the
//! *leading* two hex chars — matching the on-disk `objects/<ab>/<remainder>`
//! layout one-to-one. The leaf name is the FULL 32-hex hash (self-describing,
//! copy-pastable); the backend maps `<ab>/<full-hash>` →
//! `objects/<ab>/<full-hash[2..]>`. A path whose shard doesn't match its hash
//! prefix is [`VfsError::NotFound`].
//!
//! ## Immutability makes every hard problem easy
//!
//! A hash names one byte string forever: `getattr` size is O(1) host-file
//! metadata (no `content_len` prerequisite), `generation` is a constant (a
//! caching client never needs invalidation), and reads are plain offset/length
//! passthrough — no snapshot-at-open apparatus, no symlinks.

use async_trait::async_trait;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use kaijutsu_cas::{ContentHash, FileStore};

use crate::vfs::{DirEntry, FileAttr, SetAttr, StatFs, VfsError, VfsOps, VfsResult};

/// Coherence stamp for every object leaf. Objects are immutable — a hash names one
/// byte string forever — so the generation is a nonzero constant: a caching
/// reader never needs to invalidate. (`0` conventionally means "unknown".)
const IMMUTABLE_GENERATION: u64 = 1;

/// Read-only CAS pool rendered as a VFS tree at `/v/cas`.
pub struct CasFs {
    store: Arc<FileStore>,
}

/// What a mount-relative path resolves to within the pool.
enum Resolved {
    /// The mount root (`/v/cas`).
    Root,
    /// A shard directory (`<ab>`), the leading two hex chars of a hash.
    Shard(String),
    /// An object leaf (`<ab>/<full-hash>`), already validated shard == hash prefix.
    Object(ContentHash),
}

impl CasFs {
    /// Create a backend over the kernel's `FileStore` (typically `kernel.cas()`).
    pub fn new(store: Arc<FileStore>) -> Self {
        Self { store }
    }

    /// Split a mount-relative path into clean `Normal` segments, resolving
    /// `.`/`..` and never escaping above the mount root.
    fn segments(path: &Path) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in path.components() {
            match c {
                Component::Normal(s) => out.push(s.to_string_lossy().to_string()),
                Component::ParentDir => {
                    out.pop();
                }
                Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            }
        }
        out
    }

    /// Resolve a mount-relative path to a pool node, or `NotFound` for anything
    /// that can't name one (a bad shard, a non-hash leaf, a shard/prefix
    /// mismatch, or depth > 2).
    fn resolve(&self, path: &Path) -> VfsResult<Resolved> {
        let segs = Self::segments(path);
        match segs.as_slice() {
            [] => Ok(Resolved::Root),
            [shard] => {
                if is_two_hex(shard) {
                    Ok(Resolved::Shard(shard.clone()))
                } else {
                    Err(VfsError::not_found(shard.clone()))
                }
            }
            [shard, leaf] => {
                // The leaf must be a full 32-hex hash whose own prefix matches
                // the shard it lives under — otherwise the path is a fiction.
                let hash = ContentHash::from_str_checked(leaf)
                    .map_err(|_| VfsError::not_found(format!("{shard}/{leaf}")))?;
                if !is_two_hex(shard) || hash.prefix() != shard.as_str() {
                    return Err(VfsError::not_found(format!("{shard}/{leaf}")));
                }
                Ok(Resolved::Object(hash))
            }
            _ => Err(VfsError::not_found(segs.join("/"))),
        }
    }

    /// The on-disk object path for a hash — mirrors `FileStore::object_path`
    /// (which is private). Never exposed via `real_path` (see there).
    fn object_disk_path(&self, hash: &ContentHash) -> PathBuf {
        self.store
            .config()
            .objects_dir()
            .join(hash.prefix())
            .join(hash.remainder())
    }
}

/// A two-hex-char shard name (`objects/<ab>` layout).
fn is_two_hex(s: &str) -> bool {
    s.len() == 2 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn dir_attr() -> FileAttr {
    FileAttr::directory(0o555)
}

#[async_trait]
impl VfsOps for CasFs {
    async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
        match self.resolve(path)? {
            Resolved::Root => Ok(dir_attr()),
            Resolved::Shard(ab) => {
                let dir = self.store.config().objects_dir().join(&ab);
                if dir.is_dir() {
                    Ok(dir_attr())
                } else {
                    // Never synthesize an empty shard: a shard exists iff it does
                    // on disk.
                    Err(VfsError::not_found(ab))
                }
            }
            Resolved::Object(hash) => {
                let p = self.object_disk_path(&hash);
                match std::fs::metadata(&p) {
                    Ok(m) if m.is_file() => {
                        // Size is exact O(1) host metadata (objects are immutable
                        // — no `content_len` prerequisite). read-only 0o444.
                        let mut attr = FileAttr::file(m.len(), 0o444);
                        attr.generation = IMMUTABLE_GENERATION;
                        Ok(attr)
                    }
                    _ => Err(VfsError::not_found(hash.to_string())),
                }
            }
        }
    }

    async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
        match self.resolve(path)? {
            Resolved::Root => {
                // List the shard dirs that exist on disk. A fresh pool (no
                // objects/ yet) reads as empty, not an error.
                let objects = self.store.config().objects_dir();
                let mut entries: Vec<DirEntry> = Vec::new();
                if let Ok(rd) = std::fs::read_dir(&objects) {
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().to_string();
                        if is_two_hex(&name) && e.path().is_dir() {
                            entries.push(DirEntry::directory(name));
                        }
                    }
                }
                entries.sort_by(|a, b| a.name.cmp(&b.name));
                Ok(entries)
            }
            Resolved::Shard(ab) => {
                let dir = self.store.config().objects_dir().join(&ab);
                let rd = std::fs::read_dir(&dir)
                    .map_err(|_| VfsError::not_found(ab.clone()))?;
                let mut entries: Vec<DirEntry> = Vec::new();
                for e in rd.flatten() {
                    if !e.path().is_file() {
                        continue;
                    }
                    // Disk stores the 30-char remainder; the leaf name is the
                    // FULL hash (self-describing). Validate before surfacing.
                    let remainder = e.file_name().to_string_lossy().to_string();
                    let full = format!("{ab}{remainder}");
                    if ContentHash::from_str_checked(&full).is_ok() {
                        entries.push(DirEntry::file(full));
                    }
                }
                entries.sort_by(|a, b| a.name.cmp(&b.name));
                Ok(entries)
            }
            Resolved::Object(hash) => Err(VfsError::not_a_directory(hash.to_string())),
        }
    }

    async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        match self.resolve(path)? {
            Resolved::Object(hash) => {
                use std::io::{Read, Seek, SeekFrom};
                let p = self.object_disk_path(&hash);
                let mut f = std::fs::File::open(&p).map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => VfsError::not_found(hash.to_string()),
                    _ => VfsError::Io(e),
                })?;
                // Bound the allocation to the bytes actually available from
                // `offset`: `size` is a caller-supplied u32 (up to ~4 GiB), so a
                // huge request must not pre-allocate gigabytes for a small
                // object. Objects are immutable, so this length is stable.
                let len = f.metadata().map(|m| m.len()).unwrap_or(0);
                let want = (size as u64).min(len.saturating_sub(offset)) as usize;
                f.seek(SeekFrom::Start(offset)).map_err(VfsError::Io)?;
                // Positioned read of up to `want` bytes — never materialize the
                // whole object for a range read (the SFTP path chunks a large
                // object at 256 KiB, so whole-file reads here would be O(n²)).
                let mut buf = vec![0u8; want];
                let mut filled = 0usize;
                while filled < buf.len() {
                    match f.read(&mut buf[filled..]) {
                        Ok(0) => break, // EOF
                        Ok(n) => filled += n,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(VfsError::Io(e)),
                    }
                }
                buf.truncate(filled);
                Ok(buf)
            }
            Resolved::Root => Err(VfsError::is_a_directory("/".to_string())),
            Resolved::Shard(ab) => Err(VfsError::is_a_directory(ab)),
        }
    }

    async fn readlink(&self, path: &Path) -> VfsResult<PathBuf> {
        // No symlinks in the pool.
        Err(VfsError::NotASymlink(
            Self::segments(path).join("/"),
        ))
    }

    // ── writes: read-only by construction ──────────────────────────────────

    async fn write(&self, _path: &Path, _offset: u64, _data: &[u8]) -> VfsResult<u32> {
        Err(VfsError::ReadOnly)
    }

    async fn create(&self, _path: &Path, _mode: u32) -> VfsResult<FileAttr> {
        Err(VfsError::ReadOnly)
    }

    async fn mkdir(&self, _path: &Path, _mode: u32) -> VfsResult<FileAttr> {
        Err(VfsError::ReadOnly)
    }

    async fn unlink(&self, _path: &Path) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }

    async fn rmdir(&self, _path: &Path) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }

    async fn rename(&self, _from: &Path, _to: &Path) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }

    async fn truncate(&self, _path: &Path, _size: u64) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }

    async fn setattr(&self, _path: &Path, _attr: SetAttr) -> VfsResult<FileAttr> {
        Err(VfsError::ReadOnly)
    }

    async fn symlink(&self, _path: &Path, _target: &Path) -> VfsResult<FileAttr> {
        Err(VfsError::ReadOnly)
    }

    async fn link(&self, _oldpath: &Path, _newpath: &Path) -> VfsResult<FileAttr> {
        Err(VfsError::ReadOnly)
    }

    // ── metadata ────────────────────────────────────────────────────────────

    fn read_only(&self) -> bool {
        true
    }

    async fn statfs(&self) -> VfsResult<StatFs> {
        Ok(StatFs::default())
    }

    async fn real_path(&self, _path: &Path) -> VfsResult<Option<PathBuf>> {
        // The backend maps onto real host files, but exposing the host path
        // would let callers bypass the read-only virtual abstraction.
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_cas::ContentStore;
    use tempfile::TempDir;

    /// A `CasFs` over a temp `FileStore`, plus the dir keeping it alive.
    fn fs() -> (CasFs, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(FileStore::at_path(dir.path()));
        (CasFs::new(store.clone()), dir)
    }

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    #[tokio::test]
    async fn root_is_a_directory() {
        let (fs, _d) = fs();
        let attr = fs.getattr(p("")).await.unwrap();
        assert!(attr.is_dir());
        assert!(fs.getattr(p("/")).await.unwrap().is_dir());
    }

    #[tokio::test]
    async fn stored_object_is_a_file_with_exact_size_and_constant_generation() {
        let (fs, _d) = fs();
        let data = b"the quick brown fox";
        let hash = fs.store.store(data, "text/plain").unwrap();
        let vpath = format!("{}/{}", hash.prefix(), hash);

        let attr = fs.getattr(p(&vpath)).await.unwrap();
        assert!(attr.is_file());
        assert_eq!(attr.size, data.len() as u64);
        assert_eq!(attr.perm, 0o444, "objects are read-only");
        assert_eq!(
            attr.generation, IMMUTABLE_GENERATION,
            "immutable object → constant generation"
        );
    }

    #[tokio::test]
    async fn readdir_root_lists_shard_dirs() {
        let (fs, _d) = fs();
        let hash = fs.store.store(b"shard me", "text/plain").unwrap();

        let entries = fs.readdir(p("")).await.unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e.name == hash.prefix() && e.kind.is_dir()),
            "root should list the shard dir {}, got {entries:?}",
            hash.prefix()
        );
    }

    #[tokio::test]
    async fn readdir_shard_lists_full_hash_leaves() {
        let (fs, _d) = fs();
        let hash = fs.store.store(b"leaf name is the full hash", "text/plain").unwrap();

        let entries = fs.readdir(p(hash.prefix())).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].name,
            hash.to_string(),
            "leaf name must be the FULL 32-hex hash, not the disk remainder"
        );
        assert!(entries[0].kind.is_file());
    }

    #[tokio::test]
    async fn read_returns_the_bytes_and_honors_offset_size() {
        let (fs, _d) = fs();
        let data = b"hello world";
        let hash = fs.store.store(data, "text/plain").unwrap();
        let vpath = format!("{}/{}", hash.prefix(), hash);

        let whole = fs.read_all(p(&vpath)).await.unwrap();
        assert_eq!(whole, data);

        let mid = fs.read(p(&vpath), 6, 5).await.unwrap();
        assert_eq!(mid, b"world");

        // A read past EOF returns fewer bytes, never errors.
        let tail = fs.read(p(&vpath), 6, 100).await.unwrap();
        assert_eq!(tail, b"world");
    }

    #[tokio::test]
    async fn read_of_large_object_spanning_chunks_round_trips() {
        // The SFTP path chunks large objects; the positioned read must reassemble
        // byte-exactly across offsets.
        let (fs, _d) = fs();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let hash = fs.store.store(&data, "application/octet-stream").unwrap();
        let vpath = format!("{}/{}", hash.prefix(), hash);

        let mut reassembled = Vec::new();
        let mut off = 0u64;
        loop {
            let chunk = fs.read(p(&vpath), off, 65_536).await.unwrap();
            if chunk.is_empty() {
                break;
            }
            off += chunk.len() as u64;
            reassembled.extend_from_slice(&chunk);
        }
        assert_eq!(reassembled, data);
    }

    #[tokio::test]
    async fn shard_prefix_mismatch_is_not_found() {
        let (fs, _d) = fs();
        let hash = fs.store.store(b"mismatch", "text/plain").unwrap();
        // A well-formed hash under the WRONG shard dir is a fiction → ENOENT.
        let wrong_shard = if hash.prefix() == "00" { "11" } else { "00" };
        let vpath = format!("{wrong_shard}/{hash}");
        assert!(matches!(
            fs.getattr(p(&vpath)).await,
            Err(VfsError::NotFound(_))
        ));
        assert!(matches!(fs.read(p(&vpath), 0, 16).await, Err(VfsError::NotFound(_))));
    }

    #[tokio::test]
    async fn absent_but_wellformed_hash_is_not_found() {
        let (fs, _d) = fs();
        let hash = "abcdef01234567890123456789abcdef"; // valid shape, never stored
        let vpath = format!("ab/{hash}");
        assert!(matches!(
            fs.getattr(p(&vpath)).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn non_hex_single_component_is_not_found() {
        let (fs, _d) = fs();
        assert!(matches!(
            fs.getattr(p("not-a-shard")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn missing_shard_dir_is_not_found() {
        let (fs, _d) = fs();
        // "ab" is a valid shard name but nothing lives there on disk.
        assert!(matches!(
            fs.getattr(p("ab")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn every_write_op_is_erofs() {
        let (fs, _d) = fs();
        assert!(matches!(fs.write(p("ab/x"), 0, b"y").await, Err(VfsError::ReadOnly)));
        assert!(matches!(fs.create(p("ab/x"), 0o644).await, Err(VfsError::ReadOnly)));
        assert!(matches!(fs.mkdir(p("ab"), 0o755).await, Err(VfsError::ReadOnly)));
        assert!(matches!(fs.unlink(p("ab/x")).await, Err(VfsError::ReadOnly)));
        assert!(matches!(fs.rmdir(p("ab")).await, Err(VfsError::ReadOnly)));
        assert!(matches!(fs.rename(p("a"), p("b")).await, Err(VfsError::ReadOnly)));
        assert!(matches!(fs.truncate(p("ab/x"), 0).await, Err(VfsError::ReadOnly)));
        assert!(matches!(
            fs.setattr(p("ab/x"), SetAttr::new()).await,
            Err(VfsError::ReadOnly)
        ));
        assert!(matches!(
            fs.symlink(p("ab/x"), p("y")).await,
            Err(VfsError::ReadOnly)
        ));
        assert!(matches!(fs.link(p("a"), p("b")).await, Err(VfsError::ReadOnly)));
    }

    #[tokio::test]
    async fn read_only_and_no_real_path() {
        let (fs, _d) = fs();
        assert!(fs.read_only());
        let hash = fs.store.store(b"opaque", "text/plain").unwrap();
        let vpath = format!("{}/{}", hash.prefix(), hash);
        assert!(
            fs.real_path(p(&vpath)).await.unwrap().is_none(),
            "host path must not leak through the virtual abstraction"
        );
    }
}
