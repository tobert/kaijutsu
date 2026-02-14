//! GitCrdtBackend: CRDT-backed git worktrees.
//!
//! This backend provides a `/g/` namespace where git worktrees have CRDT-backed
//! files for real-time collaboration. The CRDT is the source of truth; the disk
//! worktree exists for external tools (cargo, clippy, go fmt, etc.).
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    CRDT (source of truth)                   │
//! │         models, kaish, rhai, editor all edit here           │
//! └───────────────────────┬─────────────────────────────────────┘
//!                         │
//!           ┌─────────────┴─────────────┐
//!           ▼                           ▼
//!    debounced flush              notify watch
//!           │                           │
//!           ▼                           ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │                 Disk worktree (for external tools)          │
//! │              cargo, clippy, go fmt, rustfmt, etc.           │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Path Structure
//!
//! - `/g/b/{repo}/...` — Bare repo access (git objects, read-only)
//! - `~/.local/share/kaijutsu/worktrees/{repo}/...` — CRDT-backed worktree
//!
//! # Document/Block Mapping
//!
//! - One document per repo:branch (e.g., `kaijutsu:main`)
//! - One block per file (block ID derived from file path hash)
//! - Lazy loading: CRDT blocks created on first access
//!
//! # File Watching
//!
//! The backend uses `notify` to watch worktrees for external changes.
//! When external tools (cargo, clippy, etc.) modify files, the watcher
//! detects the changes and syncs them back to CRDT with attribution.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use async_trait::async_trait;
use dashmap::DashMap;
use parking_lot::RwLock;

use kaish_kernel::{
    BackendError, BackendResult, EntryInfo, GitVfs, KernelBackend, PatchOp, ReadRange,
    ToolInfo, ToolResult, WriteMode, xdg_data_home,
};
use kaish_kernel::tools::{ExecContext, ToolArgs};
use kaish_kernel::vfs::{EntryType as VfsEntryType, Filesystem, MountInfo};

use kaijutsu_crdt::BlockId;
use kaijutsu_kernel::block_store::SharedBlockStore;
use kaijutsu_kernel::db::DocumentKind;

/// Configuration for a managed git repository.
#[derive(Debug, Clone)]
pub struct RepoConfig {
    /// Name of the repository (e.g., "kaijutsu").
    pub name: String,
    /// Path to the worktree on disk.
    pub worktree_path: PathBuf,
    /// Origin URL (if cloned).
    pub origin_url: Option<String>,
    /// Current branch.
    pub branch: String,
}

/// Tracks dirty files that need flushing to disk.
struct DirtyTracker {
    /// Files marked dirty, with timestamp of last modification.
    files: DashMap<(String, String), Instant>, // (doc_id, file_path) -> last_modified
    /// Debounce duration.
    debounce: Duration,
}

impl DirtyTracker {
    fn new(debounce: Duration) -> Self {
        Self {
            files: DashMap::new(),
            debounce,
        }
    }

    fn mark_dirty(&self, doc_id: &str, file_path: &str) {
        self.files.insert((doc_id.to_string(), file_path.to_string()), Instant::now());
    }

    fn get_flushable(&self) -> Vec<(String, String)> {
        let now = Instant::now();
        self.files
            .iter()
            .filter(|entry| now.duration_since(*entry.value()) >= self.debounce)
            .map(|entry| entry.key().clone())
            .collect()
    }

    fn mark_flushed(&self, doc_id: &str, file_path: &str) {
        self.files.remove(&(doc_id.to_string(), file_path.to_string()));
    }
}

/// Attribution for external changes.
#[derive(Debug, Clone)]
pub struct ChangeAttribution {
    /// Source of the change (e.g., "clippy", "cargo", "external").
    pub source: String,
    /// Command that triggered the change (if known).
    pub command: Option<String>,
    /// Timestamp.
    pub timestamp: Instant,
}

/// File change event from the watcher.
#[derive(Debug, Clone)]
pub struct FileChangeEvent {
    /// Repository name.
    pub repo: String,
    /// File path relative to worktree root.
    pub file_path: String,
    /// Kind of change.
    pub kind: FileChangeKind,
    /// Attribution (if we triggered the change).
    pub attribution: Option<ChangeAttribution>,
}

/// Kind of file change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeKind {
    /// File was created.
    Created,
    /// File was modified.
    Modified,
    /// File was deleted.
    Deleted,
}

/// Handle to a running file watcher.
pub struct WatcherHandle {
    /// The watcher itself (keep alive to continue watching).
    _watcher: RecommendedWatcher,
    /// Sender to signal shutdown.
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

impl WatcherHandle {
    /// Stop the watcher.
    pub fn stop(self) {
        let _ = self.shutdown_tx.send(());
    }
}

/// CRDT-backed git backend.
///
/// Provides the `/g/` namespace with CRDT as the primary editing interface
/// and disk worktrees for external tool compatibility.
pub struct GitCrdtBackend {
    /// CRDT document/block storage.
    blocks: SharedBlockStore,
    /// Managed repositories.
    repos: DashMap<String, RepoConfig>,
    /// Git VFS handles (lazily opened).
    git_handles: DashMap<String, Arc<GitVfs>>,
    /// Dirty file tracker for debounced flushing.
    dirty: DirtyTracker,
    /// Base directory for worktrees.
    worktrees_root: PathBuf,
    /// Base directory for bare repos.
    repos_root: PathBuf,
    /// Pending attribution for next external change detection.
    pending_attribution: RwLock<Option<ChangeAttribution>>,
    /// Channel for file change events from the watcher.
    /// When file changes are detected, events are sent here for processing.
    watcher_event_tx: mpsc::Sender<FileChangeEvent>,
    /// Receiver for file change events (moved to watcher task).
    watcher_event_rx: RwLock<Option<mpsc::Receiver<FileChangeEvent>>>,
}

impl GitCrdtBackend {
    /// Create a new git backend.
    pub fn new(blocks: SharedBlockStore) -> Self {
        let data_dir = xdg_data_home().join("kaijutsu");
        let (tx, rx) = mpsc::channel(1024);

        Self {
            blocks,
            repos: DashMap::new(),
            git_handles: DashMap::new(),
            dirty: DirtyTracker::new(Duration::from_millis(500)),
            worktrees_root: data_dir.join("worktrees"),
            repos_root: data_dir.join("repos"),
            pending_attribution: RwLock::new(None),
            watcher_event_tx: tx,
            watcher_event_rx: RwLock::new(Some(rx)),
        }
    }

    /// Create with custom paths (for testing).
    pub fn with_paths(
        blocks: SharedBlockStore,
        worktrees_root: PathBuf,
        repos_root: PathBuf,
    ) -> Self {
        let (tx, rx) = mpsc::channel(1024);

        Self {
            blocks,
            repos: DashMap::new(),
            git_handles: DashMap::new(),
            dirty: DirtyTracker::new(Duration::from_millis(500)),
            worktrees_root,
            repos_root,
            pending_attribution: RwLock::new(None),
            watcher_event_tx: tx,
            watcher_event_rx: RwLock::new(Some(rx)),
        }
    }

    /// Set attribution for the next external command.
    ///
    /// Call this before running an external command (e.g., `cargo clippy --fix`)
    /// so that resulting file changes can be attributed.
    pub fn set_pending_attribution(&self, source: &str, command: Option<&str>) {
        *self.pending_attribution.write() = Some(ChangeAttribution {
            source: source.to_string(),
            command: command.map(|s| s.to_string()),
            timestamp: Instant::now(),
        });
    }

    /// Take and clear the pending attribution.
    fn take_attribution(&self) -> Option<ChangeAttribution> {
        self.pending_attribution.write().take()
    }

    /// Register a repository for CRDT-backed access.
    ///
    /// The repository must already exist at the given path. This registers it
    /// so that its files become accessible via the `/g/` namespace and the
    /// worktree path.
    ///
    /// # Arguments
    ///
    /// * `name` - Short name for the repo (e.g., "kaijutsu")
    /// * `path` - Path to the worktree (must contain a .git directory)
    ///
    /// # Example
    ///
    /// ```ignore
    /// backend.register_repo("kaijutsu", "/home/user/src/kaijutsu")?;
    /// // Now accessible via:
    /// //   ~/.local/share/kaijutsu/worktrees/kaijutsu/
    /// ```
    pub fn register_repo(&self, name: &str, path: impl Into<PathBuf>) -> Result<(), String> {
        let source_path: PathBuf = path.into();

        // Verify it's a git repo
        if !source_path.join(".git").exists() && !source_path.join("HEAD").exists() {
            return Err(format!("{} is not a git repository", source_path.display()));
        }

        // Open GitVfs to get current branch
        let git = GitVfs::open(&source_path)
            .map_err(|e| format!("failed to open repo: {}", e))?;

        let branch = git.current_branch()
            .map_err(|e| format!("failed to get branch: {}", e))?
            .unwrap_or_else(|| "main".to_string());

        // Create symlink in worktrees directory
        let worktree_link = self.worktrees_root.join(name);
        if !worktree_link.exists() {
            std::fs::create_dir_all(&self.worktrees_root)
                .map_err(|e| format!("failed to create worktrees dir: {}", e))?;

            #[cfg(unix)]
            std::os::unix::fs::symlink(&source_path, &worktree_link)
                .map_err(|e| format!("failed to create symlink: {}", e))?;

            #[cfg(not(unix))]
            return Err("symlinks not supported on this platform".into());
        }

        // Register the repo config
        let config = RepoConfig {
            name: name.to_string(),
            worktree_path: source_path,
            origin_url: None, // Could extract from git remote
            branch,
        };
        self.repos.insert(name.to_string(), config);

        // Cache the GitVfs handle
        self.git_handles.insert(name.to_string(), Arc::new(git));

        tracing::info!(repo = %name, "registered git repository");
        Ok(())
    }

    /// Unregister a repository.
    pub fn unregister_repo(&self, name: &str) -> Result<(), String> {
        self.repos.remove(name);
        self.git_handles.remove(name);

        // Remove symlink if we created it
        let worktree_link = self.worktrees_root.join(name);
        if worktree_link.is_symlink() {
            std::fs::remove_file(&worktree_link)
                .map_err(|e| format!("failed to remove symlink: {}", e))?;
        }

        tracing::info!(repo = %name, "unregistered git repository");
        Ok(())
    }

    /// List registered repositories.
    pub fn list_repos(&self) -> Vec<String> {
        self.repos.iter().map(|r| r.key().clone()).collect()
    }

    /// Get the worktrees root directory.
    pub fn worktrees_root(&self) -> &Path {
        &self.worktrees_root
    }

    /// Resolve a VFS path to determine what it refers to.
    fn resolve_path(&self, path: &Path) -> PathResolution {
        let path_str = path.to_string_lossy();
        let components: Vec<&str> = path_str
            .trim_start_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();

        // Reject path traversal attempts
        if components.iter().any(|c| *c == "..") {
            return PathResolution::Outside;
        }

        match components.as_slice() {
            // Root: /g
            [] | ["g"] => PathResolution::GitRoot,

            // Bare repos root: /g/b
            ["g", "b"] => PathResolution::BareRoot,

            // Bare repo: /g/b/{repo}
            ["g", "b", repo] => PathResolution::BareRepo(repo.to_string()),

            // Bare repo path: /g/b/{repo}/{path...}
            ["g", "b", repo, rest @ ..] => {
                PathResolution::BareRepoPath(repo.to_string(), rest.join("/"))
            }

            // Check if it's under our worktrees directory
            _ => {
                let full_path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    PathBuf::from("/").join(path)
                };

                // Check if path is under worktrees_root
                if let Ok(rest) = full_path.strip_prefix(&self.worktrees_root) {
                    let rest_str = rest.to_string_lossy();
                    let parts: Vec<&str> = rest_str
                        .split('/')
                        .filter(|s| !s.is_empty())
                        .collect();

                    match parts.as_slice() {
                        [] => PathResolution::WorktreesRoot,
                        [repo] => PathResolution::Worktree(repo.to_string()),
                        [repo, rest @ ..] => {
                            PathResolution::WorktreeFile(repo.to_string(), rest.join("/"))
                        }
                    }
                } else {
                    PathResolution::Outside
                }
            }
        }
    }

    /// Get or create the document ID for a repo:branch.
    fn doc_id(&self, repo: &str, branch: &str) -> String {
        format!("{}:{}", repo, branch)
    }

    /// Get or create a block ID from a file path.
    ///
    /// Uses a stable hash of the file path to generate a predictable block ID.
    fn block_id_for_path(&self, doc_id: &str, file_path: &str) -> BlockId {
        // Use a stable hash of the file path for the sequence number
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        file_path.hash(&mut hasher);
        let hash = hasher.finish();

        // Use "git" as the agent ID for file blocks
        BlockId::new(doc_id, "git", hash)
    }

    /// Ensure a document exists for a repo:branch.
    fn ensure_document(&self, repo: &str, branch: &str) -> Result<String, String> {
        let doc_id = self.doc_id(repo, branch);

        if !self.blocks.contains(&doc_id) {
            self.blocks.create_document(doc_id.clone(), DocumentKind::Git, None)?;
        }

        Ok(doc_id)
    }

    /// Get or open a GitVfs handle for a repository.
    fn get_git_handle(&self, repo: &str) -> BackendResult<Arc<GitVfs>> {
        if let Some(handle) = self.git_handles.get(repo) {
            return Ok(handle.clone());
        }

        let worktree_path = self.worktrees_root.join(repo);
        if !worktree_path.exists() {
            return Err(BackendError::NotFound(format!("repository not found: {}", repo)));
        }

        let git = GitVfs::open(&worktree_path)
            .map_err(|e| BackendError::Io(format!("failed to open git repo: {}", e)))?;

        let handle = Arc::new(git);
        self.git_handles.insert(repo.to_string(), handle.clone());
        Ok(handle)
    }

    /// Get the current branch for a repository.
    fn current_branch(&self, repo: &str) -> BackendResult<String> {
        let git = self.get_git_handle(repo)?;
        git.current_branch()
            .map_err(|e| BackendError::Io(e.to_string()))?
            .ok_or_else(|| BackendError::Io("detached HEAD".to_string()))
    }

    /// Load a file from disk into CRDT (lazy loading).
    async fn load_file_to_crdt(&self, repo: &str, file_path: &str) -> BackendResult<BlockId> {
        let branch = self.current_branch(repo)?;
        let doc_id = self.ensure_document(repo, &branch)
            .map_err(|e| BackendError::Io(e))?;

        let block_id = self.block_id_for_path(&doc_id, file_path);

        // Check if block already exists
        if let Some(entry) = self.blocks.get(&doc_id) {
            let blocks = entry.doc.blocks_ordered();
            if blocks.iter().any(|b| b.id == block_id) {
                return Ok(block_id);
            }
        }

        // Read from disk
        let disk_path = self.worktrees_root.join(repo).join(file_path);
        let content = tokio::fs::read_to_string(&disk_path)
            .await
            .map_err(|e| BackendError::Io(format!("failed to read {}: {}", disk_path.display(), e)))?;

        // Create block with content
        self.blocks.insert_block(
            &doc_id,
            None, // no parent
            None, // at end
            kaijutsu_crdt::Role::System,
            kaijutsu_crdt::BlockKind::Text,
            content,
        ).map_err(|e| BackendError::Io(e))?;

        Ok(block_id)
    }

    /// Flush a file from CRDT to disk.
    async fn flush_file_to_disk(&self, repo: &str, file_path: &str) -> BackendResult<()> {
        let branch = self.current_branch(repo)?;
        let doc_id = self.doc_id(repo, &branch);
        let block_id = self.block_id_for_path(&doc_id, file_path);

        // Get block content
        let content = {
            let entry = self.blocks.get(&doc_id)
                .ok_or_else(|| BackendError::NotFound(format!("document not found: {}", doc_id)))?;
            let blocks = entry.doc.blocks_ordered();
            let block = blocks.iter()
                .find(|b| b.id == block_id)
                .ok_or_else(|| BackendError::NotFound(format!("block not found for: {}", file_path)))?;
            block.content.clone()
        };

        // Write to disk
        let disk_path = self.worktrees_root.join(repo).join(file_path);
        if let Some(parent) = disk_path.parent() {
            tokio::fs::create_dir_all(parent).await
                .map_err(|e| BackendError::Io(format!("failed to create dir: {}", e)))?;
        }
        tokio::fs::write(&disk_path, &content).await
            .map_err(|e| BackendError::Io(format!("failed to write {}: {}", disk_path.display(), e)))?;

        self.dirty.mark_flushed(&doc_id, file_path);
        Ok(())
    }

    /// Flush all dirty files to disk.
    ///
    /// Attempts to flush every dirty file. Collects errors and returns them
    /// after flushing all remaining files (does not abort on first failure).
    pub async fn flush_all(&self) -> BackendResult<()> {
        let flushable = self.dirty.get_flushable();
        let mut errors: Vec<String> = Vec::new();

        for (doc_id, file_path) in flushable {
            // Parse doc_id to get repo
            let parts: Vec<&str> = doc_id.split(':').collect();
            if let [repo, _branch] = parts.as_slice() {
                if let Err(e) = self.flush_file_to_disk(repo, &file_path).await {
                    tracing::warn!(repo = %repo, file = %file_path, error = %e, "failed to flush file");
                    errors.push(format!("{}:{}: {}", repo, file_path, e));
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(BackendError::Io(format!("flush_all: {} files failed: {}", errors.len(), errors.join("; "))))
        }
    }

    /// Check if a file is text (not binary).
    fn is_text_file(content: &[u8]) -> bool {
        // No null bytes in first 8KB
        !content.get(..8192).unwrap_or(content).contains(&0)
    }

    // =========================================================================
    // File Watcher
    // =========================================================================

    /// Start the file watcher for all registered repositories.
    ///
    /// The watcher uses `notify` to detect external file changes (from cargo,
    /// clippy, go fmt, etc.) and syncs them back to CRDT.
    ///
    /// Returns a handle that can be used to stop the watcher.
    pub fn start_watcher(self: &Arc<Self>) -> Result<WatcherHandle, String> {
        let backend = Arc::clone(self);
        let tx = self.watcher_event_tx.clone();
        let worktrees_root = self.worktrees_root.clone();

        // Create the file watcher
        let mut watcher = RecommendedWatcher::new(
            move |result: Result<Event, notify::Error>| {
                if let Ok(event) = result {
                    // Only care about create/modify/remove events
                    let kind = match event.kind {
                        EventKind::Create(_) => Some(FileChangeKind::Created),
                        EventKind::Modify(_) => Some(FileChangeKind::Modified),
                        EventKind::Remove(_) => Some(FileChangeKind::Deleted),
                        _ => None,
                    };

                    if let Some(kind) = kind {
                        for path in event.paths {
                            // Determine which repo this path belongs to
                            if let Ok(rel_path) = path.strip_prefix(&worktrees_root) {
                                let parts: Vec<_> = rel_path.components().collect();
                                if parts.len() >= 2 {
                                    let repo = parts[0].as_os_str().to_string_lossy().to_string();
                                    let file_path: PathBuf = parts[1..].iter().collect();

                                    // Skip .git directory
                                    if file_path.starts_with(".git") {
                                        continue;
                                    }

                                    let file_path_str = file_path.to_string_lossy().to_string();

                                    // Send event (non-blocking)
                                    let _ = tx.try_send(FileChangeEvent {
                                        repo,
                                        file_path: file_path_str,
                                        kind,
                                        attribution: None, // Will be filled by processor
                                    });
                                }
                            }
                        }
                    }
                }
            },
            notify::Config::default()
                .with_poll_interval(Duration::from_millis(500)),
        ).map_err(|e| format!("failed to create watcher: {}", e))?;

        // Watch the worktrees directory
        watcher.watch(&self.worktrees_root, RecursiveMode::Recursive)
            .map_err(|e| format!("failed to watch {}: {}", self.worktrees_root.display(), e))?;

        // Create shutdown channel
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();

        // Take the event receiver
        let rx = self.watcher_event_rx.write().take()
            .ok_or_else(|| "watcher already started".to_string())?;

        // Spawn the event processor task
        tokio::spawn(async move {
            let mut rx = rx;
            let mut debounce_map: std::collections::HashMap<(String, String), Instant> = std::collections::HashMap::new();
            let debounce_duration = Duration::from_millis(100);

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        tracing::info!("file watcher shutting down");
                        break;
                    }
                    Some(event) = rx.recv() => {
                        // Debounce: skip if we saw this file very recently
                        let key = (event.repo.clone(), event.file_path.clone());
                        let now = Instant::now();

                        if let Some(last) = debounce_map.get(&key) {
                            if now.duration_since(*last) < debounce_duration {
                                continue;
                            }
                        }
                        debounce_map.insert(key, now);

                        // Take pending attribution if available
                        let attribution = backend.take_attribution();
                        let event = FileChangeEvent {
                            attribution,
                            ..event
                        };

                        // Process the event
                        if let Err(e) = backend.sync_external_change(&event).await {
                            tracing::warn!(
                                repo = %event.repo,
                                file = %event.file_path,
                                error = %e,
                                "failed to sync external change"
                            );
                        } else {
                            tracing::debug!(
                                repo = %event.repo,
                                file = %event.file_path,
                                kind = ?event.kind,
                                source = ?event.attribution.as_ref().map(|a| &a.source),
                                "synced external change to CRDT"
                            );
                        }
                    }
                }
            }
        });

        tracing::info!(path = %self.worktrees_root.display(), "file watcher started");

        Ok(WatcherHandle {
            _watcher: watcher,
            shutdown_tx,
        })
    }

    /// Sync an external file change to CRDT.
    ///
    /// Called when the file watcher detects a change from an external tool.
    pub async fn sync_external_change(&self, event: &FileChangeEvent) -> BackendResult<()> {
        let branch = self.current_branch(&event.repo)?;
        let doc_id = self.ensure_document(&event.repo, &branch)
            .map_err(|e| BackendError::Io(e))?;
        let block_id = self.block_id_for_path(&doc_id, &event.file_path);

        match event.kind {
            FileChangeKind::Created | FileChangeKind::Modified => {
                // Read file from disk
                let disk_path = self.worktrees_root.join(&event.repo).join(&event.file_path);
                let content = match tokio::fs::read(&disk_path).await {
                    Ok(c) => c,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()), // File gone
                    Err(e) => return Err(BackendError::Io(e.to_string())),
                };

                // Skip binary files
                if !Self::is_text_file(&content) {
                    tracing::debug!(file = %event.file_path, "skipping binary file");
                    return Ok(());
                }

                let content_str = std::str::from_utf8(&content)
                    .map_err(|e| BackendError::Io(e.to_string()))?;

                // Check if block exists
                let block_exists = if let Some(entry) = self.blocks.get(&doc_id) {
                    entry.doc.blocks_ordered().iter().any(|b| b.id == block_id)
                } else {
                    false
                };

                if block_exists {
                    // Replace content
                    let current_len = {
                        let entry = self.blocks.get(&doc_id).unwrap();
                        let blocks = entry.doc.blocks_ordered();
                        blocks.iter().find(|b| b.id == block_id).map(|b| b.content.len()).unwrap_or(0)
                    };

                    self.blocks.edit_text(&doc_id, &block_id, 0, content_str, current_len)
                        .map_err(|e| BackendError::Io(e))?;
                } else {
                    // Create new block
                    self.blocks.insert_block(
                        &doc_id,
                        None,
                        None,
                        kaijutsu_crdt::Role::System,
                        kaijutsu_crdt::BlockKind::Text,
                        content_str,
                    ).map_err(|e| BackendError::Io(e))?;
                }
            }
            FileChangeKind::Deleted => {
                // Delete the block
                let _ = self.blocks.delete_block(&doc_id, &block_id);
            }
        }

        Ok(())
    }

    // =========================================================================
    // Branch Switching
    // =========================================================================

    /// Switch to a different branch for a repository.
    ///
    /// This creates a new document for the branch (lazy loading).
    /// Any dirty files on the current branch are flushed first.
    pub async fn switch_branch(&self, repo: &str, target_branch: &str) -> BackendResult<()> {
        // Flush any dirty files on current branch first
        self.flush_all().await?;

        // Get git handle and checkout
        let git = self.get_git_handle(repo)?;
        git.checkout(target_branch)
            .map_err(|e| BackendError::Io(format!("failed to checkout {}: {}", target_branch, e)))?;

        // Update repo config
        if let Some(mut config) = self.repos.get_mut(repo) {
            config.branch = target_branch.to_string();
        }

        // Ensure document exists for new branch (lazy creation)
        self.ensure_document(repo, target_branch)
            .map_err(|e| BackendError::Io(e))?;

        tracing::info!(repo = %repo, branch = %target_branch, "switched branch");
        Ok(())
    }

    /// Get the current branch for a repository.
    pub fn get_current_branch(&self, repo: &str) -> Option<String> {
        self.repos.get(repo).map(|r| r.branch.clone())
    }

    /// List all branches for a repository.
    pub fn list_branches(&self, repo: &str) -> BackendResult<Vec<String>> {
        let git = self.get_git_handle(repo)?;
        git.branches()
            .map_err(|e| BackendError::Io(e.to_string()))
    }

    /// Create a new branch.
    ///
    /// If `start_point` is provided, the branch is created from that commit.
    /// Otherwise, it's created from HEAD.
    pub fn create_branch(&self, repo: &str, name: &str, start_point: Option<&str>) -> BackendResult<()> {
        let git = self.get_git_handle(repo)?;

        // If we need a specific start point, checkout there first, create branch, then checkout back
        if let Some(_start) = start_point {
            // TODO: Support start_point - requires git2 ReflogEntry or commit lookup
            // For now, just create from HEAD
            tracing::warn!("start_point for create_branch not yet implemented, creating from HEAD");
        }

        git.create_branch(name)
            .map_err(|e| BackendError::Io(format!("failed to create branch {}: {}", name, e)))
    }
}

/// Result of resolving a VFS path.
#[derive(Debug)]
enum PathResolution {
    /// Root of git namespace (`/g`)
    GitRoot,
    /// Bare repos root (`/g/b`)
    BareRoot,
    /// A bare repository (`/g/b/{repo}`)
    BareRepo(String),
    /// Path within a bare repo (`/g/b/{repo}/{path}`)
    BareRepoPath(String, String),
    /// Worktrees root
    WorktreesRoot,
    /// A worktree directory
    Worktree(String),
    /// A file within a worktree
    WorktreeFile(String, String),
    /// Path is outside our managed directories
    Outside,
}

#[async_trait]
impl KernelBackend for GitCrdtBackend {
    // =========================================================================
    // File Operations
    // =========================================================================

    async fn read(&self, path: &Path, range: Option<ReadRange>) -> BackendResult<Vec<u8>> {
        match self.resolve_path(path) {
            PathResolution::GitRoot => {
                Ok(b"b/\n".to_vec()) // List available namespaces
            }
            PathResolution::BareRoot => {
                // List bare repos
                let mut listing = String::new();
                if self.repos_root.exists() {
                    if let Ok(entries) = std::fs::read_dir(&self.repos_root) {
                        for entry in entries.flatten() {
                            if let Some(name) = entry.file_name().to_str() {
                                if name.ends_with(".git") {
                                    listing.push_str(&name[..name.len()-4]);
                                    listing.push('\n');
                                }
                            }
                        }
                    }
                }
                Ok(listing.into_bytes())
            }
            PathResolution::WorktreesRoot => {
                // List worktrees
                let mut listing = String::new();
                if self.worktrees_root.exists() {
                    if let Ok(entries) = std::fs::read_dir(&self.worktrees_root) {
                        for entry in entries.flatten() {
                            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                                if let Some(name) = entry.file_name().to_str() {
                                    listing.push_str(name);
                                    listing.push('/');
                                    listing.push('\n');
                                }
                            }
                        }
                    }
                }
                Ok(listing.into_bytes())
            }
            PathResolution::Worktree(repo) => {
                // List files in worktree root (via CRDT if loaded, else disk)
                let git = self.get_git_handle(&repo)?;
                let entries = git.list(Path::new(""))
                    .await
                    .map_err(|e| BackendError::Io(e.to_string()))?;
                let listing: String = entries.iter()
                    .map(|e| format!("{}\n", e.name))
                    .collect();
                Ok(listing.into_bytes())
            }
            PathResolution::WorktreeFile(repo, file_path) => {
                // Read from CRDT (loading from disk if needed)
                let block_id = self.load_file_to_crdt(&repo, &file_path).await?;
                let branch = self.current_branch(&repo)?;
                let doc_id = self.doc_id(&repo, &branch);

                let entry = self.blocks.get(&doc_id)
                    .ok_or_else(|| BackendError::NotFound(doc_id.clone()))?;
                let blocks = entry.doc.blocks_ordered();
                let block = blocks.iter()
                    .find(|b| b.id == block_id)
                    .ok_or_else(|| BackendError::NotFound(file_path.clone()))?;

                let content = &block.content;

                // Apply range if specified
                let output = if let Some(range) = range {
                    apply_read_range(content, range)
                } else {
                    content.clone()
                };

                Ok(output.into_bytes())
            }
            PathResolution::BareRepo(_) | PathResolution::BareRepoPath(_, _) => {
                // Bare repos are read-only, delegate to disk
                Err(BackendError::InvalidOperation("bare repo access not yet implemented".into()))
            }
            PathResolution::Outside => {
                Err(BackendError::NotFound(path.to_string_lossy().to_string()))
            }
        }
    }

    async fn write(&self, path: &Path, content: &[u8], _mode: WriteMode) -> BackendResult<()> {
        let content_str = std::str::from_utf8(content)
            .map_err(|e| BackendError::Io(e.to_string()))?;

        match self.resolve_path(path) {
            PathResolution::WorktreeFile(repo, file_path) => {
                let branch = self.current_branch(&repo)?;
                let doc_id = self.ensure_document(&repo, &branch)
                    .map_err(|e| BackendError::Io(e))?;
                let block_id = self.block_id_for_path(&doc_id, &file_path);

                // Check if block exists
                let block_exists = if let Some(entry) = self.blocks.get(&doc_id) {
                    entry.doc.blocks_ordered().iter().any(|b| b.id == block_id)
                } else {
                    false
                };

                if block_exists {
                    // Replace content
                    let current_len = {
                        let entry = self.blocks.get(&doc_id).unwrap();
                        let blocks = entry.doc.blocks_ordered();
                        blocks.iter().find(|b| b.id == block_id).map(|b| b.content.len()).unwrap_or(0)
                    };

                    self.blocks.edit_text(&doc_id, &block_id, 0, content_str, current_len)
                        .map_err(|e| BackendError::Io(e))?;
                } else {
                    // Create new block
                    self.blocks.insert_block(
                        &doc_id,
                        None,
                        None,
                        kaijutsu_crdt::Role::System,
                        kaijutsu_crdt::BlockKind::Text,
                        content_str,
                    ).map_err(|e| BackendError::Io(e))?;
                }

                // Mark dirty for flush
                self.dirty.mark_dirty(&doc_id, &file_path);

                Ok(())
            }
            PathResolution::BareRepo(_) | PathResolution::BareRepoPath(_, _) => {
                Err(BackendError::PermissionDenied("bare repos are read-only".into()))
            }
            PathResolution::GitRoot | PathResolution::BareRoot |
            PathResolution::WorktreesRoot | PathResolution::Worktree(_) => {
                Err(BackendError::IsDirectory(path.to_string_lossy().to_string()))
            }
            PathResolution::Outside => {
                Err(BackendError::NotFound(path.to_string_lossy().to_string()))
            }
        }
    }

    async fn append(&self, path: &Path, content: &[u8]) -> BackendResult<()> {
        let content_str = std::str::from_utf8(content)
            .map_err(|e| BackendError::Io(e.to_string()))?;

        match self.resolve_path(path) {
            PathResolution::WorktreeFile(repo, file_path) => {
                // Ensure file is loaded
                let block_id = self.load_file_to_crdt(&repo, &file_path).await?;
                let branch = self.current_branch(&repo)?;
                let doc_id = self.doc_id(&repo, &branch);

                self.blocks.append_text(&doc_id, &block_id, content_str)
                    .map_err(|e| BackendError::Io(e))?;

                self.dirty.mark_dirty(&doc_id, &file_path);
                Ok(())
            }
            _ => Err(BackendError::InvalidOperation("can only append to worktree files".into())),
        }
    }

    async fn patch(&self, _path: &Path, _ops: &[PatchOp]) -> BackendResult<()> {
        // Similar to kaish_backend.rs implementation
        Err(BackendError::InvalidOperation("patch not yet implemented for git backend".into()))
    }

    // =========================================================================
    // Directory Operations
    // =========================================================================

    async fn list(&self, path: &Path) -> BackendResult<Vec<EntryInfo>> {
        match self.resolve_path(path) {
            PathResolution::GitRoot => {
                Ok(vec![EntryInfo::directory("b")])
            }
            PathResolution::BareRoot => {
                let mut entries = Vec::new();
                if self.repos_root.exists() {
                    if let Ok(dir_entries) = std::fs::read_dir(&self.repos_root) {
                        for entry in dir_entries.flatten() {
                            if let Some(name) = entry.file_name().to_str() {
                                if name.ends_with(".git") {
                                    entries.push(EntryInfo::directory(&name[..name.len()-4]));
                                }
                            }
                        }
                    }
                }
                Ok(entries)
            }
            PathResolution::WorktreesRoot => {
                let mut entries = Vec::new();
                if self.worktrees_root.exists() {
                    if let Ok(dir_entries) = std::fs::read_dir(&self.worktrees_root) {
                        for entry in dir_entries.flatten() {
                            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                                if let Some(name) = entry.file_name().to_str() {
                                    entries.push(EntryInfo::directory(name));
                                }
                            }
                        }
                    }
                }
                Ok(entries)
            }
            PathResolution::Worktree(repo) | PathResolution::WorktreeFile(repo, _) => {
                let git = self.get_git_handle(&repo)?;
                let subpath = match self.resolve_path(path) {
                    PathResolution::Worktree(_) => PathBuf::new(),
                    PathResolution::WorktreeFile(_, p) => PathBuf::from(p),
                    _ => unreachable!(),
                };

                git.list(&subpath)
                    .await
                    .map(|entries| {
                        entries.into_iter()
                            .map(|e| {
                                if e.entry_type == VfsEntryType::Directory {
                                    EntryInfo::directory(&e.name)
                                } else {
                                    EntryInfo::file(&e.name, e.size)
                                }
                            })
                            .collect()
                    })
                    .map_err(|e| BackendError::Io(e.to_string()))
            }
            PathResolution::BareRepo(_) | PathResolution::BareRepoPath(_, _) => {
                Err(BackendError::InvalidOperation("bare repo listing not yet implemented".into()))
            }
            PathResolution::Outside => {
                Err(BackendError::NotFound(path.to_string_lossy().to_string()))
            }
        }
    }

    async fn stat(&self, path: &Path) -> BackendResult<EntryInfo> {
        match self.resolve_path(path) {
            PathResolution::GitRoot => Ok(EntryInfo::directory("g")),
            PathResolution::BareRoot => Ok(EntryInfo::directory("b")),
            PathResolution::WorktreesRoot => Ok(EntryInfo::directory("worktrees")),
            PathResolution::Worktree(repo) => {
                if self.worktrees_root.join(&repo).exists() {
                    Ok(EntryInfo::directory(&repo))
                } else {
                    Err(BackendError::NotFound(repo))
                }
            }
            PathResolution::WorktreeFile(repo, file_path) => {
                let disk_path = self.worktrees_root.join(&repo).join(&file_path);
                let meta = tokio::fs::metadata(&disk_path).await
                    .map_err(|_| BackendError::NotFound(file_path.clone()))?;

                if meta.is_dir() {
                    Ok(EntryInfo::directory(&file_path))
                } else {
                    Ok(EntryInfo::file(&file_path, meta.len()))
                }
            }
            PathResolution::BareRepo(repo) => {
                let bare_path = self.repos_root.join(format!("{}.git", repo));
                if bare_path.exists() {
                    Ok(EntryInfo::directory(&repo))
                } else {
                    Err(BackendError::NotFound(repo))
                }
            }
            PathResolution::BareRepoPath(_, _) => {
                Err(BackendError::InvalidOperation("bare repo stat not yet implemented".into()))
            }
            PathResolution::Outside => {
                Err(BackendError::NotFound(path.to_string_lossy().to_string()))
            }
        }
    }

    async fn mkdir(&self, path: &Path) -> BackendResult<()> {
        match self.resolve_path(path) {
            PathResolution::WorktreeFile(repo, dir_path) => {
                let disk_path = self.worktrees_root.join(&repo).join(&dir_path);
                tokio::fs::create_dir_all(&disk_path).await
                    .map_err(|e| BackendError::Io(e.to_string()))
            }
            _ => Err(BackendError::InvalidOperation("can only mkdir in worktrees".into())),
        }
    }

    async fn remove(&self, path: &Path, recursive: bool) -> BackendResult<()> {
        match self.resolve_path(path) {
            PathResolution::WorktreeFile(repo, file_path) => {
                // Remove from CRDT
                let branch = self.current_branch(&repo)?;
                let doc_id = self.doc_id(&repo, &branch);
                let block_id = self.block_id_for_path(&doc_id, &file_path);

                let _ = self.blocks.delete_block(&doc_id, &block_id);

                // Remove from disk
                let disk_path = self.worktrees_root.join(&repo).join(&file_path);
                if disk_path.is_dir() {
                    if recursive {
                        tokio::fs::remove_dir_all(&disk_path).await
                    } else {
                        tokio::fs::remove_dir(&disk_path).await
                    }
                } else {
                    tokio::fs::remove_file(&disk_path).await
                }.map_err(|e| BackendError::Io(e.to_string()))
            }
            _ => Err(BackendError::InvalidOperation("can only remove worktree files".into())),
        }
    }

    async fn exists(&self, path: &Path) -> bool {
        match self.resolve_path(path) {
            PathResolution::GitRoot | PathResolution::BareRoot | PathResolution::WorktreesRoot => true,
            PathResolution::Worktree(repo) => self.worktrees_root.join(&repo).exists(),
            PathResolution::WorktreeFile(repo, file_path) => {
                self.worktrees_root.join(&repo).join(&file_path).exists()
            }
            PathResolution::BareRepo(repo) => {
                self.repos_root.join(format!("{}.git", repo)).exists()
            }
            PathResolution::BareRepoPath(repo, git_path) => {
                self.repos_root.join(format!("{}.git", repo)).join(&git_path).exists()
            }
            PathResolution::Outside => false,
        }
    }

    async fn rename(&self, _from: &Path, _to: &Path) -> BackendResult<()> {
        Err(BackendError::InvalidOperation("rename not yet implemented for git backend".into()))
    }

    async fn read_link(&self, _path: &Path) -> BackendResult<PathBuf> {
        Err(BackendError::InvalidOperation("symlinks not supported in git backend".into()))
    }

    async fn symlink(&self, _target: &Path, _link: &Path) -> BackendResult<()> {
        Err(BackendError::InvalidOperation("symlinks not supported in git backend".into()))
    }

    fn resolve_real_path(&self, path: &Path) -> Option<PathBuf> {
        match self.resolve_path(path) {
            PathResolution::Worktree(repo) => Some(self.worktrees_root.join(&repo)),
            PathResolution::WorktreeFile(repo, file_path) => {
                Some(self.worktrees_root.join(&repo).join(&file_path))
            }
            PathResolution::BareRepo(repo) => Some(self.repos_root.join(format!("{}.git", repo))),
            PathResolution::BareRepoPath(repo, git_path) => {
                Some(self.repos_root.join(format!("{}.git", repo)).join(&git_path))
            }
            _ => None,
        }
    }

    // =========================================================================
    // Tool Dispatch (git operations)
    // =========================================================================

    async fn call_tool(
        &self,
        name: &str,
        _args: ToolArgs,
        _ctx: &mut ExecContext,
    ) -> BackendResult<ToolResult> {
        // Git operations should go through kaish's git builtins
        // This backend doesn't provide its own tools
        Err(BackendError::ToolNotFound(name.to_string()))
    }

    async fn list_tools(&self) -> BackendResult<Vec<ToolInfo>> {
        Ok(vec![]) // No custom tools, use kaish's git builtins
    }

    async fn get_tool(&self, _name: &str) -> BackendResult<Option<ToolInfo>> {
        Ok(None)
    }

    // =========================================================================
    // Backend Information
    // =========================================================================

    fn read_only(&self) -> bool {
        false
    }

    fn backend_type(&self) -> &str {
        "git-crdt"
    }

    fn mounts(&self) -> Vec<MountInfo> {
        vec![
            MountInfo {
                path: PathBuf::from("/g"),
                read_only: false,
            },
            MountInfo {
                path: self.worktrees_root.clone(),
                read_only: false,
            },
        ]
    }
}

/// Apply a read range to content, returning the subset.
fn apply_read_range(content: &str, range: ReadRange) -> String {
    if range.start_line.is_some() || range.end_line.is_some() {
        let lines: Vec<&str> = content.lines().collect();
        let start = range.start_line.unwrap_or(1).saturating_sub(1);
        let end = range.end_line.unwrap_or(lines.len()).min(lines.len());
        return lines.get(start..end)
            .map(|slice| slice.join("\n"))
            .unwrap_or_default();
    }

    if range.offset.is_some() || range.limit.is_some() {
        let offset = range.offset.unwrap_or(0) as usize;
        let limit = range.limit.unwrap_or(content.len() as u64) as usize;
        let end = (offset + limit).min(content.len());
        return content.get(offset..end).unwrap_or("").to_string();
    }

    content.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_kernel::block_store::shared_block_store;

    #[test]
    fn test_path_resolution() {
        let blocks = shared_block_store("test");
        let backend = GitCrdtBackend::new(blocks);

        // Test /g paths
        assert!(matches!(backend.resolve_path(Path::new("/g")), PathResolution::GitRoot));
        assert!(matches!(backend.resolve_path(Path::new("/g/b")), PathResolution::BareRoot));
        assert!(matches!(
            backend.resolve_path(Path::new("/g/b/myrepo")),
            PathResolution::BareRepo(ref r) if r == "myrepo"
        ));
        assert!(matches!(
            backend.resolve_path(Path::new("/g/b/myrepo/objects")),
            PathResolution::BareRepoPath(ref r, ref p) if r == "myrepo" && p == "objects"
        ));
    }

    #[test]
    fn test_block_id_for_path() {
        let blocks = shared_block_store("test");
        let backend = GitCrdtBackend::new(blocks);

        let doc_id = "test:main";

        // Same path should always produce same block ID
        let id1 = backend.block_id_for_path(doc_id, "src/main.rs");
        let id2 = backend.block_id_for_path(doc_id, "src/main.rs");
        assert_eq!(id1, id2);

        // Different paths should produce different IDs
        let id3 = backend.block_id_for_path(doc_id, "src/lib.rs");
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_doc_id() {
        let blocks = shared_block_store("test");
        let backend = GitCrdtBackend::new(blocks);

        assert_eq!(backend.doc_id("kaijutsu", "main"), "kaijutsu:main");
        assert_eq!(backend.doc_id("myrepo", "feat/foo"), "myrepo:feat/foo");
    }

    // Part 4a: Path security tests

    #[test]
    fn test_resolve_path_rejects_parent_dir() {
        let blocks = shared_block_store("test");
        let backend = GitCrdtBackend::new(blocks);

        // Path with .. should be rejected as Outside
        let result = backend.resolve_path(Path::new("/g/b/../etc/passwd"));
        assert!(matches!(result, PathResolution::Outside));

        // Path with .. in the middle
        let result = backend.resolve_path(Path::new("/g/b/repo/../../../etc/passwd"));
        assert!(matches!(result, PathResolution::Outside));

        // Just .. alone
        let result = backend.resolve_path(Path::new(".."));
        assert!(matches!(result, PathResolution::Outside));
    }

    #[test]
    fn test_resolve_path_normal_paths_work() {
        let blocks = shared_block_store("test");
        let backend = GitCrdtBackend::new(blocks);

        // Normal paths should resolve correctly
        assert!(matches!(backend.resolve_path(Path::new("/g")), PathResolution::GitRoot));
        assert!(matches!(backend.resolve_path(Path::new("/g/b")), PathResolution::BareRoot));
        assert!(matches!(
            backend.resolve_path(Path::new("/g/b/myrepo")),
            PathResolution::BareRepo(ref r) if r == "myrepo"
        ));
        assert!(matches!(
            backend.resolve_path(Path::new("/g/b/myrepo/src/main.rs")),
            PathResolution::BareRepoPath(ref r, ref p) if r == "myrepo" && p == "src/main.rs"
        ));
    }
}
