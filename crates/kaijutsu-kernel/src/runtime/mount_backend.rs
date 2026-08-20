//! MountBackend: Routes file ops through kaijutsu's MountTable.
//!
//! Delegates filesystem operations to the kaijutsu kernel's `MountTable`
//! (which routes to `LocalBackend` for real files) and tool dispatch to
//! the document backends.
//!
//! # Architecture
//!
//! ```text
//! MountBackend (implements kaish KernelBackend)
//! ├── File ops → MountTable → LocalBackend → real filesystem
//! └── Tool calls → docs_tools → ToolNotFound
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use kaish_kernel::backend::ConflictError;
use kaish_kernel::tools::{ToolArgs, ToolCtx};
use kaish_kernel::vfs::{DirEntry, DirEntryKind};
use kaish_kernel::{
    BackendError, BackendResult, KernelBackend, PatchOp, ReadRange, ToolInfo, ToolResult, WriteMode,
};

use crate::file_tools::path::resolve_str;
use crate::file_tools::{CacheReadError, FileDocumentCache};
use crate::vfs::{FileType, MountTable, SetAttr, VfsError, VfsOps};

use super::kaish_backend::KaijutsuBackend;

/// Routes file *content* operations through the shared
/// `FileDocumentCache` and directory/metadata/tool operations through
/// kaijutsu's `MountTable`.
///
/// This is what makes kaish "shell scripting on the same documents":
/// a `cat`/`read`/`write`/`edit` from the shell hits the same kernel document
/// the MCP `builtin.file` tools use, keyed by the canonical absolute path.
/// Binary files (not representable as document text) fall through to the raw
/// `MountTable`; that fallthrough is a deliberate type distinction, not a
/// silent error-swallow.
pub struct MountBackend {
    /// Kaijutsu kernel's VFS mount table — directory/metadata ops and the
    /// binary-file fallback path.
    mount_table: Arc<MountTable>,
    /// Shared file-document cache — one cached view of text file content
    /// across both the kaish and MCP surfaces. Disk is the source of truth;
    /// the cache reconciles against it. See `docs/file-buffers.md`.
    file_cache: Arc<FileDocumentCache>,
    /// document backend for document tool dispatch.
    docs_tools: Arc<KaijutsuBackend>,
    /// When true, every mutating op is refused structurally with
    /// `PermissionDenied` *before* it can reach the shared mount table or the
    /// document cache — the read-only invariant for the toolie's `read_only_shell`.
    /// Reads (real files and kernel documents) still pass through. This gates the
    /// real-FS + `FileDocumentCache` surface; the kaish-VFS `/v/docs` and
    /// `/v/input` mounts are gated separately by wrapping them in
    /// [`super::read_only_fs::ReadOnlyFs`] (they don't route through here).
    read_only: bool,
}

impl MountBackend {
    /// Create a new writable MountBackend.
    pub fn new(
        mount_table: Arc<MountTable>,
        docs_tools: Arc<KaijutsuBackend>,
        file_cache: Arc<FileDocumentCache>,
    ) -> Self {
        Self {
            mount_table,
            file_cache,
            docs_tools,
            read_only: false,
        }
    }

    /// Create a read-only MountBackend: reads pass through, every mutation is
    /// refused at this boundary regardless of whether the underlying mount is
    /// writable. Used to materialize the toolie's `read_only_shell` over the
    /// *shared* mount table without exposing a write path.
    pub fn new_read_only(
        mount_table: Arc<MountTable>,
        docs_tools: Arc<KaijutsuBackend>,
        file_cache: Arc<FileDocumentCache>,
    ) -> Self {
        Self {
            mount_table,
            file_cache,
            docs_tools,
            read_only: true,
        }
    }

    /// The single read-only gate every mutating op consults. Returns
    /// `Err(PermissionDenied)` when this backend is read-only, `Ok(())`
    /// otherwise — so the op refuses by construction rather than relying on the
    /// underlying mount's own (possibly writable) policy.
    fn deny_if_read_only(&self, op: &str, path: &Path) -> BackendResult<()> {
        if self.read_only {
            return Err(BackendError::PermissionDenied(format!(
                "{op} {}: read-only shell (no writes)",
                path.display()
            )));
        }
        Ok(())
    }

    /// Canonicalize an (already absolute) backend path into the cache key form,
    /// so the kaish surface and the MCP surface address one kernel document per
    /// real file. Rejects `..`-escapes above root (untrusted input → refuse,
    /// never silently clamp).
    fn cache_key(&self, path: &Path) -> BackendResult<String> {
        resolve_str(Path::new("/"), &path.to_string_lossy())
            .map_err(|e| BackendError::PermissionDenied(e.to_string()))
    }

    /// Write straight to the VFS, honoring `WriteMode`, without touching the
    /// document cache. Used for read-only/OS mounts (so the VFS rejects cleanly)
    /// and for binary content on writable mounts.
    async fn raw_write(&self, path: &Path, content: &[u8], mode: WriteMode) -> BackendResult<()> {
        match mode {
            WriteMode::CreateNew => {
                if self.mount_table.exists(path).await {
                    return Err(BackendError::AlreadyExists(path.display().to_string()));
                }
                self.mount_table
                    .create(path, 0o644)
                    .await
                    .map_err(vfs_to_backend)?;
                self.mount_table
                    .write(path, 0, content)
                    .await
                    .map_err(vfs_to_backend)?;
                Ok(())
            }
            WriteMode::UpdateOnly => {
                if !self.mount_table.exists(path).await {
                    return Err(BackendError::NotFound(path.display().to_string()));
                }
                self.mount_table
                    .write_all(path, content)
                    .await
                    .map_err(vfs_to_backend)
            }
            WriteMode::Overwrite | WriteMode::Truncate => self
                .mount_table
                .write_all(path, content)
                .await
                .map_err(vfs_to_backend),
            _ => Err(BackendError::InvalidOperation(
                "unsupported write mode".into(),
            )),
        }
    }

    /// Append straight to the VFS without touching the cache.
    async fn raw_append(&self, path: &Path, content: &[u8]) -> BackendResult<()> {
        let attr = self.mount_table.getattr(path).await.map_err(vfs_to_backend)?;
        self.mount_table
            .write(path, attr.size, content)
            .await
            .map_err(vfs_to_backend)?;
        Ok(())
    }
}

/// Apply a `ReadRange` to a byte buffer (line- or offset-based windowing).
fn apply_range(data: Vec<u8>, range: Option<ReadRange>) -> Vec<u8> {
    match range {
        None => data,
        Some(range) => {
            if let (Some(start), Some(end)) = (range.start_line, range.end_line) {
                let text = String::from_utf8_lossy(&data);
                let lines: Vec<&str> = text.lines().collect();
                let start = (start.saturating_sub(1)).min(lines.len());
                let end = end.min(lines.len());
                let selected: Vec<&str> = lines[start..end].to_vec();
                selected.join("\n").into_bytes()
            } else if let Some(off) = range.offset {
                let off = off as usize;
                if off >= data.len() {
                    Vec::new()
                } else if let Some(lim) = range.limit {
                    let end = (off + lim as usize).min(data.len());
                    data[off..end].to_vec()
                } else {
                    data[off..].to_vec()
                }
            } else {
                data
            }
        }
    }
}

/// Convert a `VfsError` to a `BackendError`.
fn vfs_to_backend(err: VfsError) -> BackendError {
    match err {
        VfsError::NotFound(msg) => BackendError::NotFound(msg),
        VfsError::AlreadyExists(msg) => BackendError::AlreadyExists(msg),
        VfsError::PermissionDenied(msg) => BackendError::PermissionDenied(msg),
        VfsError::ReadOnly => BackendError::ReadOnly,
        VfsError::NotADirectory(msg) => BackendError::NotDirectory(msg),
        VfsError::IsADirectory(msg) => BackendError::IsDirectory(msg),
        VfsError::DirectoryNotEmpty(msg) => {
            BackendError::Io(format!("directory not empty: {}", msg))
        }
        VfsError::PathEscapesRoot(msg) => BackendError::PermissionDenied(msg),
        VfsError::InvalidPath(msg) => BackendError::InvalidOperation(msg),
        VfsError::NoMountPoint(msg) => BackendError::NotFound(msg),
        VfsError::CrossDeviceLink => BackendError::InvalidOperation("cross-device link".into()),
        VfsError::Io(e) => BackendError::Io(e.to_string()),
        other => BackendError::Io(other.to_string()),
    }
}

/// Convert a kaijutsu `FileAttr` to a kaish `DirEntry`.
fn file_attr_to_dir_entry(name: &str, attr: &crate::vfs::FileAttr) -> DirEntry {
    let kind = match attr.kind {
        FileType::File => DirEntryKind::File,
        FileType::Directory => DirEntryKind::Directory,
        FileType::Symlink => DirEntryKind::Symlink,
    };
    DirEntry {
        name: name.to_string(),
        kind,
        size: attr.size,
        modified: Some(attr.mtime),
        permissions: Some(attr.perm),
        symlink_target: None,
    }
}

/// Convert a kaijutsu `DirEntry` to a kaish `DirEntry`.
fn kj_dir_entry_to_kaish(entry: &crate::vfs::DirEntry) -> DirEntry {
    let kind = match entry.kind {
        FileType::File => DirEntryKind::File,
        FileType::Directory => DirEntryKind::Directory,
        FileType::Symlink => DirEntryKind::Symlink,
    };
    DirEntry {
        name: entry.name.clone(),
        kind,
        size: 0,
        modified: None,
        permissions: None,
        symlink_target: None,
    }
}

/// Extract the filename from a path, defaulting to the full path string.
fn path_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

#[async_trait]
impl KernelBackend for MountBackend {
    // =========================================================================
    // File Operations
    // =========================================================================

    async fn read(&self, path: &Path, range: Option<ReadRange>) -> BackendResult<Vec<u8>> {
        // Document-back only writable mounts. Reading a read-only/OS path shouldn't
        // mint a kernel document — pass it straight through the VFS.
        if self.mount_table.is_writable(path).await {
            let key = self.cache_key(path)?;
            match self.file_cache.try_read_content(&key).await {
                Ok(text) => return Ok(apply_range(text.into_bytes(), range)),
                Err(CacheReadError::NotCached) => {
                    // Missing or binary: fall through to a raw read so `cat`
                    // on a binary or absent file still works as expected.
                }
                Err(CacheReadError::Backend(e)) => {
                    // A real store error: refuse to serve stale disk bytes
                    // in its place — that would be silent data corruption.
                    return Err(BackendError::Io(e));
                }
            }
        }
        if !self.mount_table.exists(path).await {
            return Err(BackendError::NotFound(path.display().to_string()));
        }
        let data = self
            .mount_table
            .read_all(path)
            .await
            .map_err(vfs_to_backend)?;
        Ok(apply_range(data, range))
    }

    async fn write(&self, path: &Path, content: &[u8], mode: WriteMode) -> BackendResult<()> {
        self.deny_if_read_only("write", path)?;
        // Read-only / OS mounts never touch the cache: let the VFS reject the
        // write cleanly rather than poison the cache with an un-flushable edit.
        if !self.mount_table.is_writable(path).await {
            return self.raw_write(path, content, mode).await;
        }

        let key = self.cache_key(path)?;

        // Binary content can't live in a document's text: write raw and
        // drop any cached text doc so a later read reloads fresh.
        let text = match std::str::from_utf8(content) {
            Ok(t) => t,
            Err(_) => {
                self.raw_write(path, content, mode).await?;
                // Best-effort: the disk write already landed, so a refusal
                // here (an open editor session pins this path's entry, C —
                // docs/audits/2026-08-20-editor-fileio.md) must not fail the
                // whole write. It leaves a stale text shadow behind a live
                // pinned session, which is loud (not silently swallowed) and
                // strictly no worse than the old behavior of dropping the
                // pinned entry out from under that session outright.
                if let Err(e) = self.file_cache.invalidate(&key) {
                    tracing::warn!("mount_backend write (binary): {e}");
                }
                return Ok(());
            }
        };

        match mode {
            WriteMode::CreateNew => {
                if self.file_cache.exists(&key).await {
                    return Err(BackendError::AlreadyExists(path.display().to_string()));
                }
            }
            WriteMode::UpdateOnly => {
                if !self.file_cache.exists(&key).await {
                    return Err(BackendError::NotFound(path.display().to_string()));
                }
            }
            WriteMode::Overwrite | WriteMode::Truncate => {}
            _ => {
                return Err(BackendError::InvalidOperation(
                    "unsupported write mode".into(),
                ));
            }
        }

        self.file_cache
            .create_or_replace(&key, text)
            .await
            .map_err(BackendError::Io)?;
        self.file_cache.mark_dirty(&key).map_err(BackendError::Io)?;
        // Write-through: external tools (cargo, git) read the real filesystem.
        // If the flush fails, roll the edit back out of the cache so a later
        // read can't serve content that never reached disk — crash, don't
        // corrupt. A pinned entry (an open editor session, C —
        // docs/audits/2026-08-20-editor-fileio.md) refuses the rollback
        // instead: the flush failure is still reported below, but a live
        // session's buffer is never evicted to satisfy this write's cleanup.
        if let Err(e) = self.file_cache.flush_one(&key).await {
            if let Err(inv_err) = self.file_cache.invalidate(&key) {
                tracing::warn!("mount_backend write rollback: {inv_err}");
            }
            return Err(BackendError::Io(e.to_string()));
        }
        Ok(())
    }

    async fn append(&self, path: &Path, content: &[u8]) -> BackendResult<()> {
        self.deny_if_read_only("append", path)?;
        if !self.mount_table.is_writable(path).await {
            return self.raw_append(path, content).await;
        }

        let key = self.cache_key(path)?;
        let suffix = match std::str::from_utf8(content) {
            Ok(s) => s,
            Err(_) => {
                self.raw_append(path, content).await?;
                // See write()'s binary-content arm: best-effort, a refusal
                // (pinned entry, C) leaves a stale shadow rather than
                // failing an append that already landed on disk.
                if let Err(e) = self.file_cache.invalidate(&key) {
                    tracing::warn!("mount_backend append (binary): {e}");
                }
                return Ok(());
            }
        };
        // Append onto current document content.
        // NotCached = new file or binary; treat as empty (correct for append-to-new).
        // Backend = real store error; refuse — unwrap_or_default() here would
        // silently wipe the file by appending `suffix` onto "" and overwriting.
        let existing = match self.file_cache.try_read_content(&key).await {
            Ok(text) => text,
            Err(CacheReadError::NotCached) => String::new(),
            Err(CacheReadError::Backend(e)) => {
                return Err(BackendError::Io(format!(
                    "append {}: cannot read current content (would wipe file): {}",
                    path.display(),
                    e
                )));
            }
        };
        let combined = format!("{existing}{suffix}");
        self.file_cache
            .create_or_replace(&key, &combined)
            .await
            .map_err(BackendError::Io)?;
        self.file_cache.mark_dirty(&key).map_err(BackendError::Io)?;
        if let Err(e) = self.file_cache.flush_one(&key).await {
            // See write()'s flush-failure arm: a pinned entry (C) refuses the
            // rollback; the flush failure below is still reported either way.
            if let Err(inv_err) = self.file_cache.invalidate(&key) {
                tracing::warn!("mount_backend append rollback: {inv_err}");
            }
            return Err(BackendError::Io(e.to_string()));
        }
        Ok(())
    }

    async fn patch(&self, path: &Path, ops: &[PatchOp]) -> BackendResult<()> {
        self.deny_if_read_only("patch", path)?;
        // Writable mounts apply through the document cache (source of truth);
        // read-only/OS paths read+write straight through the VFS (which rejects
        // the write cleanly).
        let writable = self.mount_table.is_writable(path).await;
        let key = self.cache_key(path)?;
        let original = if writable {
            // For patch on a writable mount, both NotCached and Backend are
            // errors: we can't safely apply patch ops without the current content.
            // NotCached means the file is absent or binary — patching it is a
            // caller mistake; surface NotFound so the caller gets a clear signal.
            match self.file_cache.try_read_content(&key).await {
                Ok(t) => t,
                Err(CacheReadError::NotCached) => {
                    return Err(BackendError::NotFound(path.display().to_string()));
                }
                Err(CacheReadError::Backend(e)) => {
                    return Err(BackendError::Io(e));
                }
            }
        } else {
            let bytes = self
                .mount_table
                .read_all(path)
                .await
                .map_err(vfs_to_backend)?;
            String::from_utf8_lossy(&bytes).to_string()
        };

        // Compute the WHOLE batch against an in-memory String first — no
        // storage call happens until every op (including every `expected` CAS
        // precondition) has validated, and every offset lands on a UTF-8 char
        // boundary. See `compute_patch_op`'s doc: a partial-batch commit here
        // was `3cb3ed4f`'s bug (op N's CAS failure must not leave ops 1..N-1
        // durably applied), and a mid-char byte offset used to panic instead
        // of erroring — `check_byte_boundary` closes that.
        let mut text = original.clone();
        for op in ops {
            text = compute_patch_op(op, &text)?;
        }

        if writable {
            self.file_cache
                .create_or_replace(&key, &text)
                .await
                .map_err(BackendError::Io)?;
            self.file_cache.mark_dirty(&key).map_err(BackendError::Io)?;
            if let Err(e) = self.file_cache.flush_one(&key).await {
                // See write()'s flush-failure arm: a pinned entry (C) refuses
                // the rollback; the flush failure below is still reported.
                if let Err(inv_err) = self.file_cache.invalidate(&key) {
                    tracing::warn!("mount_backend patch rollback: {inv_err}");
                }
                return Err(BackendError::Io(e.to_string()));
            }
            Ok(())
        } else {
            self.mount_table
                .write_all(path, text.as_bytes())
                .await
                .map_err(vfs_to_backend)
        }
    }

    // =========================================================================
    // Directory Operations
    // =========================================================================

    async fn list(&self, path: &Path) -> BackendResult<Vec<DirEntry>> {
        let entries = self
            .mount_table
            .readdir(path)
            .await
            .map_err(vfs_to_backend)?;
        Ok(entries.iter().map(kj_dir_entry_to_kaish).collect())
    }

    async fn stat(&self, path: &Path) -> BackendResult<DirEntry> {
        let attr = self
            .mount_table
            .getattr(path)
            .await
            .map_err(vfs_to_backend)?;
        Ok(file_attr_to_dir_entry(&path_name(path), &attr))
    }

    async fn lstat(&self, path: &Path) -> BackendResult<DirEntry> {
        self.stat(path).await
    }

    async fn mkdir(&self, path: &Path) -> BackendResult<()> {
        self.deny_if_read_only("mkdir", path)?;
        self.mount_table
            .mkdir(path, 0o755)
            .await
            .map_err(vfs_to_backend)?;
        Ok(())
    }

    async fn set_mtime(&self, path: &Path, mtime: std::time::SystemTime) -> BackendResult<()> {
        self.deny_if_read_only("touch", path)?;
        // `touch` on an existing file routes through the VFS — never escape to
        // the host via a real-path. A read-only mount's `setattr` rejects
        // cleanly (the VFS error maps to a BackendError), satisfying the
        // "virtual/read-only mounts reject rather than silently succeed"
        // contract.
        self.mount_table
            .setattr(path, SetAttr::new().with_mtime(mtime))
            .await
            .map_err(vfs_to_backend)?;
        // We deliberately don't touch `file_cache` here. The cache keys
        // freshness on `generation`, not mtime, and a pure mtime `setattr` is
        // display-only on the document/memory backends — it does NOT advance
        // generation, so it correctly does NOT trip a reload (a `touch` must not
        // discard cached content). A real content change is what bumps
        // generation and trips the staleness check. Invalidating here would risk
        // dropping an unflushed edit, so we let the staleness logic own freshness.
        Ok(())
    }

    async fn remove(&self, path: &Path, recursive: bool) -> BackendResult<()> {
        self.deny_if_read_only("remove", path)?;
        if recursive {
            // Walk and remove children first
            self.remove_recursive(path).await
        } else {
            let attr = self
                .mount_table
                .getattr(path)
                .await
                .map_err(vfs_to_backend)?;
            if attr.is_dir() {
                self.mount_table.rmdir(path).await.map_err(vfs_to_backend)
            } else {
                self.mount_table.unlink(path).await.map_err(vfs_to_backend)
            }
        }
    }

    async fn exists(&self, path: &Path) -> bool {
        self.mount_table.exists(path).await
    }

    async fn rename(&self, from: &Path, to: &Path) -> BackendResult<()> {
        self.deny_if_read_only("rename", from)?;
        self.mount_table
            .rename(from, to)
            .await
            .map_err(vfs_to_backend)
    }

    async fn read_link(&self, path: &Path) -> BackendResult<PathBuf> {
        self.mount_table
            .readlink(path)
            .await
            .map_err(vfs_to_backend)
    }

    async fn symlink(&self, target: &Path, link: &Path) -> BackendResult<()> {
        self.deny_if_read_only("symlink", link)?;
        self.mount_table
            .symlink(link, target)
            .await
            .map_err(vfs_to_backend)?;
        Ok(())
    }

    // =========================================================================
    // Tool Dispatch
    // =========================================================================

    async fn call_tool(
        &self,
        name: &str,
        args: ToolArgs,
        ctx: &mut dyn ToolCtx,
    ) -> BackendResult<ToolResult> {
        self.docs_tools.call_tool(name, args, ctx).await
    }

    async fn list_tools(&self) -> BackendResult<Vec<ToolInfo>> {
        self.docs_tools.list_tools().await
    }

    async fn get_tool(&self, name: &str) -> BackendResult<Option<ToolInfo>> {
        self.docs_tools.get_tool(name).await
    }

    // =========================================================================
    // Backend Information
    // =========================================================================

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn backend_type(&self) -> &str {
        "mount"
    }

    fn mounts(&self) -> Vec<kaish_kernel::vfs::MountInfo> {
        self.docs_tools.mounts()
    }

    fn resolve_real_path(&self, path: &Path) -> Option<PathBuf> {
        // The subprocess seam: kaish calls this (sync) to turn the shell's
        // VFS cwd into a real host cwd before spawning an external command —
        // a `None` here disables external exec for that call. Structural
        // resolution via the mount table's sync path (longest-prefix owner +
        // `real_root`); virtual cwds (/v/*, document mounts) correctly yield None.
        self.mount_table.resolve_real_path_sync(path)
    }
}

impl MountBackend {
    /// Recursively remove a directory and all its contents.
    async fn remove_recursive(&self, path: &Path) -> BackendResult<()> {
        let entries = self
            .mount_table
            .readdir(path)
            .await
            .map_err(vfs_to_backend)?;

        for entry in &entries {
            let child = path.join(&entry.name);
            if entry.kind == FileType::Directory {
                // Recurse into subdirectory using Box::pin for async recursion
                Box::pin(self.remove_recursive(&child)).await?;
            } else {
                self.mount_table
                    .unlink(&child)
                    .await
                    .map_err(vfs_to_backend)?;
            }
        }

        self.mount_table.rmdir(path).await.map_err(vfs_to_backend)
    }
}

// =============================================================================
// PatchOp helpers
// =============================================================================
//
// Moved here from `kaish_backend.rs` (2026-08-20, see
// `docs/audits/2026-08-20-kaish-glue.md` B2/B3): this is the ONE hardened
// `PatchOp` implementation now, living with the live path (`MountBackend`,
// the backend kaish actually scripts against). `KaijutsuBackend::patch` — the
// twin these replaced there — was unreachable through any mount and is now
// stubbed to `InvalidOperation`.

/// Ensure a wire BYTE offset lands on a UTF-8 char boundary in `content`.
///
/// `PatchOp::Insert`/`Delete`/`Replace` offsets are BYTES by the kaish-types
/// contract ("Insert content at byte offset", "Delete bytes …"), and the
/// in-memory mirror ops in [`compute_patch_op`] (`insert_str`,
/// `replace_range`) panic — rather than failing gracefully — on a
/// non-boundary index. Every offset is checked here before it reaches them,
/// so a mid-char offset is a loud `BackendError`, never a panic partway
/// through a batch: the pre-fix path spliced text at a bogus position and
/// then panicked in the byte mirror's `replace_range`, leaving the file
/// corrupted behind the crash.
fn check_byte_boundary(content: &str, byte: usize, what: &str) -> BackendResult<()> {
    if byte > content.len() || !content.is_char_boundary(byte) {
        return Err(BackendError::Io(format!(
            "patch {what}: byte offset {byte} is not a char boundary in {}-byte content",
            content.len()
        )));
    }
    Ok(())
}

/// Compute the result of applying one patch op to `current_content` — pure,
/// no storage access, no side effects. `patch()` folds every op in a batch
/// through this over an in-memory `String`, so every op (including every
/// `expected` CAS precondition, and every byte boundary) is validated before
/// anything is committed — see `patch()`'s doc for why (`3cb3ed4f`: a CAS
/// failure on op N must not leave ops 1..N-1 committed).
///
/// Byte-domain throughout: offsets are wire BYTES (kaish-types contract) and
/// the mirror ops below operate on that domain directly. There is no
/// char-indexed storage call in here at all.
fn compute_patch_op(op: &PatchOp, current_content: &str) -> BackendResult<String> {
    match op {
        PatchOp::Insert { offset, content } => {
            check_byte_boundary(current_content, *offset, "insert")?;
            let mut result = current_content.to_string();
            result.insert_str(*offset, content);
            Ok(result)
        }
        PatchOp::Delete {
            offset,
            len,
            expected,
        } => {
            // Validate boundaries BEFORE the CAS check: a bogus offset
            // should report as the boundary error it is, not as a
            // misleading conflict against the empty string `.get()` yields.
            check_byte_boundary(current_content, *offset, "delete")?;
            check_byte_boundary(current_content, offset + len, "delete")?;
            if let Some(exp) = expected {
                let actual = current_content.get(*offset..*offset + *len).unwrap_or("");
                if actual != exp {
                    return Err(BackendError::Conflict(ConflictError {
                        location: format!("offset {}", offset),
                        expected: exp.clone(),
                        actual: actual.to_string(),
                    }));
                }
            }
            let mut result = current_content.to_string();
            result.replace_range(*offset..*offset + *len, "");
            Ok(result)
        }
        PatchOp::Replace {
            offset,
            len,
            content,
            expected,
        } => {
            check_byte_boundary(current_content, *offset, "replace")?;
            check_byte_boundary(current_content, offset + len, "replace")?;
            if let Some(exp) = expected {
                let actual = current_content.get(*offset..*offset + *len).unwrap_or("");
                if actual != exp {
                    return Err(BackendError::Conflict(ConflictError {
                        location: format!("offset {}", offset),
                        expected: exp.clone(),
                        actual: actual.to_string(),
                    }));
                }
            }
            let mut result = current_content.to_string();
            result.replace_range(*offset..*offset + *len, content);
            Ok(result)
        }
        PatchOp::InsertLine { line, content } => {
            // Line starts always sit on char boundaries (see line_to_byte_offset),
            // so no boundary check is needed here.
            let line_offset = line_to_byte_offset(current_content, *line);
            let mut result = current_content.to_string();
            result.insert_str(line_offset, &format!("{}\n", content));
            Ok(result)
        }
        PatchOp::DeleteLine { line, expected } => {
            let (start, end) = line_range(current_content, *line);
            let actual_line = current_content.get(start..end).unwrap_or("");

            if let Some(exp) = expected
                && actual_line.trim_end_matches('\n') != exp.trim_end_matches('\n')
            {
                return Err(BackendError::Conflict(ConflictError {
                    location: format!("line {}", line),
                    expected: exp.clone(),
                    actual: actual_line.to_string(),
                }));
            }

            let mut result = current_content.to_string();
            result.replace_range(start..end, "");
            Ok(result)
        }
        PatchOp::ReplaceLine {
            line,
            content,
            expected,
        } => {
            let (start, end) = line_range(current_content, *line);
            let actual_line = current_content.get(start..end).unwrap_or("");

            if let Some(exp) = expected
                && actual_line.trim_end_matches('\n') != exp.trim_end_matches('\n')
            {
                return Err(BackendError::Conflict(ConflictError {
                    location: format!("line {}", line),
                    expected: exp.clone(),
                    actual: actual_line.to_string(),
                }));
            }

            let replacement = format!("{}\n", content);
            let mut result = current_content.to_string();
            result.replace_range(start..end, &replacement);
            Ok(result)
        }
        PatchOp::Append { content } => Ok(format!("{}{}", current_content, content)),
    }
}

/// Get byte offset for a 1-indexed line number.
///
/// Kept LOCAL — deliberately NOT replaced by the shared
/// `block_tools/translate` helpers — because the semantics differ in two
/// load-bearing ways that are part of the kaish `PatchOp` line contract:
/// this is **1-indexed** (kaish-types: "line number (1-indexed)") where
/// translate.rs is 0-indexed, and this **clamps** a beyond-EOF line to
/// end-of-content where translate.rs errors. Swapping would silently change
/// the kaish patch surface. Outputs are BYTE offsets, consumed directly by
/// `compute_patch_op`'s byte-domain mirror ops — no char projection happens
/// here; `patch()` is the one place that projects byte to char, once, when
/// it commits the whole batch's result as a single splice.
fn line_to_byte_offset(content: &str, line: usize) -> usize {
    if line <= 1 {
        return 0;
    }

    let mut offset = 0;
    let mut current_line = 1;
    for (i, c) in content.char_indices() {
        if current_line >= line {
            return i;
        }
        if c == '\n' {
            current_line += 1;
        }
        offset = i + c.len_utf8();
    }
    offset
}

/// Get byte range for a 1-indexed line (includes newline if present).
fn line_range(content: &str, line: usize) -> (usize, usize) {
    let start = line_to_byte_offset(content, line);
    let mut end = start;

    for (i, c) in content[start..].char_indices() {
        end = start + i + c.len_utf8();
        if c == '\n' {
            return (start, end);
        }
    }

    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaish_kernel::tools::ExecContext;
    use crate::Kernel as KaijutsuKernel;
    use crate::block_store::shared_block_store;
    use crate::file_tools::FileDocumentCache;
    use crate::kernel_db::KernelDb;
    use crate::vfs::backends::{LocalBackend, MemoryBackend};
    use kaijutsu_types::PrincipalId;

    /// A fresh temporary `KernelDb` for tests that don't otherwise need one —
    /// `FileDocumentCache` requires a handle to back its durable swap-file
    /// marker (docs/file-buffers.md), even when the test never exercises it.
    fn test_kernel_db() -> Arc<parking_lot::Mutex<KernelDb>> {
        Arc::new(parking_lot::Mutex::new(KernelDb::temporary().unwrap()))
    }

    /// Create a test MountBackend with a MemoryBackend mounted at /tmp.
    async fn test_mount_backend() -> MountBackend {
        let blocks = shared_block_store(PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-mount").await);
        let sid = kaijutsu_types::SessionId::new();
        let session_contexts = crate::runtime::context_engine::session_context_map();
        session_contexts.insert(sid, kaijutsu_types::ContextId::new());
        let mount_table = Arc::new(MountTable::new());
        mount_table.mount("/tmp", MemoryBackend::new()).await;

        let file_cache = Arc::new(FileDocumentCache::new(blocks.clone(), mount_table.clone(), test_kernel_db()));

        let docs = Arc::new(KaijutsuBackend::new(
            blocks,
            kernel,
            PrincipalId::system(),
            session_contexts,
            sid,
        ));

        MountBackend::new(mount_table, docs, file_cache)
    }

    #[tokio::test]
    async fn test_write_and_read() {
        let backend = test_mount_backend().await;

        backend
            .write(
                Path::new("/tmp/test.txt"),
                b"hello world",
                WriteMode::Overwrite,
            )
            .await
            .unwrap();

        let data = backend
            .read(Path::new("/tmp/test.txt"), None)
            .await
            .unwrap();
        assert_eq!(data, b"hello world");
    }

    #[tokio::test]
    async fn test_create_new_fails_if_exists() {
        let backend = test_mount_backend().await;

        backend
            .write(Path::new("/tmp/exists.txt"), b"data", WriteMode::Overwrite)
            .await
            .unwrap();

        let result = backend
            .write(Path::new("/tmp/exists.txt"), b"new", WriteMode::CreateNew)
            .await;
        assert!(matches!(result, Err(BackendError::AlreadyExists(_))));
    }

    #[tokio::test]
    async fn test_list_directory() {
        let backend = test_mount_backend().await;

        backend
            .write(Path::new("/tmp/a.txt"), b"a", WriteMode::Overwrite)
            .await
            .unwrap();
        backend
            .write(Path::new("/tmp/b.txt"), b"b", WriteMode::Overwrite)
            .await
            .unwrap();

        let entries = backend.list(Path::new("/tmp")).await.unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"b.txt"));
    }

    #[tokio::test]
    async fn test_stat_file() {
        let backend = test_mount_backend().await;

        backend
            .write(Path::new("/tmp/stat.txt"), b"12345", WriteMode::Overwrite)
            .await
            .unwrap();

        let info = backend.stat(Path::new("/tmp/stat.txt")).await.unwrap();
        assert!(info.is_file());
        assert_eq!(info.size, 5);
    }

    #[tokio::test]
    async fn test_mkdir_and_remove() {
        let backend = test_mount_backend().await;

        backend.mkdir(Path::new("/tmp/subdir")).await.unwrap();
        assert!(backend.exists(Path::new("/tmp/subdir")).await);

        backend
            .remove(Path::new("/tmp/subdir"), false)
            .await
            .unwrap();
        assert!(!backend.exists(Path::new("/tmp/subdir")).await);
    }

    #[tokio::test]
    async fn test_tool_dispatch_not_found() {
        let backend = test_mount_backend().await;
        let mut ctx = ExecContext::with_backend(Arc::new(backend) as Arc<dyn KernelBackend>);

        // Re-create for the call
        let backend2 = test_mount_backend().await;
        let args = ToolArgs::new();
        let result = backend2.call_tool("nonexistent-tool", args, &mut ctx).await;
        assert!(matches!(result, Err(BackendError::ToolNotFound(_))));
    }

    #[tokio::test]
    async fn test_backend_type() {
        let backend = test_mount_backend().await;
        assert_eq!(backend.backend_type(), "mount");
    }

    /// The reason this whole change exists: a write from the kaish surface and
    /// a read from the MCP surface address one kernel document. We exercise both
    /// directions over a single shared `FileDocumentCache`.
    #[tokio::test]
    async fn kaish_and_mcp_share_one_kernel_document() {
        let blocks = shared_block_store(PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-xsurface").await);
        let sid = kaijutsu_types::SessionId::new();
        let session_contexts = crate::runtime::context_engine::session_context_map();
        session_contexts.insert(sid, kaijutsu_types::ContextId::new());

        let mount_table = Arc::new(MountTable::new());
        mount_table.mount("/tmp", MemoryBackend::new()).await;
        let file_cache = Arc::new(FileDocumentCache::new(blocks.clone(), mount_table.clone(), test_kernel_db()));
        let docs = Arc::new(KaijutsuBackend::new(
            blocks,
            kernel,
            PrincipalId::system(),
            session_contexts,
            sid,
        ));
        let backend = MountBackend::new(mount_table, docs, file_cache.clone());

        // kaish surface writes a file...
        backend
            .write(Path::new("/tmp/shared.rs"), b"fn main() {}", WriteMode::Overwrite)
            .await
            .unwrap();

        // ...and the MCP surface (same shared cache) sees it immediately.
        assert_eq!(
            file_cache.read_content("/tmp/shared.rs").await.unwrap(),
            "fn main() {}"
        );

        // An edit through the cache (the MCP `edit` path) is visible back
        // through a kaish read — including before any flush to disk.
        file_cache
            .create_or_replace("/tmp/shared.rs", "fn main() { /* edited */ }")
            .await
            .unwrap();
        let via_kaish = backend.read(Path::new("/tmp/shared.rs"), None).await.unwrap();
        assert_eq!(
            String::from_utf8(via_kaish).unwrap(),
            "fn main() { /* edited */ }"
        );

        // Different spellings of the same path resolve to the same document.
        let via_relative_key = backend
            .read(Path::new("/tmp/./shared.rs"), None)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(via_relative_key).unwrap(),
            "fn main() { /* edited */ }"
        );
    }

    /// Read-only / OS mounts pass through the VFS and never touch the document
    /// cache: reads work, writes are rejected cleanly, and a rejected write
    /// must NOT leave a phantom edit that a later read would serve.
    #[tokio::test]
    async fn readonly_mount_passes_through_and_does_not_poison() {
        // tempfile: unique + RAII-cleaned (no leaked `/tmp` dir across runs, and
        // no cross-process collision on a pid-named dir). Held to end of scope.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("ro.txt"), b"on-disk").unwrap();

        let blocks = shared_block_store(PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-ro").await);
        let sid = kaijutsu_types::SessionId::new();
        let session_contexts = crate::runtime::context_engine::session_context_map();
        session_contexts.insert(sid, kaijutsu_types::ContextId::new());

        let mount_table = Arc::new(MountTable::new());
        mount_table
            .mount(dir.to_str().unwrap(), LocalBackend::read_only(dir))
            .await;
        let file_cache = Arc::new(FileDocumentCache::new(blocks.clone(), mount_table.clone(), test_kernel_db()));
        let docs = Arc::new(KaijutsuBackend::new(
            blocks,
            kernel,
            PrincipalId::system(),
            session_contexts,
            sid,
        ));
        let backend = MountBackend::new(mount_table, docs, file_cache);

        let file = dir.join("ro.txt");

        // Read passes through.
        assert_eq!(backend.read(&file, None).await.unwrap(), b"on-disk");

        // Write is rejected (read-only) — the exact error variant depends on the
        // backend, but it must fail.
        let w = backend
            .write(&file, b"tampered", WriteMode::Overwrite)
            .await;
        assert!(w.is_err(), "write to a read-only mount must fail");

        // And the rejected write must not have poisoned anything: a fresh read
        // still returns the on-disk content, not the phantom edit.
        assert_eq!(backend.read(&file, None).await.unwrap(), b"on-disk");

        std::fs::remove_dir_all(dir).ok();
    }

    /// `new_read_only` is the structural read-only *mode* (for the toolie's
    /// `read_only_shell`): it refuses every mutation regardless of whether the
    /// underlying mount is writable, while reads — including kernel-owned text —
    /// still pass through. This is the gate that lets the toolie inspect a
    /// live, *writable* project tree without a write path. Distinct from
    /// `readonly_mount_passes_through_and_does_not_poison`, which exercises a
    /// per-mount read-only *backend* under a writable MountBackend.
    #[tokio::test]
    async fn read_only_mode_refuses_writes_over_a_writable_mount() {
        let blocks = shared_block_store(PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-ro-mode").await);
        let sid = kaijutsu_types::SessionId::new();
        let session_contexts = crate::runtime::context_engine::session_context_map();
        session_contexts.insert(sid, kaijutsu_types::ContextId::new());

        // A genuinely writable mount — the read-only behaviour must come from
        // the backend mode, NOT from the mount being read-only.
        let mount_table = Arc::new(MountTable::new());
        mount_table.mount("/tmp", MemoryBackend::new()).await;
        let file_cache = Arc::new(FileDocumentCache::new(blocks.clone(), mount_table.clone(), test_kernel_db()));
        let docs = Arc::new(KaijutsuBackend::new(
            blocks,
            kernel,
            PrincipalId::system(),
            session_contexts,
            sid,
        ));

        // Seed a file through a writable backend sharing the same cache/mount.
        let writable = MountBackend::new(mount_table.clone(), docs.clone(), file_cache.clone());
        writable
            .write(Path::new("/tmp/seed.txt"), b"seeded", WriteMode::Overwrite)
            .await
            .unwrap();

        // Now the read-only backend over the SAME (writable) mount table.
        let ro = MountBackend::new_read_only(mount_table, docs, file_cache);
        assert!(ro.read_only(), "read_only() must report the mode");

        // Reads pass through (kernel-owned text included).
        assert_eq!(ro.read(Path::new("/tmp/seed.txt"), None).await.unwrap(), b"seeded");
        assert!(ro.list(Path::new("/tmp")).await.is_ok(), "listing is a read");

        // Every mutation is refused with PermissionDenied — by construction,
        // before reaching the (writable) mount.
        let w = ro.write(Path::new("/tmp/new.txt"), b"x", WriteMode::Overwrite).await;
        assert!(matches!(w, Err(BackendError::PermissionDenied(_))), "write: {w:?}");
        let a = ro.append(Path::new("/tmp/seed.txt"), b"x").await;
        assert!(matches!(a, Err(BackendError::PermissionDenied(_))), "append: {a:?}");
        let m = ro.mkdir(Path::new("/tmp/d")).await;
        assert!(matches!(m, Err(BackendError::PermissionDenied(_))), "mkdir: {m:?}");
        let r = ro.remove(Path::new("/tmp/seed.txt"), false).await;
        assert!(matches!(r, Err(BackendError::PermissionDenied(_))), "remove: {r:?}");
        let mv = ro.rename(Path::new("/tmp/seed.txt"), Path::new("/tmp/moved.txt")).await;
        assert!(matches!(mv, Err(BackendError::PermissionDenied(_))), "rename: {mv:?}");

        // The refused mutations changed nothing.
        assert_eq!(ro.read(Path::new("/tmp/seed.txt"), None).await.unwrap(), b"seeded");
    }

    #[tokio::test]
    async fn test_append() {
        let backend = test_mount_backend().await;

        backend
            .write(Path::new("/tmp/append.txt"), b"hello", WriteMode::Overwrite)
            .await
            .unwrap();

        backend
            .append(Path::new("/tmp/append.txt"), b" world")
            .await
            .unwrap();

        let data = backend
            .read(Path::new("/tmp/append.txt"), None)
            .await
            .unwrap();
        assert_eq!(data, b"hello world");
    }

    /// Regression: a Backend error during `append`'s pre-read must NOT wipe the
    /// file by appending `suffix` onto "" and overwriting. The old code used
    /// `read_content(...).unwrap_or_default()`, which mapped a real backend
    /// failure to an empty string — effectively truncating the file to just the
    /// appended suffix.
    ///
    /// This test MUST FAIL on code that uses `unwrap_or_default()` on the read
    /// (or any variant that silently falls back to empty on a Backend error).
    #[tokio::test]
    async fn append_backend_error_does_not_wipe_file() {
        let blocks = shared_block_store(PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-append-nowipe").await);
        let sid = kaijutsu_types::SessionId::new();
        let session_contexts = crate::runtime::context_engine::session_context_map();
        session_contexts.insert(sid, kaijutsu_types::ContextId::new());

        let mount_table = Arc::new(MountTable::new());
        mount_table.mount("/tmp", MemoryBackend::new()).await;
        let file_cache = Arc::new(FileDocumentCache::new(blocks.clone(), mount_table.clone(), test_kernel_db()));
        let docs = Arc::new(KaijutsuBackend::new(
            blocks,
            kernel,
            PrincipalId::system(),
            session_contexts,
            sid,
        ));
        let backend = MountBackend::new(mount_table, docs, file_cache.clone());

        // Write the initial file content through the backend.
        backend
            .write(Path::new("/tmp/nowipe.txt"), b"keep me", WriteMode::Overwrite)
            .await
            .unwrap();

        // Verify the file is readable before we induce the fault.
        assert_eq!(
            backend.read(Path::new("/tmp/nowipe.txt"), None).await.unwrap(),
            b"keep me"
        );

        // Destroy the kernel document from the block store to simulate a Backend
        // error on the next read — the in-memory cache entry still points to
        // the now-gone context_id.
        let ctx_id = {
            use uuid::Uuid;
            use kaijutsu_types::ContextId;
            let uuid = Uuid::new_v5(
                &Uuid::NAMESPACE_URL,
                b"kaijutsu:file:/tmp/nowipe.txt",
            );
            ContextId::from_bytes(*uuid.as_bytes())
        };
        file_cache
            .block_store()
            .delete_document(ctx_id)
            .expect("setup: delete_document must succeed");

        // Append must FAIL (Backend error) rather than wipe the file.
        let result = backend.append(Path::new("/tmp/nowipe.txt"), b" suffix").await;
        assert!(
            result.is_err(),
            "append over a broken block store must fail, not silently wipe the file"
        );

        // The underlying VFS file must still contain the original content.
        // On old code this would contain only " suffix" (the file was wiped).
        let raw = backend
            .mount_table
            .read_all(Path::new("/tmp/nowipe.txt"))
            .await
            .unwrap();
        assert_eq!(
            raw, b"keep me",
            "file must not be wiped by a failed append: got {:?}",
            String::from_utf8_lossy(&raw)
        );
    }

    /// Regression: a Backend error during `read` must return `Err`, NOT fall
    /// through to serve stale on-disk bytes. The old code used a blanket `if
    /// let Ok(text) = read_content(...)` which silently served disk content when
    /// the block store was broken — silent data corruption.
    ///
    /// This test MUST FAIL on code that uses `if let Ok(text) = read_content`
    /// (or any pattern that falls through on ALL errors, not just NotCached).
    #[tokio::test]
    async fn read_backend_error_does_not_serve_stale_disk_bytes() {
        let blocks = shared_block_store(PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-read-nostalefallback").await);
        let sid = kaijutsu_types::SessionId::new();
        let session_contexts = crate::runtime::context_engine::session_context_map();
        session_contexts.insert(sid, kaijutsu_types::ContextId::new());

        let mount_table = Arc::new(MountTable::new());
        mount_table.mount("/tmp", MemoryBackend::new()).await;
        let file_cache = Arc::new(FileDocumentCache::new(blocks.clone(), mount_table.clone(), test_kernel_db()));
        let docs = Arc::new(KaijutsuBackend::new(
            blocks,
            kernel,
            PrincipalId::system(),
            session_contexts,
            sid,
        ));
        let backend = MountBackend::new(mount_table.clone(), docs, file_cache.clone());

        // Write the file through the backend so it's in the document cache AND on disk.
        backend
            .write(Path::new("/tmp/stale.txt"), b"doc-content", WriteMode::Overwrite)
            .await
            .unwrap();

        // Destroy the kernel document to force a Backend error on next read.
        let ctx_id = {
            use uuid::Uuid;
            use kaijutsu_types::ContextId;
            let uuid = Uuid::new_v5(&Uuid::NAMESPACE_URL, b"kaijutsu:file:/tmp/stale.txt");
            ContextId::from_bytes(*uuid.as_bytes())
        };
        file_cache
            .block_store()
            .delete_document(ctx_id)
            .expect("setup: delete_document must succeed");

        // On old code: Backend error → falls through → serves "doc-content"
        // from disk (stale, wrong). On new code: must return Err.
        let result = backend.read(Path::new("/tmp/stale.txt"), None).await;
        assert!(
            result.is_err(),
            "read must return Err on a Backend error, not serve stale disk bytes"
        );
    }

    // ── compute_patch_op × multibyte content (byte-vs-char offset regression) ──
    //
    // Moved from `kaish_backend.rs` (2026-08-20) along with the hardened
    // `compute_patch_op`/`check_byte_boundary` it pins — this file's `patch()`
    // is the only live caller now. `compute_patch_op` is pure (no storage),
    // so these call it directly; the batch-atomicity property below can only
    // be observed through `backend.patch()`.

    #[test]
    fn test_line_to_byte_offset() {
        let content = "line 1\nline 2\nline 3";

        assert_eq!(line_to_byte_offset(content, 1), 0);
        assert_eq!(line_to_byte_offset(content, 2), 7); // "line 1\n" = 7 bytes
        assert_eq!(line_to_byte_offset(content, 3), 14); // "line 1\nline 2\n" = 14 bytes
    }

    #[test]
    fn test_line_range() {
        let content = "line 1\nline 2\nline 3";

        assert_eq!(line_range(content, 1), (0, 7)); // "line 1\n"
        assert_eq!(line_range(content, 2), (7, 14)); // "line 2\n"
        assert_eq!(line_range(content, 3), (14, 20)); // "line 3" (no trailing newline)
    }

    #[test]
    fn patch_insert_line_after_multibyte_line() {
        let content = "改善 → done\nsecond";

        // 1-indexed: line 2 = before "second". `compute_patch_op` stays in
        // the byte domain throughout, so this pins the correct line-split
        // result rather than a char/byte mismatch landing in the wrong place.
        let result = compute_patch_op(
            &PatchOp::InsertLine {
                line: 2,
                content: "INSERTED".into(),
            },
            content,
        )
        .expect("insert line after a multibyte line must succeed");
        assert_eq!(result, "改善 → done\nINSERTED\nsecond");
    }

    #[test]
    fn patch_delete_line_with_multibyte_before() {
        let content = "改善 → done\nDELETE ME\nkeep";

        let result = compute_patch_op(
            &PatchOp::DeleteLine {
                line: 2,
                expected: Some("DELETE ME".into()),
            },
            content,
        )
        .expect("delete line after a multibyte line must not trip bounds");
        assert_eq!(result, "改善 → done\nkeep");
    }

    #[test]
    fn patch_replace_line_with_multibyte_before() {
        let content = "→ arrows ✅\nold\ntail";

        let result = compute_patch_op(
            &PatchOp::ReplaceLine {
                line: 2,
                content: "new".into(),
                expected: Some("old".into()),
            },
            content,
        )
        .expect("replace line after a multibyte line must succeed");
        assert_eq!(result, "→ arrows ✅\nnew\ntail");
    }

    /// Pins the byte-offset ruling for the positional ops: `PatchOp::Replace`'s
    /// wire `offset`/`len` are BYTES (kaish-types contract; the CAS check
    /// byte-slices). `compute_patch_op` stays in that byte domain end to end
    /// — the char projection only happens once `patch()` commits the whole
    /// batch's result.
    #[test]
    fn patch_byte_replace_with_multibyte_before() {
        let content = "改善X";

        // Bytes 6..7 = "X" (改善 = 6 bytes); chars 2..3 — pins that the byte
        // offset itself, not a char projection, is what's checked and used.
        let result = compute_patch_op(
            &PatchOp::Replace {
                offset: 6,
                len: 1,
                content: "Y".into(),
                expected: Some("X".into()),
            },
            content,
        )
        .expect("byte-offset replace after multibyte prefix must succeed");
        assert_eq!(result, "改善Y");
    }

    /// A wire byte offset that lands MID-CHAR is a loud error, not a panic —
    /// `check_byte_boundary` rejects it before the mirror ops
    /// (`insert_str`/`replace_range`) ever see it, since those panic rather
    /// than error on a non-boundary index.
    #[test]
    fn patch_byte_offset_mid_char_fails_loud() {
        let content = "改善";

        let err = compute_patch_op(
            &PatchOp::Replace {
                offset: 1,
                len: 1,
                content: "z".into(),
                expected: None,
            },
            content,
        )
        .expect_err("mid-char byte offset must be rejected");
        assert!(
            err.to_string().contains("char boundary"),
            "error should name the boundary problem: {err}"
        );
    }

    // ── batch atomicity + boundary safety through the real `patch()` path ──
    //
    // `patch()` computes every op in a batch against an in-memory `String`
    // via `compute_patch_op` (pure, no storage) and commits once, only after
    // the whole batch has succeeded — the property `3cb3ed4f` fixed on the
    // old `KaijutsuBackend::patch` (a CAS failure on op N must not leave ops
    // 1..N-1 durably applied) and that this file now provides for the live
    // path.

    #[tokio::test]
    async fn patch_batch_cas_failure_leaves_content_untouched() {
        let backend = test_mount_backend().await;
        let path = Path::new("/tmp/atomic.txt");
        backend
            .write(path, b"abcdef", WriteMode::Overwrite)
            .await
            .unwrap();

        // op1 would succeed alone (its CAS matches "abc"). op2's `expected`
        // does not match what op1 actually produces at that offset ("def",
        // not "MISMATCH") — the whole batch must fail, and op1 must not have
        // been left applied.
        let ops = vec![
            PatchOp::Replace {
                offset: 0,
                len: 3,
                content: "XXX".into(),
                expected: Some("abc".into()),
            },
            PatchOp::Replace {
                offset: 3,
                len: 3,
                content: "YYY".into(),
                expected: Some("MISMATCH".into()),
            },
        ];

        let err = backend
            .patch(path, &ops)
            .await
            .expect_err("mid-batch CAS mismatch must fail the whole patch");
        assert!(
            matches!(err, BackendError::Conflict(_)),
            "expected a Conflict error, got {err:?}"
        );

        let content = backend.read(path, None).await.unwrap();
        assert_eq!(
            content, b"abcdef",
            "a failed batch must leave the file byte-identical — op1 must not have landed"
        );
    }

    #[tokio::test]
    async fn patch_successful_multi_op_batch_uses_progressive_offsets() {
        let backend = test_mount_backend().await;
        let path = Path::new("/tmp/progressive.txt");
        backend
            .write(path, b"abcdefghij", WriteMode::Overwrite)
            .await
            .unwrap();

        // op2 and op3's offsets are against the content AFTER the prior op,
        // not the original.
        //   "abcdefghij"
        // → "XYZdefghij"      (op1: replace [0..3) "abc" -> "XYZ")
        // → "XYZ123defghij"   (op2: insert "123" at byte 3)
        // → "XYZ123ghij"      (op3: delete [6..9) "def")
        let ops = vec![
            PatchOp::Replace {
                offset: 0,
                len: 3,
                content: "XYZ".into(),
                expected: Some("abc".into()),
            },
            PatchOp::Insert {
                offset: 3,
                content: "123".into(),
            },
            PatchOp::Delete {
                offset: 6,
                len: 3,
                expected: Some("def".into()),
            },
        ];

        backend
            .patch(path, &ops)
            .await
            .expect("a fully valid multi-op batch must apply every op");

        let content = backend.read(path, None).await.unwrap();
        assert_eq!(content, b"XYZ123ghij");
    }

    #[tokio::test]
    async fn patch_multibyte_multi_op_batch_char_vs_byte_splice() {
        let backend = test_mount_backend().await;
        let path = Path::new("/tmp/multibyte-batch.txt");
        let original = "改善 → done";
        backend
            .write(path, original.as_bytes(), WriteMode::Overwrite)
            .await
            .unwrap();

        // op1 replaces the arrow (multibyte) itself; op2's offset is
        // computed against the content AFTER op1, not the original.
        let arrow_offset = original.find('→').unwrap();
        let arrow_len = '→'.len_utf8();
        let after_op1 = {
            let mut s = original.to_string();
            s.replace_range(arrow_offset..arrow_offset + arrow_len, "=>");
            s
        };
        let append_offset = after_op1.len();

        let ops = vec![
            PatchOp::Replace {
                offset: arrow_offset,
                len: arrow_len,
                content: "=>".into(),
                expected: Some("→".into()),
            },
            PatchOp::Insert {
                offset: append_offset,
                content: " 改善".into(),
            },
        ];

        backend
            .patch(path, &ops)
            .await
            .expect("multibyte multi-op batch must apply cleanly");

        let expected = format!("{} 改善", after_op1);
        let content = backend.read(path, None).await.unwrap();
        assert_eq!(content, expected.as_bytes());
    }

    /// A patch op with a byte offset inside a multi-byte char must return a
    /// typed error through the real `patch()` path, never panic — the live
    /// counterpart to `patch_byte_offset_mid_char_fails_loud` above, which
    /// pins the same property on the pure `compute_patch_op`.
    #[tokio::test]
    async fn patch_mid_char_byte_offset_errors_not_panics() {
        let backend = test_mount_backend().await;
        let path = Path::new("/tmp/mid-char.txt");
        backend
            .write(path, "改善".as_bytes(), WriteMode::Overwrite)
            .await
            .unwrap();

        let ops = vec![PatchOp::Replace {
            offset: 1,
            len: 1,
            content: "z".into(),
            expected: None,
        }];
        let err = backend
            .patch(path, &ops)
            .await
            .expect_err("a byte offset inside a multi-byte char must error, not panic");
        assert!(
            err.to_string().contains("char boundary"),
            "expected a boundary error, got {err}"
        );

        // The file must be untouched — the boundary check runs before any
        // commit.
        let content = backend.read(path, None).await.unwrap();
        assert_eq!(content, "改善".as_bytes());
    }
}
