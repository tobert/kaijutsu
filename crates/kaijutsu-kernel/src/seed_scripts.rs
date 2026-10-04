//! Built-in rc lifecycle scripts, embedded at build time and seeded onto
//! the deployed `/config/rc` tree (`~/.config/kaijutsu/config/rc/`) on first boot.
//!
//! A seed whose whole body is a path naming another seed is written as a real
//! symlink, not as a file holding that text — this is the init.d composition
//! format, and `include_dir!` cannot carry a link. See `docs/rc-on-disk.md`.
//!
//! These are the defaults a fresh kernel bootstraps with. Two purposes:
//!
//! - `/config/rc/default/{create,fork,drift}/*-cache.kai` — the prompt-cache
//!   recipe (`kj cache --help`),
//!   applied to every context that doesn't opt into a different
//!   `context_type`. Without this seed, fresh kernels miss all cache
//!   breakpoints until the user installs them by hand.
//! - `/config/rc/<type>/**` — the worked examples of real context_types
//!   (coder, mcp, toolie, director, musician). Most ship an `S00-stance`
//!   `.kai` script that authors instructions, reading `.md` data when needed, so the
//!   kernel-side contract is self-contained (independent of any per-client
//!   CLAUDE.md), a binding loadout, and the cache recipe.
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
/// `context_type / verb / sort_key / name / ext`; nothing else is stored
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
    /// Retired seed paths ([`RETIRED_RC_SEEDS`]) still present and left
    /// alone. Always empty with `force`, which removes them instead.
    pub retired: Vec<String>,
    /// Retired seed paths this reseed removed. Always empty without `force`.
    pub removed: Vec<String>,
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
    "default/create/S00-base.kai",
    "default/create/S00-base.md",
    "director/create/S00-base.kai",
    "director/create/S00-base.md",
    "mcp/create/S00-stance.kai",
    "mcp/create/S00-stance.md",
    "coder/create/S05-kaish.kai",
    "coder/create/S11-done.kai",
    "default/create/S05-kaish.kai",
    "director/create/S05-kaish.kai",
    "lib/create/S05-kaish.kai",
    "mcp/create/S05-kaish.kai",
    "toolie/create/S05-kaish.kai",
    "coder/create/S50-lfm2d.kai",
    "default/create/S50-lfm2d.kai",
    "director/create/S50-lfm2d.kai",
    "director/create/S51-lfm2d-observe.kai",
    "lib/create/S50-lfm2d.kai",
    "lib/hooks/lfm2d.kai",
    "mcp/create/S50-lfm2d.kai",
    "root/create/S50-lfm2d.kai",
    "root/create/S51-lfm2d-observe.kai",
    "toolie/create/S50-lfm2d.kai",
    "toolie/create/S45-shell-guard.kai",
];

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
/// A retired seed ([`RETIRED_RC_SEEDS`]) still present is named in
/// [`RcSeedReport::retired`] and left alone; with `force` it is removed and
/// named in [`RcSeedReport::removed`]. `force` deletes nothing else, so a
/// script you added under any other path is never touched. Use `git diff` to see what a
/// forced reseed changed.
///
/// Per the crash-over-corruption stance this surfaces I/O errors rather than
/// swallowing them: a half-written seed tree is corruption.
pub fn reseed_rc_files(
    root: &std::path::Path,
    force: bool,
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
        if force {
            std::fs::remove_file(&dest)?;
            report.removed.push(rel.to_string());
        } else {
            report.retired.push(rel.to_string());
        }
    }
    report.retired.sort();
    report.removed.sort();
    Ok(report)
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
        let data = dir.path().join("default/create/S00-stance.md");
        std::fs::create_dir_all(data.parent().unwrap()).unwrap();
        std::fs::write(&data, "custom instructions\n").unwrap();
        let report = reseed_rc_files(dir.path(), false).unwrap();
        assert!(report.diverged.contains(&"default/create/S00-stance.md".to_string()));
        assert_eq!(std::fs::read_to_string(&data).unwrap(), "custom instructions\n");
        assert!(read(dir.path(), "default/create/S00-stance.kai").unwrap().contains("kj block create"));
        assert!(dir.path().join("default/create/S20-cache.kai").is_symlink());
        reseed_rc_files(dir.path(), true).unwrap();
        assert_eq!(std::fs::read_to_string(data).unwrap(), seed_body("/config/rc/default/create/S00-stance.md").unwrap());
    }

    #[test]
    fn fresh_tree_gets_default_and_coder_seeds() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let n = ensure_rc_seed_files(dir.path()).expect("seed");
        assert_eq!(n, seed_files().len(), "every seed file should be written");

        for rel in [
            "default/create/S20-cache.kai",
            "default/fork/S30-cache.kai",
            "default/drift/S40-cache.kai",
            "coder/create/S00-stance.kai",
            "coder/create/S20-cache.kai",
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
            read(dir.path(), "default/create/S20-cache.kai"),
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

        for context_type in ["coder", "default", "director"] {
            let path = format!("/config/rc/{context_type}/create/S00-stance.md");
            let stance = seed_body(&path).unwrap_or_else(|| panic!("missing {path}"));
            let stance = stance.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                stance.contains("accountable"),
                "{context_type} names who is accountable for the work in its own stance"
            );
        }
    }

    /// Instruction text is read by models that are mostly not English-first.
    /// Shipped stances use plain ASCII English: no em dashes and no other
    /// languages. The one exception is a closing 頑張って line, which is
    /// encouragement, not instruction. A person's own prompts may use any
    /// language.
    #[test]
    fn shipped_stances_are_plain_ascii_english() {
        for path in [
            "/config/rc/coder/create/S00-stance.md",
            "/config/rc/default/create/S00-stance.md",
            "/config/rc/director/create/S00-stance.md",
            "/config/rc/director/create/S00-stance.kai",
            "/config/rc/assistant/create/S00-stance.kai",
        ] {
            let body = seed_body(path).unwrap_or_else(|| panic!("missing {path}"));
            let trimmed = body.trim_end();
            let body = trimmed.strip_suffix("頑張って").unwrap_or(trimmed);
            let foreign: Vec<char> = body.chars().filter(|c| !c.is_ascii()).collect();
            assert!(foreign.is_empty(), "{path} has non-ASCII characters: {foreign:?}");
        }
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
        let link = root.join("default/create/S20-cache.kai");
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
        let link = root.join("default/create/S20-cache.kai");
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
        // it changes the body every linking context_type runs. The document
        // backend refused this through an rc verb; the filesystem does not,
        // and that is the accepted trade for plain files. Pinned so the change
        // in meaning is a decision on the record rather than a surprise.
        let dir = tempfile::tempdir().expect("tmpdir");
        ensure_rc_seed_files(dir.path()).expect("seed");
        let root = dir.path();

        std::fs::write(root.join("default/create/S20-cache.kai"), "# edited\n")
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
            std::fs::symlink_metadata(root.join("default/create/S20-cache.kai"))
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
            "/config/rc/default/create/S20-cache.kai",
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
            seed_body("/config/rc/default/create/S20-cache.kai").unwrap().trim(),
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
        assert_eq!(parts.context_type, "musician");
        assert_eq!(parts.verb, "tick");
    }

    /// The shell tools' descriptions carry the one statement of kaish, so no
    /// seat seeds a second one, and reseed removes the copies earlier seeds
    /// installed.
    #[test]
    fn no_seat_seeds_a_kaish_primer() {
        for ty in ["coder", "default", "mcp", "director", "toolie", "musician", "lib"] {
            let rel = format!("{ty}/create/S05-kaish.kai");
            assert!(seed_body(&format!("/config/rc/{rel}")).is_none(), "{rel} is still seeded");
            if ty != "musician" {
                assert!(RETIRED_RC_SEEDS.contains(&rel.as_str()), "{rel} is not retired");
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
