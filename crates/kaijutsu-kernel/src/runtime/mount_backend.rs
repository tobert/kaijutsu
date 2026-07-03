//! MountBackend: Routes file ops through kaijutsu's MountTable.
//!
//! Delegates filesystem operations to the kaijutsu kernel's `MountTable`
//! (which routes to `LocalBackend` for real files) and tool dispatch to
//! the CRDT backends.
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

/// Routes file *content* operations through the shared CRDT
/// `FileDocumentCache` and directory/metadata/tool operations through
/// kaijutsu's `MountTable`.
///
/// This is what makes kaish "shell scripting on the same CRDT substrate":
/// a `cat`/`read`/`write`/`edit` from the shell hits the same CRDT document
/// the MCP `builtin.file` tools use, keyed by the canonical absolute path.
/// Binary files (not representable as CRDT text) fall through to the raw
/// `MountTable`; that fallthrough is a deliberate type distinction, not a
/// silent error-swallow.
pub struct MountBackend {
    /// Kaijutsu kernel's VFS mount table — directory/metadata ops and the
    /// binary-file fallback path.
    mount_table: Arc<MountTable>,
    /// Shared CRDT file-document cache — the source of truth for text file
    /// content across both the kaish and MCP surfaces.
    file_cache: Arc<FileDocumentCache>,
    /// CRDT backend for document tool dispatch.
    docs_tools: Arc<KaijutsuBackend>,
    /// When true, every mutating op is refused structurally with
    /// `PermissionDenied` *before* it can reach the shared mount table or CRDT
    /// cache — the read-only invariant for the toolie's `read_only_shell`.
    /// Reads (real files and CRDT documents) still pass through. This gates the
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
    /// so the kaish surface and the MCP surface address one CRDT document per
    /// real file. Rejects `..`-escapes above root (untrusted input → refuse,
    /// never silently clamp).
    fn cache_key(&self, path: &Path) -> BackendResult<String> {
        resolve_str(Path::new("/"), &path.to_string_lossy())
            .map_err(|e| BackendError::PermissionDenied(e.to_string()))
    }

    /// Write straight to the VFS, honoring `WriteMode`, without touching the
    /// CRDT cache. Used for read-only/OS mounts (so the VFS rejects cleanly)
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
        // CRDT-back only writable mounts. Reading a read-only/OS path shouldn't
        // mint a CRDT document — pass it straight through the VFS.
        if self.mount_table.is_writable(path).await {
            let key = self.cache_key(path)?;
            match self.file_cache.try_read_content(&key).await {
                Ok(text) => return Ok(apply_range(text.into_bytes(), range)),
                Err(CacheReadError::NotCached) => {
                    // Missing or binary: fall through to a raw read so `cat`
                    // on a binary or absent file still works as expected.
                }
                Err(CacheReadError::Backend(e)) => {
                    // A real CRDT/store error: refuse to serve stale disk bytes
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

        // Binary content can't live in the CRDT text substrate: write raw and
        // drop any cached text doc so a later read reloads fresh.
        let text = match std::str::from_utf8(content) {
            Ok(t) => t,
            Err(_) => {
                self.raw_write(path, content, mode).await?;
                self.file_cache.invalidate(&key);
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
        self.file_cache.mark_dirty(&key);
        // Write-through: external tools (cargo, git) read the real filesystem.
        // If the flush fails, roll the edit back out of the cache so a later
        // read can't serve content that never reached disk — crash, don't
        // corrupt.
        if let Err(e) = self.file_cache.flush_one(&key).await {
            self.file_cache.invalidate(&key);
            return Err(BackendError::Io(e));
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
                self.file_cache.invalidate(&key);
                return Ok(());
            }
        };
        // Append onto current CRDT content.
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
        self.file_cache.mark_dirty(&key);
        if let Err(e) = self.file_cache.flush_one(&key).await {
            self.file_cache.invalidate(&key);
            return Err(BackendError::Io(e));
        }
        Ok(())
    }

    async fn patch(&self, path: &Path, ops: &[PatchOp]) -> BackendResult<()> {
        self.deny_if_read_only("patch", path)?;
        // Writable mounts apply through the CRDT cache (source of truth);
        // read-only/OS paths read+write straight through the VFS (which rejects
        // the write cleanly).
        let writable = self.mount_table.is_writable(path).await;
        let key = self.cache_key(path)?;
        let mut text = if writable {
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

        for op in ops {
            match op {
                PatchOp::Insert { offset, content } => {
                    if *offset > text.len() {
                        return Err(BackendError::InvalidOperation(format!(
                            "insert offset {} beyond end of file ({})",
                            offset,
                            text.len()
                        )));
                    }
                    text.insert_str(*offset, content);
                }
                PatchOp::Delete {
                    offset,
                    len,
                    expected,
                } => {
                    let end = offset + len;
                    if end > text.len() {
                        return Err(BackendError::InvalidOperation(format!(
                            "delete range {}..{} beyond end of file ({})",
                            offset,
                            end,
                            text.len()
                        )));
                    }
                    if let Some(exp) = expected {
                        let actual = &text[*offset..end];
                        if actual != exp.as_str() {
                            return Err(BackendError::Conflict(ConflictError {
                                location: format!("offset {}", offset),
                                expected: exp.clone(),
                                actual: actual.to_string(),
                            }));
                        }
                    }
                    text.replace_range(*offset..end, "");
                }
                PatchOp::Replace {
                    offset,
                    len,
                    content,
                    expected,
                } => {
                    let end = offset + len;
                    if end > text.len() {
                        return Err(BackendError::InvalidOperation(format!(
                            "replace range {}..{} beyond end of file ({})",
                            offset,
                            end,
                            text.len()
                        )));
                    }
                    if let Some(exp) = expected {
                        let actual = &text[*offset..end];
                        if actual != exp.as_str() {
                            return Err(BackendError::Conflict(ConflictError {
                                location: format!("offset {}", offset),
                                expected: exp.clone(),
                                actual: actual.to_string(),
                            }));
                        }
                    }
                    text.replace_range(*offset..end, content);
                }
                PatchOp::InsertLine { line, content } => {
                    let mut lines: Vec<&str> = text.split('\n').collect();
                    let idx = line.saturating_sub(1).min(lines.len());
                    lines.insert(idx, content);
                    text = lines.join("\n");
                }
                PatchOp::DeleteLine { line, expected } => {
                    let mut lines: Vec<&str> = text.split('\n').collect();
                    let idx = line.saturating_sub(1);
                    if idx >= lines.len() {
                        return Err(BackendError::InvalidOperation(format!(
                            "line {} out of range ({})",
                            line,
                            lines.len()
                        )));
                    }
                    if let Some(exp) = expected
                        && lines[idx] != exp.as_str()
                    {
                        return Err(BackendError::Conflict(ConflictError {
                            location: format!("line {}", line),
                            expected: exp.clone(),
                            actual: lines[idx].to_string(),
                        }));
                    }
                    lines.remove(idx);
                    text = lines.join("\n");
                }
                PatchOp::ReplaceLine {
                    line,
                    content,
                    expected,
                } => {
                    let mut lines: Vec<&str> = text.split('\n').collect();
                    let idx = line.saturating_sub(1);
                    if idx >= lines.len() {
                        return Err(BackendError::InvalidOperation(format!(
                            "line {} out of range ({})",
                            line,
                            lines.len()
                        )));
                    }
                    if let Some(exp) = expected
                        && lines[idx] != exp.as_str()
                    {
                        return Err(BackendError::Conflict(ConflictError {
                            location: format!("line {}", line),
                            expected: exp.clone(),
                            actual: lines[idx].to_string(),
                        }));
                    }
                    lines[idx] = content;
                    text = lines.join("\n");
                }
                PatchOp::Append { content } => {
                    text.push_str(content);
                }
            }
        }

        if writable {
            self.file_cache
                .create_or_replace(&key, &text)
                .await
                .map_err(BackendError::Io)?;
            self.file_cache.mark_dirty(&key);
            if let Err(e) = self.file_cache.flush_one(&key).await {
                self.file_cache.invalidate(&key);
                return Err(BackendError::Io(e));
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
        // display-only on the CRDT/memory backends — it does NOT advance
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
        // `real_root`); virtual cwds (/v/*, CRDT mounts) correctly yield None.
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

#[cfg(test)]
mod tests {
    use super::*;
    use kaish_kernel::tools::ExecContext;
    use crate::Kernel as KaijutsuKernel;
    use crate::block_store::shared_block_store;
    use crate::file_tools::FileDocumentCache;
    use crate::vfs::backends::{LocalBackend, MemoryBackend};
    use kaijutsu_types::PrincipalId;

    /// Create a test MountBackend with a MemoryBackend mounted at /tmp.
    async fn test_mount_backend() -> MountBackend {
        let blocks = shared_block_store(PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-mount").await);
        let sid = kaijutsu_types::SessionId::new();
        let session_contexts = crate::runtime::context_engine::session_context_map();
        session_contexts.insert(sid, kaijutsu_types::ContextId::new());
        let mount_table = Arc::new(MountTable::new());
        mount_table.mount("/tmp", MemoryBackend::new()).await;

        let file_cache = Arc::new(FileDocumentCache::new(blocks.clone(), mount_table.clone()));

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
    /// a read from the MCP surface address one CRDT document. We exercise both
    /// directions over a single shared `FileDocumentCache`.
    #[tokio::test]
    async fn kaish_and_mcp_share_one_crdt_document() {
        let blocks = shared_block_store(PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-xsurface").await);
        let sid = kaijutsu_types::SessionId::new();
        let session_contexts = crate::runtime::context_engine::session_context_map();
        session_contexts.insert(sid, kaijutsu_types::ContextId::new());

        let mount_table = Arc::new(MountTable::new());
        mount_table.mount("/tmp", MemoryBackend::new()).await;
        let file_cache = Arc::new(FileDocumentCache::new(blocks.clone(), mount_table.clone()));
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

    /// Read-only / OS mounts pass through the VFS and never touch the CRDT
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
        let file_cache = Arc::new(FileDocumentCache::new(blocks.clone(), mount_table.clone()));
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

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `new_read_only` is the structural read-only *mode* (for the toolie's
    /// `read_only_shell`): it refuses every mutation regardless of whether the
    /// underlying mount is writable, while reads — including CRDT-backed text —
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
        let file_cache = Arc::new(FileDocumentCache::new(blocks.clone(), mount_table.clone()));
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

        // Reads pass through (CRDT-backed text included).
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
        let file_cache = Arc::new(FileDocumentCache::new(blocks.clone(), mount_table.clone()));
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

        // Destroy the CRDT document from the block store to simulate a Backend
        // error on the next read — the in-memory cache entry still points to
        // the now-gone context_id.
        let ctx_id = {
            use uuid::Uuid;
            use kaijutsu_crdt::ContextId;
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
    /// the CRDT store was broken — silent data corruption.
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
        let file_cache = Arc::new(FileDocumentCache::new(blocks.clone(), mount_table.clone()));
        let docs = Arc::new(KaijutsuBackend::new(
            blocks,
            kernel,
            PrincipalId::system(),
            session_contexts,
            sid,
        ));
        let backend = MountBackend::new(mount_table.clone(), docs, file_cache.clone());

        // Write the file through the backend so it's in the CRDT cache AND on disk.
        backend
            .write(Path::new("/tmp/stale.txt"), b"crdt-content", WriteMode::Overwrite)
            .await
            .unwrap();

        // Destroy the CRDT document to force a Backend error on next read.
        let ctx_id = {
            use uuid::Uuid;
            use kaijutsu_crdt::ContextId;
            let uuid = Uuid::new_v5(&Uuid::NAMESPACE_URL, b"kaijutsu:file:/tmp/stale.txt");
            ContextId::from_bytes(*uuid.as_bytes())
        };
        file_cache
            .block_store()
            .delete_document(ctx_id)
            .expect("setup: delete_document must succeed");

        // On old code: Backend error → falls through → serves "crdt-content"
        // from disk (stale, wrong). On new code: must return Err.
        let result = backend.read(Path::new("/tmp/stale.txt"), None).await;
        assert!(
            result.is_err(),
            "read must return Err on a Backend error, not serve stale disk bytes"
        );
    }
}
