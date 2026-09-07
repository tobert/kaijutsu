//! The bootstrap character — `hajime`, seeded once per fresh kernel
//! (`docs/character.md`, "Bootstrap: `hajime`, a character that exists to be
//! replaced"). Its rc walks a new user through minting their own character
//! and retiring this one.
//!
//! Absent-only, like [`crate::seed_backends::ensure_factory_backends`]: once
//! `hajime` is retired, `get_character_by_name` still finds the row, so a
//! restart never resurrects it under a fresh principal.

use kaijutsu_types::PrincipalId;

use crate::kernel_db::{CharacterRow, KernelDb, KernelDbResult};

/// The bootstrap character's kernel-owned name.
pub const HAJIME: &str = "hajime";

/// Idempotently seed the bootstrap character. Its principal id is MINTED,
/// not derived: a well-known id would make `hajime` the identical character
/// on every install, and one nobody could rotate. Returns the row — freshly
/// minted on the first call, the one already there (retired or not) on
/// every call after.
pub fn ensure_hajime(db: &mut KernelDb) -> KernelDbResult<CharacterRow> {
    if let Some(existing) = db.get_character_by_name(HAJIME)? {
        return Ok(existing);
    }
    let row = CharacterRow {
        principal_id: PrincipalId::new(),
        name: HAJIME.to_string(),
        created_at: kaijutsu_types::now_millis() as i64,
        retired_at: None,
        handoff_ctx: None,
    };
    db.insert_character(&row)?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh kernel seeds exactly one character, named `hajime`.
    #[test]
    fn seeds_exactly_one_character() {
        let mut db = KernelDb::temporary().unwrap();
        let row = ensure_hajime(&mut db).unwrap();
        assert_eq!(row.name, HAJIME);
        assert!(row.retired_at.is_none());

        let all = db.list_characters(true).unwrap();
        assert_eq!(all.len(), 1, "a fresh kernel must seed exactly one character");
        assert_eq!(all[0].name, HAJIME);
    }

    /// A restart calls `ensure_hajime` again — it must not mint a second
    /// principal under the same name.
    #[test]
    fn is_idempotent_across_restarts() {
        let mut db = KernelDb::temporary().unwrap();
        let first = ensure_hajime(&mut db).unwrap();
        let second = ensure_hajime(&mut db).unwrap();
        assert_eq!(first.principal_id, second.principal_id);
        assert_eq!(db.list_characters(true).unwrap().len(), 1);
    }

    /// Once retired, a restart's `ensure_hajime` must not resurrect it — a
    /// retired character stays retired, and its principal id never changes.
    #[test]
    fn a_retired_hajime_is_not_resurrected() {
        let mut db = KernelDb::temporary().unwrap();
        let seeded = ensure_hajime(&mut db).unwrap();
        assert!(db.retire_character(seeded.principal_id, 12345).unwrap());

        let after_restart = ensure_hajime(&mut db).unwrap();
        assert_eq!(after_restart.principal_id, seeded.principal_id);
        assert!(after_restart.retired_at.is_some());
        assert_eq!(db.list_characters(true).unwrap().len(), 1);
    }
}
