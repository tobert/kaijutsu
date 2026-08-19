//! kernel-owned file document cache.
//!
//! Maps VFS files into kernel documents, giving file edits the same
//! storage and edit semantics as block edits. Files are loaded on
//! demand and cached with LRU eviction.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use kaijutsu_types::{BlockId, BlockKind, ContentType, ContextId, Role, Status};
use parking_lot::{Mutex, RwLock};

use crate::block_store::SharedBlockStore;
use crate::kernel_db::KernelDb;
use crate::vfs::{MountTable, VfsOps};
use kaijutsu_types::DocKind;

/// Default maximum cached file documents.
const DEFAULT_MAX_CACHED: usize = 64;

/// Typed error from [`FileDocumentCache::try_read_content`].
///
/// The two variants must be treated differently by callers:
/// - [`CacheReadError::NotCached`] is **benign**: the file is absent, binary,
///   or otherwise not decodable as UTF-8 text. Callers *may*
///   fall through to a raw VFS read or treat the file as absent.
/// - [`CacheReadError::Backend`] is a **real failure** (block store I/O, block
///   not found in a live document, etc.). Callers *must* surface it — serving
///   stale or empty bytes in place of a Backend error is silent data corruption.
#[derive(Debug)]
pub enum CacheReadError {
    /// File is absent, became binary, or can't be decoded as UTF-8.
    /// Benign: fall through to a raw read or treat as absent.
    NotCached,
    /// A real backend or block store error. Surface it; never silently substitute
    /// stale bytes or an empty string.
    Backend(String),
}

impl std::fmt::Display for CacheReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheReadError::NotCached => write!(f, "file not in document cache (binary or missing)"),
            CacheReadError::Backend(e) => write!(f, "{e}"),
        }
    }
}

/// Why a flush to disk did not happen. Typed because the editor layer picks a
/// different vi message for each, and because a caller matching on a message
/// substring is a test that passes for the wrong reason.
#[derive(Debug)]
pub enum FlushError {
    /// The disk generation moved past this buffer's `loaded_generation` — the
    /// W12 condition. Only produced by [`FileDocumentCache::flush_one_guarded`]
    /// with `force: false`. See `docs/file-buffers.md`.
    DiskChanged {
        path: String,
        loaded: Option<u64>,
        disk: Option<u64>,
    },
    /// An unacknowledged recovered swap (rule 4, `docs/file-buffers.md`).
    UnacknowledgedSwap { path: String },
    /// A VFS write, a block-store read, or a swap-marker clear failed.
    Backend(String),
}

impl std::fmt::Display for FlushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FlushError::DiskChanged { path, .. } => write!(
                f,
                "{path}: the file changed on disk since it was read (add ! to override)"
            ),
            FlushError::UnacknowledgedSwap { path } => write!(
                f,
                "{path}: recovered from a swap after a cold cache and not yet \
                 acknowledged — call acknowledge_swap before flushing \
                 (docs/file-buffers.md)"
            ),
            FlushError::Backend(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for FlushError {}

/// A cached file backed by a kernel document.
struct CachedFileDoc {
    /// Deterministic ContextId derived from the file path.
    context_id: ContextId,
    /// Original file path (needed for flushing back to VFS).
    path: String,
    /// The single block holding file content.
    block_id: BlockId,
    /// Whether this file has been edited since last flush.
    dirty: bool,
    /// Last access time for LRU eviction.
    last_access: Instant,
    /// VFS `generation` (coherence stamp) when the content was last loaded from
    /// / flushed to disk. A clean entry whose backing file reports a *greater*
    /// generation is stale and gets reloaded — this is how external edits
    /// (cargo, git, the GUI, a sibling writer) become visible. Generation is
    /// used instead of mtime because it strictly advances even within one mtime
    /// tick and never steps backward (see `FileAttr::generation`). `None` means
    /// we couldn't read an attr, so we trust the cache.
    loaded_generation: Option<u64>,
    /// Disk moved past `loaded_generation` while this entry was dirty, so the
    /// staleness check could not reload without discarding unsaved work.
    /// Consumed by the vi editor's W12 `:w` guard and swap-recovery
    /// announcement (docs/file-buffers.md).
    disk_changed_since_load: bool,
    /// This entry was restored from a `dirty_file_buffers` row on a cold
    /// cache instead of being reconciled against disk — it holds unsaved
    /// work from before a kernel restart. Set by `try_get_or_load`'s
    /// `DocumentAlreadyExists` arm; cleared by `acknowledge_swap`. While
    /// true, `flush_one` refuses (rule 4, docs/file-buffers.md: not served
    /// as authoritative until acknowledged).
    swap_recovered: bool,
}

/// Cache that maps VFS files to kernel documents.
///
/// Each file becomes a document with `DocKind::File` and a single
/// `BlockKind::Text` block; edits apply directly to that block.
pub struct FileDocumentCache {
    cache: RwLock<HashMap<ContextId, CachedFileDoc>>,
    block_store: SharedBlockStore,
    vfs: Arc<MountTable>,
    max_cached: usize,
    /// Durable swap-file marker table (`dirty_file_buffers`,
    /// docs/file-buffers.md). Required, not optional: a `FileDocumentCache`
    /// with nowhere to durably record "this path has unsaved work" cannot
    /// tell a swap from a stale cache on a cold restart, which is the exact
    /// failure this table exists to prevent. Every caller — production and
    /// test alike — must supply a real `KernelDb` (`KernelDb::temporary()`
    /// is public and ungated for tests) so the code path under test is the
    /// one that ships.
    db: Arc<Mutex<KernelDb>>,
}

impl FileDocumentCache {
    /// Create a new file document cache. `db` backs the durable swap-file
    /// marker (`dirty_file_buffers`); see the field doc on `db` for why it
    /// is required rather than optional.
    pub fn new(block_store: SharedBlockStore, vfs: Arc<MountTable>, db: Arc<Mutex<KernelDb>>) -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
            block_store,
            vfs,
            max_cached: DEFAULT_MAX_CACHED,
            db,
        }
    }

    /// Get the context_id and block_id for a path, loading from VFS on cache miss.
    ///
    /// Legacy wrapper: collapses the typed [`CacheReadError`] to an opaque
    /// `String` for callers that already handle errors generically. New call
    /// sites should prefer [`try_get_or_load`](Self::try_get_or_load) so they
    /// can distinguish benign misses from real backend failures.
    pub async fn get_or_load(&self, path: &str) -> Result<(ContextId, BlockId), String> {
        self.try_get_or_load(path).await.map_err(|e| e.to_string())
    }

    /// Replace a cached block's content with the current on-disk bytes. Used to
    /// pick up external edits when a clean entry's file mtime has advanced.
    /// Only emits an edit when the content actually differs.
    ///
    /// Error classification (matches [`try_get_or_load`](Self::try_get_or_load)):
    /// - VFS not-found / UTF-8 failure → [`CacheReadError::NotCached`] (benign:
    ///   file was removed or became binary; callers drop the cache entry)
    /// - `block_snapshots` or `edit_text` failure → [`CacheReadError::Backend`]
    ///   (real store error; callers must surface it, not fall through to empty bytes)
    async fn reload_block_from_disk(
        &self,
        ctx_id: ContextId,
        block_id: &BlockId,
        path: &str,
    ) -> Result<(), CacheReadError> {
        let vfs_path = std::path::Path::new(path);

        // VFS read failures: distinguish "not there / not text" (benign) from
        // real I/O errors (Backend).
        let bytes = match self.vfs.read_all(vfs_path).await {
            Ok(b) => b,
            Err(crate::vfs::VfsError::NotFound(_)) => {
                return Err(CacheReadError::NotCached);
            }
            Err(crate::vfs::VfsError::Io(ref io_err))
                if io_err.kind() == std::io::ErrorKind::NotFound =>
            {
                return Err(CacheReadError::NotCached);
            }
            Err(e) => {
                return Err(CacheReadError::Backend(format!(
                    "failed to reread {}: {}",
                    path, e
                )));
            }
        };

        let text = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => {
                // File became binary — benign; caller drops the cache entry.
                return Err(CacheReadError::NotCached);
            }
        };

        // Fetch the existing block content so we can diff and apply a minimal
        // edit. A store error here is a real backend failure — propagate it
        // rather than defaulting to "" which would wipe the whole block.
        let snaps = self
            .block_store
            .block_snapshots(ctx_id)
            .map_err(|e| CacheReadError::Backend(format!("block_snapshots failed for {}: {}", path, e)))?;
        let old = snaps
            .iter()
            .find(|s| s.id == *block_id)
            .map(|s| s.content.clone())
            .ok_or_else(|| {
                CacheReadError::Backend(format!(
                    "block not found in live document for {} during stale reload",
                    path
                ))
            })?;

        if old != text {
            // Char-indexed delete (block text positions are chars, not bytes).
            self.block_store
                .edit_text(ctx_id, block_id, 0, &text, old.chars().count())
                .map_err(|e| {
                    CacheReadError::Backend(format!("edit_text failed for {}: {}", path, e))
                })?;
        }

        let generation = self.vfs.getattr(vfs_path).await.ok().map(|a| a.generation);
        let mut cache = self.cache.write();
        if let Some(entry) = cache.get_mut(&ctx_id) {
            entry.loaded_generation = generation;
            entry.dirty = false;
            entry.disk_changed_since_load = false;
            // A reconciled entry is disk, plain and simple — never a pending
            // swap. (Unreachable with `swap_recovered: true` today, since a
            // recovered swap is inserted `dirty: true` and this branch only
            // runs on a clean entry, but stated explicitly rather than left
            // to that invariant holding forever.)
            entry.swap_recovered = false;
        }
        Ok(())
    }

    /// Whether disk moved under a cached entry since it was loaded (or last
    /// reconciled) while dirty — the W12 condition. False for a path that
    /// isn't cached. See `docs/file-buffers.md`.
    pub fn disk_changed_since_load(&self, path: &str) -> bool {
        let ctx_id = file_context_id(path);
        self.cache
            .read()
            .get(&ctx_id)
            .map(|e| e.disk_changed_since_load)
            .unwrap_or(false)
    }

    /// Whether a cached entry is an unacknowledged recovered swap — restored
    /// from a `dirty_file_buffers` row on a cold cache instead of reconciled
    /// against disk. False for a path that isn't cached, or that was never a
    /// swap. See `docs/file-buffers.md` rule 4; cleared by
    /// [`acknowledge_swap`](Self::acknowledge_swap).
    pub fn swap_recovered(&self, path: &str) -> bool {
        let ctx_id = file_context_id(path);
        self.cache
            .read()
            .get(&ctx_id)
            .map(|e| e.swap_recovered)
            .unwrap_or(false)
    }

    /// Whether a file already exists — cached as a kernel document or present
    /// on the backing VFS. Used to report created-vs-updated on write.
    pub async fn exists(&self, path: &str) -> bool {
        let ctx_id = file_context_id(path);
        if self.cache.read().contains_key(&ctx_id) {
            return true;
        }
        self.vfs.exists(std::path::Path::new(path)).await
    }

    /// Read the current content of a file (reflects any edits applied since load).
    ///
    /// This is the legacy opaque-error wrapper kept for call sites that already
    /// have an appropriate error context (e.g. `ExecResult::failure`). New call
    /// sites that need to distinguish a benign miss from a real failure should
    /// use [`try_read_content`](Self::try_read_content) instead.
    pub async fn read_content(&self, path: &str) -> Result<String, String> {
        self.try_read_content(path)
            .await
            .map_err(|e| e.to_string())
    }

    /// Like [`read_content`](Self::read_content) but returns a typed
    /// [`CacheReadError`] so callers can distinguish benign misses from real
    /// failures.
    ///
    /// Error classification:
    /// - VFS "not found" → [`CacheReadError::NotCached`] (file absent; benign)
    /// - UTF-8 decode failure → [`CacheReadError::NotCached`] (binary file; benign)
    /// - Any other VFS or block store error → [`CacheReadError::Backend`] (real)
    pub async fn try_read_content(&self, path: &str) -> Result<String, CacheReadError> {
        let (ctx_id, block_id) = self
            .try_get_or_load(path)
            .await
            .map_err(|e| match e {
                CacheReadError::NotCached => CacheReadError::NotCached,
                CacheReadError::Backend(msg) => CacheReadError::Backend(msg),
            })?;

        let snapshots = self
            .block_store
            .block_snapshots(ctx_id)
            .map_err(|e| CacheReadError::Backend(format!("failed to read {}: {}", path, e)))?;

        // A block missing from a live document is a real inconsistency (Backend),
        // not a benign miss — the document exists but is structurally broken.
        snapshots
            .iter()
            .find(|s| s.id == block_id)
            .map(|s| s.content.clone())
            .ok_or_else(|| {
                CacheReadError::Backend(format!("block not found in document for {}", path))
            })
    }

    /// Typed variant of [`get_or_load`](Self::get_or_load): classifies errors at
    /// the source so callers can act on benign misses separately from real
    /// backend failures.
    pub(crate) async fn try_get_or_load(&self, path: &str) -> Result<(ContextId, BlockId), CacheReadError> {
        let ctx_id = file_context_id(path);
        let vfs_path = std::path::Path::new(path);

        // Fast path: already cached — same as get_or_load.
        let cached = {
            let mut cache = self.cache.write();
            cache.get_mut(&ctx_id).map(|e| {
                e.last_access = Instant::now();
                (e.context_id, e.block_id, e.dirty, e.loaded_generation)
            })
        };
        if let Some((cid, bid, dirty, loaded_generation)) = cached {
            let disk_generation = self.vfs.getattr(vfs_path).await.ok().map(|a| a.generation);
            let stale =
                matches!((disk_generation, loaded_generation), (Some(d), Some(l)) if d > l);
            if dirty {
                // A dirty buffer is unsaved work and is still served as-is —
                // never silently clobbered by disk. The staleness check still
                // runs and records its result: the W12 `:w` guard needs to
                // know disk moved, and returning before the check computed it
                // discarded that permanently. See docs/file-buffers.md.
                if stale {
                    let mut cache = self.cache.write();
                    if let Some(entry) = cache.get_mut(&ctx_id) {
                        entry.disk_changed_since_load = true;
                    }
                }
                return Ok((cid, bid));
            }
            if !stale {
                return Ok((cid, bid));
            }
            match self.reload_block_from_disk(ctx_id, &bid, path).await {
                Ok(()) => return Ok((cid, bid)),
                Err(CacheReadError::NotCached) => {
                    // Benign: file was removed or became binary. Drop the
                    // stale cache entry and surface NotCached so callers can
                    // fall through to a raw VFS read.
                    self.cache.write().remove(&ctx_id);
                    return Err(CacheReadError::NotCached);
                }
                Err(CacheReadError::Backend(msg)) => {
                    // Real backend failure (store I/O, block missing from live
                    // document). Drop the now-inconsistent entry and surface the
                    // error — callers must NOT fall through to empty bytes.
                    self.cache.write().remove(&ctx_id);
                    return Err(CacheReadError::Backend(msg));
                }
            }
        }

        // Cache miss: load from VFS. Classify errors.
        // VfsError::NotFound is the typed variant. VfsError::Io wraps OS errors
        // (from io::From<io::Error>), so a missing file arrives as
        // VfsError::Io(ErrorKind::NotFound) from the LocalBackend's getattr call
        // — we must detect both forms and treat them as NotCached (benign).
        let content = match self.vfs.read_all(vfs_path).await {
            Ok(bytes) => bytes,
            Err(crate::vfs::VfsError::NotFound(_)) => {
                return Err(CacheReadError::NotCached);
            }
            Err(crate::vfs::VfsError::Io(ref io_err))
                if io_err.kind() == std::io::ErrorKind::NotFound =>
            {
                // LocalBackend maps ENOENT through io::Error → VfsError::Io.
                return Err(CacheReadError::NotCached);
            }
            Err(e) => {
                return Err(CacheReadError::Backend(format!(
                    "failed to read {}: {}",
                    path, e
                )));
            }
        };

        let text = match String::from_utf8(content) {
            Ok(s) => s,
            Err(_) => {
                // Binary file — not an error, just not decodable as UTF-8 text.
                return Err(CacheReadError::NotCached);
            }
        };

        let loaded_generation = self.vfs.getattr(vfs_path).await.ok().map(|a| a.generation);
        let language = detect_language(path);

        let block_id = match self
            .block_store
            .create_document(ctx_id, DocKind::File, language)
        {
            Ok(()) => self
                .block_store
                .insert_block(
                    ctx_id,
                    None,
                    None,
                    Role::System,
                    BlockKind::Text,
                    text,
                    Status::Done,
                    ContentType::Plain,
                )
                .map_err(|e| {
                    CacheReadError::Backend(format!(
                        "failed to insert block for {}: {}",
                        path, e
                    ))
                })?,
            Err(crate::block_store::BlockStoreError::DocumentAlreadyExists(_)) => {
                // The block store persists file documents across restarts
                // while this in-memory cache starts cold, so a miss does not
                // mean the document is new — the existing block may hold
                // content arbitrarily older than disk. Reconcile it against
                // disk; discarding the `text` just read and serving the block
                // unreconciled is how a months-old copy reaches a writer and
                // gets flushed back. See docs/file-buffers.md.
                {
                    let mut cache = self.cache.write();
                    if let Some(entry) = cache.get_mut(&ctx_id) {
                        // Another task loaded this path between our miss and
                        // now. That load ran the same reconcile against disk,
                        // so the entry it left behind is already fresh — safe
                        // to reuse without reconciling a second time.
                        entry.last_access = Instant::now();
                        return Ok((entry.context_id, entry.block_id));
                    }
                }
                let snapshots = self
                    .block_store
                    .block_snapshots(ctx_id)
                    .map_err(|e| {
                        CacheReadError::Backend(format!(
                            "failed to read existing doc {}: {}",
                            path, e
                        ))
                    })?;
                let existing_block_id = snapshots
                    .first()
                    .map(|s| s.id)
                    .ok_or_else(|| {
                        CacheReadError::Backend(format!(
                            "document {} exists but has no blocks",
                            path
                        ))
                    })?;

                // A `dirty_file_buffers` row means this document is a swap —
                // unsaved work still in flight when the kernel went down —
                // not a stale mirror of disk. Reconciling it (like the branch
                // below) would silently discard that work, which is the exact
                // failure this table exists to prevent. See docs/file-buffers.md.
                let dirty_row = self.db.lock().get_dirty_file_buffer(path).map_err(|e| {
                    CacheReadError::Backend(format!(
                        "failed to check swap marker for {}: {}",
                        path, e
                    ))
                })?;
                if let Some(row) = dirty_row {
                    let mut cache = self.cache.write();
                    self.evict_if_needed(&mut cache);
                    cache.insert(
                        ctx_id,
                        CachedFileDoc {
                            context_id: ctx_id,
                            path: path.to_string(),
                            block_id: existing_block_id,
                            dirty: true,
                            last_access: Instant::now(),
                            loaded_generation: row.loaded_generation,
                            disk_changed_since_load: false,
                            swap_recovered: true,
                        },
                    );
                    return Ok((ctx_id, existing_block_id));
                }

                // No row: an ordinary stale mirror. Reuses the vetted
                // char-indexed splice from the stale-reload path instead of
                // duplicating it here; it re-reads disk (we already hold
                // `text`), which is a known redundant read, not a
                // correctness gap.
                self.reload_block_from_disk(ctx_id, &existing_block_id, path)
                    .await?;
                existing_block_id
            }
            Err(e) => {
                return Err(CacheReadError::Backend(format!(
                    "failed to create document for {}: {}",
                    path, e
                )));
            }
        };

        {
            let mut cache = self.cache.write();
            self.evict_if_needed(&mut cache);
            cache.insert(
                ctx_id,
                CachedFileDoc {
                    context_id: ctx_id,
                    path: path.to_string(),
                    block_id,
                    dirty: false,
                    last_access: Instant::now(),
                    loaded_generation,
                    disk_changed_since_load: false,
                    swap_recovered: false,
                },
            );
        }

        Ok((ctx_id, block_id))
    }

    /// Create or replace a file's content.
    pub async fn create_or_replace(
        &self,
        path: &str,
        content: &str,
    ) -> Result<(ContextId, BlockId), String> {
        let ctx_id = file_context_id(path);

        // If doc exists, replace its content with a full splice
        {
            let cache = self.cache.read();
            if let Some(entry) = cache.get(&ctx_id) {
                let old_content = self
                    .block_store
                    .block_snapshots(ctx_id)
                    .ok()
                    .and_then(|snaps| {
                        snaps
                            .iter()
                            .find(|s| s.id == entry.block_id)
                            .map(|s| s.content.clone())
                    })
                    .unwrap_or_default();

                // `edit_text` indexes in CHARACTERS (block text positions),
                // not bytes — delete the whole block by char count. Using
                // `old_content.len()` (bytes) over-counts on multi-byte
                // UTF-8 (e.g. 改善, em-dashes) and panics out-of-bounds.
                self.block_store
                    .edit_text(
                        ctx_id,
                        &entry.block_id,
                        0,
                        content,
                        old_content.chars().count(),
                    )
                    .map_err(|e| e.to_string())?;

                return Ok((entry.context_id, entry.block_id));
            }
        }

        // New file: create doc + block
        self.get_or_load_with_content(path, content).await
    }

    /// Drop a path's cached kernel document, if any. Used when a write bypasses
    /// the text substrate (e.g. binary content) so a later text read reloads
    /// fresh rather than serving a stale document.
    pub fn invalidate(&self, path: &str) {
        let ctx_id = file_context_id(path);
        self.cache.write().remove(&ctx_id);
    }

    /// Like [`invalidate`](Self::invalidate), but also drops the **backing shadow
    /// document** so the next read fully reloads from the VFS.
    ///
    /// Plain `invalidate` only removes the in-memory entry; the shadow document
    /// (at `file_context_id(path)`) survives in the block store. The next
    /// `get_or_load` hits `create_document`'s `DocumentAlreadyExists` arm, which
    /// reconciles that shadow's content against a fresh VFS read before serving
    /// it — so for a **self-contained file** (the doc *is* the truth, and "VFS
    /// read" means "read disk"), plain `invalidate` is already enough to pick up
    /// a change.
    ///
    /// A **config shadow** still needs the stronger call. Its real owner is the
    /// `config_context_id` block, and the vi editor / `kj rc` write it there via
    /// `block_store.edit_text` directly — never through `ConfigDocFs::write_all`.
    /// That write never advances `ConfigDocFs`'s per-path generation counter, so
    /// a shadow entry still resident in memory (not yet invalidated) has no
    /// coherence signal telling it the config changed underneath it, and would
    /// go on serving pre-edit content indefinitely. Every direct config write
    /// must invalidate the shadow explicitly rather than rely on disk-generation
    /// staleness detection. `invalidate_document` drops both the in-memory entry
    /// and the shadow document itself, so the next read reloads fresh from the
    /// VFS. The shadow is a pure cache materialization, so dropping it is safe;
    /// a delete failure is surfaced (never a swallowed stale serve).
    pub fn invalidate_document(&self, path: &str) -> Result<(), String> {
        let ctx_id = file_context_id(path);
        self.cache.write().remove(&ctx_id);
        self.block_store
            .delete_document(ctx_id)
            .map_err(|e| format!("invalidate_document({path}): {e}"))
    }

    /// Mark a file as dirty (needs flush to VFS). Also records the durable
    /// swap-file marker (`dirty_file_buffers`, docs/file-buffers.md) so the
    /// edit survives a cold cache — a swallowed failure here means unsaved
    /// work silently stops being recoverable, so the DB error bubbles rather
    /// than being logged and dropped.
    pub fn mark_dirty(&self, path: &str) -> Result<(), String> {
        let ctx_id = file_context_id(path);
        // Set the flag and read back what the DB row needs under one lock
        // acquisition, then release it before taking the DB lock — mirrors
        // the sequential (never nested) lock order used elsewhere in this
        // file.
        let entry_info = {
            let mut cache = self.cache.write();
            cache.get_mut(&ctx_id).map(|entry| {
                entry.dirty = true;
                (entry.context_id, entry.loaded_generation)
            })
        };
        if let Some((entry_ctx_id, loaded_generation)) = entry_info {
            self.db
                .lock()
                .record_dirty_file_buffer(path, entry_ctx_id, loaded_generation)
                .map_err(|e| format!("mark_dirty({path}): failed to record swap marker: {e}"))?;
        }
        Ok(())
    }

    /// Flush all dirty files back to the VFS.
    pub async fn flush_dirty(&self) -> Result<usize, String> {
        let dirty_entries: Vec<(String, ContextId, BlockId)> = {
            let cache = self.cache.read();
            cache
                .values()
                .filter(|e| e.dirty)
                .map(|e| (e.path.clone(), e.context_id, e.block_id))
                .collect()
        };

        let mut flushed = 0;
        let mut errors: Vec<String> = Vec::new();
        let mut succeeded: Vec<(ContextId, Option<u64>)> = Vec::new();

        for (path, ctx_id, block_id) in &dirty_entries {
            let content = self
                .block_store
                .block_snapshots(*ctx_id)
                .ok()
                .and_then(|snaps| {
                    snaps
                        .iter()
                        .find(|s| s.id == *block_id)
                        .map(|s| s.content.clone())
                })
                .unwrap_or_default();

            let vfs_path = std::path::Path::new(path);
            match self.vfs.write_all(vfs_path, content.as_bytes()).await {
                Ok(()) => {
                    flushed += 1;
                    let generation = self.vfs.getattr(vfs_path).await.ok().map(|a| a.generation);
                    // Disk holds the content now — the swap marker's job is
                    // done. A failure clearing it is surfaced (never
                    // swallowed), but the write to disk genuinely succeeded,
                    // so the in-memory entry below is still marked clean.
                    if let Err(e) = self.db.lock().clear_dirty_file_buffer(path) {
                        errors.push(format!(
                            "flushed {} to disk but failed to clear its swap marker: {}",
                            path, e
                        ));
                    }
                    succeeded.push((*ctx_id, generation));
                }
                Err(e) => {
                    errors.push(format!("failed to flush {}: {}", path, e));
                }
            }
        }

        // Only clear dirty flags for files that were successfully flushed to
        // disk, and stamp the post-flush generation so they aren't seen as
        // externally changed. Runs regardless of a swap-marker clear failure
        // above — disk already holds the content, so the in-memory entry is
        // genuinely clean.
        {
            let mut cache = self.cache.write();
            for (ctx_id, generation) in &succeeded {
                if let Some(entry) = cache.get_mut(ctx_id) {
                    entry.dirty = false;
                    entry.loaded_generation = *generation;
                    entry.disk_changed_since_load = false;
                }
            }
        }

        if errors.is_empty() {
            Ok(flushed)
        } else {
            Err(format!(
                "flush_dirty: {}/{} failed: {}",
                errors.len(),
                dirty_entries.len(),
                errors.join("; ")
            ))
        }
    }

    /// Flush a single file back to the VFS.
    ///
    /// Refuses while the entry is an unacknowledged recovered swap (rule 4,
    /// docs/file-buffers.md): a swap is not served as authoritative until a
    /// player has been told, and writing it over disk is exactly "serving it
    /// as authoritative." Call [`acknowledge_swap`](Self::acknowledge_swap)
    /// first. Reads are unaffected — only the flush path is gated.
    ///
    /// Does **not** perform the W12 changed-under-us check — that is
    /// [`flush_one_guarded`](Self::flush_one_guarded). This is the unguarded
    /// primitive every non-editor writer (`create_or_replace`'s callers) uses,
    /// where "flush whatever the buffer holds" is the intended meaning.
    pub async fn flush_one(&self, path: &str) -> Result<(), FlushError> {
        let ctx_id = file_context_id(path);
        let block_id = {
            let cache = self.cache.read();
            match cache.get(&ctx_id) {
                Some(entry) if entry.swap_recovered => {
                    return Err(FlushError::UnacknowledgedSwap {
                        path: path.to_string(),
                    });
                }
                Some(entry) if entry.dirty => entry.block_id,
                Some(_) => return Ok(()), // not dirty
                None => return Ok(()),    // not cached
            }
        };

        let content = self
            .block_store
            .block_snapshots(ctx_id)
            .ok()
            .and_then(|snaps| {
                snaps
                    .iter()
                    .find(|s| s.id == block_id)
                    .map(|s| s.content.clone())
            })
            .unwrap_or_default();

        let vfs_path = std::path::Path::new(path);
        self.vfs
            .write_all(vfs_path, content.as_bytes())
            .await
            .map_err(|e| FlushError::Backend(format!("failed to flush {}: {}", path, e)))?;

        // Disk holds the content now — the swap marker's job is done. Clear
        // it before touching the in-memory entry so a crash in between still
        // leaves nothing behind that would announce a swap that no longer
        // exists. Bubbled, not swallowed: mark_dirty bubbles the same class
        // of error, and a lingering row here would misreport this path as an
        // unacknowledged swap on the next cold start.
        self.db.lock().clear_dirty_file_buffer(path).map_err(|e| {
            FlushError::Backend(format!(
                "flush_one({path}): flushed to disk but failed to clear the swap marker: {e}"
            ))
        })?;

        // Stamp the post-flush generation so our own write isn't later mistaken
        // for an external change and needlessly reloaded.
        let generation = self.vfs.getattr(vfs_path).await.ok().map(|a| a.generation);
        {
            let mut cache = self.cache.write();
            if let Some(entry) = cache.get_mut(&ctx_id) {
                entry.dirty = false;
                entry.loaded_generation = generation;
                entry.disk_changed_since_load = false;
            }
        }

        Ok(())
    }

    /// Flush a single file, refusing when disk moved under the buffer since it
    /// was loaded — vim's W12 "changed since reading it". `force` (`:w!`)
    /// overrides.
    ///
    /// Separate from [`flush_one`](Self::flush_one) rather than a parameter on
    /// it because the two have genuinely different semantics: a whole-file
    /// overwrite through the VFS (`echo x > file`, `mount_backend`'s
    /// `write_all`) *means* "I do not care what is there," and
    /// `create_or_replace` does not re-stamp `loaded_generation`, so guarding
    /// that path would refuse every redirect onto an externally-edited file.
    /// The editor is the surface with a `!` to say otherwise. See
    /// `docs/file-buffers.md`.
    ///
    /// Even with `force: true`, [`flush_one`](Self::flush_one)'s own
    /// `UnacknowledgedSwap` refusal still applies — `:w!` overrides "disk
    /// changed," not "you have not been told about a recovered swap" (rule 4).
    pub async fn flush_one_guarded(&self, path: &str, force: bool) -> Result<(), FlushError> {
        if force {
            return self.flush_one(path).await;
        }

        let ctx_id = file_context_id(path);
        let entry_info = {
            let cache = self.cache.read();
            cache
                .get(&ctx_id)
                .map(|entry| (entry.loaded_generation, entry.disk_changed_since_load))
        };
        let (loaded_generation, already_flagged) = match entry_info {
            Some(info) => info,
            None => return self.flush_one(path).await,
        };

        let vfs_path = std::path::Path::new(path);
        let disk_generation = self.vfs.getattr(vfs_path).await.ok().map(|a| a.generation);
        // Same predicate `try_get_or_load`'s `stale` binding uses, OR'd with
        // the sticky flag: the flag records an observation the read path
        // already made, and `FileAttr::generation`'s own doc says a
        // `LocalBackend` generation can step *backward* (an mtime moved
        // backward), so a live check alone can lose a change the read path
        // already saw.
        let disk_moved = already_flagged
            || matches!((disk_generation, loaded_generation), (Some(d), Some(l)) if d > l);

        if disk_moved {
            let mut cache = self.cache.write();
            if let Some(entry) = cache.get_mut(&ctx_id) {
                entry.disk_changed_since_load = true;
            }
            return Err(FlushError::DiskChanged {
                path: path.to_string(),
                loaded: loaded_generation,
                disk: disk_generation,
            });
        }

        self.flush_one(path).await
    }

    /// Acknowledge a recovered swap, clearing `swap_recovered` so the entry
    /// can be flushed. A no-op for a path that isn't cached or wasn't a
    /// recovered swap. See docs/file-buffers.md rule 4.
    pub fn acknowledge_swap(&self, path: &str) {
        let ctx_id = file_context_id(path);
        let mut cache = self.cache.write();
        if let Some(entry) = cache.get_mut(&ctx_id) {
            entry.swap_recovered = false;
        }
    }

    /// Get the SharedBlockStore (for engines that need direct store access).
    pub fn block_store(&self) -> &SharedBlockStore {
        &self.block_store
    }

    /// Get the VFS mount table.
    pub fn vfs(&self) -> &Arc<MountTable> {
        &self.vfs
    }

    /// Load a file with given content (for write-new-file case).
    async fn get_or_load_with_content(
        &self,
        path: &str,
        content: &str,
    ) -> Result<(ContextId, BlockId), String> {
        let ctx_id = file_context_id(path);
        let language = detect_language(path);

        // The kernel block store persists file documents across restarts while
        // this in-memory cache starts cold. So a cache miss does NOT imply the
        // document is new — it may already exist in the store (e.g. after a
        // kernel restart). create_document fails in that case; fall back to
        // replacing the existing block's content rather than erroring.
        let block_id = match self
            .block_store
            .create_document(ctx_id, DocKind::File, language)
        {
            Ok(()) => self
                .block_store
                .insert_block(
                    ctx_id,
                    None,
                    None,
                    Role::System,
                    BlockKind::Text,
                    content,
                    Status::Done,
                    ContentType::Plain,
                )
                .map_err(|e| format!("failed to insert block for {}: {}", path, e))?,
            Err(_) => {
                // Doc already in the store (cold cache). Replace its block's
                // content with the new bytes (char-indexed delete, like the
                // cached-hit path).
                let snaps = self
                    .block_store
                    .block_snapshots(ctx_id)
                    .map_err(|e| format!("failed to read existing doc {}: {}", path, e))?;
                let existing = snaps
                    .first()
                    .ok_or_else(|| format!("document {} exists but has no blocks", path))?;
                self.block_store
                    .edit_text(
                        ctx_id,
                        &existing.id,
                        0,
                        content,
                        existing.content.chars().count(),
                    )
                    .map_err(|e| e.to_string())?;
                existing.id
            }
        };

        {
            let mut cache = self.cache.write();
            self.evict_if_needed(&mut cache);
            cache.insert(
                ctx_id,
                CachedFileDoc {
                    context_id: ctx_id,
                    path: path.to_string(),
                    block_id,
                    dirty: false,
                    last_access: Instant::now(),
                    // Not yet on disk; the next flush stamps the real generation.
                    loaded_generation: None,
                    disk_changed_since_load: false,
                    swap_recovered: false,
                },
            );
        }

        Ok((ctx_id, block_id))
    }

    /// Evict oldest clean entries if cache exceeds max size.
    /// Dirty entries are never evicted — they must be flushed first.
    fn evict_if_needed(&self, cache: &mut HashMap<ContextId, CachedFileDoc>) {
        while cache.len() >= self.max_cached {
            // Find oldest non-dirty entry
            let oldest_clean = cache
                .iter()
                .filter(|(_, e)| !e.dirty)
                .min_by_key(|(_, e)| e.last_access)
                .map(|(k, _)| *k);

            if let Some(key) = oldest_clean {
                cache.remove(&key);
            } else {
                // All entries are dirty — can't evict without data loss.
                // Allow cache to exceed max until a flush clears dirty flags.
                tracing::warn!(
                    cache_size = cache.len(),
                    max = self.max_cached,
                    "All cached file documents are dirty — skipping eviction. Call flush_dirty()."
                );
                break;
            }
        }
    }
}

/// Derive a deterministic ContextId from a file path.
///
/// File documents aren't real contexts, but BlockStore is keyed by ContextId.
/// We use UUIDv5 (namespace: URL) so the same path always maps to the same ID.
///
/// `pub(crate)` so ownership tests can assert the *absence* of a document at
/// this id: for a config-owned path the file-doc id must never be minted at all
/// (`docs/config-ownership.md`), and only this function knows where such a
/// shadow would live.
pub(crate) fn file_context_id(path: &str) -> ContextId {
    let uuid = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("kaijutsu:file:{}", path).as_bytes(),
    );
    ContextId::from_bytes(*uuid.as_bytes())
}

/// Detect programming language from file extension.
fn detect_language(path: &str) -> Option<String> {
    let ext = path.rsplit('.').next()?;
    let lang = match ext {
        "rs" => "rust",
        "py" => "python",
        "js" => "javascript",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "jsx" => "javascriptreact",
        "go" => "go",
        "rb" => "ruby",
        "lua" => "lua",
        "sh" | "bash" => "bash",
        "zsh" => "zsh",
        "c" => "c",
        "cpp" | "cc" | "cxx" => "cpp",
        "h" => "c",
        "hpp" => "cpp",
        "java" => "java",
        "kt" => "kotlin",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "json" => "json",
        "md" => "markdown",
        "html" => "html",
        "css" => "css",
        "sql" => "sql",
        "wgsl" => "wgsl",
        _ => return None,
    };
    Some(lang.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_context_id_deterministic() {
        let id1 = file_context_id("src/main.rs");
        let id2 = file_context_id("src/main.rs");
        assert_eq!(id1, id2, "same path should produce same ContextId");

        let id3 = file_context_id("/mnt/project/lib.rs");
        assert_ne!(
            id1, id3,
            "different paths should produce different ContextIds"
        );
    }

    use crate::block_store::shared_block_store;
    use crate::vfs::backends::MemoryBackend;
    use crate::vfs::VfsOps;
    use kaijutsu_types::PrincipalId;

    /// A fresh temporary `KernelDb` for tests — `FileDocumentCache` requires
    /// one to back its durable swap-file marker (docs/file-buffers.md).
    /// Factored out so `tmp_cache` (and anything else that needs its own
    /// handle) doesn't repeat the construction.
    fn tmp_db() -> Arc<Mutex<KernelDb>> {
        Arc::new(Mutex::new(KernelDb::temporary().unwrap()))
    }

    /// Build a cache over a MemoryBackend mounted at /tmp, backed by a fresh
    /// temporary KernelDb (see `tmp_db`) — the same durable path production
    /// runs, so a swap survives `invalidate` the way it survives a real
    /// restart.
    async fn tmp_cache() -> (Arc<MountTable>, FileDocumentCache) {
        let blocks = shared_block_store(PrincipalId::system());
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/tmp", MemoryBackend::new()).await;
        let cache = FileDocumentCache::new(blocks, vfs.clone(), tmp_db());
        (vfs, cache)
    }

    fn p(s: &str) -> &std::path::Path {
        std::path::Path::new(s)
    }

    #[tokio::test]
    async fn create_or_replace_handles_multibyte_when_cached() {
        // Regression: create_or_replace deleted `old_content.len()` (bytes)
        // chars from a kernel block, panicking out-of-bounds when the cached
        // content held multi-byte UTF-8 (the rc stance files: 改善, em-dashes,
        // …). It must delete by CHARACTER count.
        let (_vfs, cache) = tmp_cache().await;

        // First write loads the doc into the cache (new-file path).
        let original = "改善 — the standard we accept …\nline two";
        cache.create_or_replace("/tmp/s.md", original).await.unwrap();
        assert_eq!(cache.read_content("/tmp/s.md").await.unwrap(), original);

        // Replace the now-cached doc with different multi-byte content of a
        // *shorter* char length — the byte-vs-char bug overran here.
        let replacement = "短い";
        cache
            .create_or_replace("/tmp/s.md", replacement)
            .await
            .expect("replace cached multi-byte doc must not panic");
        assert_eq!(
            cache.read_content("/tmp/s.md").await.unwrap(),
            replacement
        );
    }

    #[tokio::test]
    async fn create_or_replace_handles_doc_in_store_but_not_cache() {
        // Regression: the block store persists file docs across restarts while
        // this cache starts cold. A cache miss with the doc still in the store
        // must replace its content, not fail create_document with
        // "document already exists". `invalidate` reproduces the cold cache.
        let (_vfs, cache) = tmp_cache().await;

        cache.create_or_replace("/tmp/r.kai", "v1").await.unwrap();
        // Simulate restart: cache entry gone, store doc remains.
        cache.invalidate("/tmp/r.kai");

        // Replace through the cold-cache path (with multi-byte, to also cover
        // the char-count delete in the fallback branch).
        cache
            .create_or_replace("/tmp/r.kai", "改善 v2 …")
            .await
            .expect("replace a store-resident doc after a cold cache");
        assert_eq!(cache.read_content("/tmp/r.kai").await.unwrap(), "改善 v2 …");
    }

    #[tokio::test]
    async fn external_change_invalidates_clean_cache() {
        let (vfs, cache) = tmp_cache().await;

        vfs.write_all(p("/tmp/f.txt"), b"v1").await.unwrap();
        assert_eq!(cache.read_content("/tmp/f.txt").await.unwrap(), "v1");

        // External writer changes the file — the backend bumps its generation,
        // which is what marks the clean cache entry stale (no mtime fiddling
        // needed; generation is the coherence signal now).
        vfs.write_all(p("/tmp/f.txt"), b"v2").await.unwrap();

        // Clean entry must reload and serve the new content.
        assert_eq!(cache.read_content("/tmp/f.txt").await.unwrap(), "v2");
    }

    #[tokio::test]
    async fn dirty_edits_survive_external_change() {
        let (vfs, cache) = tmp_cache().await;

        vfs.write_all(p("/tmp/g.txt"), b"disk-v1").await.unwrap();
        assert_eq!(cache.read_content("/tmp/g.txt").await.unwrap(), "disk-v1");

        // Local uncommitted edit (dirty, not flushed).
        cache.create_or_replace("/tmp/g.txt", "local-edit").await.unwrap();
        cache.mark_dirty("/tmp/g.txt").unwrap();

        // External writer also changes the file (bumps the backend generation).
        vfs.write_all(p("/tmp/g.txt"), b"disk-v2").await.unwrap();

        // Local edits win — we must not clobber uncommitted work with disk state.
        assert_eq!(cache.read_content("/tmp/g.txt").await.unwrap(), "local-edit");
    }

    #[test]
    fn test_detect_language() {
        assert_eq!(detect_language("main.rs"), Some("rust".to_string()));
        assert_eq!(detect_language("app.py"), Some("python".to_string()));
        assert_eq!(detect_language("Cargo.toml"), Some("toml".to_string()));
        assert_eq!(detect_language("noext"), None);
        assert_eq!(detect_language("script.sh"), Some("bash".to_string()));
        // Rhai/RON were removed from the project (project_rhai_removal); their
        // language-ID arms are vestigial. Nothing produces or consumes .ron/.rhai
        // files, so detection must not resurrect them.
        assert_eq!(detect_language("config.ron"), None);
        assert_eq!(detect_language("plugin.rhai"), None);
    }

    /// Regression: a Backend error on `try_read_content` must be returned as
    /// `Err(CacheReadError::Backend(...))`, NOT silently collapsed to
    /// `Err(CacheReadError::NotCached)`. This test MUST FAIL on any code that
    /// flattens Backend into NotCached (e.g. via `unwrap_or(NotCached)`).
    ///
    /// Technique: write a file through the cache to populate the in-memory entry,
    /// then delete the kernel document from the block store so the next
    /// `block_snapshots` call fails — simulating a store inconsistency.
    #[tokio::test]
    async fn try_read_content_backend_error_is_not_swallowed_as_not_cached() {
        let (vfs, cache) = tmp_cache().await;

        // Write through the cache so the kernel document exists.
        vfs.write_all(p("/tmp/backend_err.txt"), b"content")
            .await
            .unwrap();
        assert_eq!(
            cache.read_content("/tmp/backend_err.txt").await.unwrap(),
            "content"
        );

        // Destroy the kernel document (simulates store corruption / inconsistency).
        // The cache entry (in-memory) still points to the now-gone context_id.
        let ctx_id = file_context_id("/tmp/backend_err.txt");
        cache
            .block_store
            .delete_document(ctx_id)
            .expect("setup: delete_document must succeed");

        // try_read_content must return Backend, not NotCached.
        // A NotCached result would cause callers to fall through to a raw disk
        // read and silently serve stale on-disk bytes — silent data corruption.
        let err = cache
            .try_read_content("/tmp/backend_err.txt")
            .await
            .expect_err("must fail after block store deletion");
        assert!(
            matches!(err, CacheReadError::Backend(_)),
            "expected Backend, got NotCached — old code swallowed the error"
        );
    }

    /// Regression: `try_read_content` on a binary file (not UTF-8 representable
    /// as document text) must return `NotCached`, not `Backend`. Callers
    /// such as grep fall through to a raw VFS read for binary files and must
    /// not be blocked by a spurious backend error.
    #[tokio::test]
    async fn try_read_content_binary_file_is_not_cached() {
        let (vfs, cache) = tmp_cache().await;

        // Write binary content (invalid UTF-8) directly to the VFS.
        vfs.write_all(p("/tmp/binary.bin"), b"\xff\xfe\x00\x01binary")
            .await
            .unwrap();

        // Must be NotCached — not a backend error. Old code returned an opaque
        // String error that callers couldn't distinguish from a real failure.
        let err = cache
            .try_read_content("/tmp/binary.bin")
            .await
            .expect_err("binary file must not decode as document text");
        assert!(
            matches!(err, CacheReadError::NotCached),
            "binary file must be NotCached, not Backend: {:?}", err
        );
    }

    /// Regression: `try_read_content` on a file that doesn't exist must return
    /// `NotCached` (benign fallthrough), not `Backend`.
    #[tokio::test]
    async fn try_read_content_absent_file_is_not_cached() {
        let (_vfs, cache) = tmp_cache().await;

        let err = cache
            .try_read_content("/tmp/no_such_file_xyz.txt")
            .await
            .expect_err("absent file must fail");
        assert!(
            matches!(err, CacheReadError::NotCached),
            "absent file must be NotCached, not Backend: {:?}", err
        );
    }

    /// Regression (F1 / stale-reload): when a clean cache entry's file has a
    /// greater generation on disk, `try_get_or_load` enters the stale-reload
    /// path and calls `reload_block_from_disk`. If the block store fails at that
    /// point (e.g. the kernel document was deleted), the error MUST propagate as
    /// `CacheReadError::Backend` — not be swallowed as `NotCached`.
    ///
    /// The old code blanket-converted every reload error to `NotCached`, which
    /// caused callers like `mount_backend::append` to fall through to
    /// `String::new()` as the prior content and overwrite the file with just
    /// the suffix — silent data wipe.
    ///
    /// Setup: write a file through the cache (clean entry with a known
    /// generation), then bump the disk generation past `loaded_generation` with
    /// an external write so the entry is seen as stale, then delete the kernel
    /// document so `block_snapshots` inside `reload_block_from_disk` returns an
    /// error.
    #[tokio::test]
    async fn stale_reload_backend_error_is_not_swallowed_as_not_cached() {
        let (vfs, cache) = tmp_cache().await;

        // Seed a clean cache entry.
        vfs.write_all(p("/tmp/stale_reload.txt"), b"original")
            .await
            .unwrap();
        assert_eq!(
            cache.read_content("/tmp/stale_reload.txt").await.unwrap(),
            "original"
        );

        // Advance the disk *generation* so the cached entry looks stale. An
        // external content write bumps the backend's generation; a pure
        // setattr(mtime) deliberately would NOT (it's display-only now), so the
        // staleness signal must come from a real write.
        vfs.write_all(p("/tmp/stale_reload.txt"), b"changed on disk")
            .await
            .unwrap();

        // Break the block store so reload_block_from_disk fails on
        // block_snapshots / edit_text (both hit the store after deletion).
        let ctx_id = file_context_id("/tmp/stale_reload.txt");
        cache
            .block_store
            .delete_document(ctx_id)
            .expect("setup: delete_document must succeed");

        // The stale-reload must surface the store failure as Backend, NOT
        // silently collapse it to NotCached (which would cause append to wipe).
        let err = cache
            .try_read_content("/tmp/stale_reload.txt")
            .await
            .expect_err("stale reload over broken store must fail");
        assert!(
            matches!(err, CacheReadError::Backend(_)),
            "stale-reload backend failure must be Backend, got NotCached — \
             old code swallowed the error and would silently wipe the file: {:?}",
            err
        );
    }

    /// A document already exists in the block store with stale content while
    /// disk holds something newer. A cold cache (post-restart) must serve the
    /// DISK content, not the pre-existing document — the case the old `Err(_)`
    /// arm got backward, discarding the freshly-read disk text and serving the
    /// stale block instead. See docs/file-buffers.md.
    #[tokio::test]
    async fn stale_document_reconciles_with_newer_disk_on_cold_cache() {
        let (vfs, cache) = tmp_cache().await;

        // Load the document once so it exists in the block store.
        vfs.write_all(p("/tmp/incident.md"), b"stale content")
            .await
            .unwrap();
        assert_eq!(
            cache.read_content("/tmp/incident.md").await.unwrap(),
            "stale content"
        );

        // Simulate a kernel restart: drop the in-memory entry, leave the
        // document (and its stale content) in the block store.
        cache.invalidate("/tmp/incident.md");

        // A writer that bypasses the cache changes disk directly — exactly
        // what happens while the kernel is down or a sibling process edits
        // the file.
        vfs.write_all(p("/tmp/incident.md"), b"newer content on disk")
            .await
            .unwrap();

        // Cold cache + doc-already-exists must reconcile against disk, not
        // serve the stale pre-existing block.
        assert_eq!(
            cache.read_content("/tmp/incident.md").await.unwrap(),
            "newer content on disk",
            "must serve disk content, not the stale pre-existing document"
        );
    }

    /// A reconcile that stamps `loaded_generation` from the wrong read (or
    /// from "current disk gen" without confirming the block was actually
    /// updated) poisons the freshness stamp exactly like the original bug —
    /// the entry looks clean forever after. A *second* external write, after
    /// the reconcile in the test above, must still be visible.
    #[tokio::test]
    async fn reconcile_does_not_poison_loaded_generation() {
        let (vfs, cache) = tmp_cache().await;

        vfs.write_all(p("/tmp/incident2.md"), b"v1").await.unwrap();
        assert_eq!(cache.read_content("/tmp/incident2.md").await.unwrap(), "v1");

        cache.invalidate("/tmp/incident2.md");
        vfs.write_all(p("/tmp/incident2.md"), b"v2").await.unwrap();
        assert_eq!(cache.read_content("/tmp/incident2.md").await.unwrap(), "v2");

        // A further external write after the reconcile must still be
        // observed — proves loaded_generation tracks real disk state, not a
        // value poisoned during reconcile.
        vfs.write_all(p("/tmp/incident2.md"), b"v3").await.unwrap();
        assert_eq!(cache.read_content("/tmp/incident2.md").await.unwrap(), "v3");
    }

    /// A dirty buffer is unsaved work and must still be served as-is (rule 2,
    /// `docs/file-buffers.md`) — but the fact that disk moved under it must
    /// not be thrown away, since the W12 `:w` guard needs it.
    #[tokio::test]
    async fn dirty_entry_still_served_but_disk_change_is_recorded() {
        let (vfs, cache) = tmp_cache().await;

        vfs.write_all(p("/tmp/dirty.md"), b"disk-v1").await.unwrap();
        assert_eq!(cache.read_content("/tmp/dirty.md").await.unwrap(), "disk-v1");

        cache
            .create_or_replace("/tmp/dirty.md", "local-edit")
            .await
            .unwrap();
        cache.mark_dirty("/tmp/dirty.md").unwrap();

        assert!(
            !cache.disk_changed_since_load("/tmp/dirty.md"),
            "nothing has observed a disk move yet"
        );

        vfs.write_all(p("/tmp/dirty.md"), b"disk-v2").await.unwrap();

        assert_eq!(
            cache.read_content("/tmp/dirty.md").await.unwrap(),
            "local-edit",
            "a dirty buffer is unsaved work and must still be served"
        );
        assert!(
            cache.disk_changed_since_load("/tmp/dirty.md"),
            "disk moving under a dirty entry must be recorded for the W12 guard"
        );
    }

    /// `docs/file-buffers.md` rule 2: a dirty (unsaved) buffer is a swap file
    /// and must survive a kernel restart. `mark_dirty` records a durable
    /// `dirty_file_buffers` row; on a cold cache, `try_get_or_load`'s
    /// `DocumentAlreadyExists` arm consults that row *before* reconciling —
    /// a row present means "swap, not stale mirror," and the block is served
    /// as-is instead of being overwritten with disk content.
    #[tokio::test]
    async fn unsaved_edits_survive_a_cold_cache_as_a_recovered_swap() {
        let (vfs, cache) = tmp_cache().await;

        vfs.write_all(p("/tmp/swap.md"), b"disk-v1").await.unwrap();
        assert_eq!(cache.read_content("/tmp/swap.md").await.unwrap(), "disk-v1");

        // Local uncommitted edit — dirty, never flushed to disk.
        cache
            .create_or_replace("/tmp/swap.md", "unsaved-edit")
            .await
            .unwrap();
        cache.mark_dirty("/tmp/swap.md").unwrap();
        assert_eq!(
            cache.read_content("/tmp/swap.md").await.unwrap(),
            "unsaved-edit"
        );

        // Simulate a kernel restart: the in-memory entry (and its dirty flag)
        // is gone. Nothing flushed it first — the `dirty_file_buffers` row
        // (in the same `KernelDb` the cache holds) is what survives instead.
        cache.invalidate("/tmp/swap.md");

        // The unsaved edit comes back, not disk content — recovered as a
        // swap, not silently discarded.
        assert_eq!(
            cache.read_content("/tmp/swap.md").await.unwrap(),
            "unsaved-edit",
            "unsaved edit must survive a cold cache as a recovered swap"
        );
        assert!(
            cache.swap_recovered("/tmp/swap.md"),
            "a recovered swap must be flagged, not served as silently authoritative"
        );
    }

    /// Proves the swap-recovery check (above) does not break slice 1's
    /// incident fix: a document that is genuinely just a stale mirror (never
    /// dirty, no `dirty_file_buffers` row) must still reconcile against
    /// newer disk content on a cold cache — everything here is set up to
    /// *look* like the swap case (a document already in the store, a cold
    /// cache) except the one thing that actually marks a swap.
    #[tokio::test]
    async fn clean_document_still_reconciles_with_newer_disk_on_cold_cache() {
        let (vfs, cache) = tmp_cache().await;

        vfs.write_all(p("/tmp/clean.md"), b"v1").await.unwrap();
        assert_eq!(cache.read_content("/tmp/clean.md").await.unwrap(), "v1");
        // Never dirtied — no dirty_file_buffers row.

        cache.invalidate("/tmp/clean.md");
        vfs.write_all(p("/tmp/clean.md"), b"v2").await.unwrap();

        assert_eq!(
            cache.read_content("/tmp/clean.md").await.unwrap(),
            "v2",
            "a clean document with no swap marker must still reconcile against disk"
        );
        assert!(
            !cache.swap_recovered("/tmp/clean.md"),
            "a reconciled document is not a recovered swap"
        );
    }

    /// Flushing a dirty buffer clears its `dirty_file_buffers` row, so a
    /// later cold load sees no swap marker and reconciles against disk
    /// instead of recovering stale-and-already-flushed content.
    #[tokio::test]
    async fn flush_clears_the_swap_marker_so_a_later_cold_load_reconciles() {
        let (vfs, cache) = tmp_cache().await;

        vfs.write_all(p("/tmp/flushed.md"), b"disk-v1").await.unwrap();
        assert_eq!(cache.read_content("/tmp/flushed.md").await.unwrap(), "disk-v1");

        cache
            .create_or_replace("/tmp/flushed.md", "saved-edit")
            .await
            .unwrap();
        cache.mark_dirty("/tmp/flushed.md").unwrap();
        cache.flush_one("/tmp/flushed.md").await.unwrap();

        // Simulate a restart after the flush. If the marker weren't cleared,
        // this would be (wrongly) treated as a recovered swap.
        cache.invalidate("/tmp/flushed.md");
        vfs.write_all(p("/tmp/flushed.md"), b"disk-v2-external")
            .await
            .unwrap();

        assert_eq!(
            cache.read_content("/tmp/flushed.md").await.unwrap(),
            "disk-v2-external",
            "a flushed buffer's marker must be cleared, so a later cold load reconciles"
        );
        assert!(!cache.swap_recovered("/tmp/flushed.md"));
    }

    /// `flush_one` must refuse an unacknowledged recovered swap (rule 4,
    /// docs/file-buffers.md) and succeed once `acknowledge_swap` clears it.
    #[tokio::test]
    async fn flush_one_refuses_an_unacknowledged_swap_then_succeeds_after_ack() {
        let (vfs, cache) = tmp_cache().await;

        vfs.write_all(p("/tmp/ack.md"), b"disk-v1").await.unwrap();
        assert_eq!(cache.read_content("/tmp/ack.md").await.unwrap(), "disk-v1");

        cache
            .create_or_replace("/tmp/ack.md", "unsaved-edit")
            .await
            .unwrap();
        cache.mark_dirty("/tmp/ack.md").unwrap();
        cache.invalidate("/tmp/ack.md");

        // Reload recovers the swap.
        assert_eq!(cache.read_content("/tmp/ack.md").await.unwrap(), "unsaved-edit");
        assert!(cache.swap_recovered("/tmp/ack.md"));

        let err = cache
            .flush_one("/tmp/ack.md")
            .await
            .expect_err("flush_one must refuse an unacknowledged recovered swap");
        assert!(
            matches!(err, FlushError::UnacknowledgedSwap { .. }),
            "refusal must point at acknowledge_swap, got: {err:?}"
        );
        // Disk must be untouched by the refused flush.
        assert_eq!(
            String::from_utf8(vfs.read_all(p("/tmp/ack.md")).await.unwrap()).unwrap(),
            "disk-v1"
        );

        cache.acknowledge_swap("/tmp/ack.md");
        assert!(!cache.swap_recovered("/tmp/ack.md"));
        cache
            .flush_one("/tmp/ack.md")
            .await
            .expect("flush_one must succeed once the swap is acknowledged");
        assert_eq!(
            String::from_utf8(vfs.read_all(p("/tmp/ack.md")).await.unwrap()).unwrap(),
            "unsaved-edit"
        );
    }

    /// The W12 guard (`docs/file-buffers.md` rule 3, slice 3): a plain flush
    /// refuses once disk has moved under the buffer, and `force` (`:w!`)
    /// overrides it.
    #[tokio::test]
    async fn flush_one_guarded_refuses_when_disk_moved_then_force_overrides() {
        let (vfs, cache) = tmp_cache().await;

        vfs.write_all(p("/tmp/w12.md"), b"disk-v1").await.unwrap();
        assert_eq!(cache.read_content("/tmp/w12.md").await.unwrap(), "disk-v1");

        cache
            .create_or_replace("/tmp/w12.md", "local-edit")
            .await
            .unwrap();
        cache.mark_dirty("/tmp/w12.md").unwrap();

        // External writer moves disk out from under the buffer.
        vfs.write_all(p("/tmp/w12.md"), b"external-edit")
            .await
            .unwrap();

        let err = cache
            .flush_one_guarded("/tmp/w12.md", false)
            .await
            .expect_err("a plain flush must refuse once disk moved under the buffer");
        assert!(
            matches!(err, FlushError::DiskChanged { .. }),
            "expected DiskChanged, got: {err:?}"
        );
        // The refusal must not touch disk.
        assert_eq!(
            String::from_utf8(vfs.read_all(p("/tmp/w12.md")).await.unwrap()).unwrap(),
            "external-edit"
        );

        // `:w!` overrides.
        cache
            .flush_one_guarded("/tmp/w12.md", true)
            .await
            .expect("force must override the W12 refusal");
        assert_eq!(
            String::from_utf8(vfs.read_all(p("/tmp/w12.md")).await.unwrap()).unwrap(),
            "local-edit"
        );
    }

    /// The non-refusal case: a flush proceeds normally when disk did not move
    /// under the buffer, and clears the swap marker exactly like `flush_one`.
    #[tokio::test]
    async fn flush_one_guarded_allows_a_flush_when_disk_did_not_move() {
        let (vfs, cache) = tmp_cache().await;

        vfs.write_all(p("/tmp/w12_ok.md"), b"disk-v1").await.unwrap();
        assert_eq!(cache.read_content("/tmp/w12_ok.md").await.unwrap(), "disk-v1");

        cache
            .create_or_replace("/tmp/w12_ok.md", "local-edit")
            .await
            .unwrap();
        cache.mark_dirty("/tmp/w12_ok.md").unwrap();

        cache
            .flush_one_guarded("/tmp/w12_ok.md", false)
            .await
            .expect("flush must succeed when disk did not move");
        assert_eq!(
            String::from_utf8(vfs.read_all(p("/tmp/w12_ok.md")).await.unwrap()).unwrap(),
            "local-edit"
        );

        // Swap marker cleared: a later cold load reconciles against disk
        // instead of recovering a (nonexistent) unacknowledged swap.
        cache.invalidate("/tmp/w12_ok.md");
        vfs.write_all(p("/tmp/w12_ok.md"), b"disk-v2-external")
            .await
            .unwrap();
        assert_eq!(
            cache.read_content("/tmp/w12_ok.md").await.unwrap(),
            "disk-v2-external"
        );
        assert!(!cache.swap_recovered("/tmp/w12_ok.md"));
    }

    /// Force is not a bypass for rule 4: `:w!` overrides "disk changed," not
    /// "you have not been told about a recovered swap."
    #[tokio::test]
    async fn flush_one_guarded_still_refuses_an_unacknowledged_swap_even_when_forced() {
        let (vfs, cache) = tmp_cache().await;

        vfs.write_all(p("/tmp/w12_swap.md"), b"disk-v1")
            .await
            .unwrap();
        assert_eq!(
            cache.read_content("/tmp/w12_swap.md").await.unwrap(),
            "disk-v1"
        );

        cache
            .create_or_replace("/tmp/w12_swap.md", "unsaved-edit")
            .await
            .unwrap();
        cache.mark_dirty("/tmp/w12_swap.md").unwrap();
        cache.invalidate("/tmp/w12_swap.md");

        // Reload recovers the swap.
        assert_eq!(
            cache.read_content("/tmp/w12_swap.md").await.unwrap(),
            "unsaved-edit"
        );
        assert!(cache.swap_recovered("/tmp/w12_swap.md"));

        let err = cache
            .flush_one_guarded("/tmp/w12_swap.md", true)
            .await
            .expect_err("force must not bypass the unacknowledged-swap refusal");
        assert!(
            matches!(err, FlushError::UnacknowledgedSwap { .. }),
            "expected UnacknowledgedSwap even when forced, got: {err:?}"
        );
    }
}
