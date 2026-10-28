//! Stable per-installation id for this terminal client.
//!
//! The kernel keys per-client view state (`setLastContext`/`getClientView`,
//! the `client_views` row; `docs/shared-state.md`, "Retiring KV") by this id,
//! so the TUI and the app on one machine never overwrite each other's "which
//! context am I looking at." The id survives restarts, which the SSH identity
//! does not: two terminals of one user share a principal.
//!
//! The id is a UUID seeded under the XDG data dir on first run and read
//! thereafter. If the file is lost, the next run mints a new id and the old
//! row orphans, which is tolerable at single-user scale.

use std::path::{Path, PathBuf};

use uuid::Uuid;

/// File under `<data dir>/kaijutsu/` that holds this client's UUID. Separate
/// from the app's `client-id`, so the two clients never share a row.
const CLIENT_ID_FILE: &str = "tui-client-id";

/// The TUI's client id, seeded on first use. A read or write failure falls
/// back to an ephemeral id and a warning: per-client state is a convenience,
/// not a correctness invariant, so it must not stop the TUI from starting.
pub fn load_or_seed() -> Uuid {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("kaijutsu");
    match load_or_seed_in(&dir) {
        Ok(id) => id,
        Err(e) => {
            let ephemeral = Uuid::new_v4();
            tracing::warn!(
                dir = %dir.display(),
                error = %e,
                id = %ephemeral,
                "client id could not be persisted; last-context will not survive this run"
            );
            ephemeral
        }
    }
}

/// Read the id in `dir`, or mint and write a fresh one. A corrupt file is
/// replaced rather than honored: a malformed id would namespace under garbage.
fn load_or_seed_in(dir: &Path) -> std::io::Result<Uuid> {
    let path = dir.join(CLIENT_ID_FILE);
    if let Ok(contents) = std::fs::read_to_string(&path)
        && let Ok(id) = Uuid::parse_str(contents.trim())
    {
        return Ok(id);
    }
    std::fs::create_dir_all(dir)?;
    let id = Uuid::new_v4();
    std::fs::write(&path, id.to_string())?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_directory_seeds_an_id_that_the_next_load_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let seeded = load_or_seed_in(dir.path()).unwrap();
        assert_eq!(load_or_seed_in(dir.path()).unwrap(), seeded);
    }

    #[test]
    fn a_corrupt_file_is_replaced_not_honored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CLIENT_ID_FILE), "not-a-uuid").unwrap();
        let id = load_or_seed_in(dir.path()).unwrap();
        assert_eq!(load_or_seed_in(dir.path()).unwrap(), id);
        assert_ne!(id.to_string(), "not-a-uuid");
    }
}
