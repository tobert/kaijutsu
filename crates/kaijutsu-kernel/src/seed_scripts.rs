//! Built-in rc lifecycle scripts, embedded at build time and seeded onto
//! the deployed `/etc/rc` tree (`~/.config/kaijutsu/rc/`) on first boot.
//!
//! These are the "floor" that every kernel boots with. Two purposes:
//!
//! - `/etc/rc/default/{create,fork,drift}/*-cache.kai` — the prompt-cache
//!   recipe documented in `crates/kaijutsu-kernel/docs/help/kj-cache.md`,
//!   applied to every context that doesn't opt into a different
//!   `context_type`. Without this seed, fresh kernels miss all cache
//!   breakpoints until the user installs them by hand.
//! - `/etc/rc/<type>/**` — the worked examples of real context_types
//!   (coder, mcp, explorer, director). Each ships an `S00-stance.md` so the
//!   kernel-side contract is self-contained (independent of any per-client
//!   CLAUDE.md), a binding loadout, and the cache recipe.
//!
//! ## Storage
//!
//! rc scripts are **files**, not table rows. The bodies live as real files
//! under `assets/defaults/rc/` (a 1:1 mirror of the `/etc/rc` tree),
//! embedded here via `include_str!`. [`ensure_rc_seed_files`] writes them to
//! the deployed tree if absent (the floor); [`reseed_rc_files`] force-
//! overwrites from these defaults. Dispatch reads the deployed files; see
//! `kj/lifecycle.rs`.
//!
//! ## Seed contract
//!
//! Boot writes each embedded default to disk **only if the file is absent**:
//!
//! - Fresh install: every seed file is created.
//! - Re-open with files intact: no-op (file exists → skipped).
//! - User `rm`'d a seed: it reappears on the next boot. The seed is the
//!   floor, not a one-time gift. To override a seed, edit the file in place
//!   (`kj rc edit` or host `vim`) rather than removing it.
//!
//! ## Updating the seed
//!
//! Edit the asset file under `assets/defaults/rc/` to change what fresh
//! installs get — but not what already-deployed trees have (the file
//! exists, so the floor skips it). `kj rc reseed` is the explicit push-
//! updates path: it overwrites matching files from these embedded defaults.

// Bodies live as real files under `assets/defaults/rc/` (a 1:1 mirror of
// the `/etc/rc` tree) so they can be edited with an external editor and
// reviewed as files. `include_str!` embeds them at build time. Shared
// recipes (cache, permissive binding) appear at multiple `/etc/rc` paths;
// each const points at one representative copy in the mirror.
const CACHE_CREATE_BODY: &str =
    include_str!("../../../assets/defaults/rc/default/create/S20-cache.kai");

const CACHE_FORK_BODY: &str =
    include_str!("../../../assets/defaults/rc/default/fork/S30-cache.kai");

const CACHE_DRIFT_BODY: &str =
    include_str!("../../../assets/defaults/rc/default/drift/S40-cache.kai");

const CODER_STANCE_BODY: &str =
    include_str!("../../../assets/defaults/rc/coder/create/S00-stance.md");

const MCP_STANCE_BODY: &str =
    include_str!("../../../assets/defaults/rc/mcp/create/S00-stance.md");

const EXPLORER_STANCE_BODY: &str =
    include_str!("../../../assets/defaults/rc/explorer/create/S00-stance.md");

const DIRECTOR_STANCE_BODY: &str =
    include_str!("../../../assets/defaults/rc/director/create/S00-stance.md");

const PERMISSIVE_BINDING_BODY: &str =
    include_str!("../../../assets/defaults/rc/default/create/S10-binding.kai");

const EXPLORER_BINDING_BODY: &str =
    include_str!("../../../assets/defaults/rc/explorer/create/S10-binding.kai");

const DIRECTOR_BINDING_BODY: &str =
    include_str!("../../../assets/defaults/rc/director/create/S10-binding.kai");

/// The embedded seed manifest: `(canonical /etc/rc path, body)`. The path
/// encodes `context_type / verb / sort_key / name / ext`; nothing else is
/// stored (provenance comes from the CRDT block's principal on write).
///
/// S10 bindings must precede any rc script that calls a broker tool:
/// deny-by-default means tool calls before the loadout is assigned would be
/// refused.
const SEED_FILES: &[(&str, &str)] = &[
    // ── default — broad loadout + cache recipe ──────────────────────────
    ("/etc/rc/default/create/S10-binding.kai", PERMISSIVE_BINDING_BODY),
    ("/etc/rc/default/create/S20-cache.kai", CACHE_CREATE_BODY),
    ("/etc/rc/default/fork/S30-cache.kai", CACHE_FORK_BODY),
    ("/etc/rc/default/drift/S40-cache.kai", CACHE_DRIFT_BODY),
    // ── coder — stance + broad loadout + cache ──────────────────────────
    ("/etc/rc/coder/create/S00-stance.md", CODER_STANCE_BODY),
    ("/etc/rc/coder/create/S10-binding.kai", PERMISSIVE_BINDING_BODY),
    ("/etc/rc/coder/create/S20-cache.kai", CACHE_CREATE_BODY),
    ("/etc/rc/coder/fork/S30-cache.kai", CACHE_FORK_BODY),
    ("/etc/rc/coder/drift/S40-cache.kai", CACHE_DRIFT_BODY),
    // ── mcp — stance + broad loadout + cache (register_session) ──────────
    ("/etc/rc/mcp/create/S00-stance.md", MCP_STANCE_BODY),
    ("/etc/rc/mcp/create/S10-binding.kai", PERMISSIVE_BINDING_BODY),
    ("/etc/rc/mcp/create/S20-cache.kai", CACHE_CREATE_BODY),
    ("/etc/rc/mcp/fork/S30-cache.kai", CACHE_FORK_BODY),
    ("/etc/rc/mcp/drift/S40-cache.kai", CACHE_DRIFT_BODY),
    // ── explorer — read-only role (stance + binding + cache) ─────────────
    ("/etc/rc/explorer/create/S00-stance.md", EXPLORER_STANCE_BODY),
    ("/etc/rc/explorer/create/S10-binding.kai", EXPLORER_BINDING_BODY),
    ("/etc/rc/explorer/create/S20-cache.kai", CACHE_CREATE_BODY),
    ("/etc/rc/explorer/fork/S30-cache.kai", CACHE_FORK_BODY),
    ("/etc/rc/explorer/drift/S40-cache.kai", CACHE_DRIFT_BODY),
    // ── director — coordination role (stance + binding + cache) ──────────
    ("/etc/rc/director/create/S00-stance.md", DIRECTOR_STANCE_BODY),
    ("/etc/rc/director/create/S10-binding.kai", DIRECTOR_BINDING_BODY),
    ("/etc/rc/director/create/S20-cache.kai", CACHE_CREATE_BODY),
    ("/etc/rc/director/fork/S30-cache.kai", CACHE_FORK_BODY),
    ("/etc/rc/director/drift/S40-cache.kai", CACHE_DRIFT_BODY),
];

/// The VFS prefix every rc canonical path lives under. The deployed tree
/// (`~/.config/kaijutsu/rc/...`) and the embedded mirror (`assets/defaults/rc/`)
/// drop this prefix — the host path is `root.join(relpath)`.
pub const RC_VFS_ROOT: &str = "/etc/rc/";

/// Strip the `/etc/rc/` prefix from a canonical rc path. Returns `None`
/// for a path that isn't under the rc root (shouldn't happen for seeds).
fn rc_relpath(canonical: &str) -> Option<&str> {
    canonical.strip_prefix(RC_VFS_ROOT)
}

/// The context_type segment of a canonical rc path (`/etc/rc/<type>/...`).
fn context_type_of(canonical: &str) -> Option<&str> {
    rc_relpath(canonical).and_then(|rel| rel.split('/').next())
}

/// The embedded seed manifest as `(canonical /etc/rc path, body)` pairs.
/// `kj rc reseed` writes these back through the file cache.
pub fn seed_files() -> &'static [(&'static str, &'static str)] {
    SEED_FILES
}

/// The set of context_types we ship seeds for. `kj rc reseed --type X`
/// rejects values outside this list so typos don't silently no-op.
pub fn seeded_context_types() -> Vec<&'static str> {
    let mut types: Vec<&'static str> = SEED_FILES
        .iter()
        .filter_map(|(path, _)| context_type_of(path))
        .collect();
    types.sort();
    types.dedup();
    types
}

/// Write the embedded seed tree into `root` (the host dir mounted at
/// `/etc/rc`), creating only files that don't already exist — the "floor"
/// contract, mirroring `config_backend`'s write-default-if-missing. A
/// user's edit persists (file present → skipped); a deleted seed reappears
/// next boot. Returns the number of files newly written.
///
/// Per the crash-over-corruption stance this surfaces I/O errors rather
/// than swallowing them: a half-written seed tree is corruption, and the
/// caller decides whether a fork can proceed without its stance script.
pub fn ensure_rc_seed_files(root: &std::path::Path) -> std::io::Result<usize> {
    let mut written = 0usize;
    for (path, content) in SEED_FILES {
        let Some(rel) = rc_relpath(path) else {
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

/// Force-overwrite the deployed seed files from the embedded defaults —
/// the floor's explicit push-updates path, powering `kj rc reseed`.
/// `type_filter`, if `Some`, narrows to one context_type. Returns the
/// number of files rewritten.
pub fn reseed_rc_files(
    root: &std::path::Path,
    type_filter: Option<&str>,
) -> std::io::Result<usize> {
    let mut count = 0usize;
    for (path, content) in SEED_FILES {
        if let Some(t) = type_filter {
            if context_type_of(path) != Some(t) {
                continue;
            }
        }
        let Some(rel) = rc_relpath(path) else {
            continue;
        };
        let dest = root.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, content)?;
        count += 1;
    }
    Ok(count)
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
        assert_eq!(n, SEED_FILES.len(), "every seed file should be written");

        for rel in [
            "default/create/S20-cache.kai",
            "default/fork/S30-cache.kai",
            "default/drift/S40-cache.kai",
            "coder/create/S00-stance.md",
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
        // (file exists → skipped).
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
    fn reseed_overwrites_user_edits() {
        let dir = tempfile::tempdir().expect("tmpdir");
        ensure_rc_seed_files(dir.path()).expect("seed");
        let target = dir.path().join("default/create/S20-cache.kai");
        std::fs::write(&target, "# user override").expect("edit");

        let count = reseed_rc_files(dir.path(), None).expect("reseed");
        assert!(count > 0, "reseed should touch >0 files");
        assert!(
            std::fs::read_to_string(&target)
                .unwrap()
                .contains("kj cache add --target=tools"),
            "reseed didn't restore the embedded body"
        );
    }

    #[test]
    fn reseed_with_type_filter_skips_others() {
        let dir = tempfile::tempdir().expect("tmpdir");
        ensure_rc_seed_files(dir.path()).expect("seed");
        let default_f = dir.path().join("default/create/S20-cache.kai");
        let coder_f = dir.path().join("coder/create/S20-cache.kai");
        std::fs::write(&default_f, "# default edited").unwrap();
        std::fs::write(&coder_f, "# coder edited").unwrap();

        reseed_rc_files(dir.path(), Some("coder")).expect("reseed coder");

        assert!(
            std::fs::read_to_string(&coder_f).unwrap().contains("kj cache add"),
            "coder should be restored"
        );
        assert_eq!(
            std::fs::read_to_string(&default_f).unwrap(),
            "# default edited",
            "default edit should be preserved"
        );
    }

    #[test]
    fn seeded_context_types_covers_roles() {
        let types = seeded_context_types();
        for t in ["default", "coder", "mcp", "explorer", "director"] {
            assert!(types.contains(&t), "missing context_type: {t}");
        }
    }
}
