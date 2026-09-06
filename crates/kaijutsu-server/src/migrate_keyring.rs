//! The one-time keyring melt: harvest `auth.db`'s pre-melt usernames into
//! `kernel.db` characters, then drop the columns that named them.
//!
//! Before this slice, `auth.db`'s `principals` table carried a name.
//! `characters.name` in `kernel.db` is the only name in the system now
//! (`docs/character.md`, "`auth.db` is a keyring"), so an existing
//! installation's usernames must land there before `auth.db` sheds the
//! columns that held them — otherwise the melt silently destroys the only
//! record of who those principals were.
//!
//! [`migrate_legacy_names`] is the whole migration: absent-only per name (a
//! principal that already has a sheet is left alone), and it fails loudly on
//! a name collision rather than guessing which principal keeps it. Run once,
//! deliberately, by an operator — never wired into the server's own startup,
//! because a cross-database migration touching a live installation's only
//! identity record is not something to run unattended.

use kaijutsu_kernel::kernel_db::{CharacterRow, KernelDb, KernelDbError};
use thiserror::Error;

use crate::auth_db::AuthDb;

/// A collision the migration refuses to paper over.
#[derive(Debug, Error)]
pub enum MigrateKeyringError {
    #[error("auth.db error: {0}")]
    Auth(#[from] rusqlite::Error),
    #[error("kernel.db error: {0}")]
    Kernel(#[from] KernelDbError),
    /// A legacy username collides with a DIFFERENT principal's existing
    /// character name. Silently keeping one and dropping the other would be
    /// exactly the data loss this migration exists to prevent — the operator
    /// resolves the collision (rename one side) and reruns.
    #[error(
        "'{name}' names both {existing} and {incoming} — resolve the collision \
         (rename one side in kernel.db or auth.db) and rerun the migration"
    )]
    NameCollision {
        name: String,
        existing: String,
        incoming: String,
    },
}

/// Harvest every pre-melt `(principal_id, username)` pair from `auth_db`
/// into `kernel_db` as a character, then drop the columns that held them.
///
/// A no-op, returning `Ok(0)`, when `auth_db` has already been melted
/// (`AuthDb::has_legacy_names` is `false`) — safe to run more than once.
/// A principal that already has a character sheet is left alone: this
/// harvests names that would otherwise be lost, it never overwrites one
/// `kj character create` (or a prior run of this migration) already gave.
///
/// Returns how many new character rows were created.
pub fn migrate_legacy_names(
    auth_db: &AuthDb,
    kernel_db: &mut KernelDb,
) -> Result<usize, MigrateKeyringError> {
    if !auth_db.has_legacy_names()? {
        return Ok(0);
    }

    let legacy = auth_db.legacy_principal_names()?;
    let mut created = 0;
    for (principal_id, username) in &legacy {
        if kernel_db.get_character(*principal_id)?.is_some() {
            // Already has a sheet — a prior migration run, or a character
            // minted independently after this principal was created. Its
            // name is authoritative; the legacy username is not consulted.
            continue;
        }
        if let Some(existing) = kernel_db.get_character_by_name(username)? {
            return Err(MigrateKeyringError::NameCollision {
                name: username.clone(),
                existing: existing.principal_id.short(),
                incoming: principal_id.short(),
            });
        }
        kernel_db.insert_character(&CharacterRow {
            principal_id: *principal_id,
            name: username.clone(),
            created_at: kaijutsu_types::now_millis() as i64,
            retired_at: None,
        })?;
        created += 1;
    }

    // Only once every name has safely landed: the point of no return for
    // the two columns that held them.
    auth_db.drop_legacy_name_columns()?;
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::PrincipalId;
    use rusqlite::Connection;

    /// The pre-melt `auth.db` schema, verbatim — this is what a real,
    /// already-deployed installation's file looks like before this slice.
    const LEGACY_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS principals (
    id BLOB NOT NULL PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS credentials (
    fingerprint TEXT NOT NULL PRIMARY KEY,
    principal_id BLOB NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
    kind TEXT NOT NULL DEFAULT 'ssh_key',
    key_type TEXT NOT NULL,
    key_blob BLOB NOT NULL,
    comment TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    last_used_at INTEGER
);
"#;

    /// Build a fixture that mimics a real, already-deployed `auth.db`:
    /// several principals under the OLD schema, one with a key bound.
    /// Opening it with `AuthDb::open` afterward is exactly what happens when
    /// the new binary meets an old file — `CREATE TABLE IF NOT EXISTS` is a
    /// no-op against the existing legacy table.
    fn legacy_fixture() -> (tempfile::TempDir, PrincipalId, PrincipalId, PrincipalId) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(LEGACY_SCHEMA).unwrap();

        let amy = PrincipalId::new();
        let kaish = PrincipalId::new();
        let bare = PrincipalId::new();
        conn.execute(
            "INSERT INTO principals (id, username, display_name) VALUES (?1, ?2, ?3)",
            rusqlite::params![amy.as_bytes().as_slice(), "amy", "Amy Tobey"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO principals (id, username, display_name) VALUES (?1, ?2, ?3)",
            rusqlite::params![kaish.as_bytes().as_slice(), "kaish", "kaish"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO principals (id, username, display_name) VALUES (?1, ?2, ?3)",
            rusqlite::params![bare.as_bytes().as_slice(), "bare-principal", "bare-principal"],
        )
        .unwrap();
        // Amy has an actual key bound — the credential must survive the
        // migration untouched (same fingerprint, same principal_id).
        conn.execute(
            "INSERT INTO credentials (fingerprint, principal_id, key_type, key_blob)
             VALUES ('SHA256:fixture', ?1, 'ssh-ed25519', X'0102')",
            rusqlite::params![amy.as_bytes().as_slice()],
        )
        .unwrap();
        drop(conn);

        (dir, amy, kaish, bare)
    }

    /// The migration turns every pre-existing principal into a character
    /// carrying its old username, and existing credentials keep
    /// authenticating to the same principal afterward.
    #[test]
    fn migration_turns_principals_into_characters_with_their_old_usernames() {
        let (dir, amy, kaish, bare) = legacy_fixture();
        let auth_db = AuthDb::open(dir.path().join("auth.db")).expect("open legacy auth.db");
        assert!(auth_db.has_legacy_names().unwrap(), "fixture must look pre-melt");

        let mut kernel_db = KernelDb::temporary().unwrap();
        let created = migrate_legacy_names(&auth_db, &mut kernel_db).unwrap();
        assert_eq!(created, 3, "all three legacy principals become characters");

        assert_eq!(kernel_db.get_character(amy).unwrap().unwrap().name, "amy");
        assert_eq!(kernel_db.get_character(kaish).unwrap().unwrap().name, "kaish");
        assert_eq!(
            kernel_db.get_character(bare).unwrap().unwrap().name,
            "bare-principal"
        );

        // The schema is melted: no more legacy columns, and a fresh key can
        // now bind without tripping the old NOT NULL username constraint.
        assert!(!auth_db.has_legacy_names().unwrap());
        assert_eq!(
            auth_db.authenticate("SHA256:fixture").unwrap(),
            Some(amy),
            "the existing credential must still authenticate to the same principal"
        );
    }

    /// Re-running the migration against an already-melted database is a
    /// harmless no-op — idempotent, because a real deploy might run it more
    /// than once by accident.
    #[test]
    fn migration_is_a_no_op_on_an_already_melted_database() {
        let (dir, ..) = legacy_fixture();
        let auth_db = AuthDb::open(dir.path().join("auth.db")).unwrap();
        let mut kernel_db = KernelDb::temporary().unwrap();
        migrate_legacy_names(&auth_db, &mut kernel_db).unwrap();

        let second_run = migrate_legacy_names(&auth_db, &mut kernel_db).unwrap();
        assert_eq!(second_run, 0, "nothing left to migrate");
        assert_eq!(kernel_db.list_characters(true).unwrap().len(), 3, "no duplicates");
    }

    /// A principal that already has a character sheet (e.g. `kj character
    /// create` ran independently before the migration) is left alone — the
    /// migration harvests missing names, it never overwrites an existing one.
    #[test]
    fn migration_does_not_overwrite_an_existing_sheet() {
        let (dir, amy, ..) = legacy_fixture();
        let auth_db = AuthDb::open(dir.path().join("auth.db")).unwrap();
        let mut kernel_db = KernelDb::temporary().unwrap();
        kernel_db
            .insert_character(&CharacterRow {
                principal_id: amy,
                name: "kaijutsu-lead".to_string(),
                created_at: 1,
                retired_at: None,
            })
            .unwrap();

        let created = migrate_legacy_names(&auth_db, &mut kernel_db).unwrap();
        assert_eq!(created, 2, "amy already had a sheet; the other two are new");
        assert_eq!(
            kernel_db.get_character(amy).unwrap().unwrap().name,
            "kaijutsu-lead",
            "an existing sheet's name is never overwritten by the legacy username"
        );
    }

    /// A legacy username colliding with a DIFFERENT principal's existing
    /// character name fails loudly rather than silently dropping one side.
    #[test]
    fn migration_refuses_a_name_collision() {
        let (dir, amy, ..) = legacy_fixture();
        let auth_db = AuthDb::open(dir.path().join("auth.db")).unwrap();
        let mut kernel_db = KernelDb::temporary().unwrap();
        // A DIFFERENT principal already owns the name "amy".
        kernel_db
            .insert_character(&CharacterRow {
                principal_id: PrincipalId::new(),
                name: "amy".to_string(),
                created_at: 1,
                retired_at: None,
            })
            .unwrap();

        let err = migrate_legacy_names(&auth_db, &mut kernel_db).unwrap_err();
        assert!(matches!(err, MigrateKeyringError::NameCollision { .. }), "got: {err}");

        // Refusing must not have melted the schema out from under a retry.
        assert!(auth_db.has_legacy_names().unwrap());
        // And it must not have silently created a character for amy either.
        assert!(kernel_db.get_character(amy).unwrap().is_none());
    }
}
