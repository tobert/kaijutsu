//! CRDT-backed file document cache.
//!
//! Maps VFS files into CRDT documents, enabling concurrent editing
//! with the same operational semantics as block editing. Files are
//! loaded on demand and cached with LRU eviction.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use kaijutsu_crdt::{BlockId, BlockKind, ContentType, ContextId, Role, Status};
use parking_lot::RwLock;

use crate::block_store::SharedBlockStore;
use crate::vfs::{MountTable, VfsOps};
use kaijutsu_types::DocKind;

/// Default maximum cached file documents.
const DEFAULT_MAX_CACHED: usize = 64;

/// Typed error from [`FileDocumentCache::try_read_content`].
///
/// The two variants must be treated differently by callers:
/// - [`CacheReadError::NotCached`] is **benign**: the file is absent, binary,
///   or otherwise not representable in the CRDT text substrate. Callers *may*
///   fall through to a raw VFS read or treat the file as absent.
/// - [`CacheReadError::Backend`] is a **real failure** (CRDT store I/O, block
///   not found in a live document, etc.). Callers *must* surface it — serving
///   stale or empty bytes in place of a Backend error is silent data corruption.
#[derive(Debug)]
pub enum CacheReadError {
    /// File is absent, became binary, or can't be decoded as UTF-8.
    /// Benign: fall through to a raw read or treat as absent.
    NotCached,
    /// A real backend or CRDT store error. Surface it; never silently substitute
    /// stale bytes or an empty string.
    Backend(String),
}

impl std::fmt::Display for CacheReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheReadError::NotCached => write!(f, "file not in CRDT cache (binary or missing)"),
            CacheReadError::Backend(e) => write!(f, "{e}"),
        }
    }
}

/// A cached file backed by a CRDT document.
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
}

/// Cache that maps VFS files to CRDT documents.
///
/// Each file becomes a document with `DocKind::Code` and a single
/// `BlockKind::Text` block. Edits go through the CRDT, enabling
/// concurrent modification with proper conflict resolution.
pub struct FileDocumentCache {
    cache: RwLock<HashMap<ContextId, CachedFileDoc>>,
    block_store: SharedBlockStore,
    vfs: Arc<MountTable>,
    max_cached: usize,
}

impl FileDocumentCache {
    /// Create a new file document cache.
    pub fn new(block_store: SharedBlockStore, vfs: Arc<MountTable>) -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
            block_store,
            vfs,
            max_cached: DEFAULT_MAX_CACHED,
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
    /// Only emits a CRDT edit when the content actually differs.
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
        // CRDT edit. A store error here is a real backend failure — propagate it
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
            // Char-indexed delete (CRDT text positions are chars, not bytes).
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
        }
        Ok(())
    }

    /// Whether a file already exists — cached as a CRDT document or present
    /// on the backing VFS. Used to report created-vs-updated on write.
    pub async fn exists(&self, path: &str) -> bool {
        let ctx_id = file_context_id(path);
        if self.cache.read().contains_key(&ctx_id) {
            return true;
        }
        self.vfs.exists(std::path::Path::new(path)).await
    }

    /// Read the current content of a file (reflects any CRDT edits).
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
    /// - Any other VFS or CRDT store error → [`CacheReadError::Backend`] (real)
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
            if dirty {
                return Ok((cid, bid));
            }
            let disk_generation = self.vfs.getattr(vfs_path).await.ok().map(|a| a.generation);
            let stale =
                matches!((disk_generation, loaded_generation), (Some(d), Some(l)) if d > l);
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
                // Binary file — not an error, just not representable as CRDT text.
                return Err(CacheReadError::NotCached);
            }
        };

        let loaded_generation = self.vfs.getattr(vfs_path).await.ok().map(|a| a.generation);
        let language = detect_language(path);

        let block_id = match self
            .block_store
            .create_document(ctx_id, DocKind::Code, language)
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
            Err(_) => {
                {
                    let mut cache = self.cache.write();
                    if let Some(entry) = cache.get_mut(&ctx_id) {
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
                snapshots
                    .first()
                    .map(|s| s.id)
                    .ok_or_else(|| {
                        CacheReadError::Backend(format!(
                            "document {} exists but has no blocks",
                            path
                        ))
                    })?
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

                // `edit_text` indexes in CHARACTERS (CRDT text positions),
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

    /// Drop a path's cached CRDT document, if any. Used when a write bypasses
    /// the text substrate (e.g. binary content) so a later text read reloads
    /// fresh rather than serving a stale CRDT doc.
    pub fn invalidate(&self, path: &str) {
        let ctx_id = file_context_id(path);
        self.cache.write().remove(&ctx_id);
    }

    /// Like [`invalidate`](Self::invalidate), but also drops the **backing shadow
    /// document** so the next read fully reloads from the VFS.
    ///
    /// Plain `invalidate` only removes the in-memory entry; the shadow CRDT doc
    /// (at `file_context_id(path)`) survives, and the next `get_or_load`
    /// re-registers that same — now stale — block (the `create_document` already
    /// exists → snapshot-and-reuse path). That's correct for a self-contained
    /// file (the doc *is* the truth), but wrong for a **config shadow**: the real
    /// owner is the `config_context_id` block, and when *that* changed underneath
    /// us (e.g. the vi editor wrote it directly), the shadow must be rebuilt from
    /// the VFS, not re-served. Deleting the shadow doc forces the clean-reload
    /// path. The shadow is a pure cache materialization, so dropping it is safe;
    /// a delete failure is surfaced (never a swallowed stale serve).
    pub fn invalidate_document(&self, path: &str) -> Result<(), String> {
        let ctx_id = file_context_id(path);
        self.cache.write().remove(&ctx_id);
        self.block_store
            .delete_document(ctx_id)
            .map_err(|e| format!("invalidate_document({path}): {e}"))
    }

    /// Mark a file as dirty (needs flush to VFS).
    pub fn mark_dirty(&self, path: &str) {
        let ctx_id = file_context_id(path);
        let mut cache = self.cache.write();
        if let Some(entry) = cache.get_mut(&ctx_id) {
            entry.dirty = true;
        }
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
                    succeeded.push((*ctx_id, generation));
                }
                Err(e) => {
                    errors.push(format!("failed to flush {}: {}", path, e));
                }
            }
        }

        // Only clear dirty flags for files that were successfully flushed, and
        // stamp the post-flush generation so they aren't seen as externally
        // changed.
        {
            let mut cache = self.cache.write();
            for (ctx_id, generation) in &succeeded {
                if let Some(entry) = cache.get_mut(ctx_id) {
                    entry.dirty = false;
                    entry.loaded_generation = *generation;
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
    pub async fn flush_one(&self, path: &str) -> Result<(), String> {
        let ctx_id = file_context_id(path);
        let block_id = {
            let cache = self.cache.read();
            match cache.get(&ctx_id) {
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
            .map_err(|e| format!("failed to flush {}: {}", path, e))?;

        // Stamp the post-flush generation so our own write isn't later mistaken
        // for an external change and needlessly reloaded.
        let generation = self.vfs.getattr(vfs_path).await.ok().map(|a| a.generation);
        {
            let mut cache = self.cache.write();
            if let Some(entry) = cache.get_mut(&ctx_id) {
                entry.dirty = false;
                entry.loaded_generation = generation;
            }
        }

        Ok(())
    }

    /// Get the SharedBlockStore (for engines that need direct CRDT access).
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

        // The CRDT block store persists file documents across restarts while
        // this in-memory cache starts cold. So a cache miss does NOT imply the
        // document is new — it may already exist in the store (e.g. after a
        // kernel restart). create_document fails in that case; fall back to
        // replacing the existing block's content rather than erroring.
        let block_id = match self
            .block_store
            .create_document(ctx_id, DocKind::Code, language)
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
fn file_context_id(path: &str) -> ContextId {
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

    /// Build a cache over a MemoryBackend mounted at /tmp.
    async fn tmp_cache() -> (Arc<MountTable>, FileDocumentCache) {
        let blocks = shared_block_store(PrincipalId::system());
        let vfs = Arc::new(MountTable::new());
        vfs.mount("/tmp", MemoryBackend::new()).await;
        let cache = FileDocumentCache::new(blocks, vfs.clone());
        (vfs, cache)
    }

    fn p(s: &str) -> &std::path::Path {
        std::path::Path::new(s)
    }

    #[tokio::test]
    async fn create_or_replace_handles_multibyte_when_cached() {
        // Regression: create_or_replace deleted `old_content.len()` (bytes)
        // chars from a CRDT block, panicking out-of-bounds when the cached
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
        // Regression: the CRDT store persists file docs across restarts while
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
        cache.mark_dirty("/tmp/g.txt");

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
    /// then delete the CRDT document from the block store so the next
    /// `block_snapshots` call fails — simulating a store inconsistency.
    #[tokio::test]
    async fn try_read_content_backend_error_is_not_swallowed_as_not_cached() {
        let (vfs, cache) = tmp_cache().await;

        // Write through the cache so the CRDT document exists.
        vfs.write_all(p("/tmp/backend_err.txt"), b"content")
            .await
            .unwrap();
        assert_eq!(
            cache.read_content("/tmp/backend_err.txt").await.unwrap(),
            "content"
        );

        // Destroy the CRDT document (simulates store corruption / inconsistency).
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
    /// in the CRDT substrate) must return `NotCached`, not `Backend`. Callers
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
            .expect_err("binary file must not decode as CRDT text");
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
    /// newer mtime on disk, `try_get_or_load` enters the stale-reload path and
    /// calls `reload_block_from_disk`. If the block store fails at that point
    /// (e.g. the CRDT document was deleted), the error MUST propagate as
    /// `CacheReadError::Backend` — not be swallowed as `NotCached`.
    ///
    /// The old code blanket-converted every reload error to `NotCached`, which
    /// caused callers like `mount_backend::append` to fall through to
    /// `String::new()` as the prior content and overwrite the file with just
    /// the suffix — silent data wipe.
    ///
    /// Setup: write a file through the cache (clean entry with a known mtime),
    /// then advance the disk mtime past `loaded_mtime` so the entry is seen as
    /// stale, then delete the CRDT document so `block_snapshots` inside
    /// `reload_block_from_disk` returns an error.
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
}
