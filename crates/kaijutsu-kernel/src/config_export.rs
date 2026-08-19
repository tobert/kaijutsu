//! Lossless export/import of the kernel-owned config trees to/from a real
//! directory — the precondition for ever trusting the eventual git-worktree
//! flip (`docs/config-ownership.md`, "Lane B — the git-worktree seam,
//! shipped and deliberately unwired").
//!
//! [`ConfigDocFs`] mounts four roots — `/etc/rc`, `/etc/config`,
//! `/etc/client`, `/etc/midi` — all sharing one doc model
//! ([`crate::config_doc`]): a document is either a [`DocKind::File`] or a
//! [`DocKind::Symlink`], and directories are virtual, synthesized from
//! the set of document paths (see `runtime::config_doc_fs` module docs).
//! This module is purely additive: it does not touch mount wiring, delete any
//! document, or add a git dependency. It builds the export model
//! ([`ConfigTreeEntry`]) and proves — via the round-trip test below — that
//! walking the documents out to disk and back loses nothing.
//!
//! [`ConfigDocFs`]: crate::runtime::config_doc_fs::ConfigDocFs
//!
//! # What does NOT round-trip
//!
//! The document model is strictly narrower than a real filesystem. Anything
//! below is invisible to [`export_config_tree`] by construction — not a bug
//! in this module, but a gap this module makes visible for the eventual flip:
//!
//! - **File permissions / modes.** `ConfigDocFs::getattr` reports a fixed
//!   `0o644` for every file and `0o755` for every directory (see `getattr` in
//!   `runtime/config_doc_fs.rs`) — there is no per-document mode bit stored
//!   anywhere. A materialized tree cannot recover a mode no document ever held.
//! - **Empty directories.** Directories are virtual, synthesized from
//!   descendant document paths (`is_dir`/`readdir` in `config_doc_fs.rs`).
//!   A directory with zero documents under it does not exist as far as the
//!   kernel is concerned, so it materializes to nothing and a git worktree
//!   (which also cannot track an empty directory) loses no *additional*
//!   information here — but a real POSIX directory could have held one.
//! - **Duplicate-path collisions.** Impossible by construction: `ContextId`
//!   is a deterministic UUIDv5 of the canonical path
//!   ([`crate::config_doc::config_context_id`]), and `create_document_with_path` errors
//!   (`DocumentAlreadyExists`) rather than allow a second document at the
//!   same path. So there is no scenario where two documents claim one path —
//!   this module doesn't need to guard against it, but it's worth recording
//!   that the invariant is enforced elsewhere, not here.
//! - **"Registered but blockless" documents** (the halted-replay case named
//!   in `config_doc::first_block_id`'s doc comment) are content loss waiting
//!   to happen silently. `export_config_tree` refuses to proceed past one —
//!   see [`ConfigExportError::BlocklessDocument`].
//! - **`language`** on the `documents` row (used elsewhere for e.g.
//!   syntax-highlighting hints on `Code`/`Text` docs) is not carried — config
//!   docs never set it (`ConfigDocFs` always passes `None`), so there is
//!   nothing to lose today, but a future doc that does set it would silently
//!   drop that field through this export. Worth a note if `language` is ever
//!   pressed into service for config docs.
//!
//! # Enumeration coverage
//!
//! [`export_config_tree`] enumerates via
//! [`crate::block_store::BlockStore::documents_under_path`], which is a straight read of
//! the persisted `documents` table (`KernelDb::list_documents_under_path`,
//! `WHERE path LIKE '<root>/%' ORDER BY path`) — the same manifest
//! `ConfigDocFs::readdir` uses. It returns every row with a `path` under the
//! root; there is no separate "cache-shadow" document kind under these four
//! roots to miss — the deterministic `file_context_id` cache document
//! (`FileDocumentCache`) lives under a *different* id derivation and is only
//! ever consulted for non-config-owned paths (`owns_config_docs()` is `true`
//! for every `ConfigDocFs` mount, which short-circuits the file cache
//! entirely — see `editor.rs`). So there is nothing under `/etc/rc`,
//! `/etc/config`, `/etc/client`, or `/etc/midi` that this enumeration could
//! silently skip.

use std::path::{Path, PathBuf};

use kaijutsu_types::DocKind;
use kaijutsu_types::paths::{CLIENT_ROOT, CONFIG_ROOT, MIDI_ROOT, RC_ROOT};

use crate::block_store::{BlockStoreError, SharedBlockStore};
use crate::config_doc;
use crate::runtime::config_doc_fs::normalize_abs;

/// The four kernel-owned config mount roots, in the fixed order this module
/// always walks them. (Their lexical string order happens to match —
/// `client` < `config` < `midi` < `rc` — but ordering is enforced explicitly
/// by sorting output, not by relying on this array's order.)
const MOUNT_ROOTS: [&str; 4] = [RC_ROOT, CONFIG_ROOT, CLIENT_ROOT, MIDI_ROOT];

/// Directory name a mount root materializes under, inside the export
/// directory (`docs/config-ownership.md`, "Rulings (Amy, 2026-08-15)",
/// ruling 1: one worktree at `<data_dir>/config` holding `rc/`, `config/`,
/// `client/`, `midi/`). A `match` rather than a derived strip so an
/// unrecognized root fails loud at compile-adjacent time instead of silently
/// deriving some other subdir name.
fn root_subdir(root: &str) -> &'static str {
    match root {
        RC_ROOT => "rc",
        CONFIG_ROOT => "config",
        CLIENT_ROOT => "client",
        MIDI_ROOT => "midi",
        other => unreachable!(
            "config_export only knows the four config mount roots; got {other}"
        ),
    }
}

/// One config/rc document, captured exactly enough to reconstruct it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigTreeEntry {
    /// Which mount root this document lives under (one of `RC_ROOT`,
    /// `CONFIG_ROOT`, `CLIENT_ROOT`, `MIDI_ROOT`).
    pub root: &'static str,
    /// Path relative to `root`, no leading slash (e.g.
    /// `"coder/create/S00-stance.kai"`).
    pub rel_path: String,
    pub kind: ConfigTreeKind,
}

impl ConfigTreeEntry {
    /// The document's canonical path (`root/rel_path`) — the string
    /// `config_context_id` is keyed on.
    pub fn canonical_path(&self) -> String {
        format!("{}/{}", self.root, self.rel_path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigTreeKind {
    File { content: String },
    Symlink { target: String },
}

/// Errors from exporting, materializing, or importing a config tree.
#[derive(Debug, thiserror::Error)]
pub enum ConfigExportError {
    #[error("block store: {0}")]
    BlockStore(#[from] BlockStoreError),

    /// A document is registered (its `documents` row exists) but the
    /// document carries zero blocks — the halted-replay case
    /// `config_doc::first_block_id` names. This is data loss waiting to
    /// happen silently; the export refuses to proceed past it rather than
    /// emit a truncated tree.
    #[error(
        "config document at {root}/{rel_path} is registered but has no block \
         content (halted replay or corrupt seed) — refusing to export a \
         silently-truncated tree"
    )]
    BlocklessDocument { root: &'static str, rel_path: String },

    /// A `documents` row's path didn't actually fall under the root the SQL
    /// query filtered on. Should be unreachable (the query is `WHERE path
    /// LIKE '<root>/%'`) — kept as a loud error rather than a panic so a
    /// future storage bug surfaces as a diagnosable `Result`, not a crash
    /// with no context.
    #[error("document path {path:?} returned under root {root} but does not fall under it")]
    PathNotUnderRoot { root: &'static str, path: String },

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("non-UTF-8 path under {root}: {path:?}")]
    NonUtf8Path { root: &'static str, path: PathBuf },

    /// An imported symlink's target resolves outside its mount root. Refused
    /// loudly rather than materialized into a document (which would let it
    /// escape the mount's cross-mount permission gate — the same escape
    /// `ConfigDocFs::resolve_target` refuses at read time).
    #[error(
        "symlink {root}/{rel_path} -> {target:?} escapes its mount root \
         (resolves to {resolved})"
    )]
    SymlinkEscapesRoot {
        root: &'static str,
        rel_path: String,
        target: String,
        resolved: String,
    },
}

fn io_err(path: &Path, source: std::io::Error) -> ConfigExportError {
    ConfigExportError::Io { path: path.to_path_buf(), source }
}

/// Sort entries into the deterministic order every export/import must
/// produce: by root (fixed, matches [`MOUNT_ROOTS`]), then lexically by
/// `rel_path`. Never rely on `BlockId`/`BTreeMap`/DashMap iteration order —
/// this project's `BlockId` ordering is principal-major, not
/// position-correct (see `gotcha_blockid_vs_document_order`).
fn sort_entries(entries: &mut [ConfigTreeEntry]) {
    let root_rank = |root: &str| MOUNT_ROOTS.iter().position(|r| *r == root).unwrap_or(usize::MAX);
    entries.sort_by(|a, b| {
        root_rank(a.root)
            .cmp(&root_rank(b.root))
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });
}

/// Enumerate every File/Symlink document under all four config mount
/// roots. Deterministically ordered (root, then lexical `rel_path`) so two
/// exports of the same store are byte-identical — the comparison strategy
/// the whole migration rests on.
///
/// A document registered but blockless (halted replay) is an explicit
/// [`ConfigExportError::BlocklessDocument`], never silently skipped — a lost
/// config file during a migration is data corruption.
pub fn export_config_tree(
    blocks: &SharedBlockStore,
) -> Result<Vec<ConfigTreeEntry>, ConfigExportError> {
    let mut entries = Vec::new();
    for &root in &MOUNT_ROOTS {
        let rows = blocks.documents_under_path(root)?;
        for (path, ctx, doc_kind) in rows {
            let rel_path = path
                .strip_prefix(root)
                .and_then(|s| s.strip_prefix('/'))
                .ok_or_else(|| ConfigExportError::PathNotUnderRoot {
                    root,
                    path: path.clone(),
                })?
                .to_string();

            let content = config_doc::read_content(blocks, ctx).ok_or_else(|| {
                ConfigExportError::BlocklessDocument {
                    root,
                    rel_path: rel_path.clone(),
                }
            })?;

            // Git-style mode bit: a doc is a symlink iff its DocKind says so,
            // never inferred from content. Any other kind reads as a file —
            // mirrors `ConfigDocFs::readdir`'s own `if kind == Symlink`
            // branch, so this enumeration can never disagree with the live
            // VFS about what a document is.
            let kind = if doc_kind == DocKind::Symlink {
                ConfigTreeKind::Symlink { target: content }
            } else {
                ConfigTreeKind::File { content }
            };

            entries.push(ConfigTreeEntry { root, rel_path, kind });
        }
    }
    sort_entries(&mut entries);
    Ok(entries)
}

/// Write `entries` into a real directory tree: `dir/<subdir>/<rel_path>` for
/// each mount (`rc/`, `config/`, `client/`, `midi/`). Files are UTF-8 bytes;
/// symlinks are real symlinks via `std::os::unix::fs::symlink` (Unix-only API
/// but portable to macOS — no Linux-only calls). Dangling targets materialize
/// fine; reachability is never validated here.
///
/// Every write is an atomic whole-file replace: build the new file/symlink at
/// a sibling temp path, then `rename` over the final path, so a crash
/// mid-materialize never leaves a torn write at the destination (a partial
/// `rename` is atomic on POSIX within one filesystem).
pub fn materialize(entries: &[ConfigTreeEntry], dir: &Path) -> Result<(), ConfigExportError> {
    for entry in entries {
        let full = dir.join(root_subdir(entry.root)).join(&entry.rel_path);
        let parent = full
            .parent()
            .expect("full path always has a parent: dir/<subdir>/... ");
        std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;

        let file_name = full
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("entry");
        let tmp = parent.join(format!(".{file_name}.tmp-{}", uuid::Uuid::new_v4()));

        match &entry.kind {
            ConfigTreeKind::File { content } => {
                std::fs::write(&tmp, content.as_bytes()).map_err(|e| io_err(&tmp, e))?;
            }
            ConfigTreeKind::Symlink { target } => {
                std::os::unix::fs::symlink(target, &tmp).map_err(|e| io_err(&tmp, e))?;
            }
        }
        std::fs::rename(&tmp, &full).map_err(|e| io_err(&full, e))?;
    }
    Ok(())
}

/// Read a directory tree materialized by [`materialize`] back into the same
/// model, in the same deterministic order. Refuses (loudly) any symlink whose
/// target resolves outside its mount root — the same escape
/// `ConfigDocFs::resolve_target` refuses at live-read time, checked here
/// with the identical normalization ([`normalize_abs`]) so import and the
/// live VFS can never disagree about what counts as an escape.
pub fn import_config_tree(dir: &Path) -> Result<Vec<ConfigTreeEntry>, ConfigExportError> {
    let mut entries = Vec::new();
    for &root in &MOUNT_ROOTS {
        let base = dir.join(root_subdir(root));
        if !base.exists() {
            // No documents were ever materialized under this mount — a valid
            // "this store seeded nothing here" state, not an error.
            continue;
        }
        walk_dir(&base, &base, root, &mut entries)?;
    }
    sort_entries(&mut entries);
    Ok(entries)
}

fn walk_dir(
    base: &Path,
    current: &Path,
    root: &'static str,
    out: &mut Vec<ConfigTreeEntry>,
) -> Result<(), ConfigExportError> {
    let read_dir = std::fs::read_dir(current).map_err(|e| io_err(current, e))?;
    for dirent in read_dir {
        let dirent = dirent.map_err(|e| io_err(current, e))?;
        let path = dirent.path();
        let file_type = dirent.file_type().map_err(|e| io_err(&path, e))?;

        let rel_path = path
            .strip_prefix(base)
            .expect("walked entries are always under base")
            .components()
            .map(|c| {
                c.as_os_str()
                    .to_str()
                    .ok_or_else(|| ConfigExportError::NonUtf8Path {
                        root,
                        path: path.clone(),
                    })
            })
            .collect::<Result<Vec<_>, _>>()?
            .join("/");

        if file_type.is_symlink() {
            let target = std::fs::read_link(&path).map_err(|e| io_err(&path, e))?;
            let target =
                target
                    .to_str()
                    .ok_or_else(|| ConfigExportError::NonUtf8Path {
                        root,
                        path: path.clone(),
                    })?
                    .to_string();
            reject_if_escapes(root, &rel_path, &target)?;
            out.push(ConfigTreeEntry {
                root,
                rel_path,
                kind: ConfigTreeKind::Symlink { target },
            });
        } else if file_type.is_dir() {
            walk_dir(base, &path, root, out)?;
        } else {
            let content = std::fs::read_to_string(&path).map_err(|e| io_err(&path, e))?;
            out.push(ConfigTreeEntry {
                root,
                rel_path,
                kind: ConfigTreeKind::File { content },
            });
        }
    }
    Ok(())
}

/// The same escape check `ConfigDocFs::resolve_target` applies at live-read
/// time, reused here (via [`normalize_abs`]) rather than re-derived, so
/// import and the live VFS can never quietly disagree about what "escapes the
/// mount" means.
fn reject_if_escapes(
    root: &'static str,
    rel_path: &str,
    target: &str,
) -> Result<(), ConfigExportError> {
    let link_canonical = format!("{root}/{rel_path}");
    let joined = if target.starts_with('/') {
        target.to_string()
    } else {
        let parent = link_canonical.rsplit_once('/').map_or("", |(p, _)| p);
        format!("{parent}/{target}")
    };
    let resolved = normalize_abs(&joined);
    let under_root = resolved == root || resolved.starts_with(&format!("{root}/"));
    if !under_root {
        return Err(ConfigExportError::SymlinkEscapesRoot {
            root,
            rel_path: rel_path.to_string(),
            target: target.to_string(),
            resolved,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::shared_block_store_with_db;
    use crate::config_doc::config_context_id;
    use crate::kernel_db::KernelDb;
    use crate::runtime::config_doc_fs::ConfigDocFs;
    use crate::vfs::VfsOps as _;
    use kaijutsu_types::PrincipalId;
    use std::path::Path as StdPath;
    use std::sync::Arc;

    /// A block store backed by a temporary `KernelDb`, so config docs
    /// created via `create_document_with_path` land in the `documents`
    /// manifest `documents_under_path` reads. Mirrors the fixture in
    /// `runtime/config_doc_fs.rs` and `editor.rs`.
    fn blocks_with_db() -> SharedBlockStore {
        let creator = PrincipalId::system();
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::temporary().unwrap()));
        let ws_id = db.lock().get_or_create_default_workspace(creator).unwrap();
        shared_block_store_with_db(db, ws_id, creator)
    }

    fn find<'a>(entries: &'a [ConfigTreeEntry], root: &str, rel_path: &str) -> &'a ConfigTreeEntry {
        entries
            .iter()
            .find(|e| e.root == root && e.rel_path == rel_path)
            .unwrap_or_else(|| panic!("no entry for {root}/{rel_path} in {entries:#?}"))
    }

    #[tokio::test]
    async fn round_trip_seeded_store_is_lossless() {
        let blocks = blocks_with_db();
        let rc = ConfigDocFs::new(blocks.clone(), RC_ROOT);
        rc.seed_from_embedded().unwrap();
        let config = ConfigDocFs::new(blocks.clone(), CONFIG_ROOT);
        config
            .seed_entries(crate::config_seed::config_seed_files())
            .unwrap();

        // Add content that exercises symlinks and relative/dangling targets,
        // not just the seeded set.
        rc.write_all(StdPath::new("coder/create/S30-extra.kai"), b"kj block create")
            .await
            .unwrap();
        rc.symlink(
            StdPath::new("coder/create/S31-link.kai"),
            StdPath::new("S30-extra.kai"),
        )
        .await
        .unwrap();
        rc.symlink(
            StdPath::new("coder/create/S32-dangling.kai"),
            StdPath::new("/etc/rc/nowhere/S99-gone.kai"),
        )
        .await
        .unwrap();

        let exported = export_config_tree(&blocks).unwrap();
        assert!(!exported.is_empty());

        let tmp = tempfile::tempdir().unwrap();
        materialize(&exported, tmp.path()).unwrap();
        let imported = import_config_tree(tmp.path()).unwrap();

        assert_eq!(
            exported, imported,
            "materialize+import must exactly reproduce the exported tree"
        );
    }

    #[tokio::test]
    async fn symlink_fidelity_relative_and_dangling() {
        let blocks = blocks_with_db();
        let rc = ConfigDocFs::new(blocks.clone(), RC_ROOT);
        rc.write_all(StdPath::new("coder/create/real.kai"), b"body")
            .await
            .unwrap();
        // Relative target.
        rc.symlink(
            StdPath::new("coder/create/S05-link.kai"),
            StdPath::new("real.kai"),
        )
        .await
        .unwrap();
        // Dangling target.
        rc.symlink(
            StdPath::new("coder/create/S06-dangling.kai"),
            StdPath::new("/etc/rc/nope/nothing.kai"),
        )
        .await
        .unwrap();

        let exported = export_config_tree(&blocks).unwrap();
        let rel_link = find(&exported, RC_ROOT, "coder/create/S05-link.kai");
        assert_eq!(
            rel_link.kind,
            ConfigTreeKind::Symlink { target: "real.kai".to_string() }
        );
        let dangling = find(&exported, RC_ROOT, "coder/create/S06-dangling.kai");
        assert_eq!(
            dangling.kind,
            ConfigTreeKind::Symlink {
                target: "/etc/rc/nope/nothing.kai".to_string()
            }
        );

        let tmp = tempfile::tempdir().unwrap();
        materialize(&exported, tmp.path()).unwrap();

        // The dangling link must exist on disk as an actual (broken) symlink,
        // never resolved or copied as a file.
        let dangling_path = tmp.path().join("rc/coder/create/S06-dangling.kai");
        let meta = std::fs::symlink_metadata(&dangling_path).unwrap();
        assert!(meta.file_type().is_symlink());
        assert!(
            !dangling_path.exists(),
            "dangling target must not resolve to a real file"
        );

        let imported = import_config_tree(tmp.path()).unwrap();
        assert_eq!(exported, imported);
    }

    #[tokio::test]
    async fn ordering_is_deterministic_and_lexical_not_iteration_order() {
        let blocks = blocks_with_db();
        let rc = ConfigDocFs::new(blocks.clone(), RC_ROOT);
        // Write out of lexical order on purpose.
        for name in ["zzz.kai", "aaa.kai", "mmm.kai"] {
            rc.write_all(StdPath::new(&format!("t/create/{name}")), b"x")
                .await
                .unwrap();
        }

        let export1 = export_config_tree(&blocks).unwrap();
        let export2 = export_config_tree(&blocks).unwrap();
        assert_eq!(export1, export2, "two exports of the same store must be identical");

        let names: Vec<&str> = export1
            .iter()
            .filter(|e| e.root == RC_ROOT)
            .map(|e| e.rel_path.as_str())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "export order must be lexical by rel_path");
    }

    #[tokio::test]
    async fn non_ascii_content_round_trips() {
        let blocks = blocks_with_db();
        let rc = ConfigDocFs::new(blocks.clone(), RC_ROOT);
        let body = "# 会術\n練習になります — 日本語のテスト 🎵\n";
        rc.write_all(StdPath::new("coder/create/S00-ja.md"), body.as_bytes())
            .await
            .unwrap();

        let exported = export_config_tree(&blocks).unwrap();
        let entry = find(&exported, RC_ROOT, "coder/create/S00-ja.md");
        assert_eq!(entry.kind, ConfigTreeKind::File { content: body.to_string() });

        let tmp = tempfile::tempdir().unwrap();
        materialize(&exported, tmp.path()).unwrap();
        let imported = import_config_tree(tmp.path()).unwrap();
        assert_eq!(exported, imported);
        let round_tripped = find(&imported, RC_ROOT, "coder/create/S00-ja.md");
        assert_eq!(
            round_tripped.kind,
            ConfigTreeKind::File { content: body.to_string() }
        );
    }

    #[tokio::test]
    async fn import_rejects_symlink_escaping_mount_root() {
        let blocks = blocks_with_db();
        let rc = ConfigDocFs::new(blocks.clone(), RC_ROOT);
        rc.write_all(StdPath::new("coder/create/S00-x.kai"), b"x")
            .await
            .unwrap();
        let exported = export_config_tree(&blocks).unwrap();

        let tmp = tempfile::tempdir().unwrap();
        materialize(&exported, tmp.path()).unwrap();

        // Hand-craft an escaping symlink directly on disk, as if a bad actor
        // (or a bug) had written outside the mount's own tree — import must
        // refuse it, not silently accept it.
        let evil_path = tmp.path().join("rc/coder/create/S99-evil.kai");
        std::os::unix::fs::symlink("/etc/config/theme.toml", &evil_path).unwrap();

        let err = import_config_tree(tmp.path()).unwrap_err();
        assert!(
            matches!(err, ConfigExportError::SymlinkEscapesRoot { .. }),
            "expected SymlinkEscapesRoot, got {err:?}"
        );
    }

    #[tokio::test]
    async fn blockless_document_is_an_explicit_error_not_a_silent_skip() {
        let blocks = blocks_with_db();
        // Register a document directly (bypassing ConfigDocFs::put_content),
        // leaving it blockless — the halted-replay case.
        let canonical = format!("{RC_ROOT}/coder/create/S00-halted.kai");
        let ctx = config_context_id(&canonical);
        blocks
            .create_document_with_path(ctx, DocKind::File, None, canonical)
            .unwrap();

        let err = export_config_tree(&blocks).unwrap_err();
        assert!(
            matches!(err, ConfigExportError::BlocklessDocument { .. }),
            "expected BlocklessDocument, got {err:?}"
        );
    }

    #[tokio::test]
    async fn export_covers_all_four_mount_roots() {
        let blocks = blocks_with_db();
        for root in [RC_ROOT, CONFIG_ROOT, CLIENT_ROOT, MIDI_ROOT] {
            let fs = ConfigDocFs::new(blocks.clone(), root);
            fs.write_all(StdPath::new("probe.txt"), b"present")
                .await
                .unwrap();
        }
        let exported = export_config_tree(&blocks).unwrap();
        for root in [RC_ROOT, CONFIG_ROOT, CLIENT_ROOT, MIDI_ROOT] {
            assert!(
                exported.iter().any(|e| e.root == root && e.rel_path == "probe.txt"),
                "missing probe under {root}: {exported:#?}"
            );
        }
    }
}
