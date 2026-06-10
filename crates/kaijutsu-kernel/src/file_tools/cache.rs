//! CRDT-backed file document cache.
//!
//! Maps VFS files into CRDT documents, enabling concurrent editing
//! with the same operational semantics as block editing. Files are
//! loaded on demand and cached with LRU eviction.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use kaijutsu_crdt::{BlockId, BlockKind, ContentType, ContextId, Role, Status};
use parking_lot::RwLock;

use crate::block_store::SharedBlockStore;
use crate::vfs::{MountTable, VfsOps};
use kaijutsu_types::DocKind;

/// Default maximum cached file documents.
const DEFAULT_MAX_CACHED: usize = 64;

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
    /// VFS modification time when the content was last loaded from / flushed to
    /// disk. A clean entry whose backing file has a newer mtime is stale and
    /// gets reloaded — this is how external edits (cargo, git, the GUI) become
    /// visible. `None` means we couldn't read an mtime, so we trust the cache.
    loaded_mtime: Option<SystemTime>,
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
    pub async fn get_or_load(&self, path: &str) -> Result<(ContextId, BlockId), String> {
        let ctx_id = file_context_id(path);
        let vfs_path = std::path::Path::new(path);

        // Fast path: already cached.
        let cached = {
            let mut cache = self.cache.write();
            cache.get_mut(&ctx_id).map(|e| {
                e.last_access = Instant::now();
                (e.context_id, e.block_id, e.dirty, e.loaded_mtime)
            })
        };
        if let Some((cid, bid, dirty, loaded_mtime)) = cached {
            // Uncommitted local edits win — never clobber them with disk state.
            if dirty {
                return Ok((cid, bid));
            }
            // Clean entry: if the backing file changed under us (external edit),
            // refresh the CRDT block from disk so readers see the new content.
            let disk_mtime = self.vfs.getattr(vfs_path).await.ok().map(|a| a.mtime);
            let stale = matches!((disk_mtime, loaded_mtime), (Some(d), Some(l)) if d > l);
            if !stale {
                return Ok((cid, bid));
            }
            match self.reload_block_from_disk(ctx_id, &bid, path).await {
                Ok(()) => return Ok((cid, bid)),
                Err(e) => {
                    // Couldn't reload (became binary/unreadable/removed): drop
                    // the entry rather than serve stale content, and surface it.
                    self.cache.write().remove(&ctx_id);
                    return Err(e);
                }
            }
        }

        // Cache miss: load from VFS
        let content = self
            .vfs
            .read_all(vfs_path)
            .await
            .map_err(|e| format!("failed to read {}: {}", path, e))?;

        let text = String::from_utf8(content)
            .map_err(|e| format!("{} is not valid UTF-8: {}", path, e))?;

        // Capture the load-time mtime for staleness checks on later reads.
        let loaded_mtime = self.vfs.getattr(vfs_path).await.ok().map(|a| a.mtime);

        // Detect language from extension
        let language = detect_language(path);

        // Create CRDT document — handle race where another thread loaded it first
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
                .map_err(|e| format!("failed to insert block for {}: {}", path, e))?,
            Err(_) => {
                // Document already exists (concurrent load race). Re-check cache first,
                // then fall back to fetching the existing block from the store.
                {
                    let mut cache = self.cache.write();
                    if let Some(entry) = cache.get_mut(&ctx_id) {
                        entry.last_access = Instant::now();
                        return Ok((entry.context_id, entry.block_id));
                    }
                }
                // Not in cache yet — the other thread hasn't inserted. Grab from store.
                let snapshots = self
                    .block_store
                    .block_snapshots(ctx_id)
                    .map_err(|e| format!("failed to read existing doc {}: {}", path, e))?;
                snapshots
                    .first()
                    .map(|s| s.id)
                    .ok_or_else(|| format!("document {} exists but has no blocks", path))?
            }
        };

        // Insert into cache (evict if needed)
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
                    loaded_mtime,
                },
            );
        }

        Ok((ctx_id, block_id))
    }

    /// Replace a cached block's content with the current on-disk bytes. Used to
    /// pick up external edits when a clean entry's file mtime has advanced.
    /// Only emits a CRDT edit when the content actually differs.
    async fn reload_block_from_disk(
        &self,
        ctx_id: ContextId,
        block_id: &BlockId,
        path: &str,
    ) -> Result<(), String> {
        let vfs_path = std::path::Path::new(path);
        let bytes = self
            .vfs
            .read_all(vfs_path)
            .await
            .map_err(|e| format!("failed to reread {}: {}", path, e))?;
        let text = String::from_utf8(bytes)
            .map_err(|e| format!("{} is not valid UTF-8: {}", path, e))?;

        let old = self
            .block_store
            .block_snapshots(ctx_id)
            .ok()
            .and_then(|snaps| {
                snaps
                    .iter()
                    .find(|s| s.id == *block_id)
                    .map(|s| s.content.clone())
            })
            .unwrap_or_default();

        if old != text {
            // Char-indexed delete (CRDT text positions are chars, not bytes).
            self.block_store
                .edit_text(ctx_id, block_id, 0, &text, old.chars().count())
                .map_err(|e| e.to_string())?;
        }

        let mtime = self.vfs.getattr(vfs_path).await.ok().map(|a| a.mtime);
        let mut cache = self.cache.write();
        if let Some(entry) = cache.get_mut(&ctx_id) {
            entry.loaded_mtime = mtime;
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
    pub async fn read_content(&self, path: &str) -> Result<String, String> {
        let (ctx_id, block_id) = self.get_or_load(path).await?;

        let snapshots = self
            .block_store
            .block_snapshots(ctx_id)
            .map_err(|e| format!("failed to read {}: {}", path, e))?;

        snapshots
            .iter()
            .find(|s| s.id == block_id)
            .map(|s| s.content.clone())
            .ok_or_else(|| format!("block not found in document for {}", path))
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
        let mut succeeded: Vec<(ContextId, Option<SystemTime>)> = Vec::new();

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
                    let mtime = self.vfs.getattr(vfs_path).await.ok().map(|a| a.mtime);
                    succeeded.push((*ctx_id, mtime));
                }
                Err(e) => {
                    errors.push(format!("failed to flush {}: {}", path, e));
                }
            }
        }

        // Only clear dirty flags for files that were successfully flushed, and
        // stamp the post-flush mtime so they aren't seen as externally changed.
        {
            let mut cache = self.cache.write();
            for (ctx_id, mtime) in &succeeded {
                if let Some(entry) = cache.get_mut(ctx_id) {
                    entry.dirty = false;
                    entry.loaded_mtime = *mtime;
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

        // Stamp the post-flush mtime so our own write isn't later mistaken for
        // an external change and needlessly reloaded.
        let mtime = self.vfs.getattr(vfs_path).await.ok().map(|a| a.mtime);
        {
            let mut cache = self.cache.write();
            if let Some(entry) = cache.get_mut(&ctx_id) {
                entry.dirty = false;
                entry.loaded_mtime = mtime;
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
                    // Not yet on disk; the next flush stamps the real mtime.
                    loaded_mtime: None,
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
    use crate::vfs::{SetAttr, VfsOps};
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

        // External writer changes the file with a strictly-newer mtime.
        vfs.write_all(p("/tmp/f.txt"), b"v2").await.unwrap();
        let future = SystemTime::now() + std::time::Duration::from_secs(3600);
        vfs.setattr(p("/tmp/f.txt"), SetAttr::new().with_mtime(future))
            .await
            .unwrap();

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

        // External writer also changes the file with a newer mtime.
        vfs.write_all(p("/tmp/g.txt"), b"disk-v2").await.unwrap();
        let future = SystemTime::now() + std::time::Duration::from_secs(3600);
        vfs.setattr(p("/tmp/g.txt"), SetAttr::new().with_mtime(future))
            .await
            .unwrap();

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
}
