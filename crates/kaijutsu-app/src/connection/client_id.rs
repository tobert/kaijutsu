//! Stable per-installation client id.
//!
//! The kernel keys per-client view state (the `client_views` KernelDb row —
//! see `docs/shared-state.md` "Retiring KV", `setLastContext`/`getClientView`)
//! by this id, so two app instances never clobber each other's "which context
//! am I looking at." That needs a stable id surviving restarts — which the
//! app does not otherwise have (`"bevy-client"` is a fixed instance label,
//! not unique).
//!
//! We seed a UUID at `~/.local/share/kaijutsu/client-id` on first run and read
//! it thereafter. Failure mode, accepted by design: if the seed file is lost the
//! next run mints a new UUID and the prior row orphans under the old id (rows
//! aren't enumerated as "mine" by any other means). At single-user scale
//! that's tolerable.

use std::path::{Path, PathBuf};

use bevy::prelude::Resource;
use uuid::Uuid;

/// The installation's stable client id, as a Bevy resource.
///
/// Seeded once at startup ([`load_or_seed`]). Used to key per-client durable
/// view state so two app instances don't clobber each other's view state.
#[derive(Resource, Clone, Copy, Debug)]
pub struct ClientId(pub Uuid);

/// File name under the data dir holding the client-id UUID (hyphenated text).
const CLIENT_ID_FILE: &str = "client-id";

/// Resolve the data directory the client-id lives in: `~/.local/share/kaijutsu`
/// (XDG `data_local_dir`), falling back to the current dir if XDG can't be
/// resolved (headless/odd environments) so we never panic at startup.
fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("kaijutsu")
}

/// Load the installation's client id, seeding a fresh UUID on first run.
///
/// Uses the XDG data dir. A read/parse/write hiccup degrades to an ephemeral
/// in-memory UUID (logged) rather than failing the app — per-client KV
/// namespacing is a convenience, not a correctness invariant.
pub fn load_or_seed() -> Uuid {
    let dir = data_dir();
    match load_or_seed_in(&dir) {
        Ok(id) => id,
        Err(e) => {
            let ephemeral = Uuid::new_v4();
            log::warn!(
                "client-id: could not persist to {}: {e}; using ephemeral {ephemeral} \
                 (per-client KV state won't survive this run)",
                dir.display(),
            );
            ephemeral
        }
    }
}

/// Load-or-seed against an explicit directory. Pure enough to test: creates the
/// dir if needed, reads an existing valid UUID, or writes a fresh one.
fn load_or_seed_in(dir: &Path) -> std::io::Result<Uuid> {
    let path = dir.join(CLIENT_ID_FILE);

    if let Ok(contents) = std::fs::read_to_string(&path)
        && let Ok(id) = Uuid::parse_str(contents.trim())
    {
        return Ok(id);
    }

    // Absent, unreadable, or corrupt → mint and persist a fresh one. A corrupt
    // file is overwritten rather than honored: a malformed client-id is worse
    // than a new one (it would namespace under garbage).
    std::fs::create_dir_all(dir)?;
    let id = Uuid::new_v4();
    std::fs::write(&path, id.to_string())?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_then_reads_stable() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_seed_in(dir.path()).unwrap();
        // Second call in the same dir returns the same id (persisted).
        let second = load_or_seed_in(dir.path()).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn corrupt_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CLIENT_ID_FILE), "not-a-uuid").unwrap();
        // A garbage file yields a fresh valid id, then stabilizes.
        let id = load_or_seed_in(dir.path()).unwrap();
        let again = load_or_seed_in(dir.path()).unwrap();
        assert_eq!(id, again);
    }
}
