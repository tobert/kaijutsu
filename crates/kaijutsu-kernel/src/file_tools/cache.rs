//! CRDT-backed file document cache.
//!
//! Maps VFS files into CRDT documents, enabling concurrent editing
//! with the same operational semantics as block editing. Files are
//! loaded on demand and cached with LRU eviction.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use kaijutsu_crdt::{BlockId, BlockKind, ContextId, Role};
use parking_lot::RwLock;

use crate::block_store::SharedBlockStore;
use crate::db::DocumentKind;
use crate::vfs::{MountTable, VfsOps};

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
}

/// Cache that maps VFS files to CRDT documents.
///
/// Each file becomes a document with `DocumentKind::Code` and a single
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

        // Fast path: already cached
        {
            let mut cache = self.cache.write();
            if let Some(entry) = cache.get_mut(&ctx_id) {
                entry.last_access = Instant::now();
                return Ok((entry.context_id, entry.block_id.clone()));
            }
        }

        // Cache miss: load from VFS
        let vfs_path = std::path::Path::new(path);
        let content = self
            .vfs
            .read_all(vfs_path)
            .await
            .map_err(|e| format!("failed to read {}: {}", path, e))?;

        let text = String::from_utf8(content)
            .map_err(|e| format!("{} is not valid UTF-8: {}", path, e))?;

        // Detect language from extension
        let language = detect_language(path);

        // Create CRDT document — handle race where another thread loaded it first
        let block_id = match self
            .block_store
            .create_document(ctx_id, DocumentKind::Code, language)
        {
            Ok(()) => {
                self.block_store
                    .insert_block(ctx_id, None, None, Role::System, BlockKind::Text, text)
                    .map_err(|e| format!("failed to insert block for {}: {}", path, e))?
            }
            Err(_) => {
                // Document already exists (concurrent load race). Re-check cache first,
                // then fall back to fetching the existing block from the store.
                {
                    let mut cache = self.cache.write();
                    if let Some(entry) = cache.get_mut(&ctx_id) {
                        entry.last_access = Instant::now();
                        return Ok((entry.context_id, entry.block_id.clone()));
                    }
                }
                // Not in cache yet — the other thread hasn't inserted. Grab from store.
                let snapshots = self
                    .block_store
                    .block_snapshots(ctx_id)
                    .map_err(|e| format!("failed to read existing doc {}: {}", path, e))?;
                snapshots
                    .first()
                    .map(|s| s.id.clone())
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
                    block_id: block_id.clone(),
                    dirty: false,
                    last_access: Instant::now(),
                },
            );
        }

        Ok((ctx_id, block_id))
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

                self.block_store.edit_text(
                    ctx_id,
                    &entry.block_id,
                    0,
                    content,
                    old_content.len(),
                )?;

                return Ok((entry.context_id, entry.block_id.clone()));
            }
        }

        // New file: create doc + block
        self.get_or_load_with_content(path, content).await
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
                .map(|e| (e.path.clone(), e.context_id, e.block_id.clone()))
                .collect()
        };

        let mut flushed = 0;
        let mut errors: Vec<String> = Vec::new();
        let mut succeeded_ctx_ids: Vec<ContextId> = Vec::new();

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

            match self.vfs
                .write_all(std::path::Path::new(path), content.as_bytes())
                .await
            {
                Ok(()) => {
                    flushed += 1;
                    succeeded_ctx_ids.push(*ctx_id);
                }
                Err(e) => {
                    errors.push(format!("failed to flush {}: {}", path, e));
                }
            }
        }

        // Only clear dirty flags for files that were successfully flushed
        {
            let mut cache = self.cache.write();
            for ctx_id in &succeeded_ctx_ids {
                if let Some(entry) = cache.get_mut(ctx_id) {
                    entry.dirty = false;
                }
            }
        }

        if errors.is_empty() {
            Ok(flushed)
        } else {
            Err(format!("flush_dirty: {}/{} failed: {}", errors.len(), dirty_entries.len(), errors.join("; ")))
        }
    }

    /// Flush a single file back to the VFS.
    pub async fn flush_one(&self, path: &str) -> Result<(), String> {
        let ctx_id = file_context_id(path);
        let block_id = {
            let cache = self.cache.read();
            match cache.get(&ctx_id) {
                Some(entry) if entry.dirty => entry.block_id.clone(),
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

        self.vfs
            .write_all(std::path::Path::new(path), content.as_bytes())
            .await
            .map_err(|e| format!("failed to flush {}: {}", path, e))?;

        {
            let mut cache = self.cache.write();
            if let Some(entry) = cache.get_mut(&ctx_id) {
                entry.dirty = false;
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

        self.block_store
            .create_document(ctx_id, DocumentKind::Code, language)
            .map_err(|e| format!("failed to create document for {}: {}", path, e))?;

        let block_id = self
            .block_store
            .insert_block(ctx_id, None, None, Role::System, BlockKind::Text, content)
            .map_err(|e| format!("failed to insert block for {}: {}", path, e))?;

        {
            let mut cache = self.cache.write();
            self.evict_if_needed(&mut cache);
            cache.insert(
                ctx_id,
                CachedFileDoc {
                    context_id: ctx_id,
                    path: path.to_string(),
                    block_id: block_id.clone(),
                    dirty: false,
                    last_access: Instant::now(),
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
                .map(|(k, _)| k.clone());

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
        "ron" => "ron",
        "md" => "markdown",
        "html" => "html",
        "css" => "css",
        "sql" => "sql",
        "rhai" => "rhai",
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
        assert_ne!(id1, id3, "different paths should produce different ContextIds");
    }

    #[test]
    fn test_detect_language() {
        assert_eq!(detect_language("main.rs"), Some("rust".to_string()));
        assert_eq!(detect_language("app.py"), Some("python".to_string()));
        assert_eq!(detect_language("Cargo.toml"), Some("toml".to_string()));
        assert_eq!(detect_language("noext"), None);
        assert_eq!(detect_language("script.sh"), Some("bash".to_string()));
    }
}
