//! SQLite-backed SSH public key authorization — a keyring.
//!
//! `auth.db` answers exactly one question: which principal does this
//! fingerprint belong to? It carries no name — the given name a player
//! reads is `characters.name` in `kernel.db`, resolved through
//! `KernelDb::name_for` (`docs/character.md`, "`auth.db` is a keyring").
//!
//! Provides:
//! - SSH public key storage and lookup by fingerprint, resolving to a
//!   [`PrincipalId`]
//! - A bare `principals` bookkeeping row per id that has ever held a key —
//!   no name, just existence, so `add-key` can record a binding with the
//!   kernel down

use kaijutsu_types::PrincipalId;
use rusqlite::{Connection, Result as SqliteResult, params};
use russh::keys::ssh_key::{self, HashAlg};
use std::fs;
use std::path::Path;

/// Database handle for authentication.
pub struct AuthDb {
    conn: Connection,
    /// Owns the throwaway directory a `temporary()` database was opened
    /// under; `None` for every `open()`. Held only for its `Drop` — removing
    /// the directory (and the `.db` file inside it) once nothing can reach
    /// this handle anymore.
    _temp_dir: Option<tempfile::TempDir>,
}

/// An SSH public key record (DB columns not on the resolved principal).
#[derive(Debug, Clone)]
pub struct SshKeyRecord {
    pub fingerprint: String,
    pub principal_id: PrincipalId,
    pub key_type: String,
    pub key_blob: Vec<u8>,
    pub comment: Option<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS principals (
    id BLOB NOT NULL PRIMARY KEY,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS credentials (
    fingerprint TEXT NOT NULL PRIMARY KEY,
    principal_id BLOB NOT NULL,
    kind TEXT NOT NULL DEFAULT 'ssh_key',
    key_type TEXT NOT NULL,
    key_blob BLOB NOT NULL,
    comment TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    last_used_at INTEGER
);
"#;

impl AuthDb {
    /// Initialize connection with required PRAGMAs.
    ///
    /// WAL, so a routine `add-key` against a running server doesn't lock the
    /// whole file: in the old rollback-journal mode a writer holds an
    /// exclusive lock, and a connection authenticating meanwhile retries for
    /// `busy_timeout` and then fails `SQLITE_BUSY`. `kernel.db` has been WAL
    /// since it was written; this brings `auth.db` to the same footing now
    /// that binding a character's key is the normal path, not a once-a-machine
    /// act.
    fn init_connection(conn: &Connection) -> SqliteResult<()> {
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             PRAGMA journal_mode = WAL;",
        )?;
        Ok(())
    }

    /// Open or create an auth database at the given path.
    pub fn open<P: AsRef<Path>>(path: P) -> SqliteResult<Self> {
        if let Some(parent) = path.as_ref().parent() {
            let _ = fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        Self::init_connection(&conn)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn,
            _temp_dir: None,
        })
    }

    /// Open a real, file-backed database under a fresh throwaway directory,
    /// owned by the returned handle: the directory (and the `.db` file
    /// inside it) is removed when this `AuthDb` drops.
    ///
    /// Never `:memory:` — same rule as `KernelDb::temporary()`: a `:memory:`
    /// connection is per-connection state that never touches a file, so it
    /// never exercises real file locking or reopening the way production does.
    ///
    /// Unlike `KernelDb::temporary()`, this is **not** gated behind
    /// `cfg(test)`/`test-util` — `ssh.rs` calls it unconditionally in
    /// production as the fallback when no `--auth-db` path is configured
    /// (ephemeral, all-keys-accepted mode), so gating it the way `KernelDb`
    /// does would remove that mode from a production build. Tests use it too.
    pub fn temporary() -> SqliteResult<Self> {
        let dir = tempfile::tempdir().expect("create temporary auth db directory");
        let conn = Connection::open(dir.path().join("auth.db"))?;
        Self::init_connection(&conn)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn,
            _temp_dir: Some(dir),
        })
    }

    /// Default database path: ~/.local/share/kaijutsu/auth.db
    pub fn default_path() -> std::path::PathBuf {
        kaish_kernel::xdg_data_home()
            .join("kaijutsu")
            .join("auth.db")
    }

    // =========================================================================
    // Authentication (hot path)
    // =========================================================================

    /// Look up a principal by SSH key fingerprint.
    ///
    /// Returns the principal id if the key is authorized, `None` otherwise.
    /// Never compares the SSH login username — the fingerprint alone decides
    /// identity.
    pub fn authenticate(&self, fingerprint: &str) -> SqliteResult<Option<PrincipalId>> {
        let mut stmt = self
            .conn
            .prepare("SELECT principal_id FROM credentials WHERE fingerprint = ?1")?;

        let mut rows = stmt.query(params![fingerprint])?;
        if let Some(row) = rows.next()? {
            let id_bytes: Vec<u8> = row.get(0)?;
            let id = PrincipalId::try_from_slice(&id_bytes).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Blob,
                    "invalid PrincipalId bytes".into(),
                )
            })?;
            Ok(Some(id))
        } else {
            Ok(None)
        }
    }

    /// Update last_used_at for a key (call after successful auth).
    pub fn update_last_used(&self, fingerprint: &str) -> SqliteResult<()> {
        self.conn.execute(
            "UPDATE credentials SET last_used_at = unixepoch() WHERE fingerprint = ?1",
            params![fingerprint],
        )?;
        Ok(())
    }

    // =========================================================================
    // Principal bookkeeping
    // =========================================================================

    /// Check if the database has any credential-holding principal.
    pub fn is_empty(&self) -> SqliteResult<bool> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM principals", [], |row| row.get(0))?;
        Ok(count == 0)
    }

    /// Record a principal's bare existence row if it doesn't already have
    /// one. Never fails on a duplicate — every `add_key`/`rebind_key` call
    /// runs this first so the row exists before the credential does, with no
    /// dependency on `kernel.db` being reachable at write time (the id was
    /// already resolved from a name before this call).
    fn ensure_principal_row(&self, principal_id: PrincipalId) -> SqliteResult<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO principals (id) VALUES (?1)",
            params![principal_id.as_bytes().as_slice()],
        )?;
        Ok(())
    }

    // =========================================================================
    // Legacy name migration
    // =========================================================================
    //
    // Before this melt, `principals` carried `username`/`display_name`.
    // `open()`'s `CREATE TABLE IF NOT EXISTS` is a no-op against an existing
    // table, so opening a pre-melt `auth.db` with this code leaves those
    // columns physically in place — and their NOT NULL constraint then
    // rejects `ensure_principal_row`'s bare `INSERT INTO principals (id)`.
    // The methods below detect that shape, hand the names to the caller (who
    // writes them into `kernel.db` as characters — this file never touches
    // `kernel.db`), and then drop the columns so the table matches `SCHEMA`.
    // `docs/character.md`, "`auth.db` is a keyring".

    /// True when this database still carries the pre-melt `username` column
    /// — the marker that its `principals` rows have a name nothing has
    /// harvested into `kernel.db` yet.
    pub fn has_legacy_names(&self) -> SqliteResult<bool> {
        let mut stmt = self.conn.prepare("PRAGMA table_info(principals)")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get("name")?;
            if name == "username" {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Every pre-melt principal's `(id, username)` pair. Fails loudly if
    /// `has_legacy_names` would return `false` — there is nothing to read.
    pub fn legacy_principal_names(&self) -> SqliteResult<Vec<(PrincipalId, String)>> {
        let mut stmt = self.conn.prepare("SELECT id, username FROM principals")?;
        let rows = stmt.query_map([], |row| {
            let id_bytes: Vec<u8> = row.get(0)?;
            let id = PrincipalId::try_from_slice(&id_bytes).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Blob,
                    "invalid PrincipalId bytes".into(),
                )
            })?;
            let username: String = row.get(1)?;
            Ok((id, username))
        })?;
        rows.collect()
    }

    /// Drop the pre-melt `username`/`display_name` columns, bringing an
    /// upgraded table to the shape `SCHEMA` declares. Call only after every
    /// name `legacy_principal_names` returned has landed in `kernel.db` —
    /// this is the point of no return for those two columns.
    ///
    /// `ALTER TABLE ... DROP COLUMN` refuses a column carrying a UNIQUE
    /// constraint (`username` was `UNIQUE`) — SQLite's autoindex for it
    /// cannot be dropped independently of the table — so this rebuilds
    /// `principals` instead: a fresh table in `SCHEMA`'s shape, every
    /// `(id, created_at)` copied over, the old table replaced.
    ///
    /// Foreign keys go off for the rebuild and back on after. `DROP TABLE`
    /// on a table other rows reference performs an implicit `DELETE FROM`
    /// first when foreign key enforcement is on — which would fire
    /// `credentials`'s legacy `ON DELETE CASCADE` and erase every bound key
    /// the instant `principals` is dropped, taking the very credentials this
    /// migration exists to preserve. `credentials`'s own schema is otherwise
    /// untouched; a pre-existing installation's copy keeps its legacy
    /// `principal_id REFERENCES principals(id) ON DELETE CASCADE` —
    /// harmless from here on, since every write path ensures the referenced
    /// row first, and no id changes in this rebuild.
    pub fn drop_legacy_name_columns(&self) -> SqliteResult<()> {
        self.conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             BEGIN;
             CREATE TABLE principals_new (
                 id BLOB NOT NULL PRIMARY KEY,
                 created_at INTEGER NOT NULL DEFAULT (unixepoch())
             );
             INSERT INTO principals_new (id, created_at) SELECT id, created_at FROM principals;
             DROP TABLE principals;
             ALTER TABLE principals_new RENAME TO principals;
             COMMIT;
             PRAGMA foreign_keys = ON;",
        )?;
        Ok(())
    }

    // =========================================================================
    // SSH key management
    // =========================================================================

    /// Bind a key to an existing principal. Never mints: `principal_id`
    /// comes from the caller having already resolved a character's name
    /// (`kj character create`, then `add-key --as <name>`).
    ///
    /// Fails on a fingerprint that is already bound — `credentials.fingerprint`
    /// is the primary key, so this is an ordinary constraint violation, not a
    /// silent rebind. The caller checks first (`get_key`) so it can name the
    /// current binding rather than surfacing a raw SQLite error; `rebind_key`
    /// is the deliberate move.
    pub fn add_key(
        &self,
        principal_id: PrincipalId,
        key: &ssh_key::PublicKey,
        comment: Option<&str>,
    ) -> SqliteResult<String> {
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let key_type = key.algorithm().to_string();
        let key_blob = key.to_bytes().map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e)))
        })?;

        self.ensure_principal_row(principal_id)?;
        self.conn.execute(
            "INSERT INTO credentials (fingerprint, principal_id, kind, key_type, key_blob, comment)
             VALUES (?1, ?2, 'ssh_key', ?3, ?4, ?5)",
            params![
                fingerprint,
                principal_id.as_bytes().as_slice(),
                key_type,
                key_blob,
                comment
            ],
        )?;
        Ok(fingerprint)
    }

    /// Move an already-bound key to a different principal — the deliberate
    /// counterpart to `add_key`'s refusal. Resets `last_used_at`: the key is
    /// now a fresh binding, and its prior use belonged to the old principal.
    pub fn rebind_key(
        &self,
        principal_id: PrincipalId,
        key: &ssh_key::PublicKey,
        comment: Option<&str>,
    ) -> SqliteResult<String> {
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let key_type = key.algorithm().to_string();
        let key_blob = key.to_bytes().map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e)))
        })?;

        self.ensure_principal_row(principal_id)?;
        self.conn.execute(
            "INSERT INTO credentials (fingerprint, principal_id, kind, key_type, key_blob, comment, last_used_at)
             VALUES (?1, ?2, 'ssh_key', ?3, ?4, ?5, NULL)
             ON CONFLICT(fingerprint) DO UPDATE SET
                 principal_id = excluded.principal_id,
                 key_type = excluded.key_type,
                 key_blob = excluded.key_blob,
                 comment = excluded.comment,
                 last_used_at = NULL",
            params![
                fingerprint,
                principal_id.as_bytes().as_slice(),
                key_type,
                key_blob,
                comment
            ],
        )?;
        Ok(fingerprint)
    }

    /// List all keys for a principal.
    pub fn list_keys(&self, principal_id: PrincipalId) -> SqliteResult<Vec<SshKeyRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT fingerprint, principal_id, key_type, key_blob, comment, created_at, last_used_at
             FROM credentials WHERE principal_id = ?1 ORDER BY created_at",
        )?;

        let rows = stmt.query_map(params![principal_id.as_bytes().as_slice()], row_to_key)?;
        rows.collect()
    }

    /// List every key in the database, ordered by principal then age — the
    /// `list-keys` fingerprint-to-principal table. Resolving a principal to
    /// a character's name is the caller's job (`kernel.db`, not this file).
    pub fn list_all_keys(&self) -> SqliteResult<Vec<SshKeyRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT fingerprint, principal_id, key_type, key_blob, comment, created_at, last_used_at
             FROM credentials ORDER BY principal_id, created_at",
        )?;

        let rows = stmt.query_map([], row_to_key)?;
        rows.collect()
    }

    /// Remove a key by fingerprint.
    pub fn remove_key(&self, fingerprint: &str) -> SqliteResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM credentials WHERE fingerprint = ?1",
            params![fingerprint],
        )?;
        Ok(deleted > 0)
    }

    /// Get a key by fingerprint — the pre-check `add-key` uses to refuse an
    /// already-bound fingerprint by name rather than a raw constraint error.
    pub fn get_key(&self, fingerprint: &str) -> SqliteResult<Option<SshKeyRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT fingerprint, principal_id, key_type, key_blob, comment, created_at, last_used_at
             FROM credentials WHERE fingerprint = ?1",
        )?;

        let mut rows = stmt.query(params![fingerprint])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_key(row)?))
        } else {
            Ok(None)
        }
    }
}

/// Extract an `SshKeyRecord` from a row with columns (fingerprint,
/// principal_id, key_type, key_blob, comment, created_at, last_used_at).
fn row_to_key(row: &rusqlite::Row<'_>) -> SqliteResult<SshKeyRecord> {
    let pid_bytes: Vec<u8> = row.get(1)?;
    let principal_id = PrincipalId::try_from_slice(&pid_bytes).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Blob,
            "invalid PrincipalId".into(),
        )
    })?;
    Ok(SshKeyRecord {
        fingerprint: row.get(0)?,
        principal_id,
        key_type: row.get(2)?,
        key_blob: row.get(3)?,
        comment: row.get(4)?,
        created_at: row.get(5)?,
        last_used_at: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_key() -> ssh_key::PublicKey {
        let private = russh::keys::PrivateKey::random(
            &mut rand_v10::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();
        private.public_key().clone()
    }

    #[test]
    fn test_key_management() {
        let db = AuthDb::temporary().unwrap();
        let key = make_test_key();
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let principal_id = PrincipalId::new();

        // Bind the key to an existing (already-minted) principal.
        let fp = db.add_key(principal_id, &key, Some("test@host")).unwrap();
        assert_eq!(fp, fingerprint);
        assert!(!db.is_empty().unwrap());

        // Authenticate.
        let authed = db.authenticate(&fingerprint).unwrap().unwrap();
        assert_eq!(authed, principal_id);

        // List keys.
        let keys = db.list_keys(principal_id).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].fingerprint, fingerprint);
        assert_eq!(keys[0].comment.as_deref(), Some("test@host"));

        // Update last used.
        db.update_last_used(&fingerprint).unwrap();
        let key_record = db.get_key(&fingerprint).unwrap().unwrap();
        assert!(key_record.last_used_at.is_some());

        // Remove key.
        assert!(db.remove_key(&fingerprint).unwrap());
        assert!(db.authenticate(&fingerprint).unwrap().is_none());
    }

    /// A bound key authenticates to the character's principal — the core
    /// invariant slice 2 exists to prove.
    #[test]
    fn a_bound_key_authenticates_to_its_principal() {
        let db = AuthDb::temporary().unwrap();
        let key = make_test_key();
        let principal_id = PrincipalId::new();
        db.add_key(principal_id, &key, None).unwrap();

        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        assert_eq!(db.authenticate(&fingerprint).unwrap(), Some(principal_id));
    }

    #[test]
    fn test_multiple_keys_per_principal() {
        let db = AuthDb::temporary().unwrap();
        let principal_id = PrincipalId::new();

        let key1 = make_test_key();
        let key2 = make_test_key();

        db.add_key(principal_id, &key1, Some("laptop")).unwrap();
        db.add_key(principal_id, &key2, Some("desktop")).unwrap();

        let keys = db.list_keys(principal_id).unwrap();
        assert_eq!(keys.len(), 2);

        // Both keys should authenticate to the same principal.
        let fp1 = key1.fingerprint(HashAlg::Sha256).to_string();
        let fp2 = key2.fingerprint(HashAlg::Sha256).to_string();

        let p1 = db.authenticate(&fp1).unwrap().unwrap();
        let p2 = db.authenticate(&fp2).unwrap().unwrap();
        assert_eq!(p1, p2);
    }

    #[test]
    fn test_list_all_keys() {
        let db = AuthDb::temporary().unwrap();

        let key1 = make_test_key();
        let key2 = make_test_key();

        db.add_key(PrincipalId::new(), &key1, Some("user1@host")).unwrap();
        db.add_key(PrincipalId::new(), &key2, Some("user2@host")).unwrap();

        let all = db.list_all_keys().unwrap();
        assert_eq!(all.len(), 2);
    }

    /// Re-adding an already-bound fingerprint refuses — `fingerprint` is the
    /// credentials primary key, so a second `add_key` on the same key is an
    /// ordinary constraint violation, never a silent move.
    #[test]
    fn adding_an_already_bound_key_refuses() {
        let db = AuthDb::temporary().unwrap();
        let key = make_test_key();
        let first = PrincipalId::new();
        let second = PrincipalId::new();

        db.add_key(first, &key, None).unwrap();
        let err = db.add_key(second, &key, None).unwrap_err();
        assert!(matches!(err, rusqlite::Error::SqliteFailure(_, _)), "got: {err}");

        // The original binding must be untouched.
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        assert_eq!(db.authenticate(&fingerprint).unwrap(), Some(first));
    }

    /// `rebind_key` is the deliberate move `add_key` refuses to do silently:
    /// the fingerprint now authenticates to the new principal, and the prior
    /// binding's `last_used_at` does not survive the move.
    #[test]
    fn rebind_key_moves_the_binding() {
        let db = AuthDb::temporary().unwrap();
        let key = make_test_key();
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let hajime = PrincipalId::new();
        let amy = PrincipalId::new();

        db.add_key(hajime, &key, Some("bootstrap")).unwrap();
        db.update_last_used(&fingerprint).unwrap();
        assert!(db.get_key(&fingerprint).unwrap().unwrap().last_used_at.is_some());

        db.rebind_key(amy, &key, Some("amy@zorak")).unwrap();

        assert_eq!(db.authenticate(&fingerprint).unwrap(), Some(amy));
        let rebound = db.get_key(&fingerprint).unwrap().unwrap();
        assert_eq!(rebound.comment.as_deref(), Some("amy@zorak"));
        assert!(rebound.last_used_at.is_none(), "a fresh binding has no prior use");

        // Exactly one credential row survives the move — never two.
        assert_eq!(db.list_all_keys().unwrap().len(), 1);
    }

    /// WAL mode is on: `add-key` against a database another connection has
    /// open must not block behind a rollback-journal exclusive lock.
    #[test]
    fn opens_in_wal_mode() {
        let db = AuthDb::temporary().unwrap();
        let mode: String = db
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }
}
