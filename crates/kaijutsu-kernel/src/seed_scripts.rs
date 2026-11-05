//! Built-in rc lifecycle scripts, embedded at build time and seeded onto
//! the deployed `/config/rc` tree (`~/.config/kaijutsu/config/rc/`) on first boot.
//!
//! A seed whose whole body is a path naming another seed is written as a real
//! symlink, not as a file holding that text — this is the init.d composition
//! format, and `include_dir!` cannot carry a link. See `docs/rc-on-disk.md`.
//!
//! These are the defaults a fresh kernel bootstraps with:
//!
//! - `/config/rc/<dir>/**` — the directories the shipped characters run
//!   (`person`, `banto`, `coder`, `mcp`, `toolie`, `musician`, `council`, and
//!   the rest; `docs/character.md`, "rc"). Most ship an `S00-stance` `.kai`
//!   script that authors instructions, reading `.md` data when needed, so the
//!   kernel-side contract is self-contained (independent of any per-client
//!   CLAUDE.md), a binding loadout, and the prompt-cache recipe
//!   (`kj cache --help`).
//! - `/config/rc/lib/**` — the shared bodies those directories link to, the
//!   hook bodies, and `lib/recipes/`, scripts a player runs on demand.
//!
//! ## Storage
//!
//! rc scripts are **files**, not table rows. The bodies live as real files
//! under `assets/defaults/rc/` (a 1:1 mirror of the `/config/rc` tree),
//! embedded here via [`include_dir!`]. The embedded tree IS the manifest:
//! adding or removing a seed is just adding or removing a file under
//! `assets/defaults/rc/` — no Rust edit. Dispatch reads the deployed files;
//! see `rc/mod.rs`.
//!
//! ## Seed contract — bootstrap-once, not a floor
//!
//! The deployed tree is the **live source of truth**: what you edit (via the
//! in-app `vi`, the file tools, or host `vim`) and what dispatch runs. The
//! embedded defaults bootstrap it **once**, on a genuinely fresh install:
//!
//! - Fresh install (rc tree absent/empty): [`ensure_rc_seed_files`] writes
//!   every embedded default. The server only calls it when the tree is fresh
//!   (see `kaijutsu-server` rpc bootstrap).
//! - Re-open with files intact: untouched. Boot never auto-writes the live
//!   tree again — a script you `rm`'d stays gone, a repo-dropped seed does
//!   not linger or resurrect. Live is truth.
//! - Botched an edit? `kaijutsu-server rc reseed` reinstalls anything
//!   missing from the embedded seed ([`seed_body`]), and `--force` also
//!   restores what differs — recovery without the repo checked out. It runs
//!   off the kernel, so a botched rc script cannot lock you out of the fix.
//!   The scripts are host files, so `git checkout` reaches them too.
//!
//! ## Updating the seed
//!
//! Edit (or add/remove) the asset file under `assets/defaults/rc/` to change
//! what fresh installs bootstrap with. This does not touch already-deployed
//! trees (live is truth); `kaijutsu-server rc reseed [--force]` is the
//! explicit pull.

use include_dir::{include_dir, Dir, DirEntry};

/// The embedded `/config/rc` seed tree — a 1:1 mirror of `assets/defaults/rc/`,
/// embedded at build time. This is the manifest: every `.kai`/`.md` file
/// under it is a seed, keyed by its path.
static RC_SEED_DIR: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../../assets/defaults/rc");

/// The VFS root every rc canonical path lives under. Re-exported from
/// [`kaijutsu_types::paths::RC_ROOT`] — the single source of truth — so this
/// name keeps working for existing callers/doc links without redeclaring the
/// string. The deployed tree (`~/.config/kaijutsu/rc/...`) and the embedded
/// mirror (`assets/defaults/rc/`) drop this prefix (plus the separating `/`)
/// — the host path is `root.join(relpath)`, the embedded lookup key is
/// `relpath`.
pub use kaijutsu_types::paths::RC_ROOT as RC_VFS_ROOT;

/// Strip the `/config/rc/` prefix from a canonical rc path. Returns `None`
/// for a path that isn't under the rc root.
fn rc_relpath(canonical: &str) -> Option<&str> {
    canonical.strip_prefix(RC_VFS_ROOT)?.strip_prefix('/')
}

/// Recursively collect every embedded `.kai`/`.md` seed file as
/// `(canonical /config/rc path, body)`.
fn collect_seeds(dir: &'static Dir<'static>, out: &mut Vec<(String, &'static str)>) {
    for entry in dir.entries() {
        match entry {
            DirEntry::Dir(sub) => collect_seeds(sub, out),
            DirEntry::File(file) => {
                let rel = match file.path().to_str() {
                    Some(r) => r,
                    None => continue, // non-UTF-8 path: not a canonical rc file
                };
                if !(rel.ends_with(".kai") || rel.ends_with(".md")) {
                    continue;
                }
                let body = file
                    .contents_utf8()
                    .expect("embedded rc seed must be valid UTF-8");
                out.push((format!("{RC_VFS_ROOT}/{rel}"), body));
            }
        }
    }
}

/// The embedded seed set as `(canonical /config/rc path, body)` pairs, derived
/// by walking [`RC_SEED_DIR`]. The path encodes
/// `rc_dir / verb / sort_key / name / ext`; nothing else is stored
/// (provenance comes from the kernel block's principal on write).
pub fn seed_files() -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    collect_seeds(&RC_SEED_DIR, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The embedded seed body for one canonical rc path, or `None` if no seed
/// ships for it. Powers `kaijutsu-server rc reseed`: restore-from-default
/// without the repo checked out, and the seed half of `kj rc list`'s
/// in-sync/differs comparison.
pub fn seed_body(canonical_path: &str) -> Option<&'static str> {
    let rel = rc_relpath(canonical_path)?;
    RC_SEED_DIR.get_file(rel).and_then(|f| f.contents_utf8())
}

/// Write the embedded seed tree into `root` (the host dir mounted at
/// `/config/rc`), creating only files that don't already exist. Returns the
/// number of files newly written.
///
/// This is **bootstrap**, not a per-boot floor: the caller invokes it only
/// when the deployed tree is fresh (absent/empty). Within that single call,
/// "write if absent" lets a legacy migration that pre-wrote some files keep
/// them while the rest are filled in.
///
/// Per the crash-over-corruption stance this surfaces I/O errors rather than
/// swallowing them: a half-written seed tree is corruption, and the caller
/// decides whether a fork can proceed without its stance script.
pub fn ensure_rc_seed_files(root: &std::path::Path) -> std::io::Result<usize> {
    reseed_rc_files(root, false).map(|r| r.written)
}

/// What one reseed did, per entry in the embedded set.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RcSeedReport {
    /// Entries that were absent and are now installed.
    pub written: usize,
    /// Entries that existed, differed from their seed, and were replaced.
    /// Always 0 without `force`.
    pub replaced: usize,
    /// Entries already identical to their embedded seed. Nothing to do.
    pub unchanged: usize,
    /// Paths, relative to the rc root, that exist and differ from their
    /// embedded seed and were left alone. Always empty with `force`, where
    /// the same entries are counted in `replaced` instead.
    ///
    /// Named rather than counted on purpose: presence and agreement are
    /// different facts, and a count folds them together. A caller that only
    /// learns "73 left alone" cannot tell a pristine tree from 73 edits —
    /// which is exactly the question worth answering when an rc root is
    /// pointed somewhere other than its default.
    pub diverged: Vec<String>,
    /// Retired seed paths ([`RETIRED_RC_SEEDS`]) and retired directories
    /// ([`RETIRED_RC_DIRS`], named with a trailing `/`) still present and
    /// left alone. Always empty with `force`, which removes retired seed
    /// paths and moves retired directories aside instead.
    pub retired: Vec<String>,
    /// Paths, relative to the rc root, inside the retired directories named
    /// in `retired` that are not links to the shared `lib/` seeds: files a
    /// player may have written, which `force` moves aside with the rest of
    /// the directory. Always empty with `force`.
    pub unshipped: Vec<String>,
    /// Retired seed paths this reseed removed. Always empty without `force`.
    pub removed: Vec<String>,
    /// Retired directories this reseed moved aside with `force`, as
    /// `(directory with a trailing "/", new name beside it)`. Nothing in
    /// them is deleted.
    pub moved: Vec<(String, String)>,
    /// Retired directories that are symlinks, as `(directory with a trailing
    /// "/", link target as written)`. A reseed never follows the link: the
    /// target's files are not listed in `unshipped`, and `force` moves the
    /// link itself and leaves the target where it is.
    pub linked: Vec<(String, String)>,
    /// Retired directories `force` could not move, as `(directory with a
    /// trailing "/", the error)`. Each stays where it is; the rest of the
    /// reseed finished.
    pub failed: Vec<(String, String)>,
    /// Seed and retired paths with a `<path>.keep` file beside them, left as
    /// they are (present, edited, or absent), even with `force`.
    pub kept: Vec<String>,
}

/// Paths, relative to the rc root, that earlier seeds installed and the
/// embedded set no longer carries. Install-if-absent cannot see them, so
/// [`reseed_rc_files`] names each one still present and removes it under
/// `force`. The lfm2d entries installed the retired risk-classifier pre_call
/// hook (`docs/gate-policy-tuning.md`). The kaish primer moved into the shell
/// tools' descriptions, one statement for every seat; the coder's `done` grant
/// moved into its own binding. The toolie holds no exec, so it ships no shell
/// guard. Each context type now owns its whole stance, so the shared base and
/// its links are gone; the mcp stance went because an outside agent's own
/// system prompt is the only one its model reads.
pub const RETIRED_RC_SEEDS: &[&str] = &[
    "coder/create/S15-recall.kai",
    "lib/create/S00-base.kai",
    "lib/create/S00-base.md",
    "coder/create/S00-base.kai",
    "coder/create/S00-base.md",
    "mcp/create/S00-stance.kai",
    "mcp/create/S00-stance.md",
    "coder/create/S05-kaish.kai",
    "coder/create/S11-done.kai",
    "lib/create/S05-kaish.kai",
    "mcp/create/S05-kaish.kai",
    "toolie/create/S05-kaish.kai",
    "coder/create/S50-lfm2d.kai",
    "lib/create/S50-lfm2d.kai",
    "lib/hooks/lfm2d.kai",
    "mcp/create/S50-lfm2d.kai",
    "toolie/create/S50-lfm2d.kai",
    "toolie/create/S45-shell-guard.kai",
];

/// Directories, relative to the rc root, that earlier seeds installed and the
/// embedded set no longer carries: the characters' directories absorbed them
/// (`person/` took `root/` and `shell/`, `banto/` took `director/` and
/// `console/`, and `default/` contexts run `coder/`). [`reseed_rc_files`]
/// names each one still present, with a trailing `/`, and removes it and
/// moves it aside under `force`. Sorted.
pub const RETIRED_RC_DIRS: &[&str] = &["console", "default", "director", "root", "shell"];

/// Write the embedded seed tree into `root` (the host directory mounted at
/// `/config/rc`).
///
/// Without `force` this is install-if-absent: an entry that exists is left
/// alone, so an edit survives and a script you removed stays removed. That is
/// the bootstrap the server runs on a fresh tree. It still *compares* every
/// entry it leaves alone, and names the ones that differ from their seed in
/// [`RcSeedReport::diverged`] — leaving a file alone silently and leaving it
/// alone loudly cost the same, and only one of them tells you your rc root
/// is not what you thought.
///
/// With `force` an entry whose content differs from its embedded seed is
/// replaced, and a removed one comes back. A composed seed is restored **as a
/// symlink** even if something replaced it with a regular file — the entry is
/// removed and recreated rather than written through, because writing through
/// a link would edit the shared body instead of restoring the link.
///
/// A retired seed ([`RETIRED_RC_SEEDS`]) or retired directory
/// ([`RETIRED_RC_DIRS`]) still present is named in [`RcSeedReport::retired`]
/// and left alone. With `force` a retired seed is removed and named in
/// [`RcSeedReport::removed`]; a retired directory is renamed to
/// `<dir>.retired-<UTC timestamp>` beside it, with the files inside it, and
/// named in [`RcSeedReport::moved`]. A retired directory that is a symlink
/// moves as the link: its target is not followed, listed, or moved, and the
/// link is named in [`RcSeedReport::linked`]. A move that fails (another
/// filesystem, a busy mount) is named in [`RcSeedReport::failed`] with its
/// error and the reseed finishes. The move never replaces an existing name. `force` deletes no directory and nothing
/// you added, so a script of yours under a retired name survives at the new
/// path, and a script under any other path is never touched.
///
/// A seed or retired path with a `<path>.keep` file beside it is left exactly
/// as it is, present, edited, or absent, with or without `force`, and named in
/// [`RcSeedReport::kept`]. That is how a local edit survives a routine forced
/// reseed, and how a removed stock script stays removed.
///
/// Per the crash-over-corruption stance this surfaces I/O errors rather than
/// swallowing them: a half-written seed tree is corruption.
pub fn reseed_rc_files(
    root: &std::path::Path,
    force: bool,
) -> std::io::Result<RcSeedReport> {
    reseed_rc_files_with(root, force, &rename_noreplace)
}

/// [`reseed_rc_files`] with the directory mover chosen by the caller, so a
/// test can make a move fail. `mover(from, to)` must not replace `to`.
fn reseed_rc_files_with(
    root: &std::path::Path,
    force: bool,
    mover: &dyn Fn(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<RcSeedReport> {
    let seeds = seed_files();
    let known: std::collections::HashSet<String> =
        seeds.iter().map(|(p, _)| p.clone()).collect();
    let mut report = RcSeedReport::default();
    for (path, content) in &seeds {
        let Some(rel) = rc_relpath(path) else {
            continue;
        };
        let dest = root.join(rel);
        if is_kept(root, rel) {
            report.kept.push(rel.to_string());
            continue;
        }
        // `symlink_metadata`, not `exists()`: `exists()` follows the link, so
        // a composed link whose target is not written yet would read as absent
        // and be created twice.
        let present = std::fs::symlink_metadata(&dest).is_ok();
        if present {
            // Compare before consulting `force`: whether the entry agrees
            // with its seed is worth reporting either way, and only the
            // decision about what to DO with a disagreement depends on the
            // flag.
            if seed_entry_matches(&dest, path, content, &known) {
                report.unchanged += 1;
                continue;
            }
            if !force {
                report.diverged.push(rel.to_string());
                continue;
            }
            // Remove and recreate rather than write through: a composed seed
            // is a symlink, and writing through it would edit the shared body.
            std::fs::remove_file(&dest)?;
            report.replaced += 1;
        } else {
            report.written += 1;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match seed_link_target(path, content, &known) {
            // An init.d-style composed seed becomes a real symlink. The body
            // carries the target as a `/config/rc` path, which is meaningless on
            // disk, so it is rewritten relative to the link — that keeps the
            // tree valid wherever it is mounted or copied.
            Some(target) => {
                let canonical = canonical_link_target(path, &target);
                let Some(target_rel) = rc_relpath(&canonical) else {
                    continue;
                };
                std::os::unix::fs::symlink(relative_link(rel, target_rel), &dest)?;
            }
            None => std::fs::write(&dest, content)?,
        }
    }
    for rel in RETIRED_RC_SEEDS {
        let dest = root.join(rel);
        if std::fs::symlink_metadata(&dest).is_err() {
            continue;
        }
        if is_kept(root, rel) {
            report.kept.push(rel.to_string());
            continue;
        }
        if force {
            std::fs::remove_file(&dest)?;
            report.removed.push(rel.to_string());
        } else {
            report.retired.push(rel.to_string());
        }
    }
    for dir in RETIRED_RC_DIRS {
        let dest = root.join(dir);
        if std::fs::symlink_metadata(&dest).is_err() {
            continue;
        }
        let named = format!("{dir}/");
        if is_kept(root, dir) {
            report.kept.push(named);
            continue;
        }
        if std::fs::symlink_metadata(&dest).is_ok_and(|m| m.file_type().is_symlink()) {
            let link = std::fs::read_link(&dest).map(|t| t.to_string_lossy().into_owned()).unwrap_or_default();
            report.linked.push((named.clone(), link));
        }
        if force {
            let stamp = crate::llm::request_dump::utc_stamp(now_ms());
            // A failed move is a reported skip, not an abort: this loop runs
            // after the seed writes, so stopping here would leave a partly
            // reseeded tree with no report. The directory stays where it is.
            match move_aside(root, dir, &stamp, mover) {
                Ok(target) => {
                    let moved_to = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    report.moved.push((named, moved_to));
                }
                Err(e) => report.failed.push((named, e.to_string())),
            }
        } else {
            if !report.linked.iter().any(|(n, _)| *n == named) {
                report.unshipped.extend(unshipped_files(root, dir));
            }
            report.retired.push(named);
        }
    }
    report.retired.sort();
    report.unshipped.sort();
    report.removed.sort();
    report.moved.sort();
    report.kept.sort();
    Ok(report)
}

/// Move retired directory `dir` aside to a name nothing holds. A name taken
/// between choosing it and moving makes the mover fail with `AlreadyExists`;
/// the next name is tried.
fn move_aside(
    root: &std::path::Path,
    dir: &str,
    stamp: &str,
    mover: &dyn Fn(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<std::path::PathBuf> {
    let from = root.join(dir);
    let mut last = None;
    for _ in 0..16 {
        let target = aside_target(root, dir, stamp);
        match mover(&from, &target) {
            Ok(()) => return Ok(target),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.expect("the loop ran"))
}

/// `rename(2)` that fails with `AlreadyExists` instead of replacing `to`
/// (`renameat2` with `RENAME_NOREPLACE`, through `rustix`). A plain rename
/// would replace an empty directory or a file that appeared after the name
/// was chosen.
fn rename_noreplace(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    rustix::fs::renameat_with(rustix::fs::CWD, from, rustix::fs::CWD, to, rustix::fs::RenameFlags::NOREPLACE)
        .map_err(std::io::Error::from)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `<dir>.retired-<stamp>` under `root`; with that name taken, `-2`, `-3`, ...
/// after it. A move never lands on an earlier one.
fn aside_target(root: &std::path::Path, dir: &str, stamp: &str) -> std::path::PathBuf {
    let base = format!("{dir}.retired-{stamp}");
    let mut candidate = root.join(&base);
    let mut n = 2;
    while std::fs::symlink_metadata(&candidate).is_ok() {
        candidate = root.join(format!("{base}-{n}"));
        n += 1;
    }
    candidate
}

/// Every file under the retired directory `dir` (paths relative to `root`)
/// except a symlink whose target runs through `lib/`, which is how seeds
/// installed shared scripts. A file an old seed wrote as a regular file is
/// named too: the walk cannot tell it from a player's, and naming too much is
/// the safe side.
fn unshipped_files(root: &std::path::Path, dir: &str) -> Vec<String> {
    fn walk(root: &std::path::Path, rel: &str, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(root.join(rel)) else { return };
        for entry in entries.flatten() {
            let child = format!("{rel}/{}", entry.file_name().to_string_lossy());
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
            if meta.is_dir() {
                walk(root, &child, out);
            } else if meta.file_type().is_symlink()
                && std::fs::read_link(entry.path()).is_ok_and(|t| t.components().any(|c| c.as_os_str() == "lib"))
            {
                continue;
            } else {
                out.push(child);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, dir, &mut out);
    out
}

/// True if `<rel>.keep` exists beside a seed path: the player owns that entry,
/// whether they edited it, replaced a link with a file, or removed it. Rc
/// discovery ignores the marker, since only `SXX-name.kai` names run.
fn is_kept(root: &std::path::Path, rel: &str) -> bool {
    std::fs::symlink_metadata(root.join(format!("{rel}.keep"))).is_ok()
}

/// True if the entry at `dest` already is what the embedded seed says it
/// should be — a link pointing at the right target, or a file with the right
/// bytes. Used by a forced reseed so an untouched tree reports no churn.
fn seed_entry_matches(
    dest: &std::path::Path,
    canonical: &str,
    content: &str,
    known: &std::collections::HashSet<String>,
) -> bool {
    match seed_link_target(canonical, content, known) {
        Some(target) => {
            let want = canonical_link_target(canonical, &target);
            let Some(target_rel) = rc_relpath(&want) else {
                return false;
            };
            let Some(link_rel) = rc_relpath(canonical) else {
                return false;
            };
            std::fs::read_link(dest)
                .map(|got| got.to_string_lossy() == relative_link(link_rel, target_rel))
                .unwrap_or(false)
        }
        None => std::fs::symlink_metadata(dest)
            .map(|m| !m.file_type().is_symlink())
            .unwrap_or(false)
            && std::fs::read_to_string(dest).map(|got| got == *content).unwrap_or(false),
    }
}

/// Normalize an absolute path string: collapse `//`, drop `.`, resolve `..`
/// (popping the prior segment). Returns a clean absolute path (`/a/b/c`).
pub(crate) fn normalize_abs(path: &str) -> String {
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
pub(crate) fn seed_link_target(
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

/// Resolve a seed link body — or a live on-disk symlink's raw readlink
/// target — to its canonical `/config/rc` path. Absolute resolves as-is;
/// relative (what a real host symlink carries, `relative_link` below)
/// resolves against the link's own directory. `pub(crate)` so `kj rc`'s
/// seed-staleness comparison (`kj/rc.rs`) can canonicalize a live target to
/// the same coordinate the seed body is already in.
pub(crate) fn canonical_link_target(link_path: &str, target: &str) -> String {
    if target.starts_with('/') {
        normalize_abs(target)
    } else {
        let parent = link_path.rsplit_once('/').map_or("", |(p, _)| p);
        normalize_abs(&format!("{parent}/{target}"))
    }
}

/// A `../`-prefixed path from `link_rel`'s directory to `target_rel`, both
/// relative to the rc root. Relative targets are what let the deployed tree be
/// moved, copied, or checked into git somewhere else and still resolve.
fn relative_link(link_rel: &str, target_rel: &str) -> String {
    let depth = link_rel.matches('/').count();
    let mut out = String::new();
    for _ in 0..depth {
        out.push_str("../");
    }
    out.push_str(target_rel);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(root: &std::path::Path, rel: &str) -> Option<String> {
        std::fs::read_to_string(root.join(rel)).ok()
    }

    #[test]
    fn reseed_adds_instruction_scripts_without_overwriting_custom_data() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("coder/create/S00-stance.md");
        std::fs::create_dir_all(data.parent().unwrap()).unwrap();
        std::fs::write(&data, "custom instructions\n").unwrap();
        let report = reseed_rc_files(dir.path(), false).unwrap();
        assert!(report.diverged.contains(&"coder/create/S00-stance.md".to_string()));
        assert_eq!(std::fs::read_to_string(&data).unwrap(), "custom instructions\n");
        assert!(read(dir.path(), "coder/create/S00-stance.kai").unwrap().contains("kj block create"));
        assert!(dir.path().join("coder/create/S20-cache.kai").is_symlink());
        reseed_rc_files(dir.path(), true).unwrap();
        assert_eq!(std::fs::read_to_string(data).unwrap(), seed_body("/config/rc/coder/create/S00-stance.md").unwrap());
    }

    #[test]
    fn fresh_tree_gets_the_shared_and_coder_seeds() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let n = ensure_rc_seed_files(dir.path()).expect("seed");
        assert_eq!(n, seed_files().len(), "every seed file should be written");

        for rel in [
            "coder/create/S20-cache.kai",
            "coder/fork/S30-cache.kai",
            "coder/drift/S40-cache.kai",
            "coder/create/S00-stance.kai",
            "banto/create/S20-cache.kai",
            // The init.d-style canonical bodies the per-type scripts link to.
            "lib/create/S20-cache.kai",
            "lib/create/S10-binding.kai",
        ] {
            assert!(read(dir.path(), rel).is_some(), "missing seed: {rel}");
        }
        // Cache recipe content lives once in lib/ (the canonical body)…
        assert!(read(dir.path(), "lib/create/S20-cache.kai")
            .unwrap()
            .contains("kj cache add --target=tools"));
        // …and the per-type copy is a symlink to it, so reading through
        // reaches the same body. `composed_seeds_become_real_symlinks` pins
        // the link-ness; this pins that the composition resolves.
        assert_eq!(
            read(dir.path(), "coder/create/S20-cache.kai"),
            read(dir.path(), "lib/create/S20-cache.kai"),
            "composed seed must read as its canonical body"
        );
    }

    /// Each context type owns its whole instruction text. No shared base is
    /// linked into several types, and no kernel-wide prompt layer supplies one.
    #[test]
    fn rc_ships_no_shared_base_and_each_role_owns_its_stance() {
        let seeds = seed_files();
        let shared: Vec<&String> = seeds.iter().map(|(path, _)| path)
            .filter(|path| path.contains("S00-base")).collect();
        assert!(shared.is_empty(), "no shared base seed may ship: {shared:?}");

        // An mcp context's performer is an outside agent with its own system
        // prompt; nothing reads instruction text stored in that context.
        assert!(seed_body("/config/rc/mcp/create/S00-stance.md").is_none());
        assert!(seed_body("/config/rc/mcp/create/S00-stance.kai").is_none());

        for dir in ["coder", "banto", "council", "judge", "storyteller", "companion"] {
            let path = format!("/config/rc/{dir}/create/S00-stance.md");
            let stance = seed_body(&path).unwrap_or_else(|| panic!("missing {path}"));
            let stance = stance.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                stance.contains("accountable"),
                "{dir} names who is accountable for the work in its own stance"
            );
        }
    }

    /// Instruction text is read by models that are mostly not English-first.
    /// Shipped stances use plain ASCII English: no em dashes and no other
    /// languages, with two exceptions. A Japanese term may appear when its
    /// romaji follows in parentheses, as in 番頭 (banto), so the reader has
    /// both. A closing 頑張って line is encouragement, not instruction. A
    /// person's own prompts may use any language.
    #[test]
    fn shipped_stances_are_plain_ascii_english() {
        for path in [
            "/config/rc/coder/create/S00-stance.md",
            "/config/rc/banto/create/S00-stance.md",
            "/config/rc/council/create/S00-stance.md",
            "/config/rc/judge/create/S00-stance.md",
            "/config/rc/storyteller/create/S00-stance.md",
            "/config/rc/storyteller/submit/S10-reminder.md",
            "/config/rc/companion/create/S00-stance.md",
            "/config/rc/companion/fork/S20-stance.md",
            "/config/rc/companion/tick/S10-checkin.md",
            "/config/rc/companion/tick/S10-wrapup.md",
            "/config/rc/banto/create/S00-stance.kai",
            "/config/rc/assistant/create/S00-stance.kai",
            "/config/rc/banto/create/S06-kj-help.md",
        ] {
            let body = seed_body(path).unwrap_or_else(|| panic!("missing {path}"));
            let trimmed = body.trim_end();
            let body = trimmed.strip_suffix("頑張って").unwrap_or(trimmed);
            for (term, rest) in non_ascii_runs(body) {
                let glossed = rest.strip_prefix(" (").and_then(|r| r.split_once(')'))
                    .is_some_and(|(romaji, _)| !romaji.is_empty() && romaji.chars().all(|c| c.is_ascii_alphabetic()));
                assert!(glossed, "{path}: {term:?} needs its romaji in parentheses right after it");
            }
        }
    }

    #[test]
    fn a_japanese_term_without_its_romaji_is_refused() {
        let runs = non_ascii_runs("A 番頭 keeps the ledger.");
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].1.starts_with(" ("), "the check must see no gloss here");
    }

    /// Each maximal run of non-ASCII characters, with the text after it.
    fn non_ascii_runs(text: &str) -> Vec<(&str, &str)> {
        let mut runs = Vec::new();
        let mut start = None;
        for (i, c) in text.char_indices() {
            match (c.is_ascii(), start) {
                (false, None) => start = Some(i),
                (true, Some(s)) => {
                    runs.push((&text[s..i], &text[i..]));
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = start {
            runs.push((&text[s..], ""));
        }
        runs
    }

    #[test]
    fn coder_stance_closes_with_ganbatte() {
        let stance = seed_body("/config/rc/coder/create/S00-stance.md").expect("coder stance");
        assert!(stance.trim_end().ends_with("\n\n頑張って"), "coder stance closes with 頑張って");
    }

    #[test]
    fn reseed_force_restores_a_diverged_file_and_a_clobbered_link() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let root = dir.path();
        ensure_rc_seed_files(root).expect("seed");

        // Diverge a real script, and replace a composed link with a file.
        let script = root.join("lib/create/S20-cache.kai");
        std::fs::write(&script, "# diverged\n").expect("diverge");
        let link = root.join("coder/create/S20-cache.kai");
        std::fs::remove_file(&link).expect("remove link");
        std::fs::write(&link, "# not a link any more\n").expect("clobber");

        let report = reseed_rc_files(root, true).expect("reseed --force");
        assert!(report.replaced >= 2, "both divergences should be replaced");

        assert!(
            std::fs::read_to_string(&script)
                .unwrap()
                .contains("kj cache add --target=tools"),
            "a diverged script is restored from its embedded seed"
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "a link clobbered into a file is restored AS A LINK, not as a file \
             holding the target path"
        );
    }

    #[test]
    fn reseed_without_force_leaves_a_diverged_file_alone() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let root = dir.path();
        ensure_rc_seed_files(root).expect("seed");

        let script = root.join("lib/create/S20-cache.kai");
        std::fs::write(&script, "# mine\n").expect("diverge");

        let report = reseed_rc_files(root, false).expect("reseed");
        assert_eq!(report.replaced, 0, "install-if-absent must replace nothing");
        assert_eq!(report.written, 0, "nothing is absent");
        assert_eq!(
            std::fs::read_to_string(&script).unwrap(),
            "# mine\n",
            "an edit survives a non-forced reseed"
        );
    }

    /// A non-forced reseed must NAME what it left alone, not just count it.
    ///
    /// Presence and agreement are different facts, and reporting only the
    /// count conflates them: seventy-three files matching their seed and
    /// seventy-three hand-edits print the same line. Naming the divergent
    /// ones is what makes an rc tree pointed at a checkout obvious instead
    /// of silent.
    ///
    /// Falsified by reporting presence without comparing (the shape this
    /// replaced): `diverged` comes back empty and `unchanged` counts the
    /// edited file. Reverted afterward.
    #[test]
    fn reseed_without_force_names_the_files_it_left_alone() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let root = dir.path();
        ensure_rc_seed_files(root).expect("seed");

        std::fs::write(root.join("lib/create/S20-cache.kai"), "# mine\n").expect("diverge");

        let report = reseed_rc_files(root, false).expect("reseed");
        assert_eq!(
            report.diverged,
            vec!["lib/create/S20-cache.kai".to_string()],
            "the one edited script must be named, not folded into a count"
        );
        assert!(report.unchanged > 0, "the rest of the tree still matches its seed");
        assert_eq!(report.replaced, 0, "naming is not overwriting");

        // And with --force the same entry moves from `diverged` to `replaced`.
        let forced = reseed_rc_files(root, true).expect("forced reseed");
        assert!(forced.diverged.is_empty(), "force leaves nothing merely reported");
        assert_eq!(forced.replaced, 1, "the named file is the one overwritten");
    }

    /// An untouched tree reports no divergence at all — the quiet case has to
    /// stay quiet, or the warning is noise and gets ignored.
    #[test]
    fn reseed_on_an_untouched_tree_names_nothing() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let root = dir.path();
        ensure_rc_seed_files(root).expect("seed");

        let report = reseed_rc_files(root, false).expect("reseed");
        assert!(report.diverged.is_empty(), "a pristine tree diverges nowhere: {report:?}");
        assert_eq!(report.written, 0, "nothing is absent");
        assert!(report.unchanged > 0, "every entry matched");
    }

    /// A `.keep` beside a seeded entry is how a local edit survives the
    /// routine `--force`: the entry is named as kept, never replaced.
    ///
    /// Falsified by ignoring the marker: force restores the seed body and
    /// counts it in `replaced`.
    #[test]
    fn reseed_force_leaves_a_kept_file_alone() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let root = dir.path();
        ensure_rc_seed_files(root).expect("seed");

        let script = root.join("lib/create/S20-cache.kai");
        std::fs::write(&script, "# mine\n").expect("diverge");
        std::fs::write(root.join("lib/create/S20-cache.kai.keep"), "").expect("keep");
        // A composed link replaced by a local file is kept the same way.
        let link = root.join("coder/create/S20-cache.kai");
        std::fs::remove_file(&link).expect("remove link");
        std::fs::write(&link, "# local, not a link\n").expect("clobber");
        std::fs::write(root.join("coder/create/S20-cache.kai.keep"), "").expect("keep");

        for force in [false, true] {
            let report = reseed_rc_files(root, force).expect("reseed");
            assert_eq!(
                report.kept,
                vec![
                    "coder/create/S20-cache.kai".to_string(),
                    "lib/create/S20-cache.kai".to_string()
                ],
                "force={force}: {report:?}"
            );
            assert!(report.diverged.is_empty(), "force={force}: a kept file is not also diverged");
            assert_eq!(report.replaced, 0, "force={force}: nothing replaced");
        }
        assert_eq!(std::fs::read_to_string(&script).unwrap(), "# mine\n");
        assert!(!std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "# local, not a link\n");
    }

    /// Removing a seeded script and keeping the removal is how a stock
    /// script is turned off locally: reseed never writes it back.
    #[test]
    fn a_kept_removal_stays_removed() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let root = dir.path();
        ensure_rc_seed_files(root).expect("seed");

        let script = root.join("storyteller/submit/S10-reminder.kai");
        std::fs::remove_file(&script).expect("remove");
        std::fs::write(root.join("storyteller/submit/S10-reminder.kai.keep"), "").expect("keep");

        for force in [false, true] {
            let report = reseed_rc_files(root, force).expect("reseed");
            assert_eq!(report.written, 0, "force={force}: the removed script is not written back");
            assert_eq!(report.kept, vec!["storyteller/submit/S10-reminder.kai".to_string()]);
        }
        assert!(std::fs::symlink_metadata(&script).is_err(), "still removed");
    }

    /// A retired seed with a `.keep` is the player's now: force does not
    /// remove it.
    #[test]
    fn reseed_force_leaves_a_kept_retired_seed() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let root = dir.path();
        ensure_rc_seed_files(root).expect("seed");

        let rel = RETIRED_RC_SEEDS[0];
        let retired = root.join(rel);
        std::fs::create_dir_all(retired.parent().unwrap()).expect("mkdir");
        std::fs::write(&retired, "# still mine\n").expect("write");
        std::fs::write(root.join(format!("{rel}.keep")), "").expect("keep");

        let report = reseed_rc_files(root, true).expect("reseed --force");
        assert!(report.removed.is_empty(), "{report:?}");
        assert!(report.retired.is_empty(), "{report:?}");
        assert_eq!(report.kept, vec![rel.to_string()]);
        assert!(retired.exists());
    }

    #[test]
    fn reseed_force_restores_a_removed_script() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let root = dir.path();
        ensure_rc_seed_files(root).expect("seed");

        let script = root.join("lib/create/S20-cache.kai");
        std::fs::remove_file(&script).expect("remove");
        let report = reseed_rc_files(root, true).expect("reseed --force");
        assert!(report.written >= 1, "a removed script comes back");
        assert!(script.exists(), "restored");
    }

    #[test]
    fn composed_seeds_become_real_symlinks() {
        let dir = tempfile::tempdir().expect("tmpdir");
        ensure_rc_seed_files(dir.path()).expect("seed");
        let root = dir.path();

        // The per-type copy is a composed link, not a file whose body is a
        // path. Reading through it reaches the canonical body in lib/.
        let link = root.join("coder/create/S20-cache.kai");
        let md = std::fs::symlink_metadata(&link).expect("stat link");
        assert!(
            md.file_type().is_symlink(),
            "composed seed must be a real symlink, not a file holding a path"
        );
        let target = std::fs::read_link(&link).expect("readlink");
        assert!(
            target.is_relative(),
            "link target must be relative so the tree relocates: got {}",
            target.display()
        );
        assert!(
            std::fs::read_to_string(&link)
                .expect("read through link")
                .contains("kj cache add --target=tools"),
            "link must resolve to the canonical body"
        );

        // The canonical body itself stays a regular file.
        assert!(
            !std::fs::symlink_metadata(root.join("lib/create/S20-cache.kai"))
                .expect("stat canonical")
                .file_type()
                .is_symlink(),
            "the canonical body is the link's target, never a link itself"
        );

        // Nothing is left un-materialized: no seeded regular file may hold a
        // body that `seed_link_target` would have called a link. This is the
        // assertion that fires when a new composed seed is added and the
        // seeder does not learn about it.
        let known: std::collections::HashSet<String> =
            seed_files().into_iter().map(|(p, _)| p).collect();
        for (canonical, _) in seed_files() {
            let rel = rc_relpath(&canonical).expect("rc path");
            let dest = root.join(rel);
            if std::fs::symlink_metadata(&dest)
                .expect("stat seed")
                .file_type()
                .is_symlink()
            {
                continue;
            }
            let body = std::fs::read_to_string(&dest).expect("read seed");
            assert!(
                seed_link_target(&canonical, &body, &known)
                    .is_none(),
                "{canonical} seeded as a literal file but its body names another \
                 seed \u{2014} it should have been materialized as a symlink"
            );
        }
    }

    #[test]
    fn ensure_is_idempotent_user_edits_persist() {
        let dir = tempfile::tempdir().expect("tmpdir");
        ensure_rc_seed_files(dir.path()).expect("seed 1");

        // Edit a seed file, then re-run ensure: the edit must survive
        // (file exists → skipped). The server only calls ensure on a fresh
        // tree, but the within-call "skip existing" contract is what keeps a
        // partial (migrated) tree from being clobbered.
        // Edit a real file, not a composed link: writing through a link
        // would land in lib/ and this assertion would then be reading back
        // its own clobber. `edit_through_a_composed_link_reaches_the_shared_body`
        // covers that path deliberately.
        let target = dir.path().join("lib/create/S20-cache.kai");
        std::fs::write(&target, "# user-edited body").expect("edit");
        let n = ensure_rc_seed_files(dir.path()).expect("seed 2");
        assert_eq!(n, 0, "second ensure should write nothing");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "# user-edited body",
            "edit was clobbered by re-seed"
        );
    }

    #[test]
    fn edit_through_a_composed_link_reaches_the_shared_body() {
        // On disk a composed seed is an ordinary symlink, so writing through
        // it changes the body every linking rc directory runs. The document
        // backend refused this through an rc verb; the filesystem does not,
        // and that is the accepted trade for plain files. Pinned so the change
        // in meaning is a decision on the record rather than a surprise.
        let dir = tempfile::tempdir().expect("tmpdir");
        ensure_rc_seed_files(dir.path()).expect("seed");
        let root = dir.path();

        std::fs::write(root.join("banto/create/S20-cache.kai"), "# edited\n")
            .expect("write through link");

        assert_eq!(
            std::fs::read_to_string(root.join("lib/create/S20-cache.kai")).unwrap(),
            "# edited\n",
            "the shared body is what a write through the link reaches"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("coder/create/S20-cache.kai")).unwrap(),
            "# edited\n",
            "and every other linking type sees it"
        );
        assert!(
            std::fs::symlink_metadata(root.join("banto/create/S20-cache.kai"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "writing through a link must not replace the link with a file"
        );
    }

    #[test]
    fn seed_files_derived_from_embedded_tree() {
        let paths: Vec<String> = seed_files().into_iter().map(|(p, _)| p).collect();
        // Spot-check the roles the embedded tree ships.
        for expected in [
            "/config/rc/coder/create/S20-cache.kai",
            "/config/rc/coder/create/S00-stance.kai",
            "/config/rc/musician/tick/S10-drive.kai",
            "/config/rc/musician/create/S00-stance.md",
        ] {
            assert!(paths.contains(&expected.to_string()), "missing seed: {expected}");
        }
        // Only .kai/.md are seeds — no stray extensions leak in.
        assert!(
            paths.iter().all(|p| p.ends_with(".kai") || p.ends_with(".md")),
            "non-script file embedded as a seed: {paths:?}"
        );
    }

    #[test]
    fn seed_body_resolves_embedded_default() {
        // The canonical cache body lives in lib/ …
        let body = seed_body("/config/rc/lib/create/S20-cache.kai")
            .expect("lib cache seed must exist");
        assert!(body.contains("kj cache add --target=tools"));
        // …and a per-type path's seed body is just the link target (a seed
        // symlink — reconstructed into an actual link by `reseed_rc_files`).
        assert_eq!(
            seed_body("/config/rc/coder/create/S20-cache.kai").unwrap().trim(),
            "/config/rc/lib/create/S20-cache.kai"
        );
        // …and a path with no embedded seed is None, which is what marks it
        // `no seed` in `kj rc list` rather than `differs`.
        assert!(
            seed_body("/config/rc/none/create/S00-noop.kai").is_none(),
            "unseeded path must not resolve a body"
        );
    }

    /// The musician ships a `tick` (beat) verb script and a stance — the beat
    /// hook and persona that make a created musician self-compose.
    #[test]
    fn musician_seeds_include_beat_tick_verb() {
        assert!(
            seed_body("/config/rc/musician/tick/S10-drive.kai").is_some(),
            "musician must seed a tick/beat script"
        );
        assert!(
            seed_body("/config/rc/musician/create/S00-stance.md").is_some(),
            "musician must seed a stance"
        );
        // The tick verb is wired into the rc path grammar.
        let parts = crate::rc::parse_rc_path("/config/rc/musician/tick/S10-drive.kai")
            .expect("tick rc path must parse");
        assert_eq!(parts.rc_dir, "musician");
        assert_eq!(parts.verb, "tick");
    }

    /// The shell tools' descriptions carry the one statement of kaish, so no
    /// seat seeds a second one, and reseed removes the copies earlier seeds
    /// installed.
    #[test]
    fn no_seat_seeds_a_kaish_primer() {
        for ty in ["coder", "default", "mcp", "director", "banto", "toolie", "musician", "lib"] {
            let rel = format!("{ty}/create/S05-kaish.kai");
            assert!(seed_body(&format!("/config/rc/{rel}")).is_none(), "{rel} is still seeded");
            let retired = RETIRED_RC_SEEDS.contains(&rel.as_str()) || RETIRED_RC_DIRS.contains(&ty);
            if !["musician", "banto"].contains(&ty) {
                assert!(retired, "{rel} is not retired");
            }
        }
    }

    /// The musician ALSO seeds a fork-side hydration script — the create script
    /// doesn't run on fork, so a forked player needs its own window re-established
    /// (else it drives at tempo with full history). It branches on KJ_FORK_INFO:
    /// window a thin fork, skip a full clone (which would pin its whole log).
    #[test]
    fn musician_fork_seeds_include_hydration_window() {
        let body = seed_body("/config/rc/musician/fork/S40-hydrate.kai")
            .expect("musician must seed a fork-side hydration script");
        assert!(
            body.contains("kj context hydrate"),
            "the fork hydrate seed must set a window via `kj context hydrate`"
        );
        assert!(
            body.contains("KJ_FORK_INFO") && body.contains("full"),
            "the fork hydrate seed must branch on fork kind (skip a full clone)"
        );
    }

    /// Every seed body that is a bare path token must resolve to another
    /// seeded path. The init.d composition format carries a symlink as a file
    /// whose whole content is its target (`include_dir!` cannot embed real
    /// links), and `seed_link_target` reconstructs the link only when that
    /// target is itself in the seed set. A target that is missing — renamed,
    /// moved, or typo'd — does not fail: the body is written as a literal
    /// file whose entire content is a path string, which then runs as an rc
    /// script. This is the check that turns that silent degradation into a
    /// failing test at the moment the rename lands.
    #[test]
    fn every_bare_path_seed_body_resolves_to_a_seeded_target() {
        let seeds = seed_files();
        let known: std::collections::HashSet<String> =
            seeds.iter().map(|(p, _)| p.clone()).collect();

        let mut broken = Vec::new();
        for (path, body) in &seeds {
            let t = body.trim();
            // A link body is one bare path token. Anything carrying whitespace
            // is a script line (`. /config/rc/lib/x.kai`), not a link, and is
            // correctly seeded as content.
            let bare_path_token =
                !t.is_empty() && t.contains('/') && !t.contains(char::is_whitespace);
            if !bare_path_token {
                continue;
            }
            if seed_link_target(path, body, &known).is_none() {
                broken.push(format!("{path} → {t}"));
            }
        }

        assert!(
            broken.is_empty(),
            "these seeds look like symlinks but their targets are not seeded paths, \
             so each would silently become a file containing a path string: {broken:#?}"
        );
    }

    /// A seat ships `S45-shell-guard.kai`, the pre_call hook that denies a
    /// `shell_write` command handing text to a second shell, exactly when its
    /// `create/S10-binding.kai` grants exec-capable authority: `"*"`,
    /// `"exec"`, or `"facade:shell_write"`. A binding that grants exec but
    /// skips it is a seat where `sh -c` runs past the gate unseen; a seat
    /// holding only the read-only `shell` cannot start a program, so the
    /// guard has nothing to guard there.
    #[test]
    fn exactly_the_exec_granting_seats_ship_the_shell_guard() {
        let seeds = seed_files();
        let by_path: std::collections::HashMap<&str, &str> =
            seeds.iter().map(|(p, b)| (p.as_str(), *b)).collect();
        let known: std::collections::HashSet<String> =
            seeds.iter().map(|(p, _)| p.clone()).collect();

        // Every type directory named in the seed set, one level below
        // RC_ROOT, excluding `lib` (a shared script library, not a seat).
        let mut types: Vec<&str> = seeds
            .iter()
            .filter_map(|(p, _)| rc_relpath(p))
            .filter_map(|rel| rel.split('/').next())
            .filter(|t| *t != "lib")
            .collect();
        types.sort_unstable();
        types.dedup();

        const EXEC_GRANTS: [&str; 3] = ["\"*\"", "\"exec\"", "\"facade:shell_write\""];

        let mut missing = Vec::new();
        let mut unneeded = Vec::new();
        for ty in types {
            let binding_path = format!("{RC_VFS_ROOT}/{ty}/create/S10-binding.kai");
            let Some(&body) = by_path.get(binding_path.as_str()) else {
                continue; // no binding at all: no grant, nothing to score
            };
            // Follow one level of link — the init.d composition every
            // per-type binding actually uses — to the body that carries the
            // real grants.
            let resolved = match seed_link_target(&binding_path, body, &known) {
                Some(target) => {
                    let canonical = canonical_link_target(&binding_path, &target);
                    match by_path.get(canonical.as_str()) {
                        Some(&b) => b,
                        None => continue, // dangling link: covered elsewhere
                    }
                }
                None => body,
            };
            let grants_exec = EXEC_GRANTS.iter().any(|g| resolved.contains(g));
            let guard = format!("{RC_VFS_ROOT}/{ty}/create/S45-shell-guard.kai");
            match (grants_exec, by_path.contains_key(guard.as_str())) {
                (true, false) => missing.push(guard),
                (false, true) => unneeded.push(guard),
                _ => {}
            }
        }

        assert!(
            missing.is_empty(),
            "exec-granting seats missing the shell guard: {missing:#?}"
        );
        assert!(
            unneeded.is_empty(),
            "seats without exec that ship the shell guard: {unneeded:#?}"
        );
    }

    /// Every seat that ships the shell guard can run `shell_write`, so the
    /// council may gate it: its create lifecycle, and its fork lifecycle
    /// when it has one, ship `S50-council-seats.kai`, which builds the
    /// seat's council seats ahead of its first decision. person and mcp are
    /// the exceptions: person is every root character's own space, and mcp
    /// is the debugging and admin seat, and neither is put under a council.
    /// No other seat ships it.
    #[test]
    fn the_gated_seats_build_their_council_seats_at_create_and_fork() {
        let seeds = seed_files();
        let has = |path: String| seeds.iter().any(|(p, _)| *p == path);
        let mut types: Vec<&str> = seeds
            .iter()
            .filter_map(|(p, _)| rc_relpath(p))
            .filter_map(|rel| rel.split('/').next())
            .filter(|t| *t != "lib")
            .collect();
        types.sort_unstable();
        types.dedup();
        let mut wrong = Vec::new();
        for ty in types {
            let gated = has(format!("{RC_VFS_ROOT}/{ty}/create/S45-shell-guard.kai")) && !["person", "mcp"].contains(&ty);
            let forks = seeds.iter().any(|(p, _)| p.starts_with(&format!("{RC_VFS_ROOT}/{ty}/fork/")));
            for (verb, wanted) in [("create", gated), ("fork", gated && forks)] {
                let path = format!("{RC_VFS_ROOT}/{ty}/{verb}/S50-council-seats.kai");
                if has(path.clone()) != wanted {
                    wrong.push((path, wanted));
                }
            }
        }
        assert!(wrong.is_empty(), "council-seat scripts missing (true) or unneeded (false): {wrong:#?}");
    }

    /// A test kernel seeds this tree (`SshServerConfig::ephemeral`), and a
    /// test kernel must never reach the network. No seed body registers or
    /// calls `curl`; an rc hook that needs the network is an operator's own
    /// file, not a default.
    #[test]
    fn no_seed_reaches_the_network() {
        let reaching: Vec<String> = seed_files()
            .into_iter()
            .filter(|(_, body)| {
                body.lines().any(|line| {
                    let code = line.split('#').next().unwrap_or("");
                    code.contains("kaish-tools curl") || code.contains("curl ")
                })
            })
            .map(|(path, _)| path)
            .collect();
        assert!(reaching.is_empty(), "seeds that reach the network: {reaching:#?}");
    }

    /// banto performs the console, so its one stance carries the operator's
    /// character and the console's system-administrator duties, and it
    /// speaks of characters and their rc directories, not of types.
    #[test]
    fn banto_carries_the_operator_and_the_administrator_stance() {
        let script = seed_body("/config/rc/banto/create/S00-stance.kai").expect("banto stance script");
        assert!(script.contains("kj block create --role system") && script.contains("text/markdown"), "{script}");
        assert!(script.contains("cybernetic system"), "{script}");
        let stance = seed_body("/config/rc/banto/create/S00-stance.md").expect("banto stance");
        let flat = stance.split_whitespace().collect::<Vec<_>>().join(" ");
        for needle in [
            "番頭 (banto)",
            "keep the kernel healthy",
            "careful with that power",
            "without reading their content, unless the human asks us to look",
            "`kj character list` shows the characters",
            "`kj character create <name> --rc <dir> --model <backend>/<model>`",
            "`kj context create <character>`",
        ] {
            assert!(flat.contains(needle), "banto's stance must say {needle:?}: {stance}");
        }
        let help = seed_body("/config/rc/banto/create/S06-kj-help.md").expect("banto kj help");
        let help_flat = help.split_whitespace().collect::<Vec<_>>().join(" ");
        for (body, text) in [(&flat, "stance"), (&help_flat, "kj help")] {
            for stale in ["/config/rc/<type>", "each in the type", "context types", "context character", "a type that selects", "seat"] {
                assert!(!body.contains(stale), "banto's {text} still speaks of {stale:?}: {body}");
            }
        }
        assert!(stance.trim_end().ends_with("\n\n頑張って"), "the stance closes with 頑張って");
    }

    /// The five directories the characters replaced ship nothing; the
    /// person and banto directories that absorbed them carry their bindings.
    #[test]
    fn the_retired_directories_ship_nothing_and_their_successors_bind() {
        assert_eq!(RETIRED_RC_DIRS, ["console", "default", "director", "root", "shell"]);
        for (path, _) in seed_files() {
            let rel = rc_relpath(&path).unwrap();
            let dir = rel.split('/').next().unwrap();
            assert!(!RETIRED_RC_DIRS.contains(&dir), "{path} ships under a retired directory");
        }
        let person = seed_body("/config/rc/person/create/S10-binding.kai").expect("person binding");
        let banto = seed_body("/config/rc/banto/create/S10-binding.kai").expect("banto binding");
        for grant in ["admin", "house", "operator", "system", "exec", "facade:shell_write", "facade:submit_input"] {
            let line = format!("kj binding allow \"{grant}\"\n");
            assert!(person.contains(&line), "person grants {grant}");
            assert!(banto.contains(&line), "banto grants {grant}");
        }
        assert!(banto.contains("kj binding allow \"drive\"\n"), "banto drives the contexts it starts");
        assert!(!person.contains("kj binding allow \"drive\"\n"), "a person refuses drives");
        for script in ["S06-kj-help.kai", "S16-handoff.kai", "S17-predecessor.kai", "S20-cache.kai", "S25-datetime.kai",
                       "S45-shell-guard.kai", "S50-council-seats.kai"] {
            assert!(seed_body(&format!("/config/rc/banto/create/{script}")).is_some(), "banto/create/{script}");
        }
        assert!(seed_body("/config/rc/person/create/S45-shell-guard.kai").is_some());
    }

    /// Plant `director/` as an older seed left it, with a script of the
    /// person's own inside it.
    fn plant_retired_dir(root: &std::path::Path) {
        std::fs::create_dir_all(root.join("director/create")).unwrap();
        std::os::unix::fs::symlink("../../lib/create/S20-cache.kai", root.join("director/create/S20-cache.kai")).unwrap();
        std::fs::write(root.join("director/create/S60-mine.kai"), "kj context set . --env MINE=1\n").unwrap();
    }

    /// The one directory beside `root` that moved aside from `dir`.
    fn aside(root: &std::path::Path, dir: &str) -> Vec<std::path::PathBuf> {
        let prefix = format!("{dir}.retired-");
        let mut found: Vec<_> = std::fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(&prefix))
            .collect();
        found.sort();
        found
    }

    /// A directory the seed no longer has is named, with a trailing `/`, and
    /// left alone, and the files in it that are not links to the shared lib
    /// are named; `--force` moves it to `<dir>.retired-<UTC timestamp>` beside
    /// it with the files inside it and names the new path, deleting nothing; a
    /// `.keep` beside it keeps it through `--force`. A directory the seed
    /// never named is never touched.
    #[test]
    fn reseed_names_a_retired_directory_and_force_moves_it_aside() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        ensure_rc_seed_files(root).unwrap();
        plant_retired_dir(root);
        std::fs::create_dir_all(root.join("bassist/create")).unwrap();
        std::fs::write(root.join("bassist/create/S00-stance.kai"), "kj block create --content bass\n").unwrap();

        let report = reseed_rc_files(root, false).unwrap();
        assert_eq!(report.retired, vec!["director/".to_string()], "{report:?}");
        assert_eq!(report.unshipped, vec!["director/create/S60-mine.kai".to_string()], "only the person's own file is named: {report:?}");
        assert!(root.join("director/create/S60-mine.kai").exists(), "left alone without --force");

        let forced = reseed_rc_files(root, true).unwrap();
        assert!(forced.removed.is_empty(), "nothing is deleted: {forced:?}");
        let moved = aside(root, "director");
        assert_eq!(moved.len(), 1, "{moved:?}");
        assert_eq!(forced.moved.len(), 1, "{forced:?}");
        assert_eq!(forced.moved[0].0, "director/");
        assert_eq!(forced.moved[0].1, moved[0].file_name().unwrap().to_string_lossy());
        assert!(std::fs::symlink_metadata(root.join("director")).is_err(), "the name is free");
        assert_eq!(
            std::fs::read_to_string(moved[0].join("create/S60-mine.kai")).unwrap(),
            "kj context set . --env MINE=1\n",
            "the person's file survives at the new path"
        );
        assert!(root.join("bassist/create/S00-stance.kai").exists(), "a directory the seed never named stays");
        assert!(reseed_rc_files(root, false).unwrap().retired.is_empty());

        plant_retired_dir(root);
        std::fs::write(root.join("director.keep"), "").unwrap();
        let kept = reseed_rc_files(root, true).unwrap();
        assert!(kept.moved.is_empty() && kept.removed.is_empty() && kept.retired.is_empty(), "{kept:?}");
        assert_eq!(kept.kept, vec!["director/".to_string()]);
        assert!(root.join("director/create/S60-mine.kai").exists());
        assert_eq!(aside(root, "director").len(), 1, "no second copy");
    }

    /// A person's own directory that shares a retired name is moved, not
    /// deleted, and a name already taken by an earlier move is not reused.
    #[test]
    fn reseed_force_moves_a_users_own_retired_name_without_overwriting_an_earlier_move() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("shell/create")).unwrap();
        std::fs::write(root.join("shell/create/S10-mine.kai"), "mine\n").unwrap();
        reseed_rc_files(root, true).unwrap();
        std::fs::create_dir_all(root.join("shell/create")).unwrap();
        std::fs::write(root.join("shell/create/S10-again.kai"), "again\n").unwrap();
        reseed_rc_files(root, true).unwrap();
        let moved = aside(root, "shell");
        assert_eq!(moved.len(), 2, "{moved:?}");
        let mut names: Vec<_> = moved.iter().flat_map(|m| std::fs::read_dir(m.join("create")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned())).collect();
        names.sort();
        assert_eq!(names, ["S10-again.kai", "S10-mine.kai"]);
    }

    /// A retired directory that is itself a symlink moves as the link; its
    /// target is neither listed as the person's files nor touched, and the
    /// report names the link and where it points.
    #[test]
    fn reseed_does_not_follow_a_symlinked_retired_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        ensure_rc_seed_files(root).unwrap();
        std::fs::create_dir_all(root.join("elsewhere/create")).unwrap();
        std::fs::write(root.join("elsewhere/create/S10-theirs.kai"), "theirs\n").unwrap();
        std::os::unix::fs::symlink("elsewhere", root.join("shell")).unwrap();

        let report = reseed_rc_files(root, false).unwrap();
        assert_eq!(report.retired, vec!["shell/".to_string()], "{report:?}");
        assert!(report.unshipped.is_empty(), "the link target's files are not listed: {report:?}");
        assert_eq!(report.linked, vec![("shell/".to_string(), "elsewhere".to_string())], "{report:?}");

        let forced = reseed_rc_files(root, true).unwrap();
        assert_eq!(forced.linked, vec![("shell/".to_string(), "elsewhere".to_string())], "{forced:?}");
        let moved = aside(root, "shell");
        assert_eq!(moved.len(), 1, "{moved:?}");
        assert!(std::fs::symlink_metadata(&moved[0]).unwrap().file_type().is_symlink(), "the link itself moved");
        assert!(root.join("elsewhere/create/S10-theirs.kai").exists(), "the target stays");
    }

    /// A move that fails is a reported skip, named with its error; the rest of
    /// the reseed finishes, so no run stops half-written without a report.
    #[test]
    fn a_failed_directory_move_is_reported_and_the_reseed_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("shell/create")).unwrap();
        std::fs::write(root.join("shell/create/S10-mine.kai"), "mine\n").unwrap();
        std::fs::create_dir_all(root.join("director/create")).unwrap();

        let refuse = |from: &std::path::Path, _to: &std::path::Path| -> std::io::Result<()> {
            if from.ends_with("shell") {
                Err(std::io::Error::from_raw_os_error(18))
            } else {
                rename_noreplace(from, _to)
            }
        };
        let report = reseed_rc_files_with(root, true, &refuse).unwrap();
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert_eq!(report.failed[0].0, "shell/");
        assert!(report.failed[0].1.contains("os error 18"), "{report:?}");
        assert!(root.join("shell/create/S10-mine.kai").exists(), "the directory stays where it was");
        assert_eq!(report.moved.len(), 1, "the other directory still moved: {report:?}");
        assert!(report.written > 0, "the seed loop ran: {report:?}");
    }

    /// The move never replaces what is already at the target, even a name
    /// taken after the name was chosen.
    #[test]
    fn the_directory_move_does_not_replace_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("from")).unwrap();
        std::fs::create_dir(root.join("to")).unwrap();
        std::fs::write(root.join("to/theirs"), "x").unwrap();
        let err = rename_noreplace(&root.join("from"), &root.join("to")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(root.join("to/theirs").exists() && root.join("from").exists());
    }

    /// A name taken in the gap between choosing and moving makes the move pick
    /// the next name; the taker's entry is untouched.
    #[test]
    fn a_name_taken_in_the_gap_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("shell/create")).unwrap();
        let first = std::cell::Cell::new(true);
        let racer = |from: &std::path::Path, to: &std::path::Path| -> std::io::Result<()> {
            if first.replace(false) {
                std::fs::create_dir(to).unwrap();
                std::fs::write(to.join("racer"), "x").unwrap();
            }
            rename_noreplace(from, to)
        };
        let report = reseed_rc_files_with(root, true, &racer).unwrap();
        assert!(report.failed.is_empty(), "{report:?}");
        let moved = aside(root, "shell");
        assert_eq!(moved.len(), 2, "{moved:?}");
        assert!(moved.iter().any(|m| m.join("racer").exists()), "the racer's entry survives");
        assert!(moved.iter().any(|m| m.join("create").exists()), "the directory moved to the next name");
    }

    /// A name already taken is never reused: the move gets a numeric suffix.
    #[test]
    fn the_aside_name_is_never_one_that_exists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(aside_target(root, "shell", "S"), root.join("shell.retired-S"));
        std::fs::create_dir(root.join("shell.retired-S")).unwrap();
        assert_eq!(aside_target(root, "shell", "S"), root.join("shell.retired-S-2"));
        std::fs::create_dir(root.join("shell.retired-S-2")).unwrap();
        assert_eq!(aside_target(root, "shell", "S"), root.join("shell.retired-S-3"));
    }

    /// Plant every retired seed path: a link where the seed shipped one, a
    /// file otherwise.
    fn plant_retired(root: &std::path::Path) {
        for rel in RETIRED_RC_SEEDS {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            if rel.starts_with("lib/") || rel.contains("/S51-") {
                std::fs::write(&path, "kj context set . --env RETIRED=1\n").unwrap();
            } else {
                std::os::unix::fs::symlink("../../lib/create/retired.kai", &path).unwrap();
            }
        }
    }

    /// A retired seed is no longer embedded, so install-if-absent never
    /// sees it. Without `force` a reseed names every retired entry still
    /// present and leaves it alone.
    #[test]
    fn reseed_names_retired_seeds_still_present() {
        let dir = tempfile::tempdir().unwrap();
        ensure_rc_seed_files(dir.path()).unwrap();
        plant_retired(dir.path());
        let report = reseed_rc_files(dir.path(), false).unwrap();
        let mut want: Vec<String> = RETIRED_RC_SEEDS.iter().map(|p| p.to_string()).collect();
        want.sort();
        assert_eq!(report.retired, want);
        assert!(report.removed.is_empty());
        for rel in RETIRED_RC_SEEDS {
            assert!(std::fs::symlink_metadata(dir.path().join(rel)).is_ok(), "{rel} was left alone");
        }
    }

    /// With `force` a reseed removes every retired entry, links and files
    /// alike, and names what it removed. Nothing else is touched.
    #[test]
    fn reseed_force_removes_retired_seeds() {
        let dir = tempfile::tempdir().unwrap();
        ensure_rc_seed_files(dir.path()).unwrap();
        plant_retired(dir.path());
        let mine = dir.path().join("coder/create/S60-mine.kai");
        std::fs::write(&mine, "kj context set . --env MINE=1\n").unwrap();
        let report = reseed_rc_files(dir.path(), true).unwrap();
        assert_eq!(report.removed.len(), RETIRED_RC_SEEDS.len(), "{report:?}");
        assert!(report.retired.is_empty());
        for rel in RETIRED_RC_SEEDS {
            assert!(std::fs::symlink_metadata(dir.path().join(rel)).is_err(), "{rel} was removed");
        }
        assert!(mine.exists(), "a script the seed never named stays");
        assert_eq!(reseed_rc_files(dir.path(), false).unwrap().retired, Vec::<String>::new());
    }

    /// A retired path is never also a live seed; reseed would remove what
    /// it just installed.
    #[test]
    fn no_retired_seed_is_embedded() {
        let seeds: Vec<String> = seed_files().into_iter().filter_map(|(p, _)| rc_relpath(&p).map(str::to_string)).collect();
        for rel in RETIRED_RC_SEEDS {
            assert!(!seeds.iter().any(|s| s == rel), "{rel} is both retired and embedded");
        }
    }
}
