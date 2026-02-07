//! CRDT-backed file document cache.
//!
//! Maps VFS files into CRDT documents, enabling concurrent editing
//! with the same operational semantics as block editing. Files are
//! loaded on demand and cached with LRU eviction.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use kaijutsu_crdt::{BlockId, BlockKind, Role};
use parking_lot::RwLock;

use crate::block_store::SharedBlockStore;
use crate::db::DocumentKind;
use crate::vfs::{MountTable, VfsOps};

/// Default maximum cached file documents.
const DEFAULT_MAX_CACHED: usize = 64;

/// A cached file backed by a CRDT document.
struct CachedFileDoc {
    /// Document ID in the BlockStore (format: "file:{path}").
    document_id: String,
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
    cache: RwLock<HashMap<String, CachedFileDoc>>,
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

    /// Get the document_id and block_id for a path, loading from VFS on cache miss.
    pub async fn get_or_load(&self, path: &str) -> Result<(String, BlockId), String> {
        let doc_id = file_doc_id(path);

        // Fast path: already cached
        {
            let mut cache = self.cache.write();
            if let Some(entry) = cache.get_mut(&doc_id) {
                entry.last_access = Instant::now();
                return Ok((entry.document_id.clone(), entry.block_id.clone()));
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

        // Create CRDT document
        self.block_store
            .create_document(doc_id.clone(), DocumentKind::Code, language)
            .map_err(|e| format!("failed to create document for {}: {}", path, e))?;

        let block_id = self
            .block_store
            .insert_block(&doc_id, None, None, Role::System, BlockKind::Text, text)
            .map_err(|e| format!("failed to insert block for {}: {}", path, e))?;

        // Insert into cache (evict if needed)
        {
            let mut cache = self.cache.write();
            self.evict_if_needed(&mut cache);
            cache.insert(
                doc_id.clone(),
                CachedFileDoc {
                    document_id: doc_id.clone(),
                    block_id: block_id.clone(),
                    dirty: false,
                    last_access: Instant::now(),
                },
            );
        }

        Ok((doc_id, block_id))
    }

    /// Read the current content of a file (reflects any CRDT edits).
    pub async fn read_content(&self, path: &str) -> Result<String, String> {
        let (doc_id, block_id) = self.get_or_load(path).await?;

        let snapshots = self
            .block_store
            .block_snapshots(&doc_id)
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
    ) -> Result<(String, BlockId), String> {
        let doc_id = file_doc_id(path);

        // If doc exists, replace its content with a full splice
        {
            let cache = self.cache.read();
            if let Some(entry) = cache.get(&doc_id) {
                let old_content = self
                    .block_store
                    .block_snapshots(&doc_id)
                    .ok()
                    .and_then(|snaps| {
                        snaps
                            .iter()
                            .find(|s| s.id == entry.block_id)
                            .map(|s| s.content.clone())
                    })
                    .unwrap_or_default();

                self.block_store.edit_text(
                    &doc_id,
                    &entry.block_id,
                    0,
                    content,
                    old_content.len(),
                )?;

                return Ok((entry.document_id.clone(), entry.block_id.clone()));
            }
        }

        // New file: create doc + block
        self.get_or_load_with_content(path, content).await
    }

    /// Mark a file as dirty (needs flush to VFS).
    pub fn mark_dirty(&self, path: &str) {
        let doc_id = file_doc_id(path);
        let mut cache = self.cache.write();
        if let Some(entry) = cache.get_mut(&doc_id) {
            entry.dirty = true;
        }
    }

    /// Flush all dirty files back to the VFS.
    pub async fn flush_dirty(&self) -> Result<usize, String> {
        let dirty_paths: Vec<(String, String, BlockId)> = {
            let cache = self.cache.read();
            cache
                .values()
                .filter(|e| e.dirty)
                .map(|e| {
                    let path = e.document_id.strip_prefix("file:").unwrap_or(&e.document_id);
                    (path.to_string(), e.document_id.clone(), e.block_id.clone())
                })
                .collect()
        };

        let mut flushed = 0;
        for (path, doc_id, block_id) in &dirty_paths {
            let content = self
                .block_store
                .block_snapshots(doc_id)
                .ok()
                .and_then(|snaps| {
                    snaps
                        .iter()
                        .find(|s| s.id == *block_id)
                        .map(|s| s.content.clone())
                })
                .unwrap_or_default();

            self.vfs
                .write_all(std::path::Path::new(path), content.as_bytes())
                .await
                .map_err(|e| format!("failed to flush {}: {}", path, e))?;

            flushed += 1;
        }

        // Clear dirty flags
        {
            let mut cache = self.cache.write();
            for (_, doc_id, _) in &dirty_paths {
                if let Some(entry) = cache.get_mut(doc_id) {
                    entry.dirty = false;
                }
            }
        }

        Ok(flushed)
    }

    /// Flush a single file back to the VFS.
    pub async fn flush_one(&self, path: &str) -> Result<(), String> {
        let doc_id = file_doc_id(path);
        let block_id = {
            let cache = self.cache.read();
            match cache.get(&doc_id) {
                Some(entry) if entry.dirty => entry.block_id.clone(),
                Some(_) => return Ok(()), // not dirty
                None => return Ok(()),    // not cached
            }
        };

        let content = self
            .block_store
            .block_snapshots(&doc_id)
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
            if let Some(entry) = cache.get_mut(&doc_id) {
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
    ) -> Result<(String, BlockId), String> {
        let doc_id = file_doc_id(path);
        let language = detect_language(path);

        self.block_store
            .create_document(doc_id.clone(), DocumentKind::Code, language)
            .map_err(|e| format!("failed to create document for {}: {}", path, e))?;

        let block_id = self
            .block_store
            .insert_block(&doc_id, None, None, Role::System, BlockKind::Text, content)
            .map_err(|e| format!("failed to insert block for {}: {}", path, e))?;

        {
            let mut cache = self.cache.write();
            self.evict_if_needed(&mut cache);
            cache.insert(
                doc_id.clone(),
                CachedFileDoc {
                    document_id: doc_id.clone(),
                    block_id: block_id.clone(),
                    dirty: false,
                    last_access: Instant::now(),
                },
            );
        }

        Ok((doc_id, block_id))
    }

    /// Evict oldest entries if cache exceeds max size.
    fn evict_if_needed(&self, cache: &mut HashMap<String, CachedFileDoc>) {
        while cache.len() >= self.max_cached {
            let oldest = cache
                .iter()
                .min_by_key(|(_, e)| e.last_access)
                .map(|(k, _)| k.clone());

            if let Some(key) = oldest {
                if cache.get(&key).is_some_and(|e| e.dirty) {
                    tracing::warn!(
                        doc_id = %key,
                        "Evicting dirty file document — unflushed changes lost"
                    );
                }
                cache.remove(&key);
            } else {
                break;
            }
        }
    }
}

/// Build the document ID for a file path.
fn file_doc_id(path: &str) -> String {
    format!("file:{}", path)
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
        "h" | "hpp" => "c",
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
    fn test_file_doc_id() {
        assert_eq!(file_doc_id("src/main.rs"), "file:src/main.rs");
        assert_eq!(file_doc_id("/mnt/project/lib.rs"), "file:/mnt/project/lib.rs");
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
