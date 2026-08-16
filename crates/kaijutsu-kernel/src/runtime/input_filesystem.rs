//! Virtual filesystem for compose input (`/v/input`).
//!
//! Provides read/write access to the current context's compose draft
//! through kaish's VFS. Mounted at `/v/input` so agents and scripts can:
//!
//! - `cat /v/input` — read the current input text
//! - `echo "text" > /v/input` — replace the input content
//!
//! As of the draft-block melt, there is no longer one shared input document
//! per context. There is **one draft block per (context, principal)** — an
//! ordinary block carrying `Status::Draft` + `ephemeral`. `/v/input` reads
//! and writes the calling principal's own draft, so two players sharing a
//! context each see and edit their own text through this mount, not a
//! scratchpad shared across every participant the way it used to be.

use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::block_store::SharedBlockStore;
use super::context_engine::{SessionContextExt, SessionContextMap};
use kaijutsu_types::{ContextId, PrincipalId, SessionId};
use kaish_kernel::vfs::{DirEntry, DirEntryKind, Filesystem};

/// Virtual filesystem exposing the calling principal's compose draft at
/// `/v/input`.
///
/// Only a single virtual file exists: the root path itself represents the
/// draft text for the current context and principal. Subdirectories and
/// other paths return NotFound.
pub struct InputFilesystem {
    blocks: SharedBlockStore,
    session_contexts: SessionContextMap,
    session_id: SessionId,
    principal_id: PrincipalId,
}

impl InputFilesystem {
    /// Create a new input filesystem.
    pub fn new(
        blocks: SharedBlockStore,
        session_contexts: SessionContextMap,
        session_id: SessionId,
        principal_id: PrincipalId,
    ) -> Self {
        Self {
            blocks,
            session_contexts,
            session_id,
            principal_id,
        }
    }

    /// Read the current context ID.
    fn current_context(&self) -> io::Result<ContextId> {
        self.session_contexts
            .current(&self.session_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no active context joined"))
    }
}

#[async_trait]
impl Filesystem for InputFilesystem {
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let path_str = path.to_string_lossy();
        let normalized = path_str.trim_start_matches('/').trim_end_matches('/');

        if !normalized.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "not found"));
        }

        let ctx = self.current_context()?;

        // Deliberately does NOT call get_or_create_draft: a read must not
        // mint a block as a side effect (the old input document did, via
        // create_input_doc). No draft yet reads as empty.
        match self.blocks.draft_block(ctx, self.principal_id) {
            Ok(draft) => Ok(draft.map(|d| d.content).unwrap_or_default().into_bytes()),
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

        let ctx = self.current_context()?;
        let new_text = String::from_utf8(data.to_vec()).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("invalid UTF-8: {}", e))
        })?;

        // Default behavior for write is overwrite: discard any existing
        // draft, then re-create it from scratch if the new text is
        // non-empty. clear_draft is a no-op (returns "") when there is no
        // draft yet.
        self.blocks
            .clear_draft(ctx, self.principal_id)
            .map_err(io::Error::other)?;
        if !new_text.is_empty() {
            self.blocks
                .edit_draft(ctx, self.principal_id, 0, &new_text, 0)
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

        let ctx = self.current_context()?;

        // Get text length for size, or 0 if no draft yet. Does not create
        // one — same side-effect-free contract as read().
        let size = self
            .blocks
            .draft_block(ctx, self.principal_id)
            .ok()
            .flatten()
            .map(|d| d.content.len() as u64)
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
        let ctx = self.current_context()?;
        self.blocks
            .clear_draft(ctx, self.principal_id)
            .map_err(io::Error::other)?;
        Ok(())
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn exists(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();
        let normalized = path_str.trim_start_matches('/').trim_end_matches('/');
        // The root file only "exists" if we have an active context
        if !normalized.is_empty() {
            return false;
        }
        self.current_context().is_ok()
    }

    fn real_path(&self, _path: &Path) -> Option<PathBuf> {
        None // Virtual file, no real path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::{shared_block_store, SharedBlockStore};
    use kaijutsu_types::{DocKind, PrincipalId};

    /// One context, one document, one principal, one filesystem — the common
    /// case every test but the multi-principal one needs.
    fn test_fs() -> (InputFilesystem, ContextId) {
        let blocks = shared_block_store(PrincipalId::system());
        let ctx = ContextId::new();
        blocks
            .create_document(ctx, DocKind::Conversation, None)
            .unwrap();
        let principal = PrincipalId::system();
        let fs = test_fs_for(blocks, ctx, principal);
        (fs, ctx)
    }

    /// Build an `InputFilesystem` for an existing context/store, under its
    /// own session id, as the given principal — the building block the
    /// multi-principal test uses to give two players independent sessions
    /// pointed at the same context.
    fn test_fs_for(
        blocks: SharedBlockStore,
        ctx: ContextId,
        principal: PrincipalId,
    ) -> InputFilesystem {
        let sid = SessionId::new();
        let session_contexts = crate::runtime::context_engine::session_context_map();
        session_contexts.insert(sid, ctx);
        InputFilesystem::new(blocks, session_contexts, sid, principal)
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

    /// The reason Amy chose re-point over delete: two principals sharing a
    /// context now get *independent* `/v/input` content, one draft block
    /// each. This could not have been true of the old context-wide input
    /// document — assert it explicitly.
    #[tokio::test]
    async fn test_two_principals_have_independent_drafts() {
        let blocks = shared_block_store(PrincipalId::system());
        let ctx = ContextId::new();
        blocks
            .create_document(ctx, DocKind::Conversation, None)
            .unwrap();

        let alice = PrincipalId::for_agent_session("alice");
        let bob = PrincipalId::for_agent_session("bob");
        let fs_alice = test_fs_for(blocks.clone(), ctx, alice);
        let fs_bob = test_fs_for(blocks.clone(), ctx, bob);

        fs_alice
            .write(Path::new(""), b"alice's draft")
            .await
            .unwrap();
        fs_bob.write(Path::new(""), b"bob's draft").await.unwrap();

        let alice_data = fs_alice.read(Path::new("")).await.unwrap();
        let bob_data = fs_bob.read(Path::new("")).await.unwrap();
        assert_eq!(String::from_utf8(alice_data).unwrap(), "alice's draft");
        assert_eq!(String::from_utf8(bob_data).unwrap(), "bob's draft");
    }

    /// Reading `/v/input` before anyone has typed anything must not mint a
    /// draft block — a deliberate improvement over the old input document,
    /// which created its backing doc as a side effect of read. Assert the
    /// block count in the context is unchanged across the read.
    #[tokio::test]
    async fn test_read_with_no_draft_creates_no_block() {
        let (fs, ctx) = test_fs();
        let before = fs.blocks.block_snapshots(ctx).unwrap().len();

        let data = fs.read(Path::new("")).await.unwrap();
        assert_eq!(String::from_utf8(data).unwrap(), "");

        let after = fs.blocks.block_snapshots(ctx).unwrap().len();
        assert_eq!(
            before, after,
            "reading /v/input with no draft must not create a block"
        );
    }

    /// The draft path is char-indexed (`edit_text_as`); the old input path
    /// byte-bounds-checked. Multibyte text must round-trip without a
    /// byte/char mismatch panicking or corrupting content.
    #[tokio::test]
    async fn test_multibyte_roundtrip() {
        let (fs, _ctx) = test_fs();
        let text = "日本語 🎵 café";
        fs.write(Path::new(""), text.as_bytes()).await.unwrap();
        let data = fs.read(Path::new("")).await.unwrap();
        assert_eq!(String::from_utf8(data).unwrap(), text);
    }
}
