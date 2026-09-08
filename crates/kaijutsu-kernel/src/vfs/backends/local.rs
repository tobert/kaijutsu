//! Local filesystem backend.
//!
//! Provides access to real filesystem paths, with path security
//! to prevent escaping the root directory.

use async_trait::async_trait;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio::fs;

use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::ops::VfsOps;
use crate::vfs::types::{DirEntry, FileAttr, FileType, SetAttr, StatFs};

/// Local filesystem backend.
///
/// All operations are relative to `root`. For example, if `root` is
/// `/home/amy/project`, then `read("src/main.rs")` reads
/// `/home/amy/project/src/main.rs`.
///
/// Path security is enforced: attempts to escape via `..` are blocked.
#[derive(Debug, Clone)]
pub struct LocalBackend {
    root: PathBuf,
    read_only: bool,
    opaque: bool,
}

impl LocalBackend {
    /// Create a new local filesystem rooted at the given path.
    ///
    /// The root is canonicalized at construction time to handle symlinks
    /// (e.g. macOS `/tmp` → `/private/tmp`).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root: PathBuf = root.into();
        let root = root.canonicalize().unwrap_or(root);
        Self {
            root,
            read_only: false,
            opaque: false,
        }
    }

    /// Create a read-only local filesystem.
    pub fn read_only(root: impl Into<PathBuf>) -> Self {
        let root: PathBuf = root.into();
        let root = root.canonicalize().unwrap_or(root);
        Self {
            root,
            read_only: true,
            opaque: false,
        }
    }

    /// Set whether this filesystem is read-only.
    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    /// Mark this mount opaque to ambient sweeps (`opaque_to_sweeps` —
    /// `docs/scenes/vfs.md`). The mount stays directly readable (`kj ls`,
    /// SFTP, an explicit dive); only the FSN backdrop/world walk stops at
    /// this node instead of descending. Intended for host directories whose
    /// listing is a live view of kernel-process state rather than a real
    /// tree — `/dev/fd` on macOS reflects the calling process's own open
    /// file descriptors and races `readdir`+`lstat` into stray `EBADF`s
    /// (live-caught 2026-07-22).
    pub fn opaque(mut self, opaque: bool) -> Self {
        self.opaque = opaque;
        self
    }

    /// Get the root path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Canonicalize the deepest ancestor that exists and append the rest
    /// literally.
    ///
    /// A component that does not exist cannot be a symlink, so canonical
    /// containment on the existing prefix is containment on the whole path.
    /// Resolving only the immediate parent is not enough: when the parent is
    /// itself missing, an intermediate symlink never gets resolved, the
    /// containment check sees a path the kernel will not use, and a create
    /// walks through the link and lands outside the root.
    ///
    /// A dangling link canonicalizes as an error, so it joins the literal tail
    /// and the operation fails on it rather than traversing it.
    fn canonicalize_deepest_existing(full: &Path) -> PathBuf {
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        let mut cursor = full.to_path_buf();
        loop {
            if let Ok(resolved) = cursor.canonicalize() {
                let mut out = resolved;
                for part in tail.iter().rev() {
                    out.push(part);
                }
                return out;
            }
            let Some(name) = cursor.file_name().map(|n| n.to_os_string()) else {
                return full.to_path_buf();
            };
            let Some(parent) = cursor.parent().map(|p| p.to_path_buf()) else {
                return full.to_path_buf();
            };
            if parent.as_os_str().is_empty() {
                return full.to_path_buf();
            }
            tail.push(name);
            cursor = parent;
        }
    }

    /// Refuse a path that climbs above the mount root — lexically, before any
    /// I/O touches it.
    ///
    /// The containment check after resolution compares path *components*, so an
    /// un-normalized `<root>/../sibling` "starts with" `<root>` and passes it.
    /// Resolution only normalizes when the parent already exists, so a path
    /// with a missing parent reaches that check un-normalized and escapes.
    ///
    /// A `..` that stays inside the root is fine; only a net climb above it is
    /// refused.
    fn reject_lexical_escape(path: &Path) -> VfsResult<()> {
        let mut depth: isize = 0;
        for component in path.components() {
            match component {
                std::path::Component::ParentDir => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(VfsError::path_escapes_root(format!(
                            "{} climbs above the mount root",
                            path.display()
                        )));
                    }
                }
                std::path::Component::Normal(_) => depth += 1,
                std::path::Component::CurDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_) => {}
            }
        }
        Ok(())
    }

    /// Resolve a relative path to an absolute path within the root.
    ///
    /// Returns an error if the path escapes the root (via `..`).
    async fn resolve(&self, path: &Path) -> VfsResult<PathBuf> {
        // Strip leading slash if present
        let path = path.strip_prefix("/").unwrap_or(path);
        Self::reject_lexical_escape(path)?;

        // Handle empty path (root)
        if path.as_os_str().is_empty() {
            return Ok(self.root.clone());
        }

        // Join with root
        let full = self.root.join(path);

        // Canonicalize to resolve symlinks and ..
        // For non-existent paths, we need to check parent
        let canonical = Self::canonicalize_deepest_existing(&full);

        // Verify we haven't escaped the root
        let canonical_root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        if !canonical.starts_with(&canonical_root) {
            return Err(VfsError::path_escapes_root(format!(
                "{} is not under {}",
                canonical.display(),
                canonical_root.display()
            )));
        }

        Ok(canonical)
    }

    /// Resolve without following a final-component symlink — lstat semantics.
    ///
    /// [`Self::resolve`] canonicalizes the whole path, so a path naming a
    /// symlink comes back as the target it points at. An operation that acts on
    /// the *link* must use this instead, or it acts on the target: deleting one
    /// name in the composed rc tree would take the shared script every other
    /// context type links to.
    ///
    /// The parent is still canonicalized, so `..` cannot escape the root.
    async fn resolve_nofollow(&self, path: &Path) -> VfsResult<PathBuf> {
        let path = path.strip_prefix("/").unwrap_or(path);
        Self::reject_lexical_escape(path)?;
        if path.as_os_str().is_empty() {
            return Ok(self.root.clone());
        }
        let full = self.root.join(path);
        let parent = full
            .parent()
            .ok_or_else(|| VfsError::invalid_path("no parent"))?;
        let filename = full
            .file_name()
            .ok_or_else(|| VfsError::invalid_path("no filename"))?;
        let resolved = Self::canonicalize_deepest_existing(parent).join(filename);

        let canonical_root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        if !resolved.starts_with(&canonical_root) {
            return Err(VfsError::path_escapes_root(format!(
                "{} is not under {}",
                resolved.display(),
                canonical_root.display()
            )));
        }
        Ok(resolved)
    }

    /// Refuse an operation aimed at the mount root itself.
    ///
    /// `rmdir("")`, `rmdir("/")` and `rmdir(".")` all resolve to the root, and
    /// an empty root would then be removed — taking the mount with it.
    /// `MemoryBackend` has always refused this; one helper, used by every
    /// operation that removes or moves a name, keeps the two backends from
    /// disagreeing again.
    ///
    /// A path is the root when it names no ordinary component: `..` alone is
    /// already refused by [`Self::reject_lexical_escape`].
    fn refuse_mount_root(path: &Path) -> VfsResult<()> {
        let path = path.strip_prefix("/").unwrap_or(path);
        let names_something = path
            .components()
            .any(|c| matches!(c, std::path::Component::Normal(_)));
        if names_something {
            Ok(())
        } else {
            Err(VfsError::permission_denied("cannot remove the mount root"))
        }
    }

    /// Check if write operations are allowed.
    fn check_writable(&self) -> VfsResult<()> {
        if self.read_only {
            Err(VfsError::ReadOnly)
        } else {
            Ok(())
        }
    }

    /// Convert std::fs::Metadata to FileAttr.
    fn metadata_to_attr(meta: &std::fs::Metadata) -> FileAttr {
        let kind = if meta.is_dir() {
            FileType::Directory
        } else if meta.file_type().is_symlink() {
            FileType::Symlink
        } else {
            FileType::File
        };

        let mtime = meta
            .modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        // Host files have no version counter, so derive the coherence stamp from
        // mtime-nanos: it advances with every external edit (which is exactly
        // when the cache must reload) and matches the pre-generation behaviour
        // that compared mtime directly. Nanosecond host mtime resolution makes
        // same-stamp collisions vanishingly rare in practice.
        let generation = mtime
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        FileAttr {
            size: meta.len(),
            kind,
            perm: meta.permissions().mode(),
            mtime,
            generation,
            atime: meta.accessed().ok(),
            ctime: meta.created().ok(),
            nlink: meta.nlink() as u32,
            uid: Some(meta.uid()),
            gid: Some(meta.gid()),
        }
    }
}

#[async_trait]
impl VfsOps for LocalBackend {
    /// `lstat`, not `stat`: a symlink reports as a symlink, whether or not it
    /// resolves. Resolving with [`Self::resolve`] first would follow the final
    /// component, leaving `symlink_metadata` to describe the *target* — so a
    /// working link came back as its target's type and only a dangling one
    /// came back as a link. Callers dispatch on `is_dir()`, so that split
    /// decided whether a directory link was removed as a link or as a
    /// directory.
    async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
        let full_path = self.resolve_nofollow(path).await?;
        let meta = fs::symlink_metadata(&full_path)
            .await
            .map_err(VfsError::from)?;
        Ok(Self::metadata_to_attr(&meta))
    }

    async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
        let full_path = self.resolve(path).await?;
        let mut entries = Vec::new();
        let mut dir = fs::read_dir(&full_path).await.map_err(VfsError::from)?;

        while let Some(entry) = dir.next_entry().await.map_err(VfsError::from)? {
            let file_type = entry.file_type().await.map_err(VfsError::from)?;
            let kind = if file_type.is_dir() {
                FileType::Directory
            } else if file_type.is_symlink() {
                FileType::Symlink
            } else {
                FileType::File
            };

            entries.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind,
            });
        }

        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let full_path = self.resolve(path).await?;
        let mut file = fs::File::open(&full_path).await.map_err(VfsError::from)?;

        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(VfsError::from)?;

        let mut buffer = vec![0u8; size as usize];
        let bytes_read = file.read(&mut buffer).await.map_err(VfsError::from)?;
        buffer.truncate(bytes_read);

        Ok(buffer)
    }

    /// Read the whole file, following symlinks. Overridden because the trait
    /// default sizes the read from `getattr`, which here is lstat-like
    /// (`symlink_metadata`) and reports the *link-path* length for a symlink —
    /// the default would then cap the followed read at that size and truncate a
    /// link to a longer file. `resolve` canonicalizes (follows the link) and we
    /// read to EOF, so the size comes from the real target. A dangling link
    /// fails loud here (canonicalize errors), not as truncated/empty content.
    async fn read_all(&self, path: &Path) -> VfsResult<Vec<u8>> {
        let full_path = self.resolve(path).await?;
        fs::read(&full_path).await.map_err(VfsError::from)
    }

    /// Reads the link itself, so the final component is never followed — and
    /// containment covers the whole path, not just a literal `..` in its
    /// text. The hand-rolled check this replaces only ran when the path
    /// *spelled* a `..`, so an intermediate link pointing outside the mount
    /// carried the read out of it with no `..` and no race.
    async fn readlink(&self, path: &Path) -> VfsResult<PathBuf> {
        let full_path = self.resolve_nofollow(path).await?;
        fs::read_link(&full_path).await.map_err(VfsError::from)
    }

    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32> {
        use tokio::io::{AsyncSeekExt, AsyncWriteExt};

        self.check_writable()?;
        let full_path = self.resolve(path).await?;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(&full_path)
            .await
            .map_err(VfsError::from)?;

        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(VfsError::from)?;

        file.write_all(data).await.map_err(VfsError::from)?;

        Ok(data.len() as u32)
    }

    async fn create(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        use std::os::unix::fs::OpenOptionsExt;

        self.check_writable()?;
        let full_path = self.resolve(path).await?;

        // Ensure parent directory exists
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).await.map_err(VfsError::from)?;
        }

        // Create file with specified mode
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&full_path)
            .map_err(VfsError::from)?;

        let meta = file.metadata().map_err(VfsError::from)?;
        Ok(Self::metadata_to_attr(&meta))
    }

    async fn mkdir(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        use std::os::unix::fs::DirBuilderExt;

        self.check_writable()?;
        let full_path = self.resolve(path).await?;

        std::fs::DirBuilder::new()
            .mode(mode)
            .recursive(true)
            .create(&full_path)
            .map_err(VfsError::from)?;

        let meta = fs::metadata(&full_path).await.map_err(VfsError::from)?;
        Ok(Self::metadata_to_attr(&meta))
    }

    async fn unlink(&self, path: &Path) -> VfsResult<()> {
        self.check_writable()?;
        Self::refuse_mount_root(path)?;
        // NOT `resolve()` — it canonicalizes the final component, so deleting a
        // symlink would delete what it points at.
        let full_path = self.resolve_nofollow(path).await?;
        fs::remove_file(&full_path).await.map_err(VfsError::from)
    }

    /// Removes the directory named, never what a link points at. `resolve`
    /// would canonicalize a directory link to its target and remove that,
    /// leaving the link behind; on the composed rc tree removing one context
    /// type's link would take the directory every other type shares. A
    /// `rmdir` aimed at a symlink fails (`ENOTDIR`), which is the POSIX
    /// answer — remove the link with `unlink`.
    async fn rmdir(&self, path: &Path) -> VfsResult<()> {
        self.check_writable()?;
        Self::refuse_mount_root(path)?;
        let full_path = self.resolve_nofollow(path).await?;
        fs::remove_dir(&full_path).await.map_err(VfsError::from)
    }

    /// Moves the names given, never what they point at. `resolve` would
    /// canonicalize both ends, so renaming a symlink moved its *target* and
    /// left the link behind — the same defect `unlink`, `rmdir` and
    /// `getattr` carried, in the operation that also has a destination.
    ///
    /// There is deliberately no identity fast-path. `fs::rename` on one name
    /// spelled two ways is already a POSIX no-op that keeps the file; a guard
    /// that compared the two spellings itself would have to fold `..` exactly
    /// as the kernel does, and one that drops those components instead turns
    /// a self-rename into a destination-clearing move.
    async fn rename(&self, from: &Path, to: &Path) -> VfsResult<()> {
        self.check_writable()?;
        Self::refuse_mount_root(from)?;
        Self::refuse_mount_root(to)?;
        let from_path = self.resolve_nofollow(from).await?;
        let to_path = self.resolve_nofollow(to).await?;

        // Ensure parent of destination exists
        if let Some(parent) = to_path.parent() {
            fs::create_dir_all(parent).await.map_err(VfsError::from)?;
        }

        fs::rename(&from_path, &to_path)
            .await
            .map_err(VfsError::from)
    }

    async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()> {
        self.check_writable()?;
        let full_path = self.resolve(path).await?;

        let file = fs::OpenOptions::new()
            .write(true)
            .open(&full_path)
            .await
            .map_err(VfsError::from)?;

        file.set_len(size).await.map_err(VfsError::from)
    }

    async fn setattr(&self, path: &Path, attr: SetAttr) -> VfsResult<FileAttr> {
        self.check_writable()?;
        let full_path = self.resolve(path).await?;

        // Handle size
        if let Some(size) = attr.size {
            let file = fs::OpenOptions::new()
                .write(true)
                .open(&full_path)
                .await
                .map_err(VfsError::from)?;
            file.set_len(size).await.map_err(VfsError::from)?;
        }

        // Handle permissions
        if let Some(perm) = attr.perm {
            let permissions = std::fs::Permissions::from_mode(perm);
            fs::set_permissions(&full_path, permissions)
                .await
                .map_err(VfsError::from)?;
        }

        // Handle times via std's stable `File::set_times`/`FileTimes` — mtime
        // is load-bearing for `FileDocumentCache` staleness detection, so a
        // no-op here silently breaks cache coherence (the cache would keep
        // serving a stale snapshot after an explicit setattr). tokio::fs has
        // no async wrapper for set_times; run the syscall on the blocking
        // pool the way tokio's own fs ops do internally.
        if attr.mtime.is_some() || attr.atime.is_some() {
            let full_path = full_path.clone();
            let mtime = attr.mtime;
            let atime = attr.atime;
            tokio::task::spawn_blocking(move || {
                let file = std::fs::OpenOptions::new().write(true).open(&full_path)?;
                let mut times = std::fs::FileTimes::new();
                if let Some(mtime) = mtime {
                    times = times.set_modified(mtime);
                }
                if let Some(atime) = atime {
                    times = times.set_accessed(atime);
                }
                file.set_times(times)
            })
            .await
            .map_err(|e| VfsError::other(format!("setattr: blocking task join failed: {e}")))?
            .map_err(VfsError::from)?;
        }

        // Handle uid/gid (requires nix crate or libc)
        if attr.uid.is_some() || attr.gid.is_some() {
            // Would use nix::unistd::chown here
            // For now, skip silently
        }

        self.getattr(path).await
    }

    /// `path` is the link to create, `target` is the text it will hold.
    /// Resolved without following the final component: the name being created
    /// is a name, not something to chase.
    async fn symlink(&self, path: &Path, target: &Path) -> VfsResult<FileAttr> {
        self.check_writable()?;
        let full_path = self.resolve_nofollow(path).await?;

        // Ensure parent directory exists
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).await.map_err(VfsError::from)?;
        }

        std::os::unix::fs::symlink(target, &full_path).map_err(VfsError::from)?;

        self.getattr(path).await
    }

    async fn link(&self, oldpath: &Path, newpath: &Path) -> VfsResult<FileAttr> {
        self.check_writable()?;
        let old_full = self.resolve(oldpath).await?;
        let new_full = self.resolve(newpath).await?;

        // Ensure parent of new path exists
        if let Some(parent) = new_full.parent() {
            fs::create_dir_all(parent).await.map_err(VfsError::from)?;
        }

        fs::hard_link(&old_full, &new_full)
            .await
            .map_err(VfsError::from)?;

        self.getattr(newpath).await
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn opaque_to_sweeps(&self) -> bool {
        self.opaque
    }

    async fn statfs(&self) -> VfsResult<StatFs> {
        #[cfg(unix)]
        {
            use rustix::fs::statvfs;

            let stat = statvfs(&self.root).map_err(|e| VfsError::from(std::io::Error::from(e)))?;

            Ok(StatFs {
                blocks: stat.f_blocks,
                bfree: stat.f_bfree,
                bavail: stat.f_bavail,
                files: stat.f_files,
                ffree: stat.f_ffree,
                bsize: stat.f_bsize as u32,
                namelen: stat.f_namemax as u32,
                frsize: stat.f_frsize as u32,
            })
        }

        #[cfg(not(unix))]
        {
            Ok(StatFs::default())
        }
    }

    fn real_root(&self) -> Option<PathBuf> {
        // Root is canonicalized at construction; a 1:1 host-directory view.
        Some(self.root.clone())
    }

    async fn real_path(&self, path: &Path) -> VfsResult<Option<PathBuf>> {
        // Strip leading slash if present
        let path = path.strip_prefix("/").unwrap_or(path);
        let full = self.root.join(path);

        // Use dunce for clean canonical paths (no \\?\ on Windows)
        let canonical = dunce::canonicalize(&full).map_err(VfsError::from)?;

        // Security check: ensure path is under root
        let canonical_root = dunce::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        if !canonical.starts_with(&canonical_root) {
            return Err(VfsError::PermissionDenied(format!(
                "path escapes mount root: {}",
                path.display()
            )));
        }

        Ok(Some(canonical))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup() -> (LocalBackend, TempDir) {
        let dir = TempDir::new().unwrap();
        let backend = LocalBackend::new(dir.path());
        (backend, dir)
    }

    #[tokio::test]
    async fn test_create_and_read() {
        let (backend, _dir) = setup().await;

        backend.create(Path::new("test.txt"), 0o644).await.unwrap();
        backend
            .write(Path::new("test.txt"), 0, b"hello world")
            .await
            .unwrap();

        let data = backend.read(Path::new("test.txt"), 0, 100).await.unwrap();
        assert_eq!(data, b"hello world");
    }

    #[tokio::test]
    async fn test_partial_read() {
        let (backend, _dir) = setup().await;

        backend.create(Path::new("test.txt"), 0o644).await.unwrap();
        backend
            .write(Path::new("test.txt"), 0, b"hello world")
            .await
            .unwrap();

        let data = backend.read(Path::new("test.txt"), 6, 5).await.unwrap();
        assert_eq!(data, b"world");
    }

    #[tokio::test]
    async fn test_mkdir_and_readdir() {
        let (backend, _dir) = setup().await;

        backend.mkdir(Path::new("subdir"), 0o755).await.unwrap();
        backend
            .create(Path::new("subdir/file.txt"), 0o644)
            .await
            .unwrap();
        backend.create(Path::new("root.txt"), 0o644).await.unwrap();

        let entries = backend.readdir(Path::new("")).await.unwrap();
        let names: Vec<_> = entries.iter().map(|e| &e.name).collect();
        assert!(names.contains(&&"subdir".to_string()));
        assert!(names.contains(&&"root.txt".to_string()));
    }

    #[tokio::test]
    async fn test_read_only() {
        let (mut backend, _dir) = setup().await;
        backend.set_read_only(true);

        let result = backend.create(Path::new("test.txt"), 0o644).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_opaque_defaults_false_and_is_settable() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(!LocalBackend::new(dir.path()).opaque_to_sweeps());
        assert!(LocalBackend::new(dir.path()).opaque(true).opaque_to_sweeps());
    }

    /// A `readdir` on a directory that was never created — not even its
    /// parent — must report the typed `VfsError::NotFound`, not
    /// `VfsError::Io` wrapping `ENOENT`. This is the exact shape
    /// `kj::lifecycle::load_rc_scripts` matches on to treat "no rc
    /// directory for this (type, verb)" as zero scripts rather than a
    /// failure; a host-backed mount reporting absence any other way defeats
    /// that match silently.
    #[tokio::test]
    async fn readdir_on_a_directory_with_no_ancestor_reports_typed_not_found() {
        let (backend, _dir) = setup().await;

        let result = backend.readdir(Path::new("nonexistent-type/create")).await;

        assert!(
            matches!(result, Err(VfsError::NotFound(_))),
            "expected VfsError::NotFound, got {result:?}"
        );
    }

    /// `statfs` builds its error by hand rather than through `?`, so it is
    /// the one place a host `ENOENT` can miss the `From<io::Error>`
    /// normalization and surface as `VfsError::Io`. A backend whose root has
    /// gone away must report absence the same way every other path does.
    #[tokio::test]
    async fn statfs_on_a_missing_root_reports_typed_not_found() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let root = dir.path().join("never-created");
        let backend = LocalBackend::new(&root);

        let result = backend.statfs().await;

        assert!(
            matches!(result, Err(VfsError::NotFound(_))),
            "expected VfsError::NotFound, got {result:?}"
        );
    }

    #[tokio::test]
    async fn test_path_escape_blocked() {
        let (backend, _dir) = setup().await;

        let result = backend.read(Path::new("../../../etc/passwd"), 0, 100).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_symlink() {
        let (backend, _dir) = setup().await;

        backend
            .create(Path::new("target.txt"), 0o644)
            .await
            .unwrap();
        backend
            .write(Path::new("target.txt"), 0, b"content")
            .await
            .unwrap();

        backend
            .symlink(Path::new("link.txt"), Path::new("target.txt"))
            .await
            .unwrap();

        let target = backend.readlink(Path::new("link.txt")).await.unwrap();
        assert_eq!(target, Path::new("target.txt"));
    }

    /// `read_all` through a symlink must return the *target's* full content, not
    /// truncate to the link-path length. The trait default sizes from `getattr`
    /// (lstat — link-path bytes), so a link to a longer file would read short;
    /// the override reads to EOF after following. Regression for that bug.
    #[tokio::test]
    async fn read_all_follows_symlink_without_truncating() {
        let (backend, _dir) = setup().await;
        // Body deliberately much longer than the link path ("l.txt" = 5 bytes).
        let body = b"this body is far longer than the link path name";
        backend.create(Path::new("target.txt"), 0o644).await.unwrap();
        backend.write(Path::new("target.txt"), 0, body).await.unwrap();
        backend
            .symlink(Path::new("l.txt"), Path::new("target.txt"))
            .await
            .unwrap();

        let got = backend.read_all(Path::new("l.txt")).await.unwrap();
        assert_eq!(got, body, "read_all truncated a followed symlink");
    }

    /// A path whose parent does not exist must not escape the mount root.
    ///
    /// `resolve`'s missing-parent fallback returns an un-normalized
    /// `root.join(path)`, and `Path::starts_with` compares components — so
    /// `<root>/../sibling` "starts with" `<root>` and passes containment. The
    /// fallback's own comment claims it "will fail on actual operation", but
    /// `create` calls `create_dir_all` on the parent first, so the write lands
    /// beside the root instead of failing.
    ///
    /// Falsified by removing the lexical `..` refusal from `resolve`.
    #[tokio::test]
    async fn a_missing_parent_cannot_escape_the_root_via_dotdot() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let backend = LocalBackend::new(&root);

        let result = backend
            .create(Path::new("../sibling/escaped.txt"), 0o644)
            .await;

        assert!(
            result.is_err(),
            "a `..` path with a missing parent must be refused"
        );
        assert!(
            !tmp.path().join("sibling").exists(),
            "create escaped the mount root and made a directory beside it"
        );
    }

    /// A missing parent reached THROUGH a symlink must not escape either.
    ///
    /// The lexical `..` refusal closes one escape; this is the other. When the
    /// parent does not exist, resolution appends the whole remainder to the
    /// root literally, so an intermediate symlink is never resolved and the
    /// component-wise containment check sees an inside-the-root path. `create`
    /// then `create_dir_all`s through the link, outside the root.
    ///
    /// Falsified by resolving a missing parent to a literal `root.join(path)`
    /// instead of canonicalizing the deepest ancestor that does exist.
    #[tokio::test]
    async fn a_symlinked_intermediate_cannot_escape_when_the_parent_is_missing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let backend = LocalBackend::new(&root);

        let result = backend
            .create(Path::new("link/newdir/escaped.txt"), 0o644)
            .await;

        assert!(
            !outside.join("newdir").exists(),
            "created a directory outside the root through a symlinked intermediate"
        );
        assert!(result.is_err(), "the escape must be refused, not merely contained");
    }

    /// A DANGLING final link must not become a write target outside the root.
    ///
    /// `canonicalize` fails on a dangling link, so resolution puts it in the
    /// literal tail and containment sees an inside-the-root path. Whether that
    /// is exploitable is decided by the syscall: an `O_CREAT` without
    /// `O_EXCL` would follow the dangling chain and create the target outside.
    /// `create` uses `create_new` (`O_EXCL`) and `write`/`truncate` pass no
    /// `O_CREAT`, so every writing path here refuses instead of following.
    ///
    /// Pins that pairing. Falsified by relaxing `create_new` to `create`.
    #[tokio::test]
    async fn a_dangling_final_link_is_never_written_through() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        // Dangling: `outside/evil` does not exist yet.
        std::os::unix::fs::symlink("../outside/evil", root.join("link")).unwrap();
        let backend = LocalBackend::new(&root);

        let created = backend.create(Path::new("link"), 0o644).await;
        let written = backend.write_all(Path::new("link"), b"pwned").await;

        assert!(
            !outside.join("evil").exists(),
            "followed a dangling link and created the target outside the root"
        );
        assert!(created.is_err(), "create through a dangling link must refuse");
        assert!(written.is_err(), "write_all through a dangling link must refuse");
    }

    /// `unlink` on a symlink removes the LINK, never what it points at.
    ///
    /// `resolve()` canonicalizes the final component, so routing a delete
    /// through it hands `remove_file` the target's real path. The composed rc
    /// tree links many per-type names at one shared script, so deleting one
    /// link that way would take the script every other context type depends on.
    ///
    /// Falsified by routing `unlink` back through `resolve()`.
    #[tokio::test]
    async fn unlink_removes_the_link_not_its_target() {
        let (backend, dir) = setup().await;
        backend.create(Path::new("target.txt"), 0o644).await.unwrap();
        backend.write(Path::new("target.txt"), 0, b"keep me").await.unwrap();
        backend
            .symlink(Path::new("link.txt"), Path::new("target.txt"))
            .await
            .unwrap();

        backend.unlink(Path::new("link.txt")).await.unwrap();

        assert!(
            std::fs::symlink_metadata(dir.path().join("link.txt")).is_err(),
            "the link itself must be gone"
        );
        assert!(
            dir.path().join("target.txt").exists(),
            "unlink followed the link and deleted its target"
        );
    }

    #[tokio::test]
    async fn test_rename() {
        let (backend, _dir) = setup().await;

        backend.create(Path::new("old.txt"), 0o644).await.unwrap();
        backend
            .write(Path::new("old.txt"), 0, b"content")
            .await
            .unwrap();

        backend
            .rename(Path::new("old.txt"), Path::new("new.txt"))
            .await
            .unwrap();

        assert!(backend.getattr(Path::new("old.txt")).await.is_err());
        let data = backend.read(Path::new("new.txt"), 0, 100).await.unwrap();
        assert_eq!(data, b"content");
    }

    #[tokio::test]
    async fn test_truncate() {
        let (backend, _dir) = setup().await;

        backend.create(Path::new("test.txt"), 0o644).await.unwrap();
        backend
            .write(Path::new("test.txt"), 0, b"hello world")
            .await
            .unwrap();

        backend.truncate(Path::new("test.txt"), 5).await.unwrap();

        let data = backend.read(Path::new("test.txt"), 0, 100).await.unwrap();
        assert_eq!(data, b"hello");
    }

    #[tokio::test]
    async fn setattr_actually_sets_mtime() {
        // Regression: setattr used to open the file and discard the handle
        // without ever calling a timestamp syscall — a no-op dressed as
        // success. mtime is load-bearing for FileDocumentCache staleness
        // detection, so this must actually move the file's mtime.
        let (backend, _dir) = setup().await;
        backend.create(Path::new("test.txt"), 0o644).await.unwrap();

        let before = backend
            .getattr(Path::new("test.txt"))
            .await
            .unwrap()
            .mtime;
        let target = before + std::time::Duration::from_secs(3600);

        let mut attr = SetAttr::new();
        attr.mtime = Some(target);
        backend
            .setattr(Path::new("test.txt"), attr)
            .await
            .unwrap();

        let after = backend
            .getattr(Path::new("test.txt"))
            .await
            .unwrap()
            .mtime;
        assert_eq!(after, target, "setattr must actually set mtime, not no-op");
    }

    #[tokio::test]
    async fn test_hard_link() {
        let (backend, _dir) = setup().await;

        backend
            .create(Path::new("original.txt"), 0o644)
            .await
            .unwrap();
        backend
            .write(Path::new("original.txt"), 0, b"shared content")
            .await
            .unwrap();

        backend
            .link(Path::new("original.txt"), Path::new("linked.txt"))
            .await
            .unwrap();

        let data = backend.read(Path::new("linked.txt"), 0, 100).await.unwrap();
        assert_eq!(data, b"shared content");

        // Both should show nlink >= 2
        let attr = backend.getattr(Path::new("original.txt")).await.unwrap();
        assert!(attr.nlink >= 2);
    }

    #[tokio::test]
    async fn test_real_path() {
        let (backend, dir) = setup().await;

        // Create a file
        std::fs::write(dir.path().join("test.txt"), "hello").unwrap();

        // Resolve it
        let real = backend.real_path(Path::new("test.txt")).await.unwrap();
        assert!(real.is_some());
        let real = real.unwrap();
        assert!(real.is_absolute());
        assert!(real.ends_with("test.txt"));
    }

    #[tokio::test]
    async fn test_real_path_escape_prevention() {
        let (backend, _dir) = setup().await;

        // Attempt escape
        let result = backend.real_path(Path::new("../etc/passwd")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_real_path_nonexistent() {
        let (backend, _dir) = setup().await;

        // Non-existent path should error (can't canonicalize)
        let result = backend.real_path(Path::new("nonexistent.txt")).await;
        assert!(result.is_err());
    }

    // Part 4a: Path security tests

    #[tokio::test]
    async fn test_path_with_parent_dir_rejected() {
        let (backend, _dir) = setup().await;

        // Create a test file to ensure parent exists (for clearer error)
        backend.create(Path::new("test.txt"), 0o644).await.unwrap();

        // Attempt to escape via ..
        let result = backend.read(Path::new("../secret.txt"), 0, 100).await;
        assert!(result.is_err());

        // Verify the error is PathEscapesRoot
        match result {
            Err(VfsError::PathEscapesRoot(_)) => {} // Expected
            other => panic!("Expected PathEscapesRoot, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_normal_paths_succeed() {
        let (backend, _dir) = setup().await;

        // Create nested directory structure
        backend
            .mkdir(Path::new("subdir/nested"), 0o755)
            .await
            .unwrap();
        backend
            .create(Path::new("subdir/nested/file.txt"), 0o644)
            .await
            .unwrap();
        backend
            .write(Path::new("subdir/nested/file.txt"), 0, b"content")
            .await
            .unwrap();

        // Normal paths should work fine
        let data = backend
            .read(Path::new("subdir/nested/file.txt"), 0, 100)
            .await
            .unwrap();
        assert_eq!(data, b"content");

        // Root-level file
        backend.create(Path::new("root.txt"), 0o644).await.unwrap();
        backend
            .write(Path::new("root.txt"), 0, b"root")
            .await
            .unwrap();
        let data = backend.read(Path::new("root.txt"), 0, 100).await.unwrap();
        assert_eq!(data, b"root");
    }

    // ── The symlink/containment family ──────────────────────────────────
    //
    // These mirror the rows `kaish_vfs::conformance` pins for the same
    // resolver shape, which both trees shipped from a common ancestor. The
    // policy each operation follows is stated on the operation itself; see
    // `docs/config-namespace.md` for why the composed rc tree makes the
    // follow/no-follow split load-bearing rather than academic.

    /// `rmdir` on the mount root removes the mount itself when it happens to
    /// be empty. `MemoryBackend` refuses this explicitly, so the two backends
    /// disagreed; this pins the refusal on both spellings of "the root".
    #[tokio::test]
    async fn rmdir_refuses_the_mount_root() {
        let (backend, dir) = setup().await;

        for spelling in ["", "/", ".", "/."] {
            let r = backend.rmdir(Path::new(spelling)).await;
            assert!(
                r.is_err(),
                "rmdir({spelling:?}) must refuse the mount root, got {r:?}"
            );
        }
        assert!(dir.path().exists(), "the mount root must survive");
    }

    /// Same rule for `unlink`: the root is not a name this backend will
    /// remove, whatever it is spelled as.
    #[tokio::test]
    async fn unlink_refuses_the_mount_root() {
        let (backend, dir) = setup().await;

        for spelling in ["", "/", "."] {
            assert!(
                backend.unlink(Path::new(spelling)).await.is_err(),
                "unlink({spelling:?}) must refuse the mount root"
            );
        }
        assert!(dir.path().exists(), "the mount root must survive");
    }

    /// `getattr` is `lstat`, not `stat`. It reads `symlink_metadata`, but it
    /// used to resolve the path *following* the final component first — so a
    /// working link reported its target's type and only a dangling link came
    /// back as a symlink. That split is what let a directory link be removed
    /// as a directory.
    #[tokio::test]
    async fn getattr_reports_a_working_symlink_as_a_symlink() {
        let (backend, _dir) = setup().await;
        backend.mkdir(Path::new("realdir"), 0o755).await.unwrap();
        backend.create(Path::new("realfile"), 0o644).await.unwrap();
        backend.symlink(Path::new("dirlink"), Path::new("realdir")).await.unwrap();
        backend.symlink(Path::new("filelink"), Path::new("realfile")).await.unwrap();

        let dl = backend.getattr(Path::new("dirlink")).await.unwrap();
        assert_eq!(dl.kind, FileType::Symlink, "a working directory link is a symlink, not a directory");
        assert!(!dl.is_dir(), "is_dir() decides remove's dispatch — it must not say directory here");

        let fl = backend.getattr(Path::new("filelink")).await.unwrap();
        assert_eq!(fl.kind, FileType::Symlink, "a working file link is a symlink");

        // The targets themselves still report their own types.
        assert!(backend.getattr(Path::new("realdir")).await.unwrap().is_dir());
        assert_eq!(backend.getattr(Path::new("realfile")).await.unwrap().kind, FileType::File);
    }

    /// `rmdir` acts on the name it is given. Through a directory link it used
    /// to canonicalize to the target and remove *that*, leaving the link
    /// dangling — on the composed rc tree, removing one context type's link
    /// would take the shared directory every other type points at.
    #[tokio::test]
    async fn rmdir_through_a_directory_symlink_spares_the_target() {
        let (backend, _dir) = setup().await;
        backend.mkdir(Path::new("target"), 0o755).await.unwrap();
        backend.symlink(Path::new("link"), Path::new("target")).await.unwrap();

        // POSIX: rmdir on a symlink is ENOTDIR. Either way the target lives.
        let r = backend.rmdir(Path::new("link")).await;
        assert!(r.is_err(), "rmdir on a symlink must not succeed, got {r:?}");
        assert!(
            backend.getattr(Path::new("target")).await.unwrap().is_dir(),
            "the target directory must survive an rmdir aimed at the link"
        );
        assert_eq!(
            backend.getattr(Path::new("link")).await.unwrap().kind,
            FileType::Symlink,
            "and the link must survive too"
        );
    }

    /// `readlink` joined the raw path and checked only for a literal `..`,
    /// so an *intermediate* symlink pointing outside the root carried the
    /// read straight out of the mount. No race and no `..` required.
    #[tokio::test]
    async fn readlink_refuses_a_path_that_leaves_the_root_through_an_intermediate_link() {
        let (backend, _dir) = setup().await;
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink("/secret", outside.path().join("host-link")).unwrap();

        // An in-root name pointing at a directory outside the mount.
        std::os::unix::fs::symlink(outside.path(), _dir.path().join("out")).unwrap();

        let r = backend.readlink(Path::new("out/host-link")).await;
        assert!(
            r.is_err(),
            "readlink must not follow an intermediate link out of the mount, got {r:?}"
        );
    }

    /// The in-root case still works — containment is the rule, not a blanket
    /// refusal of links.
    #[tokio::test]
    async fn readlink_reads_an_in_root_link() {
        let (backend, _dir) = setup().await;
        backend.create(Path::new("target.txt"), 0o644).await.unwrap();
        backend.mkdir(Path::new("sub"), 0o755).await.unwrap();
        backend.symlink(Path::new("sub/link"), Path::new("../target.txt")).await.unwrap();

        assert_eq!(
            backend.readlink(Path::new("sub/link")).await.unwrap(),
            Path::new("../target.txt"),
            "readlink returns the link's own target text"
        );
    }

    /// Renaming a name onto itself keeps the file. Trivially true here
    /// because there is no identity fast-path to get wrong — pinned so that
    /// adding one has to keep it true.
    #[tokio::test]
    async fn rename_to_itself_keeps_the_file() {
        let (backend, _dir) = setup().await;
        backend.mkdir(Path::new("d"), 0o755).await.unwrap();
        backend.create(Path::new("d/file"), 0o644).await.unwrap();
        backend.write(Path::new("d/file"), 0, b"keep me").await.unwrap();

        backend.rename(Path::new("d/file"), Path::new("d/file")).await.unwrap();

        assert_eq!(backend.read_all(Path::new("d/file")).await.unwrap(), b"keep me");
    }

    /// The same rename spelled with a `..` that folds back to the same file.
    /// An identity guard that *drops* `..` instead of folding it reads this
    /// as a move between two different names and clears the destination —
    /// which is the source.
    #[tokio::test]
    async fn rename_to_itself_spelled_with_dotdot_keeps_the_file() {
        let (backend, _dir) = setup().await;
        backend.mkdir(Path::new("d"), 0o755).await.unwrap();
        backend.create(Path::new("d/file"), 0o644).await.unwrap();
        backend.write(Path::new("d/file"), 0, b"keep me").await.unwrap();

        backend.rename(Path::new("d/../d/file"), Path::new("d/file")).await.unwrap();

        assert_eq!(
            backend.read_all(Path::new("d/file")).await.unwrap(),
            b"keep me",
            "a self-rename spelled with .. must not clear the file"
        );
    }

    /// `rename` moves the link, not the file the link points at.
    #[tokio::test]
    async fn rename_moves_the_link_not_its_target() {
        let (backend, _dir) = setup().await;
        backend.create(Path::new("target.txt"), 0o644).await.unwrap();
        backend.write(Path::new("target.txt"), 0, b"target body").await.unwrap();
        backend.symlink(Path::new("link"), Path::new("target.txt")).await.unwrap();

        backend.rename(Path::new("link"), Path::new("moved")).await.unwrap();

        assert_eq!(
            backend.getattr(Path::new("moved")).await.unwrap().kind,
            FileType::Symlink,
            "the link moved"
        );
        assert_eq!(
            backend.read_all(Path::new("target.txt")).await.unwrap(),
            b"target body",
            "the target stayed where it was"
        );
        assert!(
            backend.getattr(Path::new("link")).await.is_err(),
            "the old link name is gone"
        );
    }

    /// Neither end of a rename may be the mount root.
    #[tokio::test]
    async fn rename_refuses_the_root() {
        let (backend, _dir) = setup().await;
        backend.create(Path::new("f"), 0o644).await.unwrap();

        assert!(backend.rename(Path::new(""), Path::new("f")).await.is_err());
        assert!(backend.rename(Path::new("f"), Path::new("/")).await.is_err());
        assert!(backend.rename(Path::new("f"), Path::new(".")).await.is_err());
    }
}
