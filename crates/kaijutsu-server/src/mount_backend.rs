//! MountBackend: Routes file ops through kaijutsu's MountTable.
//!
//! Replaces `CompositeBackend` by delegating filesystem operations to the
//! kaijutsu kernel's `MountTable` (which routes to `LocalBackend` for real
//! files) and tool dispatch to the CRDT backends.
//!
//! # Architecture
//!
//! ```text
//! MountBackend (implements kaish KernelBackend)
//! ├── File ops → MountTable → LocalBackend → real filesystem
//! └── Tool calls → docs_tools → git_tools → ToolNotFound
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use async_trait::async_trait;

use kaish_kernel::{
    BackendError, BackendResult, EntryInfo, KernelBackend,
    PatchOp, ReadRange, ToolInfo, ToolResult, WriteMode,
};
use kaish_kernel::backend::ConflictError;
use kaish_kernel::tools::{ExecContext, ToolArgs};

use kaijutsu_kernel::vfs::{
    FileType, MountTable, VfsError, VfsOps,
};

use crate::git_backend::GitCrdtBackend;
use crate::kaish_backend::KaijutsuBackend;

/// Routes file operations through kaijutsu's `MountTable` and tool
/// calls through the CRDT backends.
pub struct MountBackend {
    /// Kaijutsu kernel's VFS mount table for real filesystem access.
    mount_table: Arc<MountTable>,
    /// CRDT backend for document tool dispatch.
    docs_tools: Arc<KaijutsuBackend>,
    /// CRDT backend for git tool dispatch (optional).
    git_tools: Option<Arc<GitCrdtBackend>>,
}

impl MountBackend {
    /// Create a new MountBackend.
    pub fn new(
        mount_table: Arc<MountTable>,
        docs_tools: Arc<KaijutsuBackend>,
        git_tools: Option<Arc<GitCrdtBackend>>,
    ) -> Self {
        Self {
            mount_table,
            docs_tools,
            git_tools,
        }
    }
}

// TODO(dedup): entry_info_to_dir_entry below is identical to git_filesystem.rs — extract shared helper
/// Convert a `VfsError` to a `BackendError`.
fn vfs_to_backend(err: VfsError) -> BackendError {
    match err {
        VfsError::NotFound(msg) => BackendError::NotFound(msg),
        VfsError::AlreadyExists(msg) => BackendError::AlreadyExists(msg),
        VfsError::PermissionDenied(msg) => BackendError::PermissionDenied(msg),
        VfsError::ReadOnly => BackendError::ReadOnly,
        VfsError::NotADirectory(msg) => BackendError::NotDirectory(msg),
        VfsError::IsADirectory(msg) => BackendError::IsDirectory(msg),
        VfsError::DirectoryNotEmpty(msg) => BackendError::Io(format!("directory not empty: {}", msg)),
        VfsError::PathEscapesRoot(msg) => BackendError::PermissionDenied(msg),
        VfsError::InvalidPath(msg) => BackendError::InvalidOperation(msg),
        VfsError::NoMountPoint(msg) => BackendError::NotFound(msg),
        VfsError::CrossDeviceLink => BackendError::InvalidOperation("cross-device link".into()),
        VfsError::Io(e) => BackendError::Io(e.to_string()),
        other => BackendError::Io(other.to_string()),
    }
}

/// Convert a kaijutsu `FileAttr` to a kaish `EntryInfo`.
fn file_attr_to_entry_info(name: &str, attr: &kaijutsu_kernel::vfs::FileAttr) -> EntryInfo {
    let modified = attr.mtime
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs());

    EntryInfo {
        name: name.to_string(),
        is_dir: attr.kind == FileType::Directory,
        is_file: attr.kind == FileType::File,
        is_symlink: attr.kind == FileType::Symlink,
        size: attr.size,
        modified,
        permissions: Some(attr.perm),
        symlink_target: None,
    }
}

/// Convert a kaijutsu `DirEntry` to a kaish `EntryInfo`.
fn dir_entry_to_entry_info(entry: &kaijutsu_kernel::vfs::DirEntry) -> EntryInfo {
    EntryInfo {
        name: entry.name.clone(),
        is_dir: entry.kind == FileType::Directory,
        is_file: entry.kind == FileType::File,
        is_symlink: entry.kind == FileType::Symlink,
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
        match range {
            None => {
                // Read entire file
                self.mount_table
                    .read_all(path)
                    .await
                    .map_err(vfs_to_backend)
            }
            Some(ReadRange { offset: Some(off), limit: Some(lim), .. }) => {
                self.mount_table
                    .read(path, off, lim as u32)
                    .await
                    .map_err(vfs_to_backend)
            }
            Some(range) => {
                // Line-based or partial: read all then slice
                let data = self.mount_table
                    .read_all(path)
                    .await
                    .map_err(vfs_to_backend)?;

                if let (Some(start), Some(end)) = (range.start_line, range.end_line) {
                    let text = String::from_utf8_lossy(&data);
                    let lines: Vec<&str> = text.lines().collect();
                    let start = (start.saturating_sub(1)).min(lines.len());
                    let end = end.min(lines.len());
                    let selected: Vec<&str> = lines[start..end].to_vec();
                    Ok(selected.join("\n").into_bytes())
                } else if let Some(off) = range.offset {
                    let off = off as usize;
                    if off >= data.len() {
                        Ok(Vec::new())
                    } else if let Some(lim) = range.limit {
                        let end = (off + lim as usize).min(data.len());
                        Ok(data[off..end].to_vec())
                    } else {
                        Ok(data[off..].to_vec())
                    }
                } else {
                    Ok(data)
                }
            }
        }
    }

    async fn write(&self, path: &Path, content: &[u8], mode: WriteMode) -> BackendResult<()> {
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
            WriteMode::Overwrite | WriteMode::Truncate => {
                self.mount_table
                    .write_all(path, content)
                    .await
                    .map_err(vfs_to_backend)
            }
        }
    }

    async fn append(&self, path: &Path, content: &[u8]) -> BackendResult<()> {
        let attr = self.mount_table
            .getattr(path)
            .await
            .map_err(vfs_to_backend)?;
        self.mount_table
            .write(path, attr.size, content)
            .await
            .map_err(vfs_to_backend)?;
        Ok(())
    }

    async fn patch(&self, path: &Path, ops: &[PatchOp]) -> BackendResult<()> {
        // Read current content, apply patches in memory, write back
        let mut data = self.mount_table
            .read_all(path)
            .await
            .map_err(vfs_to_backend)?;
        let mut text = String::from_utf8_lossy(&data).to_string();

        for op in ops {
            match op {
                PatchOp::Insert { offset, content } => {
                    if *offset > text.len() {
                        return Err(BackendError::InvalidOperation(
                            format!("insert offset {} beyond end of file ({})", offset, text.len()),
                        ));
                    }
                    text.insert_str(*offset, content);
                }
                PatchOp::Delete { offset, len, expected } => {
                    let end = offset + len;
                    if end > text.len() {
                        return Err(BackendError::InvalidOperation(
                            format!("delete range {}..{} beyond end of file ({})", offset, end, text.len()),
                        ));
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
                PatchOp::Replace { offset, len, content, expected } => {
                    let end = offset + len;
                    if end > text.len() {
                        return Err(BackendError::InvalidOperation(
                            format!("replace range {}..{} beyond end of file ({})", offset, end, text.len()),
                        ));
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
                        return Err(BackendError::InvalidOperation(
                            format!("line {} out of range ({})", line, lines.len()),
                        ));
                    }
                    if let Some(exp) = expected {
                        if lines[idx] != exp.as_str() {
                            return Err(BackendError::Conflict(ConflictError {
                                location: format!("line {}", line),
                                expected: exp.clone(),
                                actual: lines[idx].to_string(),
                            }));
                        }
                    }
                    lines.remove(idx);
                    text = lines.join("\n");
                }
                PatchOp::ReplaceLine { line, content, expected } => {
                    let mut lines: Vec<&str> = text.split('\n').collect();
                    let idx = line.saturating_sub(1);
                    if idx >= lines.len() {
                        return Err(BackendError::InvalidOperation(
                            format!("line {} out of range ({})", line, lines.len()),
                        ));
                    }
                    if let Some(exp) = expected {
                        if lines[idx] != exp.as_str() {
                            return Err(BackendError::Conflict(ConflictError {
                                location: format!("line {}", line),
                                expected: exp.clone(),
                                actual: lines[idx].to_string(),
                            }));
                        }
                    }
                    lines[idx] = content;
                    text = lines.join("\n");
                }
                PatchOp::Append { content } => {
                    text.push_str(content);
                }
            }
        }

        data = text.into_bytes();
        self.mount_table
            .write_all(path, &data)
            .await
            .map_err(vfs_to_backend)
    }

    // =========================================================================
    // Directory Operations
    // =========================================================================

    async fn list(&self, path: &Path) -> BackendResult<Vec<EntryInfo>> {
        let entries = self.mount_table
            .readdir(path)
            .await
            .map_err(vfs_to_backend)?;
        Ok(entries.iter().map(dir_entry_to_entry_info).collect())
    }

    async fn stat(&self, path: &Path) -> BackendResult<EntryInfo> {
        let attr = self.mount_table
            .getattr(path)
            .await
            .map_err(vfs_to_backend)?;
        Ok(file_attr_to_entry_info(&path_name(path), &attr))
    }

    async fn mkdir(&self, path: &Path) -> BackendResult<()> {
        self.mount_table
            .mkdir(path, 0o755)
            .await
            .map_err(vfs_to_backend)?;
        Ok(())
    }

    async fn remove(&self, path: &Path, recursive: bool) -> BackendResult<()> {
        if recursive {
            // Walk and remove children first
            self.remove_recursive(path).await
        } else {
            let attr = self.mount_table
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
        ctx: &mut ExecContext,
    ) -> BackendResult<ToolResult> {
        // Try docs backend first (has kaijutsu tools)
        match self.docs_tools.call_tool(name, args.clone(), ctx).await {
            Ok(result) => return Ok(result),
            Err(BackendError::ToolNotFound(_)) => {}
            Err(e) => return Err(e),
        }

        // Try git backend
        if let Some(git) = &self.git_tools {
            return git.call_tool(name, args, ctx).await;
        }

        Err(BackendError::ToolNotFound(name.into()))
    }

    async fn list_tools(&self) -> BackendResult<Vec<ToolInfo>> {
        let mut tools = self.docs_tools.list_tools().await?;
        if let Some(git) = &self.git_tools {
            tools.extend(git.list_tools().await?);
        }
        Ok(tools)
    }

    async fn get_tool(&self, name: &str) -> BackendResult<Option<ToolInfo>> {
        if let Some(tool) = self.docs_tools.get_tool(name).await? {
            return Ok(Some(tool));
        }
        if let Some(git) = &self.git_tools {
            return git.get_tool(name).await;
        }
        Ok(None)
    }

    // =========================================================================
    // Backend Information
    // =========================================================================

    fn read_only(&self) -> bool {
        false
    }

    fn backend_type(&self) -> &str {
        "mount"
    }

    fn mounts(&self) -> Vec<kaish_kernel::vfs::MountInfo> {
        // We can't async here, so return the tool mounts from CRDT backends
        let mut mounts = self.docs_tools.mounts();
        if let Some(git) = &self.git_tools {
            mounts.extend(git.mounts());
        }
        mounts
    }

    fn resolve_real_path(&self, _path: &Path) -> Option<PathBuf> {
        // MountTable's real_path is async, but this trait method is sync.
        // Callers that need real path resolution should use the VFS directly.
        // Most callers (like git builtins) already resolve through ExecContext.
        None
    }
}

impl MountBackend {
    /// Recursively remove a directory and all its contents.
    async fn remove_recursive(&self, path: &Path) -> BackendResult<()> {
        let entries = self.mount_table
            .readdir(path)
            .await
            .map_err(vfs_to_backend)?;

        for entry in &entries {
            let child = path.join(&entry.name);
            if entry.kind == FileType::Directory {
                // Recurse into subdirectory using Box::pin for async recursion
                Box::pin(self.remove_recursive(&child)).await?;
            } else {
                self.mount_table.unlink(&child).await.map_err(vfs_to_backend)?;
            }
        }

        self.mount_table.rmdir(path).await.map_err(vfs_to_backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_kernel::block_store::shared_block_store;
    use kaijutsu_kernel::Kernel as KaijutsuKernel;
    use kaijutsu_kernel::vfs::backends::MemoryBackend;

    /// Create a test MountBackend with a MemoryBackend mounted at /tmp.
    async fn test_mount_backend() -> MountBackend {
        let blocks = shared_block_store("test-mount");
        let kernel = Arc::new(KaijutsuKernel::new("test-mount").await);
        let docs = Arc::new(KaijutsuBackend::new(blocks, kernel));

        let mount_table = Arc::new(MountTable::new());
        mount_table.mount("/tmp", MemoryBackend::new()).await;

        MountBackend::new(mount_table, docs, None)
    }

    #[tokio::test]
    async fn test_write_and_read() {
        let backend = test_mount_backend().await;

        backend
            .write(Path::new("/tmp/test.txt"), b"hello world", WriteMode::Overwrite)
            .await
            .unwrap();

        let data = backend.read(Path::new("/tmp/test.txt"), None).await.unwrap();
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
        assert!(info.is_file);
        assert_eq!(info.size, 5);
    }

    #[tokio::test]
    async fn test_mkdir_and_remove() {
        let backend = test_mount_backend().await;

        backend.mkdir(Path::new("/tmp/subdir")).await.unwrap();
        assert!(backend.exists(Path::new("/tmp/subdir")).await);

        backend.remove(Path::new("/tmp/subdir"), false).await.unwrap();
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

    #[tokio::test]
    async fn test_append() {
        let backend = test_mount_backend().await;

        backend
            .write(Path::new("/tmp/append.txt"), b"hello", WriteMode::Overwrite)
            .await
            .unwrap();

        backend.append(Path::new("/tmp/append.txt"), b" world").await.unwrap();

        let data = backend.read(Path::new("/tmp/append.txt"), None).await.unwrap();
        assert_eq!(data, b"hello world");
    }
}
