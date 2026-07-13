//! VFS mount table with longest-prefix routing.
//!
//! Routes filesystem operations to the appropriate backend based on path.

use async_trait::async_trait;
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use tokio::sync::RwLock;

use super::error::{VfsError, VfsResult};
use super::ops::VfsOps;
use super::types::{DirEntry, FileAttr, FileType, SetAttr, SnapshotNode, SnapshotResult, StatFs};

/// Baseline listing-generation for a directory never observed to mutate
/// since kernel boot (stage-1 groundwork, `docs/scenes/vfs.md`). Nonzero so a
/// client can tell "kernel reports live generation 1" apart from `0`, which
/// stays reserved — matching [`FileAttr::generation`]'s convention — for
/// "unknown / never observed".
pub const BASELINE_GENERATION: u64 = 1;

/// Hard ceiling on nodes returned by a single [`MountTable::snapshot`] call,
/// regardless of the caller's requested `max_entries` — walking `/` with no
/// cap must be structurally impossible (`docs/scenes/vfs.md` stage-0
/// plumbing). Generous enough for a meaningful LOD chunk, small enough to
/// bound one reply's memory/wire size.
pub const SNAPSHOT_MAX_ENTRIES: u32 = 5_000;

/// Hard ceiling on requested traversal depth for [`MountTable::snapshot`],
/// independent of `max_entries` — bounds recursion even against a
/// shallow-but-absurdly-wide `depth` request. No real project tree nests
/// this deep.
pub const SNAPSHOT_MAX_DEPTH: u32 = 64;

/// Information about a mount point.
#[derive(Debug, Clone)]
pub struct MountInfo {
    /// The mount path (e.g., "/mnt/project").
    pub path: PathBuf,
    /// Whether this mount is read-only.
    pub read_only: bool,
}

/// Routes filesystem operations to mounted backends.
///
/// Mount points are matched by longest prefix. For example, if `/mnt` and
/// `/mnt/project` are both mounted, a path like `/mnt/project/src/main.rs`
/// will be routed to the `/mnt/project` mount.
///
/// Once `freeze()` is called, `mount()` and `unmount()` become no-ops and
/// return `false`. This is the security perimeter: the set of paths visible
/// to the kernel is fixed at startup and cannot be expanded at runtime.
pub struct MountTable {
    /// Mount points, keyed by normalized path.
    mounts: RwLock<BTreeMap<PathBuf, Arc<dyn VfsOps>>>,
    /// When true, mount()/unmount() are rejected.
    frozen: AtomicBool,
    /// Per-directory listing-generation stamps (stage-1 groundwork,
    /// `docs/scenes/vfs.md`), keyed by the directory's normalized full VFS
    /// path — this table is the chokepoint every mutating op funnels
    /// through, so tracking generations here (rather than per-backend) sees
    /// every VFS-mediated structural change regardless of which backend owns
    /// the path. In-memory only, `DashMap` for lock-independence from
    /// `mounts`. See [`Self::snapshot`] for the bump policy.
    generations: dashmap::DashMap<PathBuf, u64>,
    /// Per-directory ACTIVITY totals (Lane K, FSN slice-1 digest stream,
    /// `docs/scenes/vfs.md`) — ABSOLUTE monotonic counts of content +
    /// structure mutations attributed to the directory, keyed exactly like
    /// [`Self::generations`] (normalized parent-dir VFS path via
    /// [`Self::normalize_mount_path`] + [`Self::parent_dir`]). Where
    /// `generations` tracks the name-SET (structure only), `activity` tracks
    /// HEAT: every op that bumps a generation also bumps activity, plus
    /// write/truncate/setattr which bump activity alone. In-memory only —
    /// reset-on-restart is fine and documented, same reasoning as
    /// `generations` (a restart's blank slate self-corrects as new activity
    /// arrives; there is no "since when" promise to keep).
    activity: dashmap::DashMap<PathBuf, u64>,
    /// Global activity total — the sum of every bump across every directory,
    /// tracked separately (not derived by summing `activity` each tick) so a
    /// quiet kernel's digest tick is a single relaxed load-and-compare
    /// against the subscriber's cursor, never a full-map walk.
    activity_epoch: AtomicU64,
}

impl std::fmt::Debug for MountTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MountTable")
            .field("mounts", &"<locked>")
            .finish()
    }
}

impl Default for MountTable {
    fn default() -> Self {
        Self::new()
    }
}

impl MountTable {
    /// Create a new empty mount table.
    pub fn new() -> Self {
        Self {
            mounts: RwLock::new(BTreeMap::new()),
            frozen: AtomicBool::new(false),
            generations: dashmap::DashMap::new(),
            activity: dashmap::DashMap::new(),
            activity_epoch: AtomicU64::new(0),
        }
    }

    /// Freeze the mount table. After this, `mount()` and `unmount()` are rejected.
    ///
    /// This establishes the security perimeter: the set of real paths visible
    /// to the kernel is fixed and cannot be expanded at runtime.
    pub fn freeze(&self) {
        self.frozen.store(true, Ordering::Release);
    }

    /// Check whether the mount table is frozen.
    pub fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::Acquire)
    }

    /// Mount a filesystem at the given path.
    ///
    /// The path should be absolute (start with `/`). If a filesystem is
    /// already mounted at this path, it will be replaced.
    ///
    /// Returns `false` if the table is frozen.
    pub async fn mount(&self, path: impl Into<PathBuf>, fs: impl VfsOps + 'static) -> bool {
        if self.is_frozen() {
            tracing::warn!("mount rejected: mount table is frozen");
            return false;
        }
        let path = Self::normalize_mount_path(path.into());
        let mut mounts = self.mounts.write().await;
        mounts.insert(path, Arc::new(fs));
        true
    }

    /// Mount a filesystem (already wrapped in Arc) at the given path.
    ///
    /// Returns `false` if the table is frozen.
    pub async fn mount_arc(&self, path: impl Into<PathBuf>, fs: Arc<dyn VfsOps>) -> bool {
        if self.is_frozen() {
            tracing::warn!("mount_arc rejected: mount table is frozen");
            return false;
        }
        let path = Self::normalize_mount_path(path.into());
        let mut mounts = self.mounts.write().await;
        mounts.insert(path, fs);
        true
    }

    /// Unmount the filesystem at the given path.
    ///
    /// Returns `true` if a mount was removed, `false` if nothing was mounted
    /// there or the table is frozen.
    pub async fn unmount(&self, path: impl AsRef<Path>) -> bool {
        if self.is_frozen() {
            tracing::warn!("unmount rejected: mount table is frozen");
            return false;
        }
        let path = Self::normalize_mount_path(path.as_ref().to_path_buf());
        let mut mounts = self.mounts.write().await;
        mounts.remove(&path).is_some()
    }

    /// Whether the mount backing `path` is writable (longest-prefix match).
    /// Returns `false` when no mount matches — you can't write where nothing
    /// is mounted. Used to decide whether a path is eligible for CRDT-backed
    /// editing (writable) or should pass straight through (read-only/OS).
    pub async fn is_writable(&self, path: &Path) -> bool {
        match self.find_mount(path).await {
            Ok((fs, _)) => !fs.read_only(),
            Err(_) => false,
        }
    }

    /// List all current mounts.
    pub async fn list_mounts(&self) -> Vec<MountInfo> {
        let mounts = self.mounts.read().await;
        mounts
            .iter()
            .map(|(path, fs)| MountInfo {
                path: path.clone(),
                read_only: fs.read_only(),
            })
            .collect()
    }

    /// Normalize a mount path: ensure it starts with `/` and has no trailing slash.
    fn normalize_mount_path(path: PathBuf) -> PathBuf {
        let s = path.to_string_lossy();
        let s = s.trim_end_matches('/');
        if s.is_empty() {
            PathBuf::from("/")
        } else if !s.starts_with('/') {
            PathBuf::from(format!("/{}", s))
        } else {
            PathBuf::from(s)
        }
    }

    // ========================================================================
    // Listing-generation stamps (stage-1 groundwork, docs/scenes/vfs.md)
    // ========================================================================

    /// The directory that owns `path`'s listing — `path`'s parent, normalized,
    /// falling back to `/` for a path with no parent component (shouldn't
    /// happen for well-formed absolute VFS paths, but a mutation at the
    /// namespace root is still "owned by root" rather than a panic).
    fn parent_dir(path: &Path) -> PathBuf {
        let normalized = Self::normalize_mount_path(path.to_path_buf());
        match normalized.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("/"),
        }
    }

    /// Bump the listing-generation of the directory at `dir_path` — called
    /// after a structural mutation succeeds (create/mkdir/unlink/rmdir/
    /// rename/symlink/link). `DashMap::entry` is a per-shard lock, so this
    /// never contends with the `mounts` RwLock.
    fn bump_generation(&self, dir_path: &Path) {
        let key = Self::normalize_mount_path(dir_path.to_path_buf());
        self.generations
            .entry(key)
            .and_modify(|g| *g += 1)
            .or_insert(BASELINE_GENERATION + 1);
    }

    /// The current listing-generation of `dir_path`, or
    /// [`BASELINE_GENERATION`] if it has never been observed to mutate since
    /// boot.
    /// Public per the same reasoning as [`Self::global_activity`] /
    /// [`Self::activity_snapshot`] below: the activity digest bridge
    /// (`kaijutsu-server::rpc::subscribe_vfs_activity`) stamps each wire
    /// activity entry with the directory's current listing-generation for
    /// stale-listing detection on the client, and that's a one-off lookup,
    /// not a full snapshot walk.
    pub fn generation_of(&self, dir_path: &Path) -> u64 {
        let key = Self::normalize_mount_path(dir_path.to_path_buf());
        self.generations
            .get(&key)
            .map(|g| *g)
            .unwrap_or(BASELINE_GENERATION)
    }

    // ========================================================================
    // Activity counters (Lane K, FSN slice-1 digest groundwork,
    // docs/scenes/vfs.md)
    // ========================================================================

    /// Bump the activity total of the directory at `dir_path` by one, and
    /// the global epoch alongside it — called after a backend op succeeds,
    /// same chokepoint discipline as [`Self::bump_generation`]. Unlike
    /// generations there is no "baseline" offset: a directory's activity
    /// starts implicitly at 0 (absent from the map) and every bump is +1,
    /// so the raw total IS the lifetime event count, which is exactly what
    /// makes it safe to ship as an absolute value over the wire (a client
    /// that missed N ticks still lands on the correct total on tick N+1).
    ///
    /// ORDERING INVARIANT (Release here, Acquire in
    /// [`Self::global_activity`] — same pairing precedent as the `frozen`
    /// flag above): the map write is sequenced before the epoch bump, and a
    /// reader that observes epoch N must also see every `activity`-map
    /// write sequenced before the Release that produced N. With Relaxed on
    /// both sides, a digest tick could observe the new epoch WITHOUT the
    /// map entry behind it — the digest would ship missing that entry, its
    /// commit would advance `last_global` past the bump, and the entry
    /// would strand until some future bump anywhere reopened the epoch
    /// check (the same failure shape as the truncated-commit bug in
    /// `activity.rs`, but sourced from the memory model). Masked on x86's
    /// strong ordering; live on ARM.
    fn bump_activity(&self, dir_path: &Path) {
        let key = Self::normalize_mount_path(dir_path.to_path_buf());
        self.activity.entry(key).and_modify(|a| *a += 1).or_insert(1);
        self.activity_epoch.fetch_add(1, Ordering::Release);
    }

    /// The global activity epoch — the running total of every bump across
    /// every directory since kernel boot. A digest tick that finds this
    /// unchanged from the subscriber's cursor can short-circuit without
    /// touching the per-directory map at all (see `activity.rs`).
    ///
    /// Acquire, paired with [`Self::bump_activity`]'s Release: observing
    /// epoch N here guarantees visibility of every `activity`-map write
    /// sequenced before the bump that produced N. No deterministic unit
    /// test can pin a memory-model race down — this comment pair IS the
    /// guard.
    pub fn global_activity(&self) -> u64 {
        self.activity_epoch.load(Ordering::Acquire)
    }

    /// Snapshot of per-directory activity totals, optionally filtered to
    /// directories at-or-under `prefix` (normalized the same way as every
    /// other path in this module), sorted total-descending — "what's hot,
    /// most-active first" for `kj vfs activity`. `None` returns every
    /// directory that has ever been bumped since boot.
    pub fn activity_snapshot(&self, prefix: Option<&Path>) -> Vec<(PathBuf, u64)> {
        let prefix = prefix.map(|p| Self::normalize_mount_path(p.to_path_buf()));
        let mut entries: Vec<(PathBuf, u64)> = self
            .activity
            .iter()
            .filter(|entry| match &prefix {
                None => true,
                Some(prefix) if prefix.as_os_str() == "/" => true,
                Some(prefix) => entry.key() == prefix || entry.key().starts_with(prefix),
            })
            .map(|entry| (entry.key().clone(), *entry.value()))
            .collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        entries
    }

    // ========================================================================
    // Snapshot walk (stage-0 plumbing, docs/scenes/vfs.md)
    // ========================================================================

    /// If `vfs_dir` is backed by real files (`real_path` resolves) and has a
    /// `.gitignore`, build a matcher from its content — read through the VFS
    /// (`self.read_all`, never `std::fs` directly) so virtual/CRDT backends
    /// are structurally exempt and nested mounts are respected. `None` on
    /// any miss: no real backing, no file, or unreadable. A malformed
    /// pattern line is skipped by the builder itself rather than failing the
    /// whole snapshot over one typo'd `.gitignore`.
    async fn build_ignore_level(&self, vfs_dir: &Path) -> Option<Arc<Gitignore>> {
        if self.real_path(vfs_dir).await.ok().flatten().is_none() {
            return None;
        }
        let gi_path = vfs_dir.join(".gitignore");
        let content = self.read_all(&gi_path).await.ok()?;
        let text = String::from_utf8_lossy(&content);
        let mut builder = GitignoreBuilder::new(vfs_dir);
        for line in text.lines() {
            let _ = builder.add_line(None, line);
        }
        builder.build().ok().map(Arc::new)
    }

    /// Fold the ancestor `.gitignore` stack for one classification query.
    /// Closest (deepest) directory wins outright on a definitive match —
    /// this is an approximation of git's exact cross-file precedence (a
    /// negation in a shallower file cannot override an ignore decided by a
    /// deeper one), documented as the known gap in [`Self::snapshot`].
    fn ignore_stack_matches(levels: &[Arc<Gitignore>], path: &Path, is_dir: bool) -> bool {
        for gi in levels.iter().rev() {
            match gi.matched(path, is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => continue,
            }
        }
        false
    }

    /// Recursive walk body for [`Self::snapshot`]. Returns a boxed future
    /// (manual recursion — `async fn` cannot call itself) so `snapshot`
    /// itself stays a plain `async fn`.
    ///
    /// `budget` counts remaining nodes this call is allowed to ADD beyond
    /// itself (the caller has already reserved one unit for this node);
    /// `any_truncated` is a global flag set the first time any node in the
    /// tree gets cut. `ignore_levels` is the ancestor `.gitignore` stack
    /// inherited from the parent, used to classify THIS node; if this node
    /// is a directory we descend into, its own `.gitignore` (if any) is
    /// appended for its children.
    fn snapshot_node<'a>(
        &'a self,
        vfs_path: PathBuf,
        name: String,
        depth_remaining: u32,
        budget: &'a AtomicU32,
        any_truncated: &'a AtomicBool,
        ignore_levels: Vec<Arc<Gitignore>>,
    ) -> Pin<Box<dyn std::future::Future<Output = VfsResult<SnapshotNode>> + Send + 'a>> {
        Box::pin(async move {
            let attr = self.getattr(&vfs_path).await?;
            let is_dir = attr.kind.is_dir();
            let ignored = Self::ignore_stack_matches(&ignore_levels, &vfs_path, is_dir);
            let generation = if is_dir {
                self.generation_of(&vfs_path)
            } else {
                0
            };
            let mtime_secs = attr
                .mtime
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let mut node = SnapshotNode {
                name,
                kind: attr.kind,
                size: attr.size,
                mtime_secs,
                child_count: 0,
                ignored,
                generation,
                children: Vec::new(),
                truncated_here: false,
                denied: false,
            };

            // Symlinks are never expanded, even when they target a
            // directory: avoids cycles entirely and matches the "ghost
            // column tethered by a light thread" reading (docs/scenes/vfs.md)
            // — a symlink is a leaf in this walk.
            if !is_dir {
                return Ok(node);
            }

            // A backend that refuses ambient sweeps by design (crawl-opacity,
            // `docs/slash-r.md` — `/r` client shares: every readdir there is a
            // network round trip to somebody's laptop) stops the walk here,
            // same shape as a permission refusal below: the node is visible,
            // its children are not enumerated, and the walk continues past it
            // rather than failing outright. Reuses `denied` rather than a new
            // wire field — both mean "you may not see inside," and adding a
            // distinct capnp-visible bit for FSN to render differently is
            // follow-up work, not this slice's scope.
            if let Some((_, fs)) = self.owner_of(&vfs_path).await
                && fs.opaque_to_sweeps()
            {
                node.denied = true;
                return Ok(node);
            }

            // A refused listing marks the node and stops descending, rather
            // than failing the whole walk: host /etc alone carries a dozen
            // root-only directories, and an unreadable directory is real
            // information the world renders as a seam (docs/scenes/vfs.md
            // "truth seams"). Every OTHER error still fails the call — only
            // "you may not look" is a fact about the tree; an I/O fault is a
            // fault.
            let mut entries = match self.readdir(&vfs_path).await {
                Ok(entries) => entries,
                Err(e) if e.is_permission_denied() => {
                    node.denied = true;
                    return Ok(node);
                }
                Err(e) => return Err(e),
            };
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            node.child_count = entries.len() as u32;

            if depth_remaining == 0 {
                if !entries.is_empty() {
                    node.truncated_here = true;
                    any_truncated.store(true, Ordering::Relaxed);
                }
                return Ok(node);
            }

            let mut child_ignore_levels = ignore_levels;
            if let Some(level) = self.build_ignore_level(&vfs_path).await {
                child_ignore_levels.push(level);
            }

            for entry in entries {
                let reserved = budget
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| b.checked_sub(1))
                    .is_ok();
                if !reserved {
                    node.truncated_here = true;
                    any_truncated.store(true, Ordering::Relaxed);
                    break;
                }
                let child_path = vfs_path.join(&entry.name);
                let child = match self
                    .snapshot_node(
                        child_path.clone(),
                        entry.name.clone(),
                        depth_remaining - 1,
                        budget,
                        any_truncated,
                        child_ignore_levels.clone(),
                    )
                    .await
                {
                    Ok(child) => child,
                    // A child whose own `getattr` is refused (its readdir
                    // refusal is already handled inside the recursion) still
                    // gets a seat as a denied stub — same truth-seam
                    // reasoning as the readdir arm above; `readdir` gave us
                    // its name and kind, which is all a seam needs.
                    Err(e) if e.is_permission_denied() => SnapshotNode {
                        name: entry.name,
                        kind: entry.kind,
                        size: 0,
                        mtime_secs: 0,
                        child_count: 0,
                        ignored: Self::ignore_stack_matches(
                            &child_ignore_levels,
                            &child_path,
                            entry.kind.is_dir(),
                        ),
                        generation: 0,
                        children: Vec::new(),
                        truncated_here: false,
                        denied: true,
                    },
                    // A child that VANISHED between its parent's readdir and
                    // its own getattr is skipped, not an error: live
                    // pseudo-filesystems (/proc) churn mid-walk — an exiting
                    // PID took the whole root snapshot down before this arm
                    // (live-caught 2026-07-12). The tree changing under the
                    // walk is normal (docs/scenes/vfs.md claim 4); the
                    // budget unit it reserved is simply spent. Dangling
                    // symlinks do NOT hit this arm — getattr is lstat-like
                    // and answers for the link itself.
                    Err(e) if e.is_not_found() => continue,
                    Err(e) => return Err(e),
                };
                node.children.push(child);
            }

            Ok(node)
        })
    }

    /// Recursive snapshot listing with generation stamps — the FSN world's
    /// stage-0/1 kernel plumbing (`docs/scenes/vfs.md` "Kernel plumbing:
    /// enumeration + fsnotify"). Walks VFS-mediated (`readdir`/`getattr`),
    /// never the host filesystem directly, so nested mounts and virtual
    /// backends compose correctly.
    ///
    /// **Caps**: `depth` and `max_entries` are both caller-supplied but
    /// server-clamped — [`SNAPSHOT_MAX_DEPTH`] / [`SNAPSHOT_MAX_ENTRIES`] —
    /// so a request for the whole host with no cap can never happen.
    /// `max_entries` is also floored at 1: the root node always ships even
    /// if the caller asks for 0.
    ///
    /// **Error policy**: a backend error mid-walk fails the whole call
    /// (`Err`), rather than returning a partial tree that looks complete.
    /// `truncated_here` exists precisely so an INTENTIONAL cut is visible;
    /// silently returning a half-tree on an I/O error would look identical
    /// to a deliberate depth/cap cut, which is exactly the ambiguity this
    /// avoids. The ONE carve-out is permission denial
    /// ([`VfsError::is_permission_denied`]): "you may not look" is a fact
    /// about the tree, not a fault — the refused node is included with
    /// `denied: true` and the walk continues past it (live-caught 2026-07-12:
    /// host `/etc`'s root-only directories made the whole tree
    /// un-snapshottable under the pure fail-whole-call policy). The walk
    /// ROOT itself: a refused *listing* still returns (a denied root node —
    /// the caller learns the target exists and is refused); refused
    /// *attributes* error, since there is nothing at all to say about the
    /// caller's own named target.
    ///
    /// **Generation policy** (stage-1 groundwork): directory generations
    /// track STRUCTURE (name-set) changes only —
    /// create/mkdir/unlink/rmdir/rename (both parents)/symlink/link bump the
    /// parent's generation; write/truncate/setattr do NOT. Content/activity
    /// is a separate counter (`activity`/`activity_epoch`, Lane K, FSN
    /// slice-1) that ALL of the above bump, plus write/truncate/setattr,
    /// which bump activity alone — see `activity.rs` for the digest stream
    /// built on top of it. A directory never observed to mutate since kernel
    /// boot reports
    /// [`BASELINE_GENERATION`]. Files always report generation 0.
    /// In-memory only — a kernel restart resets every counter, which is fine
    /// (`docs/scenes/vfs.md`: "a kernel restart invalidating all generations
    /// is fine and self-corrects via the full-sweep-on-reconnect rule").
    /// KNOWN GAP: generation bumps only observe VFS-mediated mutations — an
    /// external process writing directly into a LocalBackend-backed host
    /// path (e.g. `cargo build` populating `target/`) is invisible to the
    /// counter until inotify lands (stage 2); tracked in `docs/issues.md`.
    ///
    /// **`ignored`** (gitignore classification — metadata, never a filter,
    /// see `docs/scenes/vfs.md`) is real for LocalBackend-backed subtrees:
    /// `.gitignore` files are read through the VFS as the walk descends and
    /// folded closest-directory-wins (see [`Self::ignore_stack_matches`] for
    /// the precision gap vs. git's exact semantics). Only `.gitignore` files
    /// at-or-below the snapshot root are considered — an ancestor
    /// `.gitignore` above the root path is not consulted. Virtual/CRDT
    /// backends always report `ignored=false`. Both gaps are tracked in
    /// `docs/issues.md`.
    pub async fn snapshot(
        &self,
        path: &Path,
        depth: u32,
        max_entries: u32,
    ) -> VfsResult<SnapshotResult> {
        let depth = depth.min(SNAPSHOT_MAX_DEPTH);
        let cap = max_entries.clamp(1, SNAPSHOT_MAX_ENTRIES);
        let budget = AtomicU32::new(cap - 1);
        let any_truncated = AtomicBool::new(false);
        let normalized = Self::normalize_mount_path(path.to_path_buf());
        let name = normalized
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_string());

        let root = self
            .snapshot_node(normalized, name, depth, &budget, &any_truncated, Vec::new())
            .await?;
        let generation = root.generation;
        Ok(SnapshotResult {
            root,
            generation,
            truncated: any_truncated.load(Ordering::Relaxed),
        })
    }

    /// The mount point owning `path` (longest-prefix match) paired with its
    /// backend — the "what owns this path?" question. `None` when nothing is
    /// mounted over `path`. Used by the editor resolver to decide config-owned
    /// (bind straight to the CRDT block) vs. an ordinary file, and to recover the
    /// config mount root without a hardcoded prefix — see
    /// [`VfsOps::owns_config_docs`].
    pub async fn owner_of(&self, path: &Path) -> Option<(PathBuf, Arc<dyn VfsOps>)> {
        let path_str = path.to_string_lossy();
        let normalized = if path_str.starts_with('/') {
            path.to_path_buf()
        } else {
            PathBuf::from(format!("/{}", path_str))
        };

        let mounts = self.mounts.read().await;

        // Find longest matching mount point
        let mut best_match: Option<(&PathBuf, &Arc<dyn VfsOps>)> = None;

        for (mount_path, fs) in mounts.iter() {
            let mount_str = mount_path.to_string_lossy();

            // Check if the path starts with this mount point
            let is_match = if mount_str == "/" {
                true // Root matches everything
            } else {
                let normalized_str = normalized.to_string_lossy();
                normalized_str == mount_str.as_ref()
                    || normalized_str.starts_with(&format!("{}/", mount_str))
            };

            if is_match {
                // Keep the longest match
                if best_match.is_none()
                    || mount_path.as_os_str().len()
                        > best_match.expect("checked is_none").0.as_os_str().len()
                {
                    best_match = Some((mount_path, fs));
                }
            }
        }

        best_match.map(|(mount_path, fs)| (mount_path.clone(), Arc::clone(fs)))
    }

    /// Sync virtual→real path resolution for the subprocess seam: kaish's
    /// `KernelBackend::resolve_real_path` is a sync trait method, so it can't
    /// ride the async [`Self::real_path`]. Longest-prefix owner (same rule as
    /// [`Self::owner_of`]) → the owner's structural [`VfsOps::real_root`] +
    /// the relative remainder. `None` for virtual mounts (CRDT/memory) and
    /// unmounted paths — the caller treats that as "no host cwd, skip external
    /// exec". Purely structural: no existence check, no symlink resolution
    /// (the spawned child's own syscalls resolve those).
    ///
    /// Uses `try_read`: the table is frozen right after startup mounts, so a
    /// write-held lock is effectively impossible — but if it ever happens we
    /// warn loudly rather than silently degrade, because the visible symptom
    /// ("command not found" for a real binary) points nowhere near here.
    pub fn resolve_real_path_sync(&self, path: &Path) -> Option<PathBuf> {
        let path_str = path.to_string_lossy();
        let normalized = if path_str.starts_with('/') {
            path_str.into_owned()
        } else {
            format!("/{}", path_str)
        };

        let mounts = match self.mounts.try_read() {
            Ok(guard) => guard,
            Err(_) => {
                tracing::warn!(
                    path = %normalized,
                    "resolve_real_path_sync: mount table write-locked; \
                     external command resolution will fail this call"
                );
                return None;
            }
        };

        // Longest matching mount point — same matching rule as `owner_of`.
        let mut best: Option<(&PathBuf, &Arc<dyn VfsOps>)> = None;
        for (mount_path, fs) in mounts.iter() {
            let mount_str = mount_path.to_string_lossy();
            let is_match = if mount_str == "/" {
                true
            } else {
                normalized == mount_str.as_ref()
                    || normalized.starts_with(&format!("{}/", mount_str))
            };
            if is_match
                && best.is_none_or(|(b, _)| mount_path.as_os_str().len() > b.as_os_str().len())
            {
                best = Some((mount_path, fs));
            }
        }
        let (mount_path, fs) = best?;
        let root = fs.real_root()?;

        let mount_str = mount_path.to_string_lossy();
        let relative = if mount_str == "/" {
            normalized.trim_start_matches('/')
        } else {
            normalized
                .strip_prefix(mount_str.as_ref())
                .unwrap_or("")
                .trim_start_matches('/')
        };
        if relative.is_empty() {
            Some(root)
        } else {
            Some(root.join(relative))
        }
    }

    /// Find the mount point for a given path.
    ///
    /// Returns the mount and the path relative to that mount.
    async fn find_mount(&self, path: &Path) -> VfsResult<(Arc<dyn VfsOps>, PathBuf)> {
        let (mount_path, fs) = self
            .owner_of(path)
            .await
            .ok_or_else(|| VfsError::no_mount_point(path.display().to_string()))?;

        // Calculate the path relative to the matched mount.
        let path_str = path.to_string_lossy();
        let normalized = if path_str.starts_with('/') {
            path.to_string_lossy().into_owned()
        } else {
            format!("/{}", path_str)
        };
        let mount_str = mount_path.to_string_lossy();
        let relative = if mount_str == "/" {
            normalized.trim_start_matches('/').to_string()
        } else {
            normalized
                .strip_prefix(mount_str.as_ref())
                .unwrap_or("")
                .trim_start_matches('/')
                .to_string()
        };

        Ok((fs, PathBuf::from(relative)))
    }

    /// List the root directory, synthesizing entries from mount points.
    async fn list_root(&self) -> VfsResult<Vec<DirEntry>> {
        let mounts = self.mounts.read().await;
        let mut entries = Vec::new();
        let mut seen_names = std::collections::HashSet::new();

        for mount_path in mounts.keys() {
            let mount_str = mount_path.to_string_lossy();
            if mount_str == "/" {
                // Root mount: list its contents directly
                if let Some(fs) = mounts.get(mount_path)
                    && let Ok(root_entries) = fs.readdir(Path::new("")).await
                {
                    for entry in root_entries {
                        if seen_names.insert(entry.name.clone()) {
                            entries.push(entry);
                        }
                    }
                }
            } else {
                // Non-root mount: extract first path component
                let first_component = mount_str
                    .trim_start_matches('/')
                    .split('/')
                    .next()
                    .unwrap_or("");

                if !first_component.is_empty() && seen_names.insert(first_component.to_string()) {
                    entries.push(DirEntry {
                        name: first_component.to_string(),
                        kind: FileType::Directory,
                    });
                }
            }
        }

        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// The synthetic children an **intermediate mount directory** owes its
    /// existence to: the next path component of every mount point strictly
    /// under `prefix` (mounts `/v/cas` + `/v/docs` give `/v` the children
    /// `cas` and `docs`). Empty when `prefix` is not an ancestor of any
    /// mount. Such directories exist only in the mount table — the backend
    /// that happens to own the prefix (e.g. a host-`/` root mount under
    /// `/v`) may know nothing about them, so `getattr`/`readdir` must answer
    /// from here first (live-caught 2026-07-12: the snapshot walker ENOENTed
    /// on `/v` because host `/v` doesn't exist).
    async fn mount_children(&self, prefix: &Path) -> Vec<DirEntry> {
        let mounts = self.mounts.read().await;
        let mut names = std::collections::BTreeSet::new();
        for mount_path in mounts.keys() {
            if mount_path == prefix {
                continue;
            }
            if let Ok(rest) = mount_path.strip_prefix(prefix)
                && let Some(first) = rest.components().next()
            {
                names.insert(first.as_os_str().to_string_lossy().into_owned());
            }
        }
        names
            .into_iter()
            .map(|name| DirEntry { name, kind: FileType::Directory })
            .collect()
    }
}

#[async_trait]
impl VfsOps for MountTable {
    async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
        // Special case: root always exists
        let path_str = path.to_string_lossy();
        if path_str.is_empty() || path_str == "/" {
            return Ok(FileAttr::directory(0o755));
        }

        // Check if path is a mount point itself
        let normalized = Self::normalize_mount_path(path.to_path_buf());
        {
            let mounts = self.mounts.read().await;
            if mounts.contains_key(&normalized) {
                return Ok(FileAttr::directory(0o755));
            }
        }
        // An intermediate mount directory (an ancestor of some mount point,
        // e.g. `/v` under mounts `/v/cas`…) exists only in the mount table —
        // the backend owning the prefix may ENOENT it. See [`Self::mount_children`].
        if !self.mount_children(&normalized).await.is_empty() {
            return Ok(FileAttr::directory(0o755));
        }

        let (fs, relative) = self.find_mount(path).await?;
        fs.getattr(&relative).await
    }

    async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
        // Special case: listing root might need to show mount points
        let path_str = path.to_string_lossy();
        if path_str.is_empty() || path_str == "/" {
            return self.list_root().await;
        }

        // Merge the backend's own listing with any synthetic mount children
        // (see [`Self::mount_children`]) — a directory can be both real on
        // its backend AND the parent of deeper mounts. Backend misses are
        // tolerated only when synthetic children exist AND the miss is
        // NotFound (the intermediate dir has no real backing); any other
        // backend error still surfaces.
        let normalized = Self::normalize_mount_path(path.to_path_buf());
        let synthetic = self.mount_children(&normalized).await;
        let backend_entries = match self.find_mount(path).await {
            Ok((fs, relative)) => match fs.readdir(&relative).await {
                Ok(entries) => entries,
                Err(VfsError::NotFound(_)) if !synthetic.is_empty() => Vec::new(),
                Err(e)
                    if !synthetic.is_empty()
                        && matches!(&e, VfsError::Io(io) if io.kind() == std::io::ErrorKind::NotFound) =>
                {
                    Vec::new()
                }
                Err(e) => return Err(e),
            },
            Err(_) if !synthetic.is_empty() => Vec::new(),
            Err(e) => return Err(e),
        };
        if synthetic.is_empty() {
            return Ok(backend_entries);
        }
        let mut seen: std::collections::HashSet<String> =
            backend_entries.iter().map(|e| e.name.clone()).collect();
        let mut entries = backend_entries;
        for entry in synthetic {
            if seen.insert(entry.name.clone()) {
                entries.push(entry);
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        let (fs, relative) = self.find_mount(path).await?;
        fs.read(&relative, offset, size).await
    }

    /// Delegate `read_all` to the owning backend rather than using the trait
    /// default (getattr + read). The default sizes from `getattr`, which is
    /// lstat-like for a symlink and reports the *link path* length — that would
    /// truncate a followed target (e.g. a short link path masking a long rc
    /// script). A backend that follows symlinks (`ConfigCrdtFs`) sizes from the
    /// resolved target in its own `read_all`, so the read must reach it.
    async fn read_all(&self, path: &Path) -> VfsResult<Vec<u8>> {
        let (fs, relative) = self.find_mount(path).await?;
        fs.read_all(&relative).await
    }

    /// Delegate `open_read_stream` to the owning backend, same rationale as
    /// `read_all` above: the trait's DEFAULT implementation just loops
    /// `read`, which is correct only for backends where `read` is cheap and
    /// stateless. A backend that overrides `open_read_stream` to hold one
    /// handle open across the whole transfer (a future wire-backed `ShareFs`,
    /// `docs/slash-r.md` slice 0) must have that override actually reached —
    /// using the trait default here instead would silently reopen/close a
    /// remote handle per chunk.
    ///
    /// Built with `async_stream::stream!` rather than a hand-rolled
    /// `Stream` impl: the owning backend's `Arc<dyn VfsOps>` and the
    /// translated relative path are both local to one async generator body
    /// (the macro's `async move` block), so the inner `BoxStream` — which
    /// borrows them — never has to outlive its owner in a separate struct.
    /// That's the same soundness the compiler already grants ordinary
    /// `async`/`.await` locals; no unsafe self-referencing required.
    fn open_read_stream<'a>(
        &'a self,
        path: &'a Path,
    ) -> futures::stream::BoxStream<'a, VfsResult<bytes::Bytes>> {
        use futures::StreamExt;
        Box::pin(async_stream::stream! {
            let (fs, relative) = match self.find_mount(path).await {
                Ok(v) => v,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };
            let mut inner = fs.open_read_stream(&relative);
            while let Some(item) = inner.next().await {
                yield item;
            }
        })
    }

    async fn readlink(&self, path: &Path) -> VfsResult<PathBuf> {
        let (fs, relative) = self.find_mount(path).await?;
        fs.readlink(&relative).await
    }

    // Deliberately does NOT bump listing-generation: see the note above
    // `truncate`/`setattr` below — writing bytes into an existing file
    // changes content/activity, not the parent directory's name set. It DOES
    // bump activity (Lane K, FSN slice-1) — the digest stream now exists
    // (`activity.rs`), and this is exactly the heat it reports.
    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32> {
        let (fs, relative) = self.find_mount(path).await?;
        let n = fs.write(&relative, offset, data).await?;
        self.bump_activity(&Self::parent_dir(path));
        Ok(n)
    }

    async fn create(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        let (fs, relative) = self.find_mount(path).await?;
        let attr = fs.create(&relative, mode).await?;
        self.bump_generation(&Self::parent_dir(path));
        self.bump_activity(&Self::parent_dir(path));
        Ok(attr)
    }

    async fn mkdir(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        let (fs, relative) = self.find_mount(path).await?;
        let attr = fs.mkdir(&relative, mode).await?;
        self.bump_generation(&Self::parent_dir(path));
        self.bump_activity(&Self::parent_dir(path));
        Ok(attr)
    }

    async fn unlink(&self, path: &Path) -> VfsResult<()> {
        let (fs, relative) = self.find_mount(path).await?;
        fs.unlink(&relative).await?;
        self.bump_generation(&Self::parent_dir(path));
        self.bump_activity(&Self::parent_dir(path));
        Ok(())
    }

    async fn rmdir(&self, path: &Path) -> VfsResult<()> {
        let (fs, relative) = self.find_mount(path).await?;
        fs.rmdir(&relative).await?;
        self.bump_generation(&Self::parent_dir(path));
        self.bump_activity(&Self::parent_dir(path));
        Ok(())
    }

    async fn rename(&self, from: &Path, to: &Path) -> VfsResult<()> {
        // Both paths must be in the same mount
        let (from_fs, from_relative) = self.find_mount(from).await?;
        let (to_fs, to_relative) = self.find_mount(to).await?;

        // Check if they're the same mount (by Arc pointer)
        if !Arc::ptr_eq(&from_fs, &to_fs) {
            return Err(VfsError::CrossDeviceLink);
        }

        from_fs.rename(&from_relative, &to_relative).await?;
        // A rename changes the name-set of BOTH parents (the source loses a
        // name, the destination gains one), even when they're the same
        // directory — bumping twice there is harmlessly redundant, still
        // monotonic. Same reasoning extends to activity: both parents felt
        // heat, even if it's the same directory bumped twice.
        self.bump_generation(&Self::parent_dir(from));
        self.bump_generation(&Self::parent_dir(to));
        self.bump_activity(&Self::parent_dir(from));
        self.bump_activity(&Self::parent_dir(to));
        Ok(())
    }

    // truncate/setattr deliberately do NOT bump generation: listing-generation
    // tracks STRUCTURE (the directory's name set), not content/activity. They
    // DO bump activity — content mutation (truncate) and metadata mutation
    // (setattr, e.g. chmod) both count as heat by decision: an `ls -la`-visible
    // change to a file is exactly the kind of "something happened here" signal
    // the digest stream exists to surface, even though it leaves the parent's
    // name-set untouched.
    async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()> {
        let (fs, relative) = self.find_mount(path).await?;
        fs.truncate(&relative, size).await?;
        self.bump_activity(&Self::parent_dir(path));
        Ok(())
    }

    async fn setattr(&self, path: &Path, attr: SetAttr) -> VfsResult<FileAttr> {
        let (fs, relative) = self.find_mount(path).await?;
        let attr = fs.setattr(&relative, attr).await?;
        self.bump_activity(&Self::parent_dir(path));
        Ok(attr)
    }

    async fn symlink(&self, path: &Path, target: &Path) -> VfsResult<FileAttr> {
        let (fs, relative) = self.find_mount(path).await?;
        let attr = fs.symlink(&relative, target).await?;
        self.bump_generation(&Self::parent_dir(path));
        self.bump_activity(&Self::parent_dir(path));
        Ok(attr)
    }

    async fn link(&self, oldpath: &Path, newpath: &Path) -> VfsResult<FileAttr> {
        // Both paths must be in the same mount
        let (old_fs, old_relative) = self.find_mount(oldpath).await?;
        let (new_fs, new_relative) = self.find_mount(newpath).await?;

        if !Arc::ptr_eq(&old_fs, &new_fs) {
            return Err(VfsError::CrossDeviceLink);
        }

        let attr = old_fs.link(&old_relative, &new_relative).await?;
        // Only newpath's parent gains a name; oldpath's parent's listing is
        // unchanged (a hard link doesn't remove the original name). Same
        // reasoning for activity: the heat lands where the new name appeared.
        self.bump_generation(&Self::parent_dir(newpath));
        self.bump_activity(&Self::parent_dir(newpath));
        Ok(attr)
    }

    fn read_only(&self) -> bool {
        // Mount table itself isn't read-only; individual mounts might be
        false
    }

    async fn statfs(&self) -> VfsResult<StatFs> {
        // Return stats from root mount if available
        let mounts = self.mounts.read().await;
        if let Some(root_fs) = mounts.get(&PathBuf::from("/")) {
            return root_fs.statfs().await;
        }
        Ok(StatFs::default())
    }

    async fn real_path(&self, path: &Path) -> VfsResult<Option<PathBuf>> {
        let (fs, relative) = self.find_mount(path).await?;
        fs.real_path(&relative).await
    }
}

/// A `MemoryBackend` that opts out of ambient sweeps — the crawl-opacity test
/// double standing in for `ShareFs` (`docs/slash-r.md`) without pulling the
/// share machinery into this module's tests.
#[cfg(test)]
struct OpaqueBackend(crate::vfs::backends::MemoryBackend);

#[cfg(test)]
#[async_trait]
impl VfsOps for OpaqueBackend {
    async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
        self.0.getattr(path).await
    }
    async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
        self.0.readdir(path).await
    }
    async fn read(&self, path: &Path, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        self.0.read(path, offset, size).await
    }
    async fn readlink(&self, path: &Path) -> VfsResult<PathBuf> {
        self.0.readlink(path).await
    }
    async fn write(&self, path: &Path, offset: u64, data: &[u8]) -> VfsResult<u32> {
        self.0.write(path, offset, data).await
    }
    async fn create(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        self.0.create(path, mode).await
    }
    async fn mkdir(&self, path: &Path, mode: u32) -> VfsResult<FileAttr> {
        self.0.mkdir(path, mode).await
    }
    async fn unlink(&self, path: &Path) -> VfsResult<()> {
        self.0.unlink(path).await
    }
    async fn rmdir(&self, path: &Path) -> VfsResult<()> {
        self.0.rmdir(path).await
    }
    async fn rename(&self, from: &Path, to: &Path) -> VfsResult<()> {
        self.0.rename(from, to).await
    }
    async fn truncate(&self, path: &Path, size: u64) -> VfsResult<()> {
        self.0.truncate(path, size).await
    }
    async fn setattr(&self, path: &Path, attr: SetAttr) -> VfsResult<FileAttr> {
        self.0.setattr(path, attr).await
    }
    async fn symlink(&self, path: &Path, target: &Path) -> VfsResult<FileAttr> {
        self.0.symlink(path, target).await
    }
    async fn link(&self, oldpath: &Path, newpath: &Path) -> VfsResult<FileAttr> {
        self.0.link(oldpath, newpath).await
    }
    fn read_only(&self) -> bool {
        self.0.read_only()
    }
    async fn statfs(&self) -> VfsResult<StatFs> {
        self.0.statfs().await
    }
    async fn real_path(&self, path: &Path) -> VfsResult<Option<PathBuf>> {
        self.0.real_path(path).await
    }
    fn opaque_to_sweeps(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::backends::MemoryBackend;

    #[tokio::test]
    async fn snapshot_does_not_descend_into_an_opaque_mount() {
        let table = MountTable::new();
        table.mount("/", MemoryBackend::new()).await;
        let opaque = OpaqueBackend(MemoryBackend::new());
        opaque.mkdir(Path::new("laptop-a"), 0o755).await.unwrap();
        opaque
            .write_all(Path::new("laptop-a/secret.txt"), b"never crawled")
            .await
            .unwrap();
        table.mount("/r", opaque).await;

        let result = table.snapshot(Path::new("/"), 8, 100).await.unwrap();
        let r_node = result
            .root
            .children
            .iter()
            .find(|c| c.name == "r")
            .expect("/r appears in the snapshot");
        assert!(r_node.denied, "an opaque mount's node is marked as not-walked");
        assert!(
            r_node.children.is_empty(),
            "an opaque mount's children must never be enumerated by the sweep"
        );
    }

    #[tokio::test]
    async fn test_basic_mount() {
        let table = MountTable::new();
        let scratch = MemoryBackend::new();
        scratch.create(Path::new("test.txt"), 0o644).await.unwrap();
        scratch
            .write(Path::new("test.txt"), 0, b"hello")
            .await
            .unwrap();

        table.mount("/scratch", scratch).await;

        let data = table
            .read(Path::new("/scratch/test.txt"), 0, 100)
            .await
            .unwrap();
        assert_eq!(data, b"hello");
    }

    #[tokio::test]
    async fn test_multiple_mounts() {
        let table = MountTable::new();

        let scratch = MemoryBackend::new();
        scratch.create(Path::new("a.txt"), 0o644).await.unwrap();
        scratch
            .write(Path::new("a.txt"), 0, b"scratch")
            .await
            .unwrap();
        table.mount("/scratch", scratch).await;

        let data = MemoryBackend::new();
        data.create(Path::new("b.txt"), 0o644).await.unwrap();
        data.write(Path::new("b.txt"), 0, b"data").await.unwrap();
        table.mount("/data", data).await;

        assert_eq!(
            table
                .read(Path::new("/scratch/a.txt"), 0, 100)
                .await
                .unwrap(),
            b"scratch"
        );
        assert_eq!(
            table.read(Path::new("/data/b.txt"), 0, 100).await.unwrap(),
            b"data"
        );
    }

    #[tokio::test]
    async fn test_nested_mount() {
        let table = MountTable::new();

        let outer = MemoryBackend::new();
        outer.create(Path::new("outer.txt"), 0o644).await.unwrap();
        outer
            .write(Path::new("outer.txt"), 0, b"outer")
            .await
            .unwrap();
        table.mount("/mnt", outer).await;

        let inner = MemoryBackend::new();
        inner.create(Path::new("inner.txt"), 0o644).await.unwrap();
        inner
            .write(Path::new("inner.txt"), 0, b"inner")
            .await
            .unwrap();
        table.mount("/mnt/project", inner).await;

        // /mnt/outer.txt should come from outer mount
        assert_eq!(
            table
                .read(Path::new("/mnt/outer.txt"), 0, 100)
                .await
                .unwrap(),
            b"outer"
        );

        // /mnt/project/inner.txt should come from inner mount
        assert_eq!(
            table
                .read(Path::new("/mnt/project/inner.txt"), 0, 100)
                .await
                .unwrap(),
            b"inner"
        );
    }

    #[tokio::test]
    async fn test_list_root() {
        let table = MountTable::new();
        table.mount("/scratch", MemoryBackend::new()).await;
        table.mount("/mnt/a", MemoryBackend::new()).await;
        table.mount("/mnt/b", MemoryBackend::new()).await;

        let entries = table.readdir(Path::new("/")).await.unwrap();
        let names: Vec<_> = entries.iter().map(|e| &e.name).collect();

        assert!(names.contains(&&"scratch".to_string()));
        assert!(names.contains(&&"mnt".to_string()));
    }

    #[tokio::test]
    async fn test_unmount() {
        let table = MountTable::new();

        let fs = MemoryBackend::new();
        fs.create(Path::new("test.txt"), 0o644).await.unwrap();
        fs.write(Path::new("test.txt"), 0, b"data").await.unwrap();
        table.mount("/scratch", fs).await;

        assert!(
            table
                .read(Path::new("/scratch/test.txt"), 0, 100)
                .await
                .is_ok()
        );

        table.unmount("/scratch").await;

        assert!(
            table
                .read(Path::new("/scratch/test.txt"), 0, 100)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_list_mounts() {
        let table = MountTable::new();
        table.mount("/scratch", MemoryBackend::new()).await;
        table.mount("/data", MemoryBackend::new()).await;

        let mounts = table.list_mounts().await;
        assert_eq!(mounts.len(), 2);

        let paths: Vec<_> = mounts.iter().map(|m| &m.path).collect();
        assert!(paths.contains(&&PathBuf::from("/scratch")));
        assert!(paths.contains(&&PathBuf::from("/data")));
    }

    #[tokio::test]
    async fn test_no_mount_error() {
        let table = MountTable::new();
        let result = table.read(Path::new("/nothing/here.txt"), 0, 100).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_root_mount() {
        let table = MountTable::new();

        let root = MemoryBackend::new();
        root.create(Path::new("at-root.txt"), 0o644).await.unwrap();
        root.write(Path::new("at-root.txt"), 0, b"root file")
            .await
            .unwrap();
        table.mount("/", root).await;

        let data = table.read(Path::new("/at-root.txt"), 0, 100).await.unwrap();
        assert_eq!(data, b"root file");
    }

    #[tokio::test]
    async fn test_write_through_table() {
        let table = MountTable::new();
        table.mount("/scratch", MemoryBackend::new()).await;

        table
            .create(Path::new("/scratch/new.txt"), 0o644)
            .await
            .unwrap();
        table
            .write(Path::new("/scratch/new.txt"), 0, b"created")
            .await
            .unwrap();

        let data = table
            .read(Path::new("/scratch/new.txt"), 0, 100)
            .await
            .unwrap();
        assert_eq!(data, b"created");
    }

    #[tokio::test]
    async fn test_stat_mount_point() {
        let table = MountTable::new();
        table.mount("/scratch", MemoryBackend::new()).await;

        let attr = table.getattr(Path::new("/scratch")).await.unwrap();
        assert!(attr.is_dir());
    }

    #[tokio::test]
    async fn test_stat_root() {
        let table = MountTable::new();
        let attr = table.getattr(Path::new("/")).await.unwrap();
        assert!(attr.is_dir());
    }

    #[tokio::test]
    async fn test_cross_mount_rename_fails() {
        let table = MountTable::new();
        table.mount("/a", MemoryBackend::new()).await;
        table.mount("/b", MemoryBackend::new()).await;

        table.create(Path::new("/a/file.txt"), 0o644).await.unwrap();

        let result = table
            .rename(Path::new("/a/file.txt"), Path::new("/b/file.txt"))
            .await;
        assert!(matches!(result, Err(VfsError::CrossDeviceLink)));
    }

    #[tokio::test]
    async fn test_real_path_memory_returns_none() {
        let table = MountTable::new();
        table.mount("/scratch", MemoryBackend::new()).await;
        table
            .create(Path::new("/scratch/test.txt"), 0o644)
            .await
            .unwrap();

        let real = table
            .real_path(Path::new("/scratch/test.txt"))
            .await
            .unwrap();
        assert!(real.is_none());
    }

    #[tokio::test]
    async fn test_real_path_local_returns_path() {
        use crate::vfs::backends::LocalBackend;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello").unwrap();

        let table = MountTable::new();
        table
            .mount("/mnt/project", LocalBackend::new(dir.path()))
            .await;

        let real = table
            .real_path(Path::new("/mnt/project/test.txt"))
            .await
            .unwrap();
        assert!(real.is_some());
        let real = real.unwrap();
        assert!(real.is_absolute());
        assert!(real.ends_with("test.txt"));
    }

    /// The subprocess seam: sync resolution maps Local-backed mounts to real
    /// host paths (longest-prefix wins), and virtual mounts yield `None` so
    /// external exec is skipped for CRDT/memory cwds.
    #[tokio::test]
    async fn resolve_real_path_sync_maps_local_and_skips_virtual() {
        use crate::vfs::backends::LocalBackend;

        let outer = tempfile::tempdir().unwrap();
        let inner = tempfile::tempdir().unwrap();

        let table = MountTable::new();
        table.mount("/", LocalBackend::read_only(outer.path())).await;
        table
            .mount("/mnt/project", LocalBackend::new(inner.path()))
            .await;
        table.mount("/scratch", MemoryBackend::new()).await;

        // Longest prefix: /mnt/project/* maps into the inner root…
        let real = table
            .resolve_real_path_sync(Path::new("/mnt/project/src/lib.rs"))
            .expect("local mount resolves");
        assert_eq!(real, inner.path().canonicalize().unwrap().join("src/lib.rs"));
        // …the mount point itself maps to the root exactly…
        assert_eq!(
            table.resolve_real_path_sync(Path::new("/mnt/project")),
            Some(inner.path().canonicalize().unwrap()),
        );
        // …everything else falls to "/" (relative remainder preserved)…
        assert_eq!(
            table.resolve_real_path_sync(Path::new("/home/user")),
            Some(outer.path().canonicalize().unwrap().join("home/user")),
        );
        // …and a virtual mount has no real side.
        assert_eq!(table.resolve_real_path_sync(Path::new("/scratch/x")), None);
    }

    #[tokio::test]
    async fn test_is_writable_honors_read_only_and_longest_prefix() {
        use crate::vfs::backends::LocalBackend;

        let dir = tempfile::tempdir().unwrap();
        let table = MountTable::new();
        // Read-only root, writable subtree (longest-prefix wins) — mirrors the
        // server's `/` (read_only) + `~/src` (read-write) layout.
        table.mount("/", LocalBackend::read_only(dir.path())).await;
        table.mount("/sub", MemoryBackend::new()).await;

        assert!(!table.is_writable(Path::new("/etc/hostname")).await);
        assert!(!table.is_writable(Path::new("/anything")).await);
        assert!(table.is_writable(Path::new("/sub/file.rs")).await);
        // No matching mount at all → not writable.
        let empty = MountTable::new();
        assert!(!empty.is_writable(Path::new("/nope")).await);
    }

    #[tokio::test]
    async fn test_freeze_blocks_mount() {
        let table = MountTable::new();
        assert!(table.mount("/scratch", MemoryBackend::new()).await);

        table.freeze();
        assert!(table.is_frozen());

        // mount after freeze returns false
        assert!(!table.mount("/other", MemoryBackend::new()).await);

        // original mount still works
        let mounts = table.list_mounts().await;
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].path, PathBuf::from("/scratch"));
    }

    #[tokio::test]
    async fn test_freeze_blocks_unmount() {
        let table = MountTable::new();
        table.mount("/scratch", MemoryBackend::new()).await;
        table.freeze();

        // unmount after freeze returns false
        assert!(!table.unmount("/scratch").await);

        // mount is still there
        let mounts = table.list_mounts().await;
        assert_eq!(mounts.len(), 1);
    }

    #[tokio::test]
    async fn test_freeze_does_not_block_reads_writes() {
        let table = MountTable::new();
        table.mount("/scratch", MemoryBackend::new()).await;
        table
            .create(Path::new("/scratch/test.txt"), 0o644)
            .await
            .unwrap();
        table
            .write(Path::new("/scratch/test.txt"), 0, b"hello")
            .await
            .unwrap();

        table.freeze();

        // reads still work
        let data = table
            .read(Path::new("/scratch/test.txt"), 0, 100)
            .await
            .unwrap();
        assert_eq!(data, b"hello");

        // writes still work (freeze only affects mount/unmount, not data ops)
        table
            .write(Path::new("/scratch/test.txt"), 0, b"updated")
            .await
            .unwrap();
        let data = table
            .read(Path::new("/scratch/test.txt"), 0, 100)
            .await
            .unwrap();
        assert_eq!(data, b"updated");
    }

    // ========================================================================
    // Snapshot walk (stage-0 plumbing)
    // ========================================================================

    /// Build `/scratch` with a small tree:
    /// ```text
    /// /scratch
    ///   a.txt
    ///   sub/
    ///     b.txt
    ///     c.txt
    /// ```
    async fn setup_snapshot_tree() -> MountTable {
        let table = MountTable::new();
        let scratch = MemoryBackend::new();
        scratch.create(Path::new("a.txt"), 0o644).await.unwrap();
        scratch.mkdir(Path::new("sub"), 0o755).await.unwrap();
        scratch.create(Path::new("sub/b.txt"), 0o644).await.unwrap();
        scratch.create(Path::new("sub/c.txt"), 0o644).await.unwrap();
        table.mount("/scratch", scratch).await;
        table
    }

    #[tokio::test]
    async fn snapshot_depth_zero_returns_just_root_and_flags_truncation() {
        let table = setup_snapshot_tree().await;
        let result = table
            .snapshot(Path::new("/scratch"), 0, SNAPSHOT_MAX_ENTRIES)
            .await
            .unwrap();

        assert_eq!(result.root.name, "scratch");
        assert!(result.root.kind.is_dir());
        // Real count is still reported even though nothing was walked.
        assert_eq!(result.root.child_count, 2);
        assert!(result.root.children.is_empty());
        assert!(result.root.truncated_here, "depth cut must flag truncated_here");
        assert!(result.truncated, "outer `truncated` must roll up the cut");
    }

    #[tokio::test]
    async fn snapshot_walks_full_tree_within_depth_and_cap() {
        let table = setup_snapshot_tree().await;
        let result = table
            .snapshot(Path::new("/scratch"), 5, SNAPSHOT_MAX_ENTRIES)
            .await
            .unwrap();

        assert!(!result.truncated);
        assert!(!result.root.truncated_here);
        assert_eq!(result.root.children.len(), 2);
        let names: Vec<_> = result.root.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "sub"]); // sorted

        let sub = result.root.children.iter().find(|c| c.name == "sub").unwrap();
        assert!(sub.kind.is_dir());
        assert_eq!(sub.child_count, 2);
        assert!(!sub.truncated_here);
        assert_eq!(sub.children.len(), 2);
        let sub_names: Vec<_> = sub.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(sub_names, vec!["b.txt", "c.txt"]);
    }

    #[tokio::test]
    async fn snapshot_entry_cap_truncates_and_reports_real_child_count() {
        let table = setup_snapshot_tree().await;
        // Budget: root (1) + a.txt (1) = 2 total; "sub" and its children never
        // get visited, so the cut is flagged on the root, not deeper.
        let result = table.snapshot(Path::new("/scratch"), 5, 2).await.unwrap();

        assert!(result.truncated);
        assert!(result.root.truncated_here);
        // Real count unaffected by the cut.
        assert_eq!(result.root.child_count, 2);
        assert_eq!(result.root.children.len(), 1);
        assert_eq!(result.root.children[0].name, "a.txt");
    }

    #[tokio::test]
    async fn snapshot_symlink_is_a_leaf_even_targeting_a_directory() {
        let table = setup_snapshot_tree().await;
        table
            .symlink(Path::new("/scratch/link"), Path::new("sub"))
            .await
            .unwrap();

        let result = table.snapshot(Path::new("/scratch"), 5, SNAPSHOT_MAX_ENTRIES).await.unwrap();
        let link = result
            .root
            .children
            .iter()
            .find(|c| c.name == "link")
            .expect("symlink node present");
        assert!(link.kind.is_symlink());
        // Never expanded, regardless of depth/cap — avoids cycles.
        assert!(link.children.is_empty());
        assert_eq!(link.child_count, 0);
        assert!(!link.truncated_here);
    }

    #[tokio::test]
    async fn snapshot_on_missing_path_errors_the_whole_call() {
        let table = MountTable::new();
        table.mount("/scratch", MemoryBackend::new()).await;
        let result = table.snapshot(Path::new("/scratch/nope"), 3, 100).await;
        assert!(result.is_err(), "a backend error mid/at-walk must fail the call, not return a partial tree");
    }

    #[tokio::test]
    async fn snapshot_max_entries_zero_still_ships_the_root() {
        let table = setup_snapshot_tree().await;
        let result = table.snapshot(Path::new("/scratch"), 5, 0).await.unwrap();
        // Floored at 1: the root always ships.
        assert_eq!(result.root.name, "scratch");
        assert!(result.root.truncated_here);
    }

    // ========================================================================
    // Listing-generation stamps
    // ========================================================================

    #[tokio::test]
    async fn directory_never_mutated_reports_baseline_generation() {
        let table = MountTable::new();
        let scratch = MemoryBackend::new();
        // Pre-existing, built directly on the backend (bypasses the table —
        // like a mount seeded before it's wired in) so nothing inside "sub"
        // has ever gone through the table's generation chokepoint.
        scratch.mkdir(Path::new("sub"), 0o755).await.unwrap();
        table.mount("/scratch", scratch).await;

        // Bump root's generation through the table without ever touching
        // inside "sub".
        table.create(Path::new("/scratch/a.txt"), 0o644).await.unwrap();

        let result = table
            .snapshot(Path::new("/scratch"), 5, SNAPSHOT_MAX_ENTRIES)
            .await
            .unwrap();
        assert!(
            result.root.generation > BASELINE_GENERATION,
            "root's generation should have bumped via the table-mediated create"
        );
        let sub = result.root.children.iter().find(|c| c.name == "sub").unwrap();
        assert_eq!(
            sub.generation, BASELINE_GENERATION,
            "sub was never mutated through the table, so it stays baseline"
        );
    }

    #[tokio::test]
    async fn mkdir_and_create_bump_parent_generation() {
        let table = MountTable::new();
        table.mount("/scratch", MemoryBackend::new()).await;
        let before = table.snapshot(Path::new("/scratch"), 0, 10).await.unwrap();
        assert_eq!(before.root.generation, BASELINE_GENERATION);

        table.create(Path::new("/scratch/a.txt"), 0o644).await.unwrap();
        let after_create = table.snapshot(Path::new("/scratch"), 0, 10).await.unwrap();
        assert!(after_create.root.generation > before.root.generation);

        table.mkdir(Path::new("/scratch/sub"), 0o755).await.unwrap();
        let after_mkdir = table.snapshot(Path::new("/scratch"), 0, 10).await.unwrap();
        assert!(after_mkdir.root.generation > after_create.root.generation);
    }

    #[tokio::test]
    async fn unlink_and_rmdir_bump_parent_generation() {
        let table = setup_snapshot_tree().await;
        let before = table.snapshot(Path::new("/scratch"), 0, 10).await.unwrap();

        table.unlink(Path::new("/scratch/a.txt")).await.unwrap();
        let after_unlink = table.snapshot(Path::new("/scratch"), 0, 10).await.unwrap();
        assert!(after_unlink.root.generation > before.root.generation);

        table.unlink(Path::new("/scratch/sub/b.txt")).await.unwrap();
        table.unlink(Path::new("/scratch/sub/c.txt")).await.unwrap();
        let before_rmdir = table.snapshot(Path::new("/scratch/sub"), 0, 10).await.unwrap();
        table.rmdir(Path::new("/scratch/sub")).await.unwrap();
        let after_rmdir = table.snapshot(Path::new("/scratch"), 0, 10).await.unwrap();
        assert!(after_rmdir.root.generation > after_unlink.root.generation);
        // Sanity: rmdir's target itself isn't queried post-removal — its own
        // generation is moot once the name is gone (only the parent matters).
        let _ = before_rmdir;
    }

    #[tokio::test]
    async fn rename_bumps_both_parent_generations() {
        let table = MountTable::new();
        table.mount("/a", MemoryBackend::new()).await;
        table.mount("/b", MemoryBackend::new()).await;
        table.create(Path::new("/a/file.txt"), 0o644).await.unwrap();

        let a_before = table.snapshot(Path::new("/a"), 0, 10).await.unwrap();
        let b_before = table.snapshot(Path::new("/b"), 0, 10).await.unwrap();

        table
            .rename(Path::new("/a/file.txt"), Path::new("/a/renamed.txt"))
            .await
            .unwrap();
        let a_after = table.snapshot(Path::new("/a"), 0, 10).await.unwrap();
        assert!(a_after.root.generation > a_before.root.generation);

        // Cross-mount rename isn't supported (CrossDeviceLink), so exercise
        // the "both parents" claim within one mount via nested dirs instead.
        table.mkdir(Path::new("/b/src"), 0o755).await.unwrap();
        table.mkdir(Path::new("/b/dst"), 0o755).await.unwrap();
        table.create(Path::new("/b/src/f.txt"), 0o644).await.unwrap();
        let src_before = table.snapshot(Path::new("/b/src"), 0, 10).await.unwrap();
        let dst_before = table.snapshot(Path::new("/b/dst"), 0, 10).await.unwrap();
        table
            .rename(Path::new("/b/src/f.txt"), Path::new("/b/dst/f.txt"))
            .await
            .unwrap();
        let src_after = table.snapshot(Path::new("/b/src"), 0, 10).await.unwrap();
        let dst_after = table.snapshot(Path::new("/b/dst"), 0, 10).await.unwrap();
        assert!(src_after.root.generation > src_before.root.generation);
        assert!(dst_after.root.generation > dst_before.root.generation);
        let _ = b_before;
    }

    #[tokio::test]
    async fn write_truncate_setattr_do_not_bump_generation() {
        let table = setup_snapshot_tree().await;
        let before = table.snapshot(Path::new("/scratch"), 0, 10).await.unwrap();

        table.write(Path::new("/scratch/a.txt"), 0, b"hi").await.unwrap();
        table.truncate(Path::new("/scratch/a.txt"), 1).await.unwrap();
        table
            .setattr(Path::new("/scratch/a.txt"), SetAttr::new().with_perm(0o600))
            .await
            .unwrap();

        let after = table.snapshot(Path::new("/scratch"), 0, 10).await.unwrap();
        assert_eq!(
            before.root.generation, after.root.generation,
            "content/activity ops must not bump listing-generation (structure only)"
        );
    }

    #[tokio::test]
    async fn files_always_report_generation_zero() {
        let table = setup_snapshot_tree().await;
        let result = table.snapshot(Path::new("/scratch"), 5, SNAPSHOT_MAX_ENTRIES).await.unwrap();
        let file = result.root.children.iter().find(|c| c.name == "a.txt").unwrap();
        assert_eq!(file.generation, 0);
    }

    // ========================================================================
    // Activity counters (Lane K, stage-1 digest groundwork, docs/scenes/vfs.md)
    // ========================================================================

    /// Helper: current activity total recorded for `dir`, or 0 if never bumped.
    fn activity_of(table: &MountTable, dir: &Path) -> u64 {
        table
            .activity_snapshot(Some(dir))
            .into_iter()
            .find(|(p, _)| p == dir)
            .map(|(_, total)| total)
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn write_truncate_setattr_bump_activity_not_generation() {
        let table = setup_snapshot_tree().await;
        let scratch = Path::new("/scratch");
        let gen_before = table.snapshot(scratch, 0, 10).await.unwrap().root.generation;
        let epoch_before = table.global_activity();
        let act_before = activity_of(&table, scratch);

        table.write(Path::new("/scratch/a.txt"), 0, b"hi").await.unwrap();
        table.truncate(Path::new("/scratch/a.txt"), 1).await.unwrap();
        table
            .setattr(Path::new("/scratch/a.txt"), SetAttr::new().with_perm(0o600))
            .await
            .unwrap();

        let gen_after = table.snapshot(scratch, 0, 10).await.unwrap().root.generation;
        assert_eq!(
            gen_before, gen_after,
            "content ops must not bump listing-generation (structure only)"
        );

        let epoch_after = table.global_activity();
        assert_eq!(
            epoch_after - epoch_before,
            3,
            "write+truncate+setattr = 3 activity bumps on the global epoch"
        );
        let act_after = activity_of(&table, scratch);
        assert_eq!(
            act_after - act_before,
            3,
            "all three content ops land on the file's parent directory"
        );
    }

    #[tokio::test]
    async fn create_mkdir_unlink_rmdir_symlink_bump_both_counters() {
        let table = MountTable::new();
        table.mount("/scratch", MemoryBackend::new()).await;
        let scratch = Path::new("/scratch");

        macro_rules! assert_both_bumped {
            ($before_gen:expr, $before_act:expr) => {{
                let after_gen = table.snapshot(scratch, 0, 10).await.unwrap().root.generation;
                let after_act = activity_of(&table, scratch);
                assert!(after_gen > $before_gen, "generation must bump");
                assert!(after_act > $before_act, "activity must bump");
                (after_gen, after_act)
            }};
        }

        let gen0 = table.snapshot(scratch, 0, 10).await.unwrap().root.generation;
        let act0 = activity_of(&table, scratch);

        table.create(Path::new("/scratch/a.txt"), 0o644).await.unwrap();
        let (gen1, act1) = assert_both_bumped!(gen0, act0);

        table.mkdir(Path::new("/scratch/sub"), 0o755).await.unwrap();
        let (gen2, act2) = assert_both_bumped!(gen1, act1);

        table.symlink(Path::new("/scratch/link"), Path::new("a.txt")).await.unwrap();
        let (gen3, act3) = assert_both_bumped!(gen2, act2);

        table.unlink(Path::new("/scratch/a.txt")).await.unwrap();
        let (gen4, act4) = assert_both_bumped!(gen3, act3);

        table.unlink(Path::new("/scratch/link")).await.unwrap();
        let (gen5, act5) = assert_both_bumped!(gen4, act4);

        table.rmdir(Path::new("/scratch/sub")).await.unwrap();
        let _ = assert_both_bumped!(gen5, act5);
    }

    #[tokio::test]
    async fn link_bumps_only_newpath_parent_activity_and_generation() {
        // MemoryBackend doesn't support hard links; LocalBackend does (real
        // `fs::hard_link` on the underlying tempdir).
        use crate::vfs::backends::LocalBackend;
        let dir = tempfile::tempdir().unwrap();
        let table = MountTable::new();
        table.mount("/scratch", LocalBackend::new(dir.path())).await;
        table.mkdir(Path::new("/scratch/src"), 0o755).await.unwrap();
        table.mkdir(Path::new("/scratch/dst"), 0o755).await.unwrap();
        table.create(Path::new("/scratch/src/file.txt"), 0o644).await.unwrap();

        let src = Path::new("/scratch/src");
        let dst = Path::new("/scratch/dst");
        let src_gen_before = table.snapshot(src, 0, 10).await.unwrap().root.generation;
        let src_act_before = activity_of(&table, src);
        let dst_gen_before = table.snapshot(dst, 0, 10).await.unwrap().root.generation;
        let dst_act_before = activity_of(&table, dst);

        table
            .link(Path::new("/scratch/src/file.txt"), Path::new("/scratch/dst/hardlink"))
            .await
            .unwrap();

        let src_gen_after = table.snapshot(src, 0, 10).await.unwrap().root.generation;
        let src_act_after = activity_of(&table, src);
        assert_eq!(
            src_gen_before, src_gen_after,
            "oldpath's parent's name-set is unchanged by a hard link"
        );
        assert_eq!(
            src_act_before, src_act_after,
            "oldpath's parent gets no activity credit for a link"
        );

        let dst_gen_after = table.snapshot(dst, 0, 10).await.unwrap().root.generation;
        let dst_act_after = activity_of(&table, dst);
        assert!(dst_gen_after > dst_gen_before, "newpath's parent gains a name");
        assert!(dst_act_after > dst_act_before, "newpath's parent is where the heat lands");
    }

    #[tokio::test]
    async fn rename_bumps_activity_of_both_parents() {
        let table = MountTable::new();
        table.mount("/scratch", MemoryBackend::new()).await;
        table.mkdir(Path::new("/scratch/src"), 0o755).await.unwrap();
        table.mkdir(Path::new("/scratch/dst"), 0o755).await.unwrap();
        table.create(Path::new("/scratch/src/f.txt"), 0o644).await.unwrap();

        let src = Path::new("/scratch/src");
        let dst = Path::new("/scratch/dst");
        let src_act_before = activity_of(&table, src);
        let dst_act_before = activity_of(&table, dst);

        table
            .rename(Path::new("/scratch/src/f.txt"), Path::new("/scratch/dst/f.txt"))
            .await
            .unwrap();

        assert!(activity_of(&table, src) > src_act_before, "source parent loses a name = heat");
        assert!(activity_of(&table, dst) > dst_act_before, "dest parent gains a name = heat");
    }

    #[tokio::test]
    async fn reads_never_bump_activity() {
        let table = setup_snapshot_tree().await;
        let epoch_before = table.global_activity();

        let _ = table.read(Path::new("/scratch/a.txt"), 0, 100).await.unwrap();
        let _ = table.read_all(Path::new("/scratch/a.txt")).await.unwrap();
        let _ = table.readdir(Path::new("/scratch")).await.unwrap();
        let _ = table.getattr(Path::new("/scratch/a.txt")).await.unwrap();

        assert_eq!(
            table.global_activity(),
            epoch_before,
            "reads must never bump the activity epoch — heat is change, not access"
        );
    }

    #[tokio::test]
    async fn failed_op_bumps_no_activity() {
        let table = MountTable::new();
        let epoch_before = table.global_activity();

        let result = table.write(Path::new("/nothing/here.txt"), 0, b"x").await;
        assert!(result.is_err());

        assert_eq!(
            table.global_activity(),
            epoch_before,
            "a failed backend op must not register as activity"
        );
    }

    // ========================================================================
    // gitignore classification (`ignored`)
    // ========================================================================

    #[tokio::test]
    async fn gitignore_classifies_matching_entries_under_a_local_backed_subtree() {
        use crate::vfs::backends::LocalBackend;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(dir.path().join("keep.txt"), "keep").unwrap();
        std::fs::write(dir.path().join("drop.log"), "drop").unwrap();

        let table = MountTable::new();
        table.mount("/mnt/project", LocalBackend::new(dir.path())).await;

        let result = table
            .snapshot(Path::new("/mnt/project"), 3, SNAPSHOT_MAX_ENTRIES)
            .await
            .unwrap();

        let keep = result.root.children.iter().find(|c| c.name == "keep.txt").unwrap();
        let drop = result.root.children.iter().find(|c| c.name == "drop.log").unwrap();
        assert!(!keep.ignored, "keep.txt must not be classified as ignored");
        assert!(drop.ignored, "drop.log must be classified as ignored");
        // Metadata, never a filter: the ignored entry is still present with
        // full attributes, not skipped.
        assert!(!drop.name.is_empty());
    }

    #[tokio::test]
    async fn virtual_backends_never_report_ignored() {
        // No gitignore semantics apply outside real-file-backed subtrees —
        // even a file literally named like a common ignore pattern stays
        // `ignored: false` on a MemoryBackend.
        let table = MountTable::new();
        let scratch = MemoryBackend::new();
        scratch.create(Path::new("target"), 0o644).await.unwrap();
        table.mount("/scratch", scratch).await;

        let result = table.snapshot(Path::new("/scratch"), 3, SNAPSHOT_MAX_ENTRIES).await.unwrap();
        let target = result.root.children.iter().find(|c| c.name == "target").unwrap();
        assert!(!target.ignored);
    }

    // ── snapshot: permission denial is a seam, not a failure ──
    // (live-caught 2026-07-12: host /etc's root-only directories made the
    // whole tree un-snapshottable under the pure fail-whole-call policy)

    /// A backend that refuses inspection. `MountTable::getattr` answers for a
    /// mount POINT itself without consulting the backend, so the refusals
    /// live one level in: `deny_readdir: true` refuses the root listing
    /// (exercising the walker's readdir-denied arm); `deny_readdir: false`
    /// lists one child ("secret") whose own `getattr` then refuses
    /// (exercising the denied-stub arm in the parent loop).
    struct DenyBackend {
        deny_readdir: bool,
    }

    #[async_trait]
    impl VfsOps for DenyBackend {
        async fn getattr(&self, path: &Path) -> VfsResult<FileAttr> {
            if path.as_os_str().is_empty() || path == Path::new("/") {
                return Ok(FileAttr::directory(0o700));
            }
            Err(VfsError::permission_denied(path.display().to_string()))
        }
        async fn readdir(&self, path: &Path) -> VfsResult<Vec<DirEntry>> {
            if self.deny_readdir {
                return Err(VfsError::permission_denied(path.display().to_string()));
            }
            Ok(vec![DirEntry { name: "secret".into(), kind: FileType::Directory }])
        }
        async fn read(&self, path: &Path, _offset: u64, _size: u32) -> VfsResult<Vec<u8>> {
            Err(VfsError::permission_denied(path.display().to_string()))
        }
        async fn readlink(&self, path: &Path) -> VfsResult<PathBuf> {
            Err(VfsError::permission_denied(path.display().to_string()))
        }
        async fn write(&self, path: &Path, _offset: u64, _data: &[u8]) -> VfsResult<u32> {
            Err(VfsError::permission_denied(path.display().to_string()))
        }
        async fn create(&self, path: &Path, _mode: u32) -> VfsResult<FileAttr> {
            Err(VfsError::permission_denied(path.display().to_string()))
        }
        async fn mkdir(&self, path: &Path, _mode: u32) -> VfsResult<FileAttr> {
            Err(VfsError::permission_denied(path.display().to_string()))
        }
        async fn unlink(&self, path: &Path) -> VfsResult<()> {
            Err(VfsError::permission_denied(path.display().to_string()))
        }
        async fn rmdir(&self, path: &Path) -> VfsResult<()> {
            Err(VfsError::permission_denied(path.display().to_string()))
        }
        async fn rename(&self, from: &Path, _to: &Path) -> VfsResult<()> {
            Err(VfsError::permission_denied(from.display().to_string()))
        }
        async fn truncate(&self, path: &Path, _size: u64) -> VfsResult<()> {
            Err(VfsError::permission_denied(path.display().to_string()))
        }
        async fn setattr(&self, path: &Path, _attr: SetAttr) -> VfsResult<FileAttr> {
            Err(VfsError::permission_denied(path.display().to_string()))
        }
        async fn symlink(&self, path: &Path, _target: &Path) -> VfsResult<FileAttr> {
            Err(VfsError::permission_denied(path.display().to_string()))
        }
        async fn link(&self, oldpath: &Path, _newpath: &Path) -> VfsResult<FileAttr> {
            Err(VfsError::permission_denied(oldpath.display().to_string()))
        }
        fn read_only(&self) -> bool {
            true
        }
        async fn statfs(&self) -> VfsResult<StatFs> {
            Ok(StatFs::default())
        }
        async fn real_path(&self, _path: &Path) -> VfsResult<Option<PathBuf>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn snapshot_denied_readdir_becomes_a_seam_not_an_error() {
        let table = setup_snapshot_tree().await;
        // The underlay dir must exist for the parent's readdir to list it;
        // the deny mount then shadows it (longest-prefix routing).
        table.mkdir(Path::new("/scratch/locked"), 0o700).await.unwrap();
        table.mount("/scratch/locked", DenyBackend { deny_readdir: true }).await;

        let result =
            table.snapshot(Path::new("/scratch"), 5, SNAPSHOT_MAX_ENTRIES).await.unwrap();

        let locked = result.root.children.iter().find(|c| c.name == "locked").unwrap();
        assert!(locked.denied, "refused listing must mark the node denied");
        assert!(locked.children.is_empty());
        assert_eq!(locked.child_count, 0, "a denied listing's child count is unknowable");
        assert!(!locked.truncated_here, "denial is not the walker's own cut");
        assert!(!result.truncated, "denial must not roll up as truncation");

        // The refusal is contained: siblings still walk fully.
        let sub = result.root.children.iter().find(|c| c.name == "sub").unwrap();
        assert!(!sub.denied);
        assert_eq!(sub.children.len(), 2, "denied sibling must not stop sub's own walk");
    }

    #[tokio::test]
    async fn snapshot_denied_child_getattr_becomes_a_denied_stub() {
        let table = setup_snapshot_tree().await;
        table.mkdir(Path::new("/scratch/vault"), 0o700).await.unwrap();
        table.mount("/scratch/vault", DenyBackend { deny_readdir: false }).await;

        let result =
            table.snapshot(Path::new("/scratch"), 5, SNAPSHOT_MAX_ENTRIES).await.unwrap();

        // vault itself lists fine (one child, "secret"); secret's own
        // getattr is refused, so it seats as a denied stub with the name and
        // kind its parent's readdir supplied.
        let vault = result.root.children.iter().find(|c| c.name == "vault").unwrap();
        assert!(!vault.denied);
        let secret = vault.children.iter().find(|c| c.name == "secret").unwrap();
        assert!(secret.denied, "refused attributes must stub the child as denied");
        assert!(secret.kind.is_dir(), "the stub keeps readdir's kind");
        assert!(secret.children.is_empty());
        assert_eq!(secret.generation, 0);
    }

    // ── intermediate mount directories (live-caught 2026-07-12: the
    // snapshot walker ENOENTed on /v — an ancestor of mounts that exists
    // only in the mount table, not on the backend owning the prefix) ──

    #[tokio::test]
    async fn intermediate_mount_dir_getattr_and_readdir_are_synthesized() {
        let table = MountTable::new();
        table.mount("/v/cas", MemoryBackend::new()).await;
        table.mount("/v/docs", MemoryBackend::new()).await;

        let attr = table.getattr(Path::new("/v")).await.unwrap();
        assert!(attr.kind.is_dir(), "/v exists as a synthetic directory");

        let entries = table.readdir(Path::new("/v")).await.unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["cas", "docs"], "next components of the mounts under /v");
    }

    #[tokio::test]
    async fn intermediate_mount_dir_merges_with_a_real_backing_dir() {
        // A directory can be real on its backend AND the parent of a deeper
        // mount — both listings merge, backend entries winning name ties.
        let table = setup_snapshot_tree().await;
        table.mount("/scratch/sub/extra", MemoryBackend::new()).await;

        let entries = table.readdir(Path::new("/scratch/sub")).await.unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["b.txt", "c.txt", "extra"], "real entries + the synthetic mount child");
    }

    #[tokio::test]
    async fn snapshot_walks_through_an_intermediate_mount_dir() {
        let table = setup_snapshot_tree().await;
        let cas = MemoryBackend::new();
        cas.create(Path::new("blob.bin"), 0o644).await.unwrap();
        table.mount("/v/cas", cas).await;

        let result = table.snapshot(Path::new("/"), 3, SNAPSHOT_MAX_ENTRIES).await.unwrap();
        let v = result.root.children.iter().find(|c| c.name == "v").unwrap();
        assert!(v.kind.is_dir());
        assert!(!v.denied);
        let cas = v.children.iter().find(|c| c.name == "cas").unwrap();
        assert_eq!(cas.children.len(), 1, "the walk reaches through /v into the mount");
        assert_eq!(cas.children[0].name, "blob.bin");
    }

    #[tokio::test]
    async fn snapshot_root_denial_semantics() {
        let table = setup_snapshot_tree().await;
        table.mkdir(Path::new("/scratch/locked"), 0o700).await.unwrap();
        table.mount("/scratch/locked", DenyBackend { deny_readdir: true }).await;

        // A root whose LISTING is refused still returns — as a denied node
        // (the caller learns the path exists and is refused, same seam
        // semantics as a mid-walk child).
        let result = table.snapshot(Path::new("/scratch/locked"), 3, 100).await.unwrap();
        assert!(result.root.denied);
        assert!(result.root.children.is_empty());

        // A root whose ATTRIBUTES are refused errors — there is nothing at
        // all to say about the caller's own named target. (Not reachable
        // through a mount point — MountTable::getattr answers for those
        // itself — so exercised via the deny backend's own interior.)
        let result = table.snapshot(Path::new("/scratch/locked/secret"), 3, 100).await;
        assert!(result.unwrap_err().is_permission_denied());
    }
}
