//! `ConfigCrdtFs` — a CRDT-native VFS backend for config/rc content.
//!
//! This is the backend that lets the CRDT be the **sole owner** of the
//! `/etc/rc` tree (and, later, the config TOMLs): file ops map straight onto
//! `BlockStore` documents, with **no host-disk backing, no write-through flush,
//! and no mtime-vs-disk reload**. That deletes — by construction, for this
//! mount — the dual-ownership silent-fallback cluster documented in
//! `docs/config-crdt-ownership.md` (stale-bytes serve, append-wipe, mtime no-op,
//! stale-rc-seed). There is one truth here: the CRDT document.
//!
//! ## Model (shared with `ConfigCrdtBackend`)
//!
//! Each path maps to a single-block [`DocKind::Config`] document keyed by
//! [`config_context_id`]. The `documents` table — which `create_document_with_path`
//! populates — doubles as the **readdir manifest**: directories are virtual,
//! synthesized from the set of descendant paths. So the doc and its manifest
//! entry are one write, never two stores to drift.
//!
//! ## mtime
//!
//! `getattr` returns an in-memory, monotonically-advancing mtime (bumped on
//! every write). It is **not** a host-file sync — there is no host file. It is a
//! version stamp on the single source of truth, kept only so that the one
//! remaining `FileDocumentCache` consumer (an agent `builtin.file:read
//! /etc/rc/…`) re-reads after a `kj rc set` instead of serving a stale mirror.
//! The "which is truth?" bug class stays gone: there is nothing to disagree.

use async_trait::async_trait;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use dashmap::DashMap;
use kaijutsu_crdt::{BlockKind, ContentType, Role, Status};

use crate::block_store::SharedBlockStore;
use crate::config_doc::{self, config_context_id, CONFIG_DOC_KIND};
use crate::vfs::{DirEntry, FileAttr, FileType, SetAttr, StatFs, VfsError, VfsOps, VfsResult};

/// CRDT-native VFS backend owning a path subtree (e.g. `/etc/rc`).
pub struct ConfigCrdtFs {
    /// CRDT document/block storage — the single source of truth.
    blocks: SharedBlockStore,
    /// Canonical mount root (e.g. `/etc/rc`). The MountTable hands us
    /// mount-relative paths; we re-prepend this to key documents by their
    /// canonical path, so the manifest stays coherent with how `kj rc` and the
    /// lifecycle dispatch reason about scripts.
    root: String,
    /// Per-path version stamp, advanced on write. See module docs.
    mtimes: DashMap<String, SystemTime>,
}

impl ConfigCrdtFs {
    /// Create a backend rooted at canonical path `root` (e.g. `/etc/rc`).
    pub fn new(blocks: SharedBlockStore, root: impl Into<String>) -> Self {
        let mut root = root.into();
        // Normalize: leading slash, no trailing slash.
        while root.ends_with('/') {
            root.pop();
        }
        if !root.starts_with('/') {
            root.insert(0, '/');
        }
        Self {
            blocks,
            root,
            mtimes: DashMap::new(),
        }
    }

    /// Normalize a mount-relative path to a clean relative form (no leading
    /// `/`, `.`/`..` resolved, never escaping above the mount root).
    fn normalize_rel(path: &Path) -> PathBuf {
        let mut result = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(s) => result.push(s),
                Component::ParentDir => {
                    result.pop();
                }
                Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            }
        }
        result
    }

    /// Canonical path for a mount-relative `path` (`<root>/<rel>`, or `<root>`
    /// for the mount root itself).
    fn canonical(&self, path: &Path) -> String {
        let rel = Self::normalize_rel(path);
        if rel.as_os_str().is_empty() {
            self.root.clone()
        } else {
            format!("{}/{}", self.root, rel.to_string_lossy())
        }
    }

    /// Read a file's content, or `None` if no document exists at `canonical`.
    fn content_of(&self, canonical: &str) -> Option<String> {
        let ctx = config_context_id(canonical);
        if !self.blocks.contains(ctx) {
            return None;
        }
        config_doc::read_content(&self.blocks, ctx)
    }

    /// True if any document lives strictly under `canonical` (i.e. it names a
    /// virtual directory). Empty when the DB is absent.
    fn is_dir(&self, canonical: &str) -> bool {
        // The mount root is always a directory.
        if canonical == self.root {
            return true;
        }
        self.blocks
            .documents_under_path(canonical)
            .map(|rows| !rows.is_empty())
            .unwrap_or(false)
    }

    /// Replace (or seed) the single-block content of the document at `canonical`.
    /// Creates the document — carrying its path into the manifest — when absent.
    fn put_content(&self, canonical: &str, text: &str) -> VfsResult<()> {
        let ctx = config_context_id(canonical);
        let crdt_err = |e: String| VfsError::other(format!("crdt: {e}"));

        if self.blocks.contains(ctx) {
            if let Some(block_id) = config_doc::first_block_id(&self.blocks, ctx) {
                let old_len = config_doc::content_char_len(&self.blocks, ctx);
                self.blocks
                    .edit_text(ctx, &block_id, 0, text, old_len)
                    .map_err(|e| crdt_err(e.to_string()))?;
            } else {
                // Registered but blockless (halted replay) — seed the block.
                self.insert_block(ctx, text).map_err(crdt_err)?;
            }
        } else {
            self.blocks
                .create_document_with_path(
                    ctx,
                    CONFIG_DOC_KIND,
                    None,
                    canonical.to_string(),
                )
                .map_err(|e| crdt_err(e.to_string()))?;
            self.insert_block(ctx, text).map_err(crdt_err)?;
        }
        self.mtimes.insert(canonical.to_string(), SystemTime::now());
        Ok(())
    }

    fn insert_block(
        &self,
        ctx: kaijutsu_crdt::ContextId,
        text: &str,
    ) -> Result<(), String> {
        self.blocks
            .insert_block(
                ctx,
                None,
                None,
                Role::System,
                BlockKind::Text,
                text,
                Status::Done,
                ContentType::Plain,
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn mtime_of(&self, canonical: &str) -> SystemTime {
        self.mtimes
            .get(canonical)
            .map(|e| *e.value())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }

    /// True when this backend owns no documents yet — the "fresh install" gate.
    /// Replaces the old host-dir-empty check: seeding is keyed on the CRDT
    /// namespace being empty, not on a host directory.
    pub fn is_empty(&self) -> bool {
        self.blocks
            .documents_under_path(&self.root)
            .map(|rows| rows.is_empty())
            .unwrap_or(true)
    }

    /// Seed every embedded rc default ([`crate::seed_scripts::seed_files`]) into
    /// the CRDT, skipping any path already present. Returns the count newly
    /// written. This replaces `ensure_rc_seed_files` (embedded → host disk): the
    /// embedded tree is the seed, the CRDT is the owner thereafter.
    ///
    /// Per the crash-over-corruption stance a write failure aborts loudly — a
    /// half-seeded rc tree is corruption, and the caller decides whether a fork
    /// can proceed without its stance script.
    pub fn seed_from_embedded(&self) -> VfsResult<usize> {
        self.seed_entries(crate::seed_scripts::seed_files())
    }

    /// Seed `(canonical path, body)` entries into the CRDT, skipping any path
    /// already present or not under this backend's root. Returns the count
    /// newly written. This is the shared absent-only, fail-loud seed core —
    /// [`seed_from_embedded`] (rc, from the embedded tree) and the config
    /// mount (from [`crate::config_seed::config_seed_files`]) both call it, so
    /// one rule serves rc AND config.
    ///
    /// Per the crash-over-corruption stance a write failure aborts loudly — a
    /// half-seeded namespace is corruption, and the caller decides whether to
    /// proceed without it.
    ///
    /// [`seed_from_embedded`]: Self::seed_from_embedded
    pub fn seed_entries<S: AsRef<str>>(
        &self,
        entries: impl IntoIterator<Item = (String, S)>,
    ) -> VfsResult<usize> {
        let mut written = 0usize;
        for (canonical, body) in entries {
            // Only seed paths that fall under this backend's root, and only if
            // absent (a live user edit is never clobbered).
            if !canonical.starts_with(&self.root) {
                continue;
            }
            if self.content_of(&canonical).is_some() {
                continue;
            }
            self.put_content(&canonical, body.as_ref())?;
            written += 1;
        }
        Ok(written)
    }
}

#[async_trait]
impl VfsOps for ConfigCrdtFs {
    async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
        let canonical = self.canonical(path);
        if let Some(content) = self.content_of(&canonical) {
            let mut attr = FileAttr::file(content.len() as u64, 0o644);
            attr.mtime = self.mtime_of(&canonical);
            return Ok(attr);
        }
        if self.is_dir(&canonical) {
            return Ok(FileAttr::directory(0o755));
        }
        Err(VfsError::not_found(canonical))
    }

    async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
        let canonical = self.canonical(path);
        // A file is not a directory.
        if self.content_of(&canonical).is_some() {
            return Err(VfsError::not_a_directory(canonical));
        }
        let rows = self
            .blocks
            .documents_under_path(&canonical)
            .map_err(|e| VfsError::other(format!("crdt: {e}")))?;
        if rows.is_empty() && canonical != self.root {
            return Err(VfsError::not_found(canonical));
        }

        // Synthesize immediate children: for each descendant path, take the
        // first segment under `canonical`. A segment with more path after it is
        // a (virtual) directory; otherwise a file.
        let prefix = format!("{canonical}/");
        let mut seen = std::collections::BTreeMap::new();
        for (p, _ctx) in rows {
            let Some(rest) = p.strip_prefix(&prefix) else {
                continue;
            };
            match rest.split_once('/') {
                Some((dir, _)) => {
                    seen.entry(dir.to_string()).or_insert(FileType::Directory);
                }
                None => {
                    seen.insert(rest.to_string(), FileType::File);
                }
            }
        }
        Ok(seen
            .into_iter()
            .map(|(name, kind)| DirEntry { name, kind })
            .collect())
    }

    async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        let canonical = self.canonical(path);
        match self.content_of(&canonical) {
            Some(content) => {
                let bytes = content.into_bytes();
                let start = (offset as usize).min(bytes.len());
                let end = (start + size as usize).min(bytes.len());
                Ok(bytes[start..end].to_vec())
            }
            None if self.is_dir(&canonical) => Err(VfsError::is_a_directory(canonical)),
            None => Err(VfsError::not_found(canonical)),
        }
    }

    async fn readlink(&self, path: &Path) -> VfsResult<PathBuf> {
        Err(VfsError::NotASymlink(self.canonical(path)))
    }

    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32> {
        let canonical = self.canonical(path);
        if self.is_dir(&canonical) && self.content_of(&canonical).is_none() {
            return Err(VfsError::is_a_directory(canonical));
        }
        // Splice `data` at byte `offset` into the current content. Config/rc
        // content is text; a write that would produce non-UTF-8 is rejected
        // (fail loud) rather than silently corrupting the CRDT text.
        let mut buf = self
            .content_of(&canonical)
            .map(String::into_bytes)
            .unwrap_or_default();
        let off = offset as usize;
        let end = off + data.len();
        if buf.len() < end {
            buf.resize(end, 0);
        }
        buf[off..end].copy_from_slice(data);
        let text = String::from_utf8(buf)
            .map_err(|_| VfsError::other(format!("write to {canonical} produced non-UTF-8 content")))?;
        self.put_content(&canonical, &text)?;
        Ok(data.len() as u32)
    }

    async fn create(&self, path: &Path, _mode: u32) -> VfsResult<FileAttr> {
        let canonical = self.canonical(path);
        if self.content_of(&canonical).is_some() {
            return Err(VfsError::already_exists(canonical));
        }
        if self.is_dir(&canonical) {
            return Err(VfsError::is_a_directory(canonical));
        }
        self.put_content(&canonical, "")?;
        let mut attr = FileAttr::file(0, 0o644);
        attr.mtime = self.mtime_of(&canonical);
        Ok(attr)
    }

    async fn mkdir(&self, path: &Path, _mode: u32) -> VfsResult<FileAttr> {
        // Directories are virtual (synthesized from descendant paths). Creating
        // one is a no-op success so that nested file creation "just works"; a
        // path that already names a file is a conflict.
        let canonical = self.canonical(path);
        if self.content_of(&canonical).is_some() {
            return Err(VfsError::already_exists(canonical));
        }
        Ok(FileAttr::directory(0o755))
    }

    async fn unlink(&self, path: &Path) -> VfsResult<()> {
        let canonical = self.canonical(path);
        if self.content_of(&canonical).is_none() {
            if self.is_dir(&canonical) {
                return Err(VfsError::is_a_directory(canonical));
            }
            return Err(VfsError::not_found(canonical));
        }
        let ctx = config_context_id(&canonical);
        self.blocks
            .delete_document(ctx)
            .map_err(|e| VfsError::other(format!("crdt: {e}")))?;
        self.mtimes.remove(&canonical);
        Ok(())
    }

    async fn rmdir(&self, path: &Path) -> VfsResult<()> {
        let canonical = self.canonical(path);
        if self.content_of(&canonical).is_some() {
            return Err(VfsError::not_a_directory(canonical));
        }
        if self.is_dir(&canonical) {
            return Err(VfsError::directory_not_empty(canonical));
        }
        // Virtual, empty directory: nothing persisted to remove.
        Ok(())
    }

    async fn rename(&self, from: &Path, to: &Path) -> VfsResult<()> {
        let from_c = self.canonical(from);
        let to_c = self.canonical(to);
        let Some(content) = self.content_of(&from_c) else {
            return Err(VfsError::not_found(from_c));
        };
        if self.content_of(&to_c).is_some() {
            return Err(VfsError::already_exists(to_c));
        }
        self.put_content(&to_c, &content)?;
        let ctx = config_context_id(&from_c);
        self.blocks
            .delete_document(ctx)
            .map_err(|e| VfsError::other(format!("crdt: {e}")))?;
        self.mtimes.remove(&from_c);
        Ok(())
    }

    async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()> {
        let canonical = self.canonical(path);
        let mut buf = self
            .content_of(&canonical)
            .ok_or_else(|| VfsError::not_found(canonical.clone()))?
            .into_bytes();
        buf.resize(size as usize, 0);
        let text = String::from_utf8(buf).map_err(|_| {
            VfsError::other(format!("truncate of {canonical} split a multi-byte char"))
        })?;
        self.put_content(&canonical, &text)
    }

    async fn setattr(&self, path: &Path, set: SetAttr) -> VfsResult<FileAttr> {
        // Only size changes have meaning here (truncate/extend the content).
        // mtime/perm/uid/gid are not host-backed; accept the call but reflect
        // only what we actually model, so callers don't get a spurious error.
        let canonical = self.canonical(path);
        if let Some(size) = set.size {
            self.truncate(path, size).await?;
        }
        self.getattr(Path::new(&canonical[self.root.len()..])).await
    }

    async fn symlink(&self, path: &Path, _target: &Path) -> VfsResult<FileAttr> {
        Err(VfsError::other(format!(
            "symlinks unsupported in CRDT config backend: {}",
            self.canonical(path)
        )))
    }

    async fn link(&self, _oldpath: &Path, newpath: &Path) -> VfsResult<FileAttr> {
        Err(VfsError::other(format!(
            "hard links unsupported in CRDT config backend: {}",
            self.canonical(newpath)
        )))
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn statfs(&self) -> VfsResult<StatFs> {
        Ok(StatFs::default())
    }

    async fn real_path(&self, _path: &Path) -> VfsResult<Option<PathBuf>> {
        // CRDT-native: no host filesystem backing.
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::shared_block_store_with_db;
    use crate::kernel_db::KernelDb;
    use kaijutsu_crdt::PrincipalId;
    use std::sync::Arc;

    /// A block store with a real (in-memory) KernelDb, so the `documents`
    /// manifest — which backs readdir — is actually populated by
    /// `create_document_with_path`. A bare `shared_block_store` has no DB and
    /// would make every readdir empty.
    fn fs() -> ConfigCrdtFs {
        let creator = PrincipalId::system();
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let ws_id = db.lock().get_or_create_default_workspace(creator).unwrap();
        let blocks = shared_block_store_with_db(db, ws_id, creator);
        ConfigCrdtFs::new(blocks, "/etc/rc")
    }

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    #[tokio::test]
    async fn write_then_read_roundtrips() {
        let fs = fs();
        fs.write_all(p("coder/create/S00-stance.md"), b"be kind")
            .await
            .unwrap();
        let got = fs.read_all(p("coder/create/S00-stance.md")).await.unwrap();
        assert_eq!(got, b"be kind");
    }

    #[tokio::test]
    async fn overwrite_replaces_not_appends() {
        let fs = fs();
        fs.write_all(p("a/create/S00-x.kai"), b"original content here")
            .await
            .unwrap();
        // write_all truncates first — the shorter content must fully replace,
        // never leave a stale suffix (the append-wipe bug class, inverted).
        fs.write_all(p("a/create/S00-x.kai"), b"new").await.unwrap();
        let got = fs.read_all(p("a/create/S00-x.kai")).await.unwrap();
        assert_eq!(got, b"new");
    }

    #[tokio::test]
    async fn readdir_synthesizes_virtual_directories() {
        let fs = fs();
        fs.write_all(p("coder/create/S00-stance.md"), b"x")
            .await
            .unwrap();
        fs.write_all(p("coder/create/S20-cache.kai"), b"y")
            .await
            .unwrap();
        fs.write_all(p("coder/fork/S30-cache.kai"), b"z")
            .await
            .unwrap();
        fs.write_all(p("default/create/S20-cache.kai"), b"w")
            .await
            .unwrap();

        // Top level: the two context types, as directories.
        let top: Vec<_> = fs.readdir(p("")).await.unwrap();
        assert_eq!(
            top.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["coder", "default"]
        );
        assert!(top.iter().all(|e| e.kind.is_dir()));

        // Under coder: the verbs.
        let verbs: Vec<_> = fs
            .readdir(p("coder"))
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(verbs, vec!["create", "fork"]);

        // Leaf dir: the script files.
        let files = fs.readdir(p("coder/create")).await.unwrap();
        assert_eq!(
            files
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            vec!["S00-stance.md", "S20-cache.kai"]
        );
        assert!(files.iter().all(|e| e.kind.is_file()));
    }

    #[tokio::test]
    async fn getattr_distinguishes_file_dir_and_absent() {
        let fs = fs();
        fs.write_all(p("coder/create/S00-stance.md"), b"hello")
            .await
            .unwrap();

        let file = fs.getattr(p("coder/create/S00-stance.md")).await.unwrap();
        assert!(file.is_file());
        assert_eq!(file.size, 5);

        let dir = fs.getattr(p("coder/create")).await.unwrap();
        assert!(dir.is_dir());

        assert!(matches!(
            fs.getattr(p("coder/create/missing.kai")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn unlink_removes_the_document() {
        let fs = fs();
        fs.write_all(p("a/create/S00-x.kai"), b"data").await.unwrap();
        assert!(fs.exists(p("a/create/S00-x.kai")).await);
        fs.unlink(p("a/create/S00-x.kai")).await.unwrap();
        assert!(!fs.exists(p("a/create/S00-x.kai")).await);
        assert!(matches!(
            fs.unlink(p("a/create/S00-x.kai")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn mtime_advances_on_write_for_cache_coherence() {
        let fs = fs();
        fs.write_all(p("a/create/S00-x.kai"), b"v1").await.unwrap();
        let m1 = fs.getattr(p("a/create/S00-x.kai")).await.unwrap().mtime;
        // Ensure the clock moves.
        std::thread::sleep(std::time::Duration::from_millis(2));
        fs.write_all(p("a/create/S00-x.kai"), b"v2").await.unwrap();
        let m2 = fs.getattr(p("a/create/S00-x.kai")).await.unwrap().mtime;
        assert!(m2 > m1, "mtime must advance on write: {m1:?} !< {m2:?}");
    }

    #[tokio::test]
    async fn real_path_is_none_crdt_native() {
        let fs = fs();
        assert!(fs.real_path(p("a/create/S00-x.kai")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn partial_read_honors_offset_and_size() {
        let fs = fs();
        fs.write_all(p("a/create/S00-x.kai"), b"hello world")
            .await
            .unwrap();
        let mid = fs.read(p("a/create/S00-x.kai"), 6, 5).await.unwrap();
        assert_eq!(mid, b"world");
    }

    #[tokio::test]
    async fn seed_from_embedded_populates_and_is_idempotent() {
        let fs = fs();
        assert!(fs.is_empty(), "a fresh backend owns nothing");

        let n = fs.seed_from_embedded().unwrap();
        assert_eq!(
            n,
            crate::seed_scripts::seed_files().len(),
            "every embedded seed should be written on a fresh backend"
        );
        assert!(!fs.is_empty(), "seeded backend is no longer empty");

        // A known seed round-trips through the CRDT (read via the VFS, not disk).
        let stance = fs
            .read_all(p("coder/create/S00-stance.md"))
            .await
            .expect("coder stance must seed");
        assert!(!stance.is_empty());

        // readdir reflects the seeded tree.
        let types: Vec<_> = fs
            .readdir(p(""))
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(types.contains(&"coder".to_string()), "types: {types:?}");
        assert!(types.contains(&"default".to_string()), "types: {types:?}");

        // Second seed writes nothing (idempotent: paths already present).
        assert_eq!(fs.seed_from_embedded().unwrap(), 0);
    }

    /// The same backend, mounted at `/etc/config`, owns the config files via
    /// the shared `seed_entries` core — proving one backend type serves rc AND
    /// config (slice 2). A known config file round-trips through the VFS.
    #[tokio::test]
    async fn config_mount_seeds_and_reads_via_seed_entries() {
        let creator = PrincipalId::system();
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::in_memory().unwrap()));
        let ws_id = db.lock().get_or_create_default_workspace(creator).unwrap();
        let blocks = shared_block_store_with_db(db, ws_id, creator);
        let fs = ConfigCrdtFs::new(blocks, "/etc/config");

        assert!(fs.is_empty(), "fresh config mount owns nothing");
        let n = fs.seed_entries(crate::config_seed::config_seed_files()).unwrap();
        assert_eq!(n, 4, "the four config files seed on a fresh mount");

        // models.toml round-trips through the VFS (read mount-relative).
        let models = fs.read_all(p("models.toml")).await.unwrap();
        assert_eq!(models, crate::config_seed::DEFAULT_MODELS_CONFIG.as_bytes());

        // readdir lists the flat config set.
        let entries: Vec<_> = fs
            .readdir(p(""))
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(entries.contains(&"theme.toml".to_string()), "entries: {entries:?}");
        assert!(entries.contains(&"system.md".to_string()), "entries: {entries:?}");

        // Idempotent: a second seed writes nothing.
        assert_eq!(fs.seed_entries(crate::config_seed::config_seed_files()).unwrap(), 0);
    }

    /// A user edit must survive re-seeding — seed only fills absent paths, it
    /// never clobbers live content (parity with the old host `ensure` contract).
    #[tokio::test]
    async fn re_seed_preserves_user_edits() {
        let fs = fs();
        fs.seed_from_embedded().unwrap();
        fs.write_all(p("coder/create/S00-stance.md"), b"# my override")
            .await
            .unwrap();
        assert_eq!(fs.seed_from_embedded().unwrap(), 0, "nothing new to seed");
        let got = fs.read_all(p("coder/create/S00-stance.md")).await.unwrap();
        assert_eq!(got, b"# my override", "re-seed clobbered a live edit");
    }
}
