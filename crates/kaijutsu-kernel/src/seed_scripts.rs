//! Built-in rc lifecycle scripts, embedded at build time and seeded onto
//! the deployed `/etc/rc` tree (`~/.config/kaijutsu/rc/`) on first boot.
//!
//! These are the defaults a fresh kernel bootstraps with. Two purposes:
//!
//! - `/etc/rc/default/{create,fork,drift}/*-cache.kai` — the prompt-cache
//!   recipe documented in `crates/kaijutsu-kernel/docs/help/kj-cache.md`,
//!   applied to every context that doesn't opt into a different
//!   `context_type`. Without this seed, fresh kernels miss all cache
//!   breakpoints until the user installs them by hand.
//! - `/etc/rc/<type>/**` — the worked examples of real context_types
//!   (coder, mcp, toolie, director, musician). Most ship an `S00-stance`
//!   (`.md`, or `.kai` when the stance tunes itself to the bound model) so the
//!   kernel-side contract is self-contained (independent of any per-client
//!   CLAUDE.md), a binding loadout, and the cache recipe.
//!
//! ## Storage
//!
//! rc scripts are **files**, not table rows. The bodies live as real files
//! under `assets/defaults/rc/` (a 1:1 mirror of the `/etc/rc` tree),
//! embedded here via [`include_dir!`]. The embedded tree IS the manifest:
//! adding or removing a seed is just adding or removing a file under
//! `assets/defaults/rc/` — no Rust edit. Dispatch reads the deployed files;
//! see `kj/lifecycle.rs`.
//!
//! ## Seed contract — bootstrap-once, not a floor
//!
//! The deployed tree is the **live source of truth**: what you edit (via
//! `kj rc edit`, the in-app `vi`, or host `vim`) and what dispatch runs. The
//! embedded defaults bootstrap it **once**, on a genuinely fresh install:
//!
//! - Fresh install (rc tree absent/empty): [`ensure_rc_seed_files`] writes
//!   every embedded default. The server only calls it when the tree is fresh
//!   (see `kaijutsu-server` rpc bootstrap).
//! - Re-open with files intact: untouched. Boot never auto-writes the live
//!   tree again — a script you `rm`'d stays gone, a repo-dropped seed does
//!   not linger or resurrect. Live is truth.
//! - Botched an edit? `kj rc reset <path>` restores that one file from its
//!   embedded seed ([`seed_body`]) — targeted recovery without the repo
//!   checked out. There is no bulk reseed: moving config between live and the
//!   repo is left to the user and kai scripts (git is the bridge).
//!
//! ## Updating the seed
//!
//! Edit (or add/remove) the asset file under `assets/defaults/rc/` to change
//! what fresh installs bootstrap with. This does not touch already-deployed
//! trees (live is truth); `kj rc reset <path>` is the explicit per-file pull.

use include_dir::{include_dir, Dir, DirEntry};

/// The embedded `/etc/rc` seed tree — a 1:1 mirror of `assets/defaults/rc/`,
/// embedded at build time. This is the manifest: every `.kai`/`.md` file
/// under it is a seed, keyed by its path.
static RC_SEED_DIR: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../../assets/defaults/rc");

/// The VFS prefix every rc canonical path lives under. The deployed tree
/// (`~/.config/kaijutsu/rc/...`) and the embedded mirror (`assets/defaults/rc/`)
/// drop this prefix — the host path is `root.join(relpath)`, the embedded
/// lookup key is `relpath`.
pub const RC_VFS_ROOT: &str = "/etc/rc/";

/// Strip the `/etc/rc/` prefix from a canonical rc path. Returns `None`
/// for a path that isn't under the rc root.
fn rc_relpath(canonical: &str) -> Option<&str> {
    canonical.strip_prefix(RC_VFS_ROOT)
}

/// Recursively collect every embedded `.kai`/`.md` seed file as
/// `(canonical /etc/rc path, body)`.
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
                out.push((format!("{RC_VFS_ROOT}{rel}"), body));
            }
        }
    }
}

/// The embedded seed set as `(canonical /etc/rc path, body)` pairs, derived
/// by walking [`RC_SEED_DIR`]. The path encodes
/// `context_type / verb / sort_key / name / ext`; nothing else is stored
/// (provenance comes from the CRDT block's principal on write).
pub fn seed_files() -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    collect_seeds(&RC_SEED_DIR, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The embedded seed body for one canonical rc path, or `None` if no seed
/// ships for it. Powers `kj rc reset <path>`: targeted restore-from-default
/// without the repo checked out.
pub fn seed_body(canonical_path: &str) -> Option<&'static str> {
    let rel = rc_relpath(canonical_path)?;
    RC_SEED_DIR.get_file(rel).and_then(|f| f.contents_utf8())
}

/// Write the embedded seed tree into `root` (the host dir mounted at
/// `/etc/rc`), creating only files that don't already exist. Returns the
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
    let mut written = 0usize;
    for (path, content) in seed_files() {
        let Some(rel) = rc_relpath(&path) else {
            continue;
        };
        let dest = root.join(rel);
        if dest.exists() {
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, content)?;
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(root: &std::path::Path, rel: &str) -> Option<String> {
        std::fs::read_to_string(root.join(rel)).ok()
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
        ] {
            assert!(read(dir.path(), rel).is_some(), "missing seed: {rel}");
        }
        // Cache recipe content round-trips from the embedded asset.
        assert!(read(dir.path(), "default/create/S20-cache.kai")
            .unwrap()
            .contains("kj cache add --target=tools"));
    }

    #[test]
    fn ensure_is_idempotent_user_edits_persist() {
        let dir = tempfile::tempdir().expect("tmpdir");
        ensure_rc_seed_files(dir.path()).expect("seed 1");

        // Edit a seed file, then re-run ensure: the edit must survive
        // (file exists → skipped). The server only calls ensure on a fresh
        // tree, but the within-call "skip existing" contract is what keeps a
        // partial (migrated) tree from being clobbered.
        let target = dir.path().join("default/create/S20-cache.kai");
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
    fn seed_files_derived_from_embedded_tree() {
        let paths: Vec<String> = seed_files().into_iter().map(|(p, _)| p).collect();
        // Spot-check the roles the embedded tree ships.
        for expected in [
            "/etc/rc/default/create/S20-cache.kai",
            "/etc/rc/coder/create/S00-stance.kai",
            "/etc/rc/musician/tick/S10-drive.kai",
            "/etc/rc/musician/create/S00-stance.md",
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
        // A seeded path resolves to its embedded body…
        let body = seed_body("/etc/rc/default/create/S20-cache.kai")
            .expect("default cache seed must exist");
        assert!(body.contains("kj cache add --target=tools"));
        // …and a path with no embedded seed is None (the `kj rc reset` guard).
        assert!(
            seed_body("/etc/rc/none/create/S00-noop.kai").is_none(),
            "unseeded path must not resolve a body"
        );
    }

    /// The musician ships a `tick` (beat) verb script and a stance — the beat
    /// hook and persona that make a created musician self-compose.
    #[test]
    fn musician_seeds_include_beat_tick_verb() {
        assert!(
            seed_body("/etc/rc/musician/tick/S10-drive.kai").is_some(),
            "musician must seed a tick/beat script"
        );
        assert!(
            seed_body("/etc/rc/musician/create/S00-stance.md").is_some(),
            "musician must seed a stance"
        );
        // The tick verb is wired into the rc path grammar.
        let parts = crate::kj::rc::parse_rc_path("/etc/rc/musician/tick/S10-drive.kai")
            .expect("tick rc path must parse");
        assert_eq!(parts.context_type, "musician");
        assert_eq!(parts.verb, "tick");
    }

    /// The musician seeds the hydration-window guard at create — `kj context
    /// hydrate` pins the prefix + sets the sliding tail so a self-driving
    /// musician doesn't re-hydrate its whole history every turn (the cost guard).
    #[test]
    fn musician_seeds_include_hydration_window() {
        let body = seed_body("/etc/rc/musician/create/S30-hydrate.kai")
            .expect("musician must seed the hydration-window script");
        assert!(
            body.contains("kj context hydrate"),
            "the hydrate seed must set a window via `kj context hydrate`"
        );
    }

    /// The musician ALSO seeds a fork-side hydration script — the create script
    /// doesn't run on fork, so a forked player needs its own window re-established
    /// (else it drives at tempo with full history). It branches on KJ_FORK_INFO:
    /// window a thin fork, skip a full clone (which would pin its whole log).
    #[test]
    fn musician_fork_seeds_include_hydration_window() {
        let body = seed_body("/etc/rc/musician/fork/S40-hydrate.kai")
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
}
