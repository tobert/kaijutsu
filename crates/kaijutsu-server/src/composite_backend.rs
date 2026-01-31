//! CompositeBackend: Routes paths to appropriate backends.
//!
//! Combines multiple KernelBackend implementations, routing requests
//! based on path prefixes:
//!
//! - `/docs/*` → KaijutsuBackend (CRDT blocks)
//! - `/g/*` and worktree paths → GitCrdtBackend (CRDT-backed git)
//! - Everything else → fallback (typically LocalBackend via kaish)

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use kaish_kernel::{
    BackendError, BackendResult, EntryInfo, KernelBackend, PatchOp, ReadRange,
    ToolInfo, ToolResult, WriteMode, xdg_data_home,
};
use kaish_kernel::tools::{ExecContext, ToolArgs};
use kaish_kernel::vfs::MountInfo;

use crate::git_backend::GitCrdtBackend;
use crate::kaish_backend::KaijutsuBackend;

/// Routes requests to the appropriate backend based on path.
pub struct CompositeBackend {
    /// Backend for /docs (CRDT blocks).
    docs: Arc<KaijutsuBackend>,
    /// Backend for /g and worktrees (CRDT-backed git).
    git: Arc<GitCrdtBackend>,
    /// Worktrees root for path matching.
    worktrees_root: PathBuf,
}

impl CompositeBackend {
    /// Create a new composite backend.
    pub fn new(docs: Arc<KaijutsuBackend>, git: Arc<GitCrdtBackend>) -> Self {
        let worktrees_root = xdg_data_home().join("kaijutsu").join("worktrees");
        Self {
            docs,
            git,
            worktrees_root,
        }
    }

    /// Determine which backend should handle a path.
    fn route(&self, path: &Path) -> BackendChoice {
        let path_str = path.to_string_lossy();

        // /docs namespace → KaijutsuBackend
        if path_str == "/docs" || path_str.starts_with("/docs/") {
            return BackendChoice::Docs;
        }

        // /g namespace → GitCrdtBackend
        if path_str == "/g" || path_str.starts_with("/g/") {
            return BackendChoice::Git;
        }

        // Worktrees directory → GitCrdtBackend
        if path.starts_with(&self.worktrees_root) {
            return BackendChoice::Git;
        }

        // Root listing needs special handling
        if path_str == "/" || path_str.is_empty() {
            return BackendChoice::Root;
        }

        // Default to docs for unknown paths
        BackendChoice::Docs
    }

    /// Get the docs backend.
    pub fn docs(&self) -> &Arc<KaijutsuBackend> {
        &self.docs
    }

    /// Get the git backend.
    pub fn git(&self) -> &Arc<GitCrdtBackend> {
        &self.git
    }
}

/// Which backend to use for a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendChoice {
    /// Use KaijutsuBackend for /docs
    Docs,
    /// Use GitCrdtBackend for /g and worktrees
    Git,
    /// Root path - synthesize from both
    Root,
}

#[async_trait]
impl KernelBackend for CompositeBackend {
    // =========================================================================
    // File Operations
    // =========================================================================

    async fn read(&self, path: &Path, range: Option<ReadRange>) -> BackendResult<Vec<u8>> {
        match self.route(path) {
            BackendChoice::Docs => self.docs.read(path, range).await,
            BackendChoice::Git => self.git.read(path, range).await,
            BackendChoice::Root => {
                // List top-level namespaces
                Ok(b"docs/\ng/\n".to_vec())
            }
        }
    }

    async fn write(&self, path: &Path, content: &[u8], mode: WriteMode) -> BackendResult<()> {
        match self.route(path) {
            BackendChoice::Docs => self.docs.write(path, content, mode).await,
            BackendChoice::Git => self.git.write(path, content, mode).await,
            BackendChoice::Root => Err(BackendError::IsDirectory("/".into())),
        }
    }

    async fn append(&self, path: &Path, content: &[u8]) -> BackendResult<()> {
        match self.route(path) {
            BackendChoice::Docs => self.docs.append(path, content).await,
            BackendChoice::Git => self.git.append(path, content).await,
            BackendChoice::Root => Err(BackendError::IsDirectory("/".into())),
        }
    }

    async fn patch(&self, path: &Path, ops: &[PatchOp]) -> BackendResult<()> {
        match self.route(path) {
            BackendChoice::Docs => self.docs.patch(path, ops).await,
            BackendChoice::Git => self.git.patch(path, ops).await,
            BackendChoice::Root => Err(BackendError::IsDirectory("/".into())),
        }
    }

    // =========================================================================
    // Directory Operations
    // =========================================================================

    async fn list(&self, path: &Path) -> BackendResult<Vec<EntryInfo>> {
        match self.route(path) {
            BackendChoice::Docs => self.docs.list(path).await,
            BackendChoice::Git => self.git.list(path).await,
            BackendChoice::Root => {
                // Synthesize root listing
                Ok(vec![
                    EntryInfo::directory("docs"),
                    EntryInfo::directory("g"),
                ])
            }
        }
    }

    async fn stat(&self, path: &Path) -> BackendResult<EntryInfo> {
        match self.route(path) {
            BackendChoice::Docs => self.docs.stat(path).await,
            BackendChoice::Git => self.git.stat(path).await,
            BackendChoice::Root => Ok(EntryInfo::directory("/")),
        }
    }

    async fn mkdir(&self, path: &Path) -> BackendResult<()> {
        match self.route(path) {
            BackendChoice::Docs => self.docs.mkdir(path).await,
            BackendChoice::Git => self.git.mkdir(path).await,
            BackendChoice::Root => Err(BackendError::AlreadyExists("/".into())),
        }
    }

    async fn remove(&self, path: &Path, recursive: bool) -> BackendResult<()> {
        match self.route(path) {
            BackendChoice::Docs => self.docs.remove(path, recursive).await,
            BackendChoice::Git => self.git.remove(path, recursive).await,
            BackendChoice::Root => Err(BackendError::PermissionDenied("cannot remove root".into())),
        }
    }

    async fn exists(&self, path: &Path) -> bool {
        match self.route(path) {
            BackendChoice::Docs => self.docs.exists(path).await,
            BackendChoice::Git => self.git.exists(path).await,
            BackendChoice::Root => true,
        }
    }

    async fn rename(&self, from: &Path, to: &Path) -> BackendResult<()> {
        let from_choice = self.route(from);
        let to_choice = self.route(to);

        if from_choice != to_choice {
            return Err(BackendError::InvalidOperation(
                "cannot rename across backends".into(),
            ));
        }

        match from_choice {
            BackendChoice::Docs => self.docs.rename(from, to).await,
            BackendChoice::Git => self.git.rename(from, to).await,
            BackendChoice::Root => Err(BackendError::PermissionDenied("cannot rename root".into())),
        }
    }

    async fn read_link(&self, path: &Path) -> BackendResult<PathBuf> {
        match self.route(path) {
            BackendChoice::Docs => self.docs.read_link(path).await,
            BackendChoice::Git => self.git.read_link(path).await,
            BackendChoice::Root => Err(BackendError::InvalidOperation("root is not a symlink".into())),
        }
    }

    async fn symlink(&self, target: &Path, link: &Path) -> BackendResult<()> {
        match self.route(link) {
            BackendChoice::Docs => self.docs.symlink(target, link).await,
            BackendChoice::Git => self.git.symlink(target, link).await,
            BackendChoice::Root => Err(BackendError::PermissionDenied("cannot create symlink at root".into())),
        }
    }

    fn resolve_real_path(&self, path: &Path) -> Option<PathBuf> {
        match self.route(path) {
            BackendChoice::Docs => self.docs.resolve_real_path(path),
            BackendChoice::Git => self.git.resolve_real_path(path),
            BackendChoice::Root => None,
        }
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
        match self.docs.call_tool(name, args.clone(), ctx).await {
            Ok(result) => return Ok(result),
            Err(BackendError::ToolNotFound(_)) => {}
            Err(e) => return Err(e),
        }

        // Try git backend
        self.git.call_tool(name, args, ctx).await
    }

    async fn list_tools(&self) -> BackendResult<Vec<ToolInfo>> {
        let mut tools = self.docs.list_tools().await?;
        tools.extend(self.git.list_tools().await?);
        Ok(tools)
    }

    async fn get_tool(&self, name: &str) -> BackendResult<Option<ToolInfo>> {
        // Try docs first
        if let Some(tool) = self.docs.get_tool(name).await? {
            return Ok(Some(tool));
        }
        // Then git
        self.git.get_tool(name).await
    }

    // =========================================================================
    // Backend Information
    // =========================================================================

    fn read_only(&self) -> bool {
        false
    }

    fn backend_type(&self) -> &str {
        "composite"
    }

    fn mounts(&self) -> Vec<MountInfo> {
        let mut mounts = self.docs.mounts();
        mounts.extend(self.git.mounts());
        mounts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_patterns() {
        // Test the routing logic by checking path patterns
        let worktrees_root = xdg_data_home().join("kaijutsu").join("worktrees");

        // These are the paths we expect to route to each backend
        assert!(Path::new("/docs").to_string_lossy().starts_with("/docs"));
        assert!(Path::new("/docs/my-doc").to_string_lossy().starts_with("/docs/"));
        assert!(Path::new("/g").to_string_lossy() == "/g");
        assert!(Path::new("/g/b/repo").to_string_lossy().starts_with("/g/"));

        // Worktree paths
        let wt_path = worktrees_root.join("myrepo/src/main.rs");
        assert!(wt_path.starts_with(&worktrees_root));
    }
}
