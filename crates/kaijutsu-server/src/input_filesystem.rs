//! Virtual filesystem for the input document (`/v/input`).
//!
//! Provides read/write access to the current context's input document
//! through kaish's VFS. Mounted at `/v/input` so agents and scripts can:
//!
//! - `cat /v/input` — read the current input text
//! - `echo "text" > /v/input` — replace the input content
//!
//! The input document is a CRDT-backed scratchpad per context, shared
//! across all participants (human compose box, agents, MCP tools).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use kaijutsu_kernel::block_store::SharedBlockStore;
use kaijutsu_types::ContextId;
use kaish_kernel::vfs::{DirEntry, DirEntryKind, Filesystem};

/// Shared mutable context ID (same type as kaish_backend::SharedContextId).
type SharedContextId = Arc<RwLock<ContextId>>;

/// Virtual filesystem exposing the input document at `/v/input`.
///
/// Only a single virtual file exists: the root path itself represents
/// the input document text for the current context. Subdirectories
/// and other paths return NotFound.
pub struct InputFilesystem {
    blocks: SharedBlockStore,
    context_id: SharedContextId,
}

impl InputFilesystem {
    /// Create a new input filesystem.
    pub fn new(blocks: SharedBlockStore, context_id: SharedContextId) -> Self {
        Self { blocks, context_id }
    }

    /// Read the current context ID.
    fn current_context(&self) -> ContextId {
        *self.context_id.read().expect("context_id lock poisoned")
    }
}

#[async_trait]
impl Filesystem for InputFilesystem {
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let path_str = path.to_string_lossy();
        let normalized = path_str.trim_start_matches('/').trim_end_matches('/');

        if !normalized.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("/v/input has no subpaths (got: {})", path_str),
            ));
        }

        let ctx = self.current_context();

        // Ensure the input doc exists (idempotent)
        self.blocks
            .create_input_doc(ctx)
            .map_err(io::Error::other)?;

        match self.blocks.get_input_text(ctx) {
            Ok(text) => Ok(text.into_bytes()),
            Err(e) => Err(io::Error::other(e)),
        }
    }

    async fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let path_str = path.to_string_lossy();
        let normalized = path_str.trim_start_matches('/').trim_end_matches('/');

        if !normalized.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("/v/input has no subpaths (got: {})", path_str),
            ));
        }

        let ctx = self.current_context();
        let new_text = String::from_utf8(data.to_vec()).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("invalid UTF-8: {}", e))
        })?;

        // Ensure the input doc exists (idempotent)
        self.blocks
            .create_input_doc(ctx)
            .map_err(io::Error::other)?;

        // Clear existing content first, then write new text
        let _ = self.blocks.clear_input(ctx);

        if !new_text.is_empty() {
            self.blocks
                .edit_input(ctx, 0, &new_text, 0)
                .map_err(io::Error::other)?;
        }

        Ok(())
    }

    async fn list(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let path_str = path.to_string_lossy();
        let normalized = path_str.trim_start_matches('/').trim_end_matches('/');

        if normalized.is_empty() {
            // The root of /v/input is the file itself — not a directory
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "/v/input is a file, not a directory",
            ));
        }

        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("/v/input has no subpaths (got: {})", path_str),
        ))
    }

    async fn stat(&self, path: &Path) -> io::Result<DirEntry> {
        let path_str = path.to_string_lossy();
        let normalized = path_str.trim_start_matches('/').trim_end_matches('/');

        if !normalized.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("/v/input has no subpaths (got: {})", path_str),
            ));
        }

        let ctx = self.current_context();

        // Get text length for size, or 0 if no input doc
        let size = self
            .blocks
            .get_input_text(ctx)
            .map(|t| t.len() as u64)
            .unwrap_or(0);

        Ok(DirEntry {
            name: "input".to_string(),
            kind: DirEntryKind::File,
            size,
            modified: None,
            permissions: Some(0o644),
            symlink_target: None,
        })
    }

    async fn mkdir(&self, _path: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "/v/input does not support directories",
        ))
    }

    async fn remove(&self, path: &Path) -> io::Result<()> {
        let path_str = path.to_string_lossy();
        let normalized = path_str.trim_start_matches('/').trim_end_matches('/');

        if !normalized.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("/v/input has no subpaths (got: {})", path_str),
            ));
        }

        // "Removing" the input file clears its content
        let ctx = self.current_context();
        self.blocks.clear_input(ctx).map_err(io::Error::other)?;
        Ok(())
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn exists(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();
        let normalized = path_str.trim_start_matches('/').trim_end_matches('/');
        // The root file always "exists"
        normalized.is_empty()
    }

    fn real_path(&self, _path: &Path) -> Option<PathBuf> {
        None // Virtual file, no real path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_kernel::block_store::shared_block_store;
    use kaijutsu_types::PrincipalId;

    fn test_fs() -> (InputFilesystem, ContextId) {
        let ctx = ContextId::new();
        let blocks = shared_block_store(PrincipalId::system());
        let shared_ctx = Arc::new(RwLock::new(ctx));
        (InputFilesystem::new(blocks, shared_ctx), ctx)
    }

    #[tokio::test]
    async fn test_read_empty() {
        let (fs, _ctx) = test_fs();
        let data = fs.read(Path::new("")).await.unwrap();
        assert_eq!(String::from_utf8(data).unwrap(), "");
    }

    #[tokio::test]
    async fn test_write_and_read() {
        let (fs, _ctx) = test_fs();
        fs.write(Path::new(""), b"hello world").await.unwrap();
        let data = fs.read(Path::new("")).await.unwrap();
        assert_eq!(String::from_utf8(data).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_overwrite() {
        let (fs, _ctx) = test_fs();
        fs.write(Path::new(""), b"first").await.unwrap();
        fs.write(Path::new(""), b"second").await.unwrap();
        let data = fs.read(Path::new("")).await.unwrap();
        assert_eq!(String::from_utf8(data).unwrap(), "second");
    }

    #[tokio::test]
    async fn test_remove_clears() {
        let (fs, _ctx) = test_fs();
        fs.write(Path::new(""), b"content").await.unwrap();
        fs.remove(Path::new("")).await.unwrap();
        let data = fs.read(Path::new("")).await.unwrap();
        assert_eq!(String::from_utf8(data).unwrap(), "");
    }

    #[tokio::test]
    async fn test_stat() {
        let (fs, _ctx) = test_fs();
        fs.write(Path::new(""), b"hello").await.unwrap();
        let entry = fs.stat(Path::new("")).await.unwrap();
        assert_eq!(entry.name, "input");
        assert_eq!(entry.kind, DirEntryKind::File);
        assert_eq!(entry.size, 5);
    }

    #[tokio::test]
    async fn test_subpath_not_found() {
        let (fs, _ctx) = test_fs();
        assert!(fs.read(Path::new("subpath")).await.is_err());
        assert!(fs.write(Path::new("subpath"), b"data").await.is_err());
        assert!(!fs.exists(Path::new("subpath")).await);
    }

    #[tokio::test]
    async fn test_list_is_not_directory() {
        let (fs, _ctx) = test_fs();
        assert!(fs.list(Path::new("")).await.is_err());
    }
}
