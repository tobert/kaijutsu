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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use dashmap::DashMap;
use kaijutsu_crdt::{BlockKind, ContentType, Role, Status};
use kaijutsu_types::DocKind;

use crate::block_store::SharedBlockStore;
use crate::config_doc::{self, config_context_id, CONFIG_DOC_KIND};
use crate::vfs::{DirEntry, FileAttr, FileType, SetAttr, StatFs, VfsError, VfsOps, VfsResult};

/// Max symlink hops before [`VfsError::TooManySymlinks`]. Guards against cycles
/// (`a → b → a`) and pathological chains. POSIX uses values around 8–40; rc
/// composition needs only one or two hops, so 8 is generous.
const MAX_SYMLINK_DEPTH: usize = 8;

/// CRDT-native VFS backend owning a path subtree (e.g. `/etc/rc`).
pub struct ConfigCrdtFs {
    /// CRDT document/block storage — the single source of truth.
    blocks: SharedBlockStore,
    /// Canonical mount root (e.g. `/etc/rc`). The MountTable hands us
    /// mount-relative paths; we re-prepend this to key documents by their
    /// canonical path, so the manifest stays coherent with how `kj rc` and the
    /// lifecycle dispatch reason about scripts.
    root: String,
    /// Per-path wall-clock mtime, refreshed on write. **Display only** (`ls -l`,
    /// SFTP attrs); not the coherence signal. See module docs and `generations`.
    mtimes: DashMap<String, SystemTime>,
    /// Per-path strictly-advancing generation — the coherence stamp the file
    /// cache compares (and a future SFTP `OPEN` captures for its TOCTOU guard).
    /// Sourced from `gen_clock`, so two writes within one `SystemTime::now()`
    /// tick still produce distinct, increasing values. See `docs/sftp.md`.
    generations: DashMap<String, u64>,
    /// Monotonic source for `generations`. Never reused, never goes backward.
    gen_clock: AtomicU64,
    /// Display-mtime fallback for paths we hold no write record for (seeded
    /// defaults, freshly-replayed docs). Replaces the old `UNIX_EPOCH` (1970)
    /// default, which made caching SFTP clients make bad decisions.
    created: SystemTime,
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
            generations: DashMap::new(),
            gen_clock: AtomicU64::new(0),
            created: SystemTime::now(),
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

    /// The link target of the symlink document at `canonical`, or `None` when no
    /// symlink lives there. This is the git-style "mode bit" check: a doc is a
    /// link iff its [`DocKind`] is `Symlink` — never inferred from content, which
    /// for a link *is* the target string and would otherwise read as a file body.
    fn link_target(&self, canonical: &str) -> Option<String> {
        let ctx = config_context_id(canonical);
        if self.blocks.document_kind(ctx) != Some(DocKind::Symlink) {
            return None;
        }
        config_doc::read_content(&self.blocks, ctx)
    }

    /// Resolve a target string (the link body) against the link's own path into
    /// a canonical absolute path under this mount. Absolute targets are taken
    /// as-is (normalized); relative targets resolve against the link's parent
    /// directory — POSIX symlink semantics. Fails loud if the result escapes the
    /// mount root: cross-mount resolution would have to go through the
    /// `MountTable` (and its permission gate), which this backend cannot see.
    fn resolve_target(&self, link_canonical: &str, target: &str) -> VfsResult<String> {
        let joined = if target.starts_with('/') {
            target.to_string()
        } else {
            let parent = link_canonical.rsplit_once('/').map_or("", |(p, _)| p);
            format!("{parent}/{target}")
        };
        let norm = normalize_abs(&joined);
        let under_root =
            norm == self.root || norm.starts_with(&format!("{}/", self.root));
        if !under_root {
            return Err(VfsError::other(format!(
                "symlink target {norm} escapes mount {}",
                self.root
            )));
        }
        Ok(norm)
    }

    /// Follow a symlink chain to its terminal canonical path, capped at
    /// [`MAX_SYMLINK_DEPTH`] hops. The terminal path need not exist (a dangling
    /// link resolves fine here and surfaces as `NotFound` only when read) — but
    /// a cycle or an over-long chain fails loud with `TooManySymlinks`. A
    /// non-symlink path resolves to itself.
    fn resolve(&self, canonical: &str) -> VfsResult<String> {
        let mut current = canonical.to_string();
        for _ in 0..MAX_SYMLINK_DEPTH {
            match self.link_target(&current) {
                None => return Ok(current),
                Some(target) => current = self.resolve_target(&current, &target)?,
            }
        }
        Err(VfsError::TooManySymlinks)
    }

    /// Follow any symlink chain at `canonical` (an absolute path **already under
    /// this mount root**, e.g. `/etc/rc/coder/create/S10-binding.kai`) to its
    /// terminal canonical path. Public so off-backend callers that bind to a
    /// document by path — notably the vi editor's `resolve_editor_target` — land
    /// on the SAME terminal block this backend reads/executes, instead of the
    /// symlink's own block. A non-symlink path resolves to itself; a cycle or an
    /// over-long chain fails loud.
    pub fn resolve_canonical(&self, canonical: &str) -> VfsResult<String> {
        self.resolve(canonical)
    }

    /// Create the symlink document at `canonical` whose single block holds the
    /// raw `target` string (git-style). Caller guarantees nothing already lives
    /// at the path.
    fn put_link(&self, canonical: &str, target: &str) -> VfsResult<()> {
        let ctx = config_context_id(canonical);
        let crdt_err = |e: String| VfsError::other(format!("crdt: {e}"));
        self.blocks
            .create_document_with_path(ctx, DocKind::Symlink, None, canonical.to_string())
            .map_err(|e| crdt_err(e.to_string()))?;
        self.insert_block(ctx, target).map_err(crdt_err)?;
        self.bump(canonical);
        Ok(())
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
        self.bump(canonical);
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

    /// Record a content mutation at `canonical`: advance the coherence
    /// generation (strictly, from `gen_clock`) and refresh the display mtime.
    ///
    /// The drawn value is folded in with `max`, not a blind `insert`: two
    /// writers racing on the same path each draw a distinct `gen_clock` value,
    /// but a bare fetch-then-insert could land them out of order and *reverse*
    /// the stored stamp (writer A draws 5, B draws 6, B inserts 6, A clobbers
    /// with 5). DashMap's `entry` lock serializes the read-modify-write per key,
    /// so the higher draw always wins regardless of insert order — generation
    /// never regresses. (A `getattr` between the draw and the store sees the
    /// prior value or 0, never a regressed one — a transient that self-heals.)
    fn bump(&self, canonical: &str) {
        let g = self.gen_clock.fetch_add(1, Ordering::Relaxed) + 1;
        self.generations
            .entry(canonical.to_string())
            .and_modify(|v| *v = (*v).max(g))
            .or_insert(g);
        self.mtimes.insert(canonical.to_string(), SystemTime::now());
    }

    /// Drop the per-path stamps for a removed path. A later recreate gets a
    /// fresh, higher generation from `gen_clock`, so any cache entry that loaded
    /// the old file still sees the new one as changed.
    fn forget(&self, canonical: &str) {
        self.generations.remove(canonical);
        self.mtimes.remove(canonical);
    }

    /// Strictly-advancing coherence stamp for `canonical`; `0` ("unknown") when
    /// we hold no write record (a seeded default or replayed doc not yet
    /// mutated through this backend instance).
    fn gen_of(&self, canonical: &str) -> u64 {
        self.generations
            .get(canonical)
            .map(|e| *e.value())
            .unwrap_or(0)
    }

    fn mtime_of(&self, canonical: &str) -> SystemTime {
        self.mtimes
            .get(canonical)
            .map(|e| *e.value())
            .unwrap_or(self.created)
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
        // Collect first so seed symlinks resolve against the *full* set: a seed
        // file whose body is just a path to another seeded path becomes a
        // symlink (the in-repo init.d composition format — see
        // [`seed_link_target`]). Order-independent: we check the path set, not
        // what's already written.
        let entries: Vec<(String, S)> = entries.into_iter().collect();
        let known: std::collections::HashSet<String> =
            entries.iter().map(|(p, _)| p.clone()).collect();

        let mut written = 0usize;
        for (canonical, body) in &entries {
            // Only seed paths that fall under this backend's root, and only if
            // absent (a live user edit is never clobbered).
            if !canonical.starts_with(&self.root) {
                continue;
            }
            if self.content_of(canonical).is_some() {
                continue;
            }
            match seed_link_target(canonical, body.as_ref(), &known) {
                Some(target) => self.put_link(canonical, &target)?,
                None => self.put_content(canonical, body.as_ref())?,
            }
            written += 1;
        }
        Ok(written)
    }
}

/// Normalize an absolute path string: collapse `//`, drop `.`, resolve `..`
/// (popping the prior segment). Returns a clean absolute path (`/a/b/c`). Used
/// to canonicalize a symlink target before keying its document.
fn normalize_abs(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

/// If `body` is a **seed symlink** — its sole content a path resolving to
/// another seeded path in `known` — return the raw target string; otherwise
/// `None` (seed it as a literal file).
///
/// This is the in-repo init.d composition format: a checked-in seed file whose
/// content is just the target path seeds as a symlink instead of a literal
/// file. `include_dir!` can't carry real symlinks (it follows them and embeds
/// the target's bytes), so the link relationship rides in the file *content*
/// and is reconstructed here. Detection is deliberately confined to the
/// authored, closed seed set and guarded by "the target must be a real seeded
/// path", so a one-line script can't be mistaken for a link.
pub fn seed_link_target(
    link_path: &str,
    body: &str,
    known: &std::collections::HashSet<String>,
) -> Option<String> {
    let t = body.trim();
    // A link body is a single path token, not a script: one line, path-shaped.
    if t.is_empty() || t.contains('\n') || !t.contains('/') {
        return None;
    }
    let resolved = if t.starts_with('/') {
        normalize_abs(t)
    } else {
        let parent = link_path.rsplit_once('/').map_or("", |(p, _)| p);
        normalize_abs(&format!("{parent}/{t}"))
    };
    (resolved != link_path && known.contains(&resolved)).then(|| t.to_string())
}

#[async_trait]
impl VfsOps for ConfigCrdtFs {
    async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
        let canonical = self.canonical(path);
        // lstat-like: report the link itself, not its target (matches
        // LocalBackend, which uses `symlink_metadata`). Must precede the file
        // branch — a link's content *is* the target and would read as a file.
        if let Some(target) = self.link_target(&canonical) {
            let mut attr = FileAttr::symlink(target.len() as u64);
            attr.mtime = self.mtime_of(&canonical);
            attr.generation = self.gen_of(&canonical);
            return Ok(attr);
        }
        if let Some(content) = self.content_of(&canonical) {
            let mut attr = FileAttr::file(content.len() as u64, 0o644);
            attr.mtime = self.mtime_of(&canonical);
            attr.generation = self.gen_of(&canonical);
            return Ok(attr);
        }
        if self.is_dir(&canonical) {
            let mut attr = FileAttr::directory(0o755);
            attr.mtime = self.mtime_of(&canonical);
            attr.generation = self.gen_of(&canonical);
            return Ok(attr);
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
        for (p, _ctx, kind) in rows {
            let Some(rest) = p.strip_prefix(&prefix) else {
                continue;
            };
            match rest.split_once('/') {
                Some((dir, _)) => {
                    seen.entry(dir.to_string()).or_insert(FileType::Directory);
                }
                None => {
                    // A leaf doc is a symlink or a regular file per its kind
                    // (the git-style mode bit), so `ls`/readdir and
                    // `load_rc_scripts` can see links without a follow-up stat.
                    let ft = if kind == DocKind::Symlink {
                        FileType::Symlink
                    } else {
                        FileType::File
                    };
                    seen.insert(rest.to_string(), ft);
                }
            }
        }
        Ok(seen
            .into_iter()
            .map(|(name, kind)| DirEntry { name, kind })
            .collect())
    }

    async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        // Auto-follow symlinks (POSIX `read` semantics) so every consumer —
        // `load_rc_scripts`, an agent `builtin.file:read`, `cat` — gets the
        // target's bytes for free. A dangling link surfaces here as NotFound.
        let canonical = self.resolve(&self.canonical(path))?;
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

    /// Read the whole file, following symlinks. Overridden because the trait's
    /// default sizes the read from `getattr`, which for a link is lstat-like and
    /// reports the *target-string* length — that would truncate the followed
    /// content (and silently shorten an rc script behind a link). Here we size
    /// from the resolved target. This is the path `load_rc_scripts` takes.
    async fn read_all(&self, path: &Path) -> VfsResult<Vec<u8>> {
        let canonical = self.resolve(&self.canonical(path))?;
        match self.content_of(&canonical) {
            Some(content) => Ok(content.into_bytes()),
            None if self.is_dir(&canonical) => Err(VfsError::is_a_directory(canonical)),
            None => Err(VfsError::not_found(canonical)),
        }
    }

    async fn readlink(&self, path: &Path) -> VfsResult<PathBuf> {
        let canonical = self.canonical(path);
        // The raw stored target, unresolved (POSIX `readlink`).
        match self.link_target(&canonical) {
            Some(target) => Ok(PathBuf::from(target)),
            None => Err(VfsError::NotASymlink(canonical)),
        }
    }

    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32> {
        // Follow symlinks (POSIX): a write through a link edits its target, so a
        // link never gets silently overwritten with file bytes. A write through
        // a dangling link creates the target.
        let canonical = self.resolve(&self.canonical(path))?;
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
        attr.generation = self.gen_of(&canonical);
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
        self.forget(&canonical);
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
        let from_link = self.link_target(&from_c);
        // content_of is Some for a file *or* a link (its body is the target);
        // None for both means the source is absent.
        let from_content = self.content_of(&from_c);
        if from_link.is_none() && from_content.is_none() {
            return Err(VfsError::not_found(from_c));
        }
        if self.content_of(&to_c).is_some() {
            return Err(VfsError::already_exists(to_c));
        }
        // Preserve link-ness: a renamed symlink stays a symlink rather than
        // collapsing into a regular file whose body is the target path.
        if let Some(target) = from_link {
            self.put_link(&to_c, &target)?;
        } else {
            self.put_content(&to_c, &from_content.unwrap())?;
        }
        let ctx = config_context_id(&from_c);
        self.blocks
            .delete_document(ctx)
            .map_err(|e| VfsError::other(format!("crdt: {e}")))?;
        self.forget(&from_c);
        Ok(())
    }

    async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()> {
        let canonical = self.resolve(&self.canonical(path))?;
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
        // A size change is a content mutation — `truncate` advances generation.
        // An mtime change is display-only: we now honor it (so `cp -p`, `touch
        // -d`, rsync's mtime preservation no longer silently vanish) by storing
        // it in the display-mtime map, but deliberately do NOT bump generation,
        // so a pure attribute touch never triggers a needless cache reload.
        // perm/uid/gid stay unmodeled (virtual fs), accepted without error.
        let canonical = self.canonical(path);
        if let Some(size) = set.size {
            self.truncate(path, size).await?;
        }
        if let Some(mtime) = set.mtime {
            self.mtimes.insert(canonical.clone(), mtime);
        }
        self.getattr(Path::new(&canonical[self.root.len()..])).await
    }

    async fn symlink(&self, path: &Path, target: &Path) -> VfsResult<FileAttr> {
        let canonical = self.canonical(path);
        // content_of returns Some for a file *or* an existing link (its body is
        // the target), so this rejects clobbering either.
        if self.content_of(&canonical).is_some() {
            return Err(VfsError::already_exists(canonical));
        }
        if self.is_dir(&canonical) {
            return Err(VfsError::is_a_directory(canonical));
        }
        let target_str = target.to_string_lossy();
        if target_str.is_empty() {
            return Err(VfsError::other(format!(
                "symlink {canonical}: empty target"
            )));
        }
        // Dangling targets are allowed at create time (git/POSIX) — resolution
        // failure surfaces loudly on read, not here.
        self.put_link(&canonical, &target_str)?;
        Ok(FileAttr::symlink(target_str.len() as u64))
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

    fn owns_config_docs(&self) -> bool {
        true
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

    /// The guarantee wall-clock mtime cannot make: two writes landing within a
    /// single `SystemTime` tick still produce strictly-increasing generation,
    /// so the file cache never serves stale content after a rapid rewrite. This
    /// is the coherence bug the generation primitive fixes — no `sleep` here, on
    /// purpose.
    #[tokio::test]
    async fn generation_strictly_advances_within_one_mtime_tick() {
        let fs = fs();
        fs.write_all(p("a/create/S00-x.kai"), b"v1").await.unwrap();
        let g1 = fs.getattr(p("a/create/S00-x.kai")).await.unwrap().generation;
        fs.write_all(p("a/create/S00-x.kai"), b"v2").await.unwrap();
        let g2 = fs.getattr(p("a/create/S00-x.kai")).await.unwrap().generation;
        assert_ne!(g1, 0, "a written file must carry a nonzero generation");
        assert!(g2 > g1, "generation must strictly advance: {g1} !< {g2}");
    }

    /// Regression: a path with no write record (here a virtual directory) must
    /// report a real timestamp, not the `UNIX_EPOCH` (1970) the old default
    /// served — which made caching SFTP clients make bad decisions.
    #[tokio::test]
    async fn unwritten_path_mtime_is_not_epoch() {
        let fs = fs();
        fs.write_all(p("a/create/S00-x.kai"), b"data").await.unwrap();
        let dir = fs.getattr(p("a/create")).await.unwrap();
        assert!(dir.is_dir());
        assert!(
            dir.mtime > SystemTime::UNIX_EPOCH,
            "virtual-dir mtime must be a real timestamp, not the 1970 epoch"
        );
    }

    /// `setattr(mtime)` is honored for display (so `cp -p` / `touch -d` / rsync
    /// mtime-preservation no longer silently vanish) but must NOT advance the
    /// coherence generation — a pure attribute touch should never trigger a
    /// cache reload.
    #[tokio::test]
    async fn setattr_mtime_is_display_only_and_does_not_bump_generation() {
        let fs = fs();
        fs.write_all(p("a/create/S00-x.kai"), b"data").await.unwrap();
        let g_before = fs.getattr(p("a/create/S00-x.kai")).await.unwrap().generation;

        let stamp = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        fs.setattr(p("a/create/S00-x.kai"), SetAttr::new().with_mtime(stamp))
            .await
            .unwrap();

        let attr = fs.getattr(p("a/create/S00-x.kai")).await.unwrap();
        assert_eq!(attr.mtime, stamp, "setattr(mtime) must be reflected for display");
        assert_eq!(
            attr.generation, g_before,
            "a pure mtime setattr must NOT bump the coherence generation"
        );
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
            .read_all(p("coder/create/S00-stance.kai"))
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

    // ── seed-symlink detection (in-repo init.d composition format) ──────

    #[test]
    fn seed_link_target_detects_resolving_paths_only() {
        let known: std::collections::HashSet<String> = [
            "/etc/rc/lib/create/S20-cache.kai",
            "/etc/rc/coder/create/S20-cache.kai",
            "/etc/rc/coder/create/S00-stance.kai",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        // Absolute path resolving to a known seed → link (raw target returned).
        assert_eq!(
            seed_link_target(
                "/etc/rc/coder/create/S20-cache.kai",
                "/etc/rc/lib/create/S20-cache.kai\n",
                &known,
            ),
            Some("/etc/rc/lib/create/S20-cache.kai".to_string())
        );
        // Relative path resolving against the link's parent → link.
        assert_eq!(
            seed_link_target(
                "/etc/rc/coder/create/S20-cache.kai",
                "../../lib/create/S20-cache.kai",
                &known,
            ),
            Some("../../lib/create/S20-cache.kai".to_string())
        );
        // A real (multi-line) script body → NOT a link.
        assert_eq!(
            seed_link_target(
                "/etc/rc/coder/create/S00-stance.kai",
                "# stance\nkj block create --role system\n",
                &known,
            ),
            None
        );
        // A single path-shaped line that does NOT resolve to a seed → NOT a
        // link (the guard against mistaking a one-line script for a link).
        assert_eq!(
            seed_link_target(
                "/etc/rc/coder/create/S00-stance.kai",
                "/etc/rc/nope/create/x.kai",
                &known,
            ),
            None
        );
        // A self-referential path is not a link.
        assert_eq!(
            seed_link_target(
                "/etc/rc/coder/create/S20-cache.kai",
                "/etc/rc/coder/create/S20-cache.kai",
                &known,
            ),
            None
        );
    }

    #[tokio::test]
    async fn seed_entries_reconstructs_symlink_from_path_body() {
        let fs = fs();
        // Two entries: a canonical body and a path-content link to it.
        let entries = vec![
            (
                "/etc/rc/lib/create/S20-cache.kai".to_string(),
                "kj cache add --target=tools --ttl=extended",
            ),
            (
                "/etc/rc/coder/create/S20-cache.kai".to_string(),
                "/etc/rc/lib/create/S20-cache.kai",
            ),
        ];
        let n = fs.seed_entries(entries).unwrap();
        assert_eq!(n, 2);

        // The per-type path seeded as a real symlink (not a file of the path).
        let attr = fs.getattr(p("coder/create/S20-cache.kai")).await.unwrap();
        assert_eq!(attr.kind, FileType::Symlink);
        // …and reading it follows to the canonical body.
        let got = fs.read_all(p("coder/create/S20-cache.kai")).await.unwrap();
        assert_eq!(got, b"kj cache add --target=tools --ttl=extended");
    }

    // ── symlinks (init.d-style rc composition) ──────────────────────────

    #[tokio::test]
    async fn symlink_read_follows_to_target() {
        let fs = fs();
        fs.write_all(p("lib/create/binding.kai"), b"allow rc-write")
            .await
            .unwrap();
        fs.symlink(
            p("coder/create/S10-binding.kai"),
            Path::new("/etc/rc/lib/create/binding.kai"),
        )
        .await
        .unwrap();

        // read auto-follows the link to the target's bytes.
        let got = fs.read_all(p("coder/create/S10-binding.kai")).await.unwrap();
        assert_eq!(got, b"allow rc-write");

        // readlink returns the raw stored target, unresolved.
        let target = fs.readlink(p("coder/create/S10-binding.kai")).await.unwrap();
        assert_eq!(target, Path::new("/etc/rc/lib/create/binding.kai"));

        // getattr is lstat-like: it reports the link itself.
        let attr = fs.getattr(p("coder/create/S10-binding.kai")).await.unwrap();
        assert_eq!(attr.kind, FileType::Symlink);
    }

    #[tokio::test]
    async fn readdir_reports_symlink_kind() {
        let fs = fs();
        fs.write_all(p("lib/create/stance.md"), b"be kind")
            .await
            .unwrap();
        fs.symlink(
            p("coder/create/S00-stance.md"),
            Path::new("/etc/rc/lib/create/stance.md"),
        )
        .await
        .unwrap();

        let entries = fs.readdir(p("coder/create")).await.unwrap();
        let e = entries.iter().find(|e| e.name == "S00-stance.md").unwrap();
        assert_eq!(e.kind, FileType::Symlink);
    }

    #[tokio::test]
    async fn symlink_relative_target_resolves_against_link_dir() {
        let fs = fs();
        fs.write_all(p("coder/create/real.kai"), b"body").await.unwrap();
        // Relative target resolves against the link's parent dir.
        fs.symlink(p("coder/create/S05-link.kai"), Path::new("real.kai"))
            .await
            .unwrap();
        let got = fs.read_all(p("coder/create/S05-link.kai")).await.unwrap();
        assert_eq!(got, b"body");

        // A `..`-relative target resolves too.
        fs.write_all(p("lib/create/shared.kai"), b"shared").await.unwrap();
        fs.symlink(
            p("coder/create/S06-shared.kai"),
            Path::new("../../lib/create/shared.kai"),
        )
        .await
        .unwrap();
        let got = fs.read_all(p("coder/create/S06-shared.kai")).await.unwrap();
        assert_eq!(got, b"shared");
    }

    #[tokio::test]
    async fn symlink_cycle_fails_loud() {
        let fs = fs();
        fs.symlink(p("a/create/S00-x.kai"), Path::new("/etc/rc/a/create/S00-y.kai"))
            .await
            .unwrap();
        fs.symlink(p("a/create/S00-y.kai"), Path::new("/etc/rc/a/create/S00-x.kai"))
            .await
            .unwrap();
        assert!(matches!(
            fs.read_all(p("a/create/S00-x.kai")).await,
            Err(VfsError::TooManySymlinks)
        ));
    }

    #[tokio::test]
    async fn dangling_symlink_creates_ok_but_read_is_not_found() {
        let fs = fs();
        // Creating a link to a non-existent target succeeds (git/POSIX).
        fs.symlink(
            p("coder/create/S00-gone.md"),
            Path::new("/etc/rc/lib/create/missing.md"),
        )
        .await
        .unwrap();
        // The link exists (getattr/readlink work)…
        assert_eq!(
            fs.readlink(p("coder/create/S00-gone.md")).await.unwrap(),
            Path::new("/etc/rc/lib/create/missing.md")
        );
        // …but reading through it fails loud, not as empty content.
        assert!(matches!(
            fs.read_all(p("coder/create/S00-gone.md")).await,
            Err(VfsError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn symlink_target_escaping_mount_fails_on_read() {
        let fs = fs();
        fs.symlink(p("coder/create/S00-evil.md"), Path::new("/etc/config/theme.toml"))
            .await
            .unwrap();
        // Resolution is confined to the mount root — a cross-mount target is
        // rejected loudly rather than silently reaching another backend's data.
        let err = fs.read_all(p("coder/create/S00-evil.md")).await.unwrap_err();
        assert!(
            format!("{err}").contains("escapes mount"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn write_through_symlink_edits_target_not_link() {
        let fs = fs();
        fs.write_all(p("lib/create/real.kai"), b"v1").await.unwrap();
        fs.symlink(
            p("coder/create/S10-real.kai"),
            Path::new("/etc/rc/lib/create/real.kai"),
        )
        .await
        .unwrap();

        fs.write_all(p("coder/create/S10-real.kai"), b"v2").await.unwrap();

        // The target changed…
        assert_eq!(fs.read_all(p("lib/create/real.kai")).await.unwrap(), b"v2");
        // …and the link is still a link (not collapsed into a file).
        assert_eq!(
            fs.getattr(p("coder/create/S10-real.kai")).await.unwrap().kind,
            FileType::Symlink
        );
    }

    #[tokio::test]
    async fn unlink_removes_link_not_target() {
        let fs = fs();
        fs.write_all(p("lib/create/real.kai"), b"keep").await.unwrap();
        fs.symlink(
            p("coder/create/S10-real.kai"),
            Path::new("/etc/rc/lib/create/real.kai"),
        )
        .await
        .unwrap();

        fs.unlink(p("coder/create/S10-real.kai")).await.unwrap();
        assert!(!fs.exists(p("coder/create/S10-real.kai")).await, "link gone");
        // The target survives.
        assert_eq!(fs.read_all(p("lib/create/real.kai")).await.unwrap(), b"keep");
    }

    #[tokio::test]
    async fn symlink_over_existing_path_conflicts() {
        let fs = fs();
        fs.write_all(p("coder/create/S00-x.kai"), b"file").await.unwrap();
        assert!(matches!(
            fs.symlink(p("coder/create/S00-x.kai"), Path::new("/etc/rc/lib/y.kai"))
                .await,
            Err(VfsError::AlreadyExists(_))
        ));
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
